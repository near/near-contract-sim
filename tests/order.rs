//! Receipt order: `Fault::Hold` and `Fault::HoldResult`, injected directly
//! and by `explore`'s scan, on the fuzz contract. A hold keeps a receipt back as nearcore's outgoing
//! buffer can, so a receipt created later goes ahead of it; a callback
//! still waits for its results; held receipts go out in the order they
//! were held, ahead of the block's new receipts; and the chain isn't idle,
//! or snapshotted, while one is held.

mod fuzz_common;
use fuzz_common::*;
use near_contract_sim::explore::{
    Faults, Harness, Op as XOp, OpDone, Plan, Skip, Verdict, submit_injected,
};
use near_contract_sim::{Balance, CryptoHash, Fault, Gas, PendingState, Receipts, Sim, TxOutcome};
use serde::{Deserialize, Serialize};
use std::panic::{AssertUnwindSafe, catch_unwind};

/// `c.near` and `d.near` run the fuzz contract; `u0.near`..`u2.near` call
/// them.
fn order_sim() -> Sim {
    let mut sim = Sim::new();
    for c in ["c.near", "d.near"] {
        sim.create_account(c, Balance::from_near(1_000));
        sim.deploy(c, fuzz_wasm());
    }
    for i in 0..3 {
        sim.create_account(format!("u{i}.near"), Balance::from_near(100));
    }
    sim
}

/// A call from `c.near` to `to` that runs `then`, with 10 Tgas for itself
/// and the gas of the calls `then` makes.
fn forward(to: &str, then: &[Op]) -> Op {
    let inner = then.iter().map(|op| match op {
        Op::Call { gas, .. } | Op::Then { gas, .. } => *gas,
        _ => 0,
    });
    let gas = 10 * TGAS + inner.sum::<u64>();
    Op::Call { to: to.into(), args: encode(then), deposit: 0, gas, weight: 0 }
}

/// `user` asks `c.near` to run `prog`.
fn ask<'a>(sim: &'a mut Sim, user: &str, prog: &[Op]) -> near_contract_sim::Tx<'a> {
    sim.call(user, "c.near", "run").args(encode(prog)).gas(Gas::from_teragas(200))
}

/// `user` asks `c.near` to run `prog`, as an op.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct Ask {
    user: String,
    prog: Vec<Op>,
}

fn asks(user: &str, prog: &[Op]) -> Ask {
    Ask { user: user.into(), prog: prog.to_vec() }
}

impl XOp for Ask {
    fn submit(&self, sim: &mut Sim) -> Result<(), Skip> {
        ask(sim, &self.user, &self.prog).submit();
        Ok(())
    }
}

/// Each of `faults` for each of `blocks`, at every receipt it fits.
fn holds(faults: &[fn(u64) -> Fault], blocks: &[u64]) -> Faults {
    let all = faults.iter().flat_map(|f| blocks.iter().map(move |&b| f(b)));
    Faults::new(Receipts::all(), all)
}

/// The receipt of `out` that logged `line`.
fn logged<'a>(out: &'a TxOutcome, line: &str) -> &'a near_contract_sim::ReceiptOutcome {
    let r = out.receipts.iter().find(|r| r.logs.iter().any(|l| l == line));
    r.unwrap_or_else(|| panic!("no receipt logged {line:?}:\n{out}"))
}

/// Where each receipt ran: (height, position among the receipts that ran
/// in blocks `from..`).
fn ran_at(sim: &Sim, from: u64, id: &CryptoHash) -> (u64, usize) {
    let all = sim.receipts_in(from..);
    let pos = all.iter().position(|r| r.receipt_id == *id).expect("it ran");
    (all[pos].block_height, pos)
}

/// `c.near` sends itself two calls in one block, `x` and `wrap`, and
/// `wrap` sends itself a third, `late`. Held for 2 blocks, `x` runs after
/// `late`, which was created a block after it on the same route.
#[test]
fn a_later_self_receipt_overtakes_a_held_one() {
    let prog = [
        forward("c.near", &[Op::Log("x".into())]),
        forward("c.near", &[forward("c.near", &[Op::Log("late".into())])]),
    ];
    let mut sim = order_sim();
    let plain = ask(&mut sim.clone(), "u0.near", &prog).transact();
    assert!(logged(&plain, "x").block_height < logged(&plain, "late").block_height, "{plain}");

    let out = ask(&mut sim, "u0.near", &prog).inject(Receipts::index(0), Fault::Hold(2)).transact();
    out.assert_success();
    let (x, late) = (logged(&out, "x"), logged(&out, "late"));
    assert_eq!(x.block_height, logged(&plain, "x").block_height + 2, "{out}");
    assert_eq!((x.held, late.held), (Some(2), None));
    assert!(out.to_string().contains("[held 2 blocks]"), "{out}");
    assert!(late.block_height < x.block_height, "{out}");
    sim.findings().assert_none();
}

/// The same across two users' transactions: `u1`'s self-call, created a
/// block after `u0`'s, overtakes it while `u0`'s is held.
#[test]
fn another_users_later_receipt_overtakes_a_held_one() {
    let mut sim = order_sim();
    let mark = sim.height();
    let a = ask(&mut sim, "u0.near", &[forward("c.near", &[Op::Log("a".into())])])
        .inject(Receipts::index(0), Fault::Hold(2))
        .submit();
    sim.step();
    let b = ask(&mut sim, "u1.near", &[forward("c.near", &[Op::Log("b".into())])]).submit();
    sim.settle();
    let (a, b) = (sim.outcome(a), sim.outcome(b));
    let (a, b) = (
        ran_at(&sim, mark, &logged(&a, "a").receipt_id),
        ran_at(&sim, mark, &logged(&b, "b").receipt_id),
    );
    assert!(b < a, "b at {b:?}, a at {a:?}");
    sim.findings().assert_none();
}

/// A callback runs only once its result has arrived, whatever is held:
/// the call it waits on, the callback itself, or the data receipt that
/// carries the result. It always reads the call's success.
#[test]
fn a_callback_never_runs_before_its_result() {
    let prog = [
        forward("d.near", &[Op::Log("callee".into())]),
        Op::Then { args: encode(&[Op::Results]), gas: 20 * TGAS, weight: 0 },
    ];
    let sim = order_sim();
    let plain = ask(&mut sim.clone(), "u0.near", &prog).transact();
    let callback_at = |out: &TxOutcome| {
        let r = out.receipts.iter().rev().find(|r| r.receiver_id.as_str() == "c.near").unwrap();
        assert_eq!(r.logs, ["n1|+"], "{out}");
        r.block_height
    };
    let base = callback_at(&plain);
    let callee = |out: &TxOutcome| logged(out, "callee").block_height;
    // (hold the result?, what, blocks, where the callback runs). Held for
    // 1 block, the callback arrives with its result, as it would unheld.
    let holds = [
        (false, Receipts::call("d.near", "run"), 3, base + 3),
        (false, Receipts::callback("run"), 1, base),
        (false, Receipts::callback("run"), 3, base + 2),
        (true, Receipts::call("d.near", "run"), 1, base + 1),
        (true, Receipts::call("d.near", "run"), 3, base + 3),
    ];
    for (result, at, blocks, runs_in) in holds {
        let mut sim = sim.clone();
        let tx = ask(&mut sim, "u0.near", &prog);
        let tx = if result {
            tx.inject(at, Fault::HoldResult(blocks))
        } else {
            tx.inject(at, Fault::Hold(blocks))
        };
        let out = tx.transact();
        out.assert_success();
        assert!(callback_at(&out) > callee(&out), "{out}");
        assert_eq!(callback_at(&out), runs_in, "{out}");
        let cb = out.receipts.iter().rev().find(|r| r.receiver_id.as_str() == "c.near").unwrap();
        assert_eq!(cb.result_held, result.then_some(blocks), "{out}");
        sim.findings().assert_none();
    }

    // Every hold a scan tries keeps it so: the call and the callback, 3
    // lengths each, and the call's result.
    let order = move |d: &OpDone<Ask>| {
        let out = d.outcome().ok_or("one transaction")?;
        if callback_at(out) > callee(out) { Ok(()) } else { Err(format!("callback first:\n{out}")) }
    };
    let harness = Harness::new(sim.clone()).per_op("callback after its result", order);
    let plan = Plan::new().op(asks("u0.near", &prog));
    let report = harness
        .scan(std::slice::from_ref(&plan))
        .faults(self::holds(&[Fault::Hold, Fault::HoldResult], &[1, 2, 5]))
        .run();
    report.assert_clean();
    assert_eq!(report.cases(), 1 + 9, "{report}");

    // Held 1 block, the call (or its result) makes the callback late; the
    // callback held 1 block arrives with its result, as it would unheld.
    let on_time = move |d: &OpDone<Ask>| {
        let out = d.outcome().ok_or("one transaction")?;
        if callback_at(out) == base { Ok(()) } else { Err("late".to_owned()) }
    };
    let harness = Harness::new(sim).per_op("on time", on_time);
    let cases = [
        (Receipts::index(0), Fault::Hold(1), Verdict::Findings),
        (Receipts::index(0), Fault::HoldResult(1), Verdict::Findings),
        (Receipts::index(1), Fault::Hold(1), Verdict::Clean),
    ];
    for (at, fault, verdict) in cases {
        let run = harness.run(&plan.clone().inject(at, fault));
        assert_eq!(run.verdict(), verdict, "{run}");
    }
    // A held result lands on the callback it goes to, as a forced row says.
    let audit =
        harness.scan(std::slice::from_ref(&plan)).faults(self::holds(&[Fault::HoldResult], &[1]));
    let shown = audit.run().to_string();
    assert!(shown.contains("forced (HoldResult(1) at c.near::run)"), "{shown}");
}

/// `nth` counts only receipts the hold fits: of `d.near.run` (0) and its
/// callback (1), only the call's result goes to a callback, so
/// `Receipts::all().nth(0)` with `HoldResult` is `Receipts::index(0)`, and
/// `.nth(1)` holds nothing, where `Receipts::index(1)` panics. Every
/// receipt fits a `Hold`.
#[test]
fn nth_holds_the_kth_receipt_the_hold_fits() {
    let prog = [
        forward("d.near", &[Op::Log("callee".into())]),
        Op::Then { args: encode(&[Op::Results]), gas: 20 * TGAS, weight: 0 },
    ];
    let sim = order_sim();
    let run = |at: Receipts, fault: Fault| {
        let mut sim = sim.clone();
        let out = ask(&mut sim, "u0.near", &prog).inject(at, fault).transact();
        (format!("{out:#}\n{:?}", out.receipts), sim.state_root())
    };
    let result = Fault::HoldResult(2);
    let by_nth = run(Receipts::all().nth(0), result.clone());
    assert_eq!(by_nth, run(Receipts::index(0), result.clone()));
    assert!(by_nth.0.contains("[result held 2 blocks]"), "{}", by_nth.0);
    let none = run(Receipts::all().nth(1), result.clone());
    assert_eq!(none, run(Receipts::all().nth(7), Fault::Hold(1)));
    assert!(!none.0.contains("held "), "{}", none.0);
    let strict = catch_unwind(AssertUnwindSafe(|| run(Receipts::index(1), result.clone())));
    let msg = strict.unwrap_err().downcast_ref::<String>().cloned().unwrap_or_default();
    assert!(msg.contains("its result goes to no callback"), "{msg}");
    assert_eq!(
        run(Receipts::callbacks().nth(0), Fault::Hold(3)),
        run(Receipts::index(1), Fault::Hold(3))
    );
}

/// Held receipts go out in the order they were held: `b`, held for 1
/// block, waits behind `a`, held for 3, and both go ahead of `c`, which
/// left the block that released them.
#[test]
fn held_receipts_go_out_first_in_first_out() {
    let mut sim = order_sim();
    let mark = sim.height();
    let send = |sim: &mut Sim, user: &str, line: &str, blocks: u64| {
        let tx = ask(sim, user, &[forward("c.near", &[Op::Log(line.into())])]);
        if blocks > 0 {
            tx.inject(Receipts::index(0), Fault::Hold(blocks)).submit()
        } else {
            tx.submit()
        }
    };
    let (a, b) = (send(&mut sim, "u0.near", "a", 3), send(&mut sim, "u1.near", "b", 1));
    let (ha, hb) = (a, b);
    // Both leave block mark + 2; `a` is due to run in mark + 6, `b` in
    // mark + 4.
    sim.step();
    sim.step();
    assert_eq!(sim.height(), mark + 2);
    // `c` converts in mark + 4, runs in mark + 5 and its call leaves there
    // with the held ones.
    sim.step();
    let c = send(&mut sim, "u2.near", "c", 0);
    let hc = c;
    sim.settle();
    let at =
        |h: CryptoHash, line: &str| ran_at(&sim, mark, &logged(&sim.outcome(h), line).receipt_id);
    let (a, b, c) = (at(a, "a"), at(b, "b"), at(c, "c"));
    assert_eq!((a.0, b.0, c.0), (mark + 6, mark + 6, mark + 6));
    assert!(a.1 < b.1 && b.1 < c.1, "a {a:?}, b {b:?}, c {c:?}");
    // `b` asked for 1 block and waited 3, behind `a`: its outcome says 3.
    let held = |h: CryptoHash, line: &str| logged(&sim.outcome(h), line).held;
    assert_eq!((held(ha, "a"), held(hb, "b"), held(hc, "c")), (Some(3), Some(3), None));
    sim.findings().assert_none();
}

/// While a receipt is held the chain isn't idle: `run` produces blocks
/// until it has gone and run, conservation waits, and a snapshot refuses.
#[test]
fn run_and_snapshots_wait_for_held_receipts() {
    let mut sim = order_sim();
    let start = sim.height();
    let h = ask(&mut sim, "u0.near", &[forward("d.near", &[Op::Log("d".into())])])
        .inject(Receipts::index(0), Fault::Hold(20))
        .submit();
    for _ in 0..5 {
        sim.step();
    }
    assert!(!sim.is_idle());
    // Conservation is skipped, not failed, while NEAR is held in flight.
    sim.findings().assert_none();
    let held = sim.clone();
    let err = catch_unwind(AssertUnwindSafe(|| {
        Sim::cached(std::time::SystemTime::now(), || held.clone());
    }))
    .unwrap_err();
    let msg = err.downcast_ref::<&str>().copied().unwrap_or_default();
    assert!(msg.starts_with("a snapshot needs no receipt held (Fault::Hold)"), "{msg}");

    sim.settle();
    assert!(sim.is_idle());
    let out = sim.outcome(h);
    out.assert_success();
    // It left block start + 2 and would have run in start + 3; held for
    // 20, it runs in start + 23.
    assert_eq!(logged(&out, "d").block_height, start + 23, "{out}");
    sim.findings().assert_none();
}

/// The same holds from the same state give the same receipts, ids, gas,
/// blocks and state root.
#[test]
fn the_same_holds_give_the_same_run() {
    let prog = [
        forward("d.near", &[Op::Write { key: b"k".to_vec(), value: b"1".to_vec() }]),
        Op::Then { args: encode(&[Op::Results, Op::Env(0)]), gas: 20 * TGAS, weight: 0 },
    ];
    let sim = order_sim();
    let once = || {
        let mut sim = sim.clone();
        let out = ask(&mut sim, "u0.near", &prog)
            .inject(Receipts::index(0), Fault::HoldResult(2))
            .inject(Receipts::index(1), Fault::Hold(4))
            .transact();
        (
            format!("{out}"),
            out.receipts
                .iter()
                .map(|r| (r.receipt_id, r.block_height, r.gas_burnt))
                .collect::<Vec<_>>(),
            sim.state_root(),
        )
    };
    assert_eq!(once(), once());
    let scan = || {
        let plan = Plan::new().op(asks("u0.near", &prog));
        Harness::new(sim.clone())
            .scan(&[plan])
            .faults(holds(&[Fault::Hold, Fault::HoldResult], &[1, 3]))
            .run()
    };
    let first = scan();
    assert_eq!(first.cases(), 1 + 6, "two receipts and one result, 2 lengths each:\n{first}");
    let again = scan();
    let shape = |a: &near_contract_sim::explore::Audit<Ask>| {
        (a.cases(), a.found().len(), a.incomplete().to_vec())
    };
    assert_eq!(shape(&first), shape(&again));
}

/// An absolute setter, last write wins: `u0` sets `v` to 1 on `d.near`
/// through `c.near`, then `u1` sets it to 2. Unheld, 2 wins. With `u0`'s
/// write held for 2 blocks, it lands last and the stale 1 wins.
#[test]
fn a_held_write_lands_last_and_a_stale_value_wins() {
    let set = |v: &str| [forward("d.near", &[Op::Write { key: b"v".to_vec(), value: v.into() }])];
    let value = |sim: &Sim| sim.state("d.near", b"v").map(|v| String::from_utf8(v).unwrap());
    let mut sim = order_sim();
    ask(&mut sim, "u0.near", &set("1")).inject(Receipts::index(0), Fault::Hold(2)).submit();
    sim.step();
    ask(&mut sim, "u1.near", &set("2")).transact().assert_success();
    assert_eq!(value(&sim).as_deref(), Some("1"), "the stale write landed last");
    assert!(sim.outcome(sim.txs()[sim.txs().len() - 2]).to_string().contains("[held 2 blocks]"));
    sim.findings().assert_none();

    // Unheld, the latest write wins.
    let mut sim = order_sim();
    ask(&mut sim, "u0.near", &set("1")).submit();
    sim.step();
    ask(&mut sim, "u1.near", &set("2")).transact().assert_success();
    assert_eq!(value(&sim).as_deref(), Some("2"));
}

/// One transaction sends two absolute updates: the first straight away,
/// the second from a later receipt. A scan finds the hold that lets the
/// first land last: `Hold(2)` on the first update, and nothing else. Sent
/// from the first update's callback instead, the second always waits for
/// the first, and the scan is clean.
#[test]
fn a_scan_finds_the_stale_write() {
    let write = |v: &str| forward("d.near", &[Op::Write { key: b"v".to_vec(), value: v.into() }]);
    let latest = |d: &OpDone<Ask>| match d.world().state("d.near", b"v").as_deref() {
        Some(b"2") => Ok(()),
        v => Err(format!("v is {v:?}")),
    };
    let harness = Harness::new(order_sim()).per_op("the latest write wins", latest);
    let detached = [write("1"), forward("c.near", &[write("2")])];
    let all_holds = || holds(&[Fault::Hold, Fault::HoldResult], &[1, 2]);
    let report =
        harness.scan(&[Plan::new().op(asks("u0.near", &detached))]).faults(all_holds()).run();
    let [found] = report.found() else { panic!("{report}") };
    let inject = found.run.plan().steps[0].inject[0].clone();
    assert_eq!(
        (inject.at, inject.fault),
        (Receipts::occurrence("d.near", "run", 0), Fault::Hold(2)),
        "{report}"
    );
    assert_eq!(found.finding.message, "v is Some([49])");
    // Only that variant: replaying each other hold of the first update is clean.
    let plan = Plan::new().op(asks("u0.near", &detached));
    harness.run(&plan.clone().inject(Receipts::index(0), Fault::Hold(1))).assert_clean();

    let chained = [write("1"), Op::Then { args: encode(&[write("2")]), gas: 40 * TGAS, weight: 0 }];
    let holds = holds(&[Fault::Hold, Fault::HoldResult], &[1, 2, 5]);
    harness.scan(&[Plan::new().op(asks("u0.near", &chained))]).faults(holds).run().assert_clean();
}

/// What a hold stands for, by nearcore's own congestion control and no
/// hold: 1.2 MB calls fill the bandwidth grant of the route a shard has to
/// itself, so each waits a block in the outgoing buffer. A small call
/// `c.near` makes after three of them, in the same block, goes out at once
/// and runs a block before them.
#[test]
fn nearcore_lets_a_later_receipt_overtake_a_buffered_one() {
    let mut sim = order_sim();
    for i in 0..4 {
        sim.create_account(format!("w{i}.near"), Balance::from_near(100));
    }
    let big = |i: u8| {
        let then = [Op::YieldResume { slot: 9, payload: vec![i; 1_200_000] }];
        Op::Call { to: "d.near".into(), args: encode(&then), deposit: 0, gas: 30 * TGAS, weight: 0 }
    };
    let users = ["u0.near", "u1.near", "u2.near", "w1.near", "w2.near", "w3.near"];
    let bigs: Vec<CryptoHash> =
        users.iter().zip(0u8..).map(|(u, i)| ask(&mut sim, u, &[big(i)]).submit()).collect();
    let small = ask(&mut sim, "w0.near", &[forward("d.near", &[Op::Log("small".into())])]).submit();
    sim.settle();
    let small = sim.outcome(small);
    let (made, ran) = (small.receipts[0].block_height, logged(&small, "small").block_height);
    assert_eq!(ran, made + 1, "{small}");
    let mut overtaken = 0;
    for h in &bigs {
        let out = sim.outcome(*h);
        out.assert_success();
        let d = out.receipts.iter().find(|r| r.receiver_id.as_str() == "d.near" && !r.is_refund());
        let big = (out.receipts[0].block_height, d.unwrap().block_height);
        // Each waits a block; those made with the small call run after it.
        assert_eq!(big.1, big.0 + 2, "{out}");
        overtaken += usize::from(big == (made, ran + 1));
    }
    assert_eq!(overtaken, 3);
    sim.findings().assert_none();
}

/// Six ~1.2 MB calls, one per user, submitted together.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct Bigs;

impl XOp for Bigs {
    fn submit(&self, sim: &mut Sim) -> Result<(), Skip> {
        let big = |i: u8| {
            let then = [Op::YieldResume { slot: 9, payload: vec![i; 1_200_000] }];
            Op::Call {
                to: "d.near".into(),
                args: encode(&then),
                deposit: 0,
                gas: 30 * TGAS,
                weight: 0,
            }
        };
        let users = ["u0.near", "u1.near", "u2.near", "w0.near", "w1.near", "w2.near"];
        for (u, i) in users.iter().zip(0u8..) {
            ask(sim, u, &[big(i)]).submit();
        }
        Ok(())
    }
}

/// A receipt congestion control keeps in the outgoing buffer is past where
/// a hold applies: a scan has no variant that holds it.
/// Six 1.2 MB calls fill the bandwidth grant, so each waits a block.
#[test]
fn a_scan_skips_a_buffered_receipt() {
    let mut sim = order_sim();
    for i in 0..3 {
        sim.create_account(format!("w{i}.near"), Balance::from_near(100));
    }
    let report =
        Harness::new(sim).scan(&[Plan::new().op(Bigs)]).faults(holds(&[Fault::Hold], &[1])).run();
    // Each user's call waits in the buffer: no hold fits it, so the scan
    // runs the plain case alone.
    assert_eq!((report.cases(), report.found().len()), (1, 0), "{report}");
}

/// Two users' absolute updates; the world counts how many were asked, so
/// the check judges only once both are.
/// A race against a later call is a plan of two steps: `u1`'s update,
/// submitted `after` blocks after `u0`'s. Which hold lets `u0`'s earlier
/// update land last depends on `after`: a later racer needs a longer hold,
/// and neither of these is long enough for `after = 2`.
#[test]
fn a_held_update_races_a_later_transaction() {
    let set = |v: &str| [forward("d.near", &[Op::Write { key: b"v".to_vec(), value: v.into() }])];
    let harness = Harness::new(order_sim()).at_rest_ops(
        "the latest write wins",
        |sim: &Sim, asked: &[OpDone<Ask>]| match (asked.len(), sim.state("d.near", b"v").as_deref())
        {
            (0 | 1, _) | (_, Some(b"2")) => Ok(()),
            (_, v) => Err(format!("v is {v:?}")),
        },
    );
    let race = |after: u16, hold: Option<u64>| {
        let mut plan = Plan::new().op(asks("u0.near", &set("1")));
        if let Some(h) = hold {
            plan = plan.inject(Receipts::index(0), Fault::Hold(h));
        }
        harness.run(&plan.blocks(after).op(asks("u1.near", &set("2")))).verdict()
    };
    let stale: Vec<(u16, u64)> = [0, 1, 2]
        .into_iter()
        .flat_map(|after| [1, 2].map(|h| (after, h)))
        .filter(|&(after, h)| race(after, Some(h)) == Verdict::Findings)
        .collect();
    assert_eq!(stale, [(0, 1), (0, 2), (1, 2)]);
    for after in [0, 1, 2] {
        assert_eq!(race(after, None), Verdict::Clean, "unheld, after {after}");
    }
    // A scan of each race finds it exactly where a hold does.
    for (after, found) in [(0, true), (2, false)] {
        let plan =
            Plan::new().op(asks("u0.near", &set("1"))).blocks(after).op(asks("u1.near", &set("2")));
        let report = harness.scan(&[plan]).faults(holds(&[Fault::Hold], &[1, 2])).run();
        assert_eq!(report.found_at("the latest write wins", "world").is_some(), found, "{report}");
    }
}

/// A receipt that calls nothing, such as a transfer, is named by its
/// action: `Receipts::action("Transfer")` holds it, and a scan labels it
/// by its receiver alone.
#[test]
fn a_receipt_without_a_method_is_named_by_its_action() {
    let prog = [Op::Transfer { to: "d.near".into(), amount: 1 }, forward("d.near", &[])];
    let mut sim = order_sim();
    let out = ask(&mut sim, "u0.near", &prog)
        .inject(Receipts::action("Transfer"), Fault::Hold(2))
        .transact();
    let transfer = out.receipts.iter().find(|r| r.actions == ["Transfer"] && !r.is_refund());
    assert_eq!(transfer.unwrap().held, Some(2), "{out}");
    let plan = Plan::new().op(asks("u0.near", &prog));
    let report = Harness::new(order_sim()).scan(&[plan]).faults(holds(&[Fault::Hold], &[1])).run();
    report.assert_clean();
    assert!(report.cases() > 1, "the transfer and the call held: {report}");
    report.assert_reached("d.near::run");
}

/// The edges: a result that goes to no callback can't be held, and the
/// height can't move back while a receipt is held.
#[test]
fn order_edges() {
    let prog = [forward("d.near", &[])];
    let sim = order_sim();
    let panics = |f: &dyn Fn()| {
        let err = catch_unwind(AssertUnwindSafe(f)).unwrap_err();
        let s = err.downcast_ref::<String>().cloned();
        s.or(err.downcast_ref::<&str>().map(|s| s.to_string())).unwrap()
    };

    let msg = panics(&|| {
        let mut sim = sim.clone();
        let _ = ask(&mut sim, "u0.near", &prog)
            .inject(Receipts::index(0), Fault::HoldResult(1))
            .transact();
    });
    assert!(
        msg.starts_with(
            "HoldResult(1) at Receipts::index(0) (d.near::run): its result goes to no callback"
        ),
        "{msg}"
    );

    let msg = panics(&|| {
        let mut sim = sim.clone();
        ask(&mut sim, "u0.near", &prog).inject(Receipts::index(0), Fault::Hold(5)).submit();
        sim.step();
        sim.step();
        sim.set_height(sim.height() - 1);
    });
    assert!(msg.contains("while a receipt is held (Fault::Hold): settle() first"), "{msg}");
    // A pin that isn't a rewind, landed by a call to another account, moves
    // the last block back below the held receipt's: refused, before it moves.
    let msg = panics(&|| {
        let mut sim = sim.clone();
        ask(&mut sim, "u0.near", &prog).inject(Receipts::index(0), Fault::Hold(5)).submit();
        sim.step();
        sim.step();
        sim.set_height(sim.height() + 1);
        ask(&mut sim, "u1.near", &prog).submit();
    });
    assert!(msg.contains("rewinds below block") && msg.contains("settle() first"), "{msg}");

    // A copy of a submitted transaction (same nonce) must carry the same
    // holds: its hash doesn't cover them. The same ones are fine.
    let mut copy = sim.clone();
    let nonce = 1_000_000_000;
    ask(&mut copy, "u0.near", &prog)
        .nonce(nonce)
        .inject(Receipts::index(0), Fault::Hold(2))
        .submit();
    ask(&mut copy, "u0.near", &prog)
        .nonce(nonce)
        .inject(Receipts::index(0), Fault::Hold(2))
        .submit();
    let msg = panics(&|| {
        let mut copy = copy.clone();
        ask(&mut copy, "u0.near", &prog)
            .nonce(nonce)
            .inject(Receipts::index(0), Fault::Hold(3))
            .submit();
    });
    assert!(msg.contains("was submitted before with other injections"), "{msg}");
}

/// The first submission of `Flaky` asks for a call to d.near; every later
/// one, for a call to c.near.
static FLAKY_FIRST: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Flaky;

impl XOp for Flaky {
    fn submit(&self, sim: &mut Sim) -> Result<(), Skip> {
        let to = match FLAKY_FIRST.swap(false, std::sync::atomic::Ordering::SeqCst) {
            true => "d.near",
            false => "c.near",
        };
        ask(sim, "u0.near", &[forward(to, &[])]).submit();
        Ok(())
    }
}

/// An op must submit the same transaction each time: a variant whose
/// receipt isn't the one the plain case had stops the scan with a harness
/// error, rather than pass as a clean run.
#[test]
fn a_scan_stops_when_an_op_is_not_deterministic() {
    let audit = Harness::new(order_sim())
        .scan(&[Plan::new().op(Flaky)])
        .faults(holds(&[Fault::Hold], &[1]))
        .run();
    let why = audit.errors().join("; ");
    assert!(
        why.contains("receipt 0 was d.near::run in the plain case and c.near::run here"),
        "{why}"
    );
}

/// A receipt's fault and its hold are separate slots, so both apply: the
/// receipt is held, then runs starved.
#[test]
fn a_fault_and_a_hold_both_apply() {
    let prog = [
        forward("d.near", &[]),
        Op::Then { args: encode(&[Op::Results]), gas: 20 * TGAS, weight: 0 },
    ];
    let mut sim = order_sim();
    let out = ask(&mut sim, "u0.near", &prog)
        .inject(Receipts::index(0), Fault::OutOfGas)
        .inject(Receipts::index(0), Fault::Hold(2))
        .transact();
    let held = out.receipts.iter().find(|r| r.held.is_some()).expect("a held receipt");
    assert_eq!((held.held, held.fault.clone()), (Some(2), Some(Fault::OutOfGas)), "{out}");
}

/// `c.near` sends itself `x` and `wrap` in one block, and `wrap` sends
/// itself `late`: unheld, `x` runs a block before `late`. Spawned receipt 0
/// is `x`, 1 `wrap`, 2 `late`.
fn race() -> [Op; 2] {
    [
        forward("c.near", &[Op::Log("x".into())]),
        forward("c.near", &[forward("c.near", &[Op::Log("late".into())])]),
    ]
}

/// `HoldUntil` reverses the race with no block arithmetic: `x`, held until
/// `late` has run, runs in the next block, whichever selector names
/// `late` (`nth` counts every receipt it selects, `x` and `wrap`
/// included). Held until `wrap`, which left with it, `x` runs the block
/// after `wrap`, ahead of `late`, which left the block that released it.
#[test]
fn hold_until_runs_a_receipt_right_after_the_one_it_names() {
    let mut sim = order_sim();
    let plain = ask(&mut sim.clone(), "u0.near", &race()).transact();
    let (x, late) = (logged(&plain, "x"), logged(&plain, "late"));
    assert_eq!(x.block_height + 1, late.block_height, "{plain}");

    for target in [Receipts::index(2), Receipts::call("c.near", "run").nth(2)] {
        let fault = Fault::HoldUntil(target);
        let out =
            ask(&mut sim.clone(), "u0.near", &race()).inject(Receipts::index(0), fault).transact();
        out.assert_success();
        let (x, late) = (logged(&out, "x"), logged(&out, "late"));
        assert_eq!(x.block_height, late.block_height + 1, "{out}");
        assert_eq!((x.held, late.held), (Some(2), None), "{out}");
    }

    let mark = sim.height();
    let h = ask(&mut sim, "u0.near", &race())
        .inject(Receipts::index(0), Fault::HoldUntil(Receipts::index(1)))
        .submit();
    sim.settle();
    let out = sim.outcome(h);
    let (x, late) = (
        ran_at(&sim, mark, &logged(&out, "x").receipt_id),
        ran_at(&sim, mark, &logged(&out, "late").receipt_id),
    );
    assert_eq!(x.0, late.0, "{out}");
    assert!(x.1 < late.1, "x {x:?}, late {late:?}");
    assert_eq!(logged(&out, "x").held, Some(1), "{out}");
    sim.findings().assert_none();
}

/// While it waits, a held receipt is listed as held until the receipt it
/// names, once that one has left its block.
#[test]
fn a_receipt_held_until_another_is_listed_with_it() {
    let mut sim = order_sim();
    let h = ask(&mut sim, "u0.near", &race())
        .inject(Receipts::index(0), Fault::HoldUntil(Receipts::index(2)))
        .submit();
    let state = |sim: &Sim| {
        let pending = sim.outcome(h).pending;
        pending.iter().map(|p| p.state).find(|s| matches!(s, PendingState::HeldUntil { .. }))
    };
    // The transaction converts, then its receipt spawns `x` and `wrap`.
    sim.step();
    sim.step();
    assert_eq!(state(&sim), Some(PendingState::HeldUntil { receipt_id: None }));
    assert!(state(&sim).unwrap().to_string().contains("held until a receipt it names"));
    sim.step();
    let Some(PendingState::HeldUntil { receipt_id: Some(late) }) = state(&sim) else {
        panic!("{:?}", sim.outcome(h).pending)
    };
    assert!(!sim.is_idle());
    sim.settle();
    let out = sim.outcome(h);
    assert_eq!(logged(&out, "late").receipt_id, late);
    assert_eq!(state(&sim), None);
}

/// A `HoldUntil` that can't apply panics, as a fault that doesn't fit
/// does: the receipt it names already ran (`x`, in the block `late` left),
/// is the held receipt itself, or never runs. One whose receipt never
/// leaves a block, or waits on the held one, is released once nothing
/// else is in flight, and panics there.
#[test]
fn hold_until_edges() {
    let sim = order_sim();
    let panics = |at: Receipts, until: Receipts, prog: &[Op]| {
        let mut sim = sim.clone();
        let fault = Fault::HoldUntil(until);
        let err = catch_unwind(AssertUnwindSafe(|| {
            let _ = ask(&mut sim, "u0.near", prog).inject(at, fault).transact();
        }))
        .unwrap_err();
        err.downcast_ref::<String>().cloned().unwrap_or_default()
    };
    let msg = panics(Receipts::index(2), Receipts::index(0), &race());
    let start = "HoldUntil(Receipts::index(0)) at Receipts::index(2) (c.near::run): receipt ";
    assert!(msg.starts_with(start) && msg.ends_with(", which it waits for, already ran"), "{msg}");
    let msg = panics(Receipts::index(0), Receipts::all(), &race());
    assert!(msg.ends_with("(c.near::run): it would wait for itself"), "{msg}");
    let msg = panics(Receipts::index(0), Receipts::method("nope"), &race());
    let never = "(c.near::run): nothing it waits for left a block, and nothing else is in flight";
    assert!(msg.ends_with(never), "{msg}");
    // The call held until its own callback, which waits for its result.
    let callback = [
        forward("d.near", &[Op::Log("callee".into())]),
        Op::Then { args: encode(&[Op::Results]), gas: 20 * TGAS, weight: 0 },
    ];
    let msg = panics(Receipts::index(0), Receipts::callbacks(), &callback);
    assert!(msg.contains("(d.near::run): receipt "), "{msg}");
    assert!(msg.ends_with(", which it waits for, never ran, and nothing else is in flight"));
}

/// In a plan, a `HoldUntil` that can't apply is passed over: the receipt
/// still goes, once nothing else is in flight, and the run says why it
/// applied to none. One that applies lands where it held.
#[test]
fn a_plans_hold_until_lands_or_says_why_not() {
    let harness = Harness::new(order_sim());
    let plan = Plan::new().op(asks("u0.near", &race()));
    let run =
        harness.run(&plan.clone().inject(Receipts::index(0), Fault::HoldUntil(Receipts::index(2))));
    run.assert_clean();
    let run =
        harness.run(&plan.inject(Receipts::index(0), Fault::HoldUntil(Receipts::method("nope"))));
    assert_eq!(run.verdict(), Verdict::Incomplete, "{run}");
    let shown = run.to_string();
    assert!(
        shown.contains("applied to no receipt: at c.near::run: nothing it waits for left a block"),
        "{shown}"
    );
}

/// Outside a harness, `submit_injected` forces faults on an op's first
/// transaction as `Tx::inject` does, panicking where one can't apply, and
/// leaves later submissions alone.
#[test]
fn an_op_submits_with_injections_outside_a_harness() {
    let mut sim = order_sim();
    let until = |n| [(Receipts::index(0), Fault::HoldUntil(Receipts::index(n)))];
    let n = sim.txs().len();
    submit_injected(&mut sim, &asks("u0.near", &race()), until(2)).unwrap();
    let plain = ask(&mut sim, "u1.near", &race()).submit();
    sim.settle();
    let [held, plain] = [sim.txs()[n], plain].map(|h| sim.outcome(h));
    let at = |out: &TxOutcome, line| logged(out, line).block_height;
    assert_eq!(at(&held, "x"), at(&held, "late") + 1, "{held}");
    assert_eq!(at(&plain, "x") + 1, at(&plain, "late"), "{plain}");

    let err = catch_unwind(AssertUnwindSafe(|| {
        let mut sim = order_sim();
        let ask = asks("u0.near", &race());
        let late = [(Receipts::index(2), Fault::HoldUntil(Receipts::index(0)))];
        submit_injected(&mut sim, &ask, late).unwrap();
        sim.settle();
    }))
    .unwrap_err();
    let msg = err.downcast_ref::<String>().cloned().unwrap_or_default();
    assert!(msg.ends_with("which it waits for, already ran"), "{msg}");
    let err = catch_unwind(AssertUnwindSafe(|| {
        submit_injected(
            &mut order_sim(),
            &asks("u0.near", &[]),
            [(Receipts::all(), Fault::Hold(0))],
        )
    }))
    .unwrap_err();
    let msg = err.downcast_ref::<String>().cloned().unwrap_or_default();
    assert!(msg.contains("holds nothing"), "{msg}");
    // An op that submits nothing has nothing to inject into; one that
    // skips is fine.
    let err =
        catch_unwind(AssertUnwindSafe(|| submit_injected(&mut order_sim(), &Nothing, until(1))))
            .unwrap_err();
    let msg = err.downcast_ref::<String>().cloned().unwrap_or_default();
    assert_eq!(msg, "submit_injected: Nothing submitted no transaction to inject into");
    assert!(submit_injected(&mut order_sim(), &Nothing, []).is_ok());
    assert!(submit_injected(&mut order_sim(), &Skips, until(1)).is_err());
    // Inside a harness's op, a plan's injection is the way.
    let run = Harness::new(order_sim()).run(&Plan::new().op(Nested));
    assert!(run.to_string().contains("submit_injected inside a harness's op"), "{run}");
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Nothing;

impl XOp for Nothing {
    fn submit(&self, _: &mut Sim) -> Result<(), Skip> {
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Skips;

impl XOp for Skips {
    fn submit(&self, _: &mut Sim) -> Result<(), Skip> {
        Err("skips".into())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Nested;

impl XOp for Nested {
    fn submit(&self, sim: &mut Sim) -> Result<(), Skip> {
        submit_injected(sim, &asks("u0.near", &race()), [(Receipts::all(), Fault::Hold(1))])
    }
}

/// A `HoldUntil` beside a `Hold`, several held until one receipt, and a
/// call held until another call's callback has run.
#[test]
fn hold_until_waits_for_any_receipt_and_holds_several() {
    // `x`, held until `wrap`, goes ahead of `late`, held a block after it.
    let mut sim = order_sim();
    let out = ask(&mut sim, "u0.near", &race())
        .inject(Receipts::call("c.near", "run").nth(0), Fault::HoldUntil(Receipts::index(1)))
        .inject(Receipts::index(2), Fault::Hold(1))
        .transact();
    let (x, late) = (logged(&out, "x"), logged(&out, "late"));
    assert_eq!((x.held, late.held), (Some(1), Some(1)), "{out}");
    assert!(x.block_height < late.block_height, "{out}");

    // `a` and `b` both wait for `late`.
    let prog = [
        forward("c.near", &[Op::Log("a".into())]),
        forward("c.near", &[Op::Log("b".into())]),
        forward("c.near", &[forward("c.near", &[Op::Log("late".into())])]),
    ];
    let out = ask(&mut order_sim(), "u0.near", &prog)
        .inject(Receipts::index(0), Fault::HoldUntil(Receipts::index(3)))
        .inject(Receipts::index(1), Fault::HoldUntil(Receipts::index(3)))
        .transact();
    let late = logged(&out, "late").block_height;
    for line in ["a", "b"] {
        assert_eq!(logged(&out, line).block_height, late + 1, "{out}");
    }

    // A call held until a callback (of the first call) has run.
    let prog = [
        forward("d.near", &[Op::Log("callee".into())]),
        Op::Then { args: encode(&[Op::Log("cb".into())]), gas: 20 * TGAS, weight: 0 },
        forward("c.near", &[Op::Log("x".into())]),
    ];
    let out = ask(&mut order_sim(), "u0.near", &prog)
        .inject(Receipts::index(2), Fault::HoldUntil(Receipts::callbacks()))
        .transact();
    out.assert_success();
    assert_eq!(logged(&out, "x").block_height, logged(&out, "cb").block_height + 1, "{out}");
}

/// A hold's receipts go out first in, first out, so a receipt held until
/// one that is held after it waits for nothing that can run: it goes once
/// nothing else is in flight, and the panic says why, after the block is
/// logged, so the chain stays whole and settles.
#[test]
fn a_receipt_held_until_one_held_behind_it_goes_when_nothing_else_can() {
    let mut sim = order_sim();
    let h = ask(&mut sim, "u0.near", &race())
        .inject(Receipts::index(0), Fault::HoldUntil(Receipts::index(2)))
        .inject(Receipts::index(1), Fault::Hold(1))
        .submit();
    let err = catch_unwind(AssertUnwindSafe(|| sim.settle())).unwrap_err();
    let msg = err.downcast_ref::<String>().cloned().unwrap_or_default();
    let why = "nothing it waits for left a block, and nothing else is in flight but the 1 held \
               behind it, which can't leave before it (holds go out first in, first out)";
    assert!(msg.ends_with(why), "{msg}");
    sim.settle();
    let out = sim.outcome(h);
    out.assert_success();
    let (x, late) = (logged(&out, "x"), logged(&out, "late"));
    assert!(x.held.is_some() && x.block_height < late.block_height, "{out}");
}

/// A receipt held until one that runs while it waits behind an earlier
/// hold goes out with that hold, not before: `x` waits for `wrap`, which
/// runs a block after both leave, but `a`, held 3 blocks ahead of it,
/// keeps it until `a` goes.
#[test]
fn a_receipt_held_until_one_that_ran_waits_behind_earlier_holds() {
    let mut sim = order_sim();
    let a = ask(&mut sim, "u0.near", &[forward("c.near", &[Op::Log("a".into())])])
        .inject(Receipts::index(0), Fault::Hold(3))
        .submit();
    let x = ask(&mut sim, "u1.near", &race())
        .inject(Receipts::index(0), Fault::HoldUntil(Receipts::index(1)))
        .submit();
    sim.settle();
    let (a, x) = (sim.outcome(a), sim.outcome(x));
    let (ra, rx, wrap) = (logged(&a, "a"), logged(&x, "x"), &x.receipts[2]);
    assert_eq!((ra.held, rx.held), (Some(3), Some(3)), "{a}\n{x}");
    assert_eq!(ra.block_height, rx.block_height, "{a}\n{x}");
    assert!(wrap.block_height + 1 < rx.block_height, "{x}");
}
