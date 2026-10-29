//! Built-in checks (`ReceiptOutcome::unhandled`, `Sim::findings`), alone
//! and in `explore` harnesses, on small contracts.
//! The intents showcase is `tests/invariants_intents.rs`.

mod common;
mod fuzz_common;
use common::{ledger_pair_wasm, tip_jar_wasm};
use fuzz_common::*;
use near_contract_sim::explore::{Call, Gap, Harness, Op as XOp, OpDone, Plan, Skip, plans};
use near_contract_sim::{
    Balance, CryptoHash, Failure, Fault, Finding, Gas, ReceiptOutcome, Receipts, Sim, TxOutcome,
    builtin,
};
use proptest::prelude::*;
use proptest::test_runner::TestCaseError;
use serde::{Deserialize, Serialize};

/// The receipts of `out` that failed, refunds aside.
fn failed(out: &TxOutcome) -> Vec<&ReceiptOutcome> {
    out.receipts.iter().filter(|r| !r.is_refund() && r.failure().is_some()).collect()
}

fn unhandled(out: &TxOutcome) -> Vec<&ReceiptOutcome> {
    out.receipts.iter().filter(|r| r.unhandled).collect()
}

/// The transaction of each violation.
fn txs(vs: &[Finding]) -> Vec<CryptoHash> {
    vs.iter().map(|v| v.tx.expect("a transaction")).collect()
}

/// `check`, conservation alone: random programs and failed meta-transaction
/// calls leave unhandled failures behind, which don't matter here.
fn conservation(sim: &Sim) -> Result<(), Vec<Finding>> {
    let mut vs = sim.findings().into_iter().collect::<Vec<_>>();
    vs.retain(|v| v.check == builtin::CONSERVATION);
    if vs.is_empty() { Ok(()) } else { Err(vs) }
}

fn tip_sim() -> Sim {
    let mut sim = Sim::new();
    sim.create_account("alice.near", Balance::from_near(100));
    sim.deploy("jar.near", tip_jar_wasm());
    sim
}

/// Generate actual receivers, including implicit account creation and rejected
/// transfers. Proptest owns both selection and shrinking.
fn receivers() -> impl Strategy<Value = String> {
    prop_oneof![
        prop::sample::select(vec!["alice.near".to_owned(), "jar.near".to_owned()]),
        any::<u64>().prop_map(|n| hex(&CryptoHash::hash_bytes(&n.to_le_bytes()).0)),
        any::<u64>().prop_map(|n| {
            format!("0x{}", &hex(&CryptoHash::hash_bytes(&n.to_le_bytes()).0)[..40])
        }),
        (0u16..1000).prop_map(|n| format!("missing{n}.near")),
    ]
}

/// alice.near tips `to` 1 NEAR through the jar's `method` (`tip` sends it
/// detached, `tip_checked` handles the result), with `tgas`.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct Tip {
    method: String,
    to: String,
    tgas: u64,
}

impl XOp for Tip {
    fn submit(&self, sim: &mut Sim) -> Result<(), Skip> {
        let call = sim.call("alice.near", "jar.near", &self.method).args(self.to.clone());
        call.gas(Gas::from_teragas(self.tgas)).submit();
        Ok(())
    }
}

/// Tips through `method` to generated receivers, with 5 to 300 Tgas.
fn tips(method: &'static str) -> impl Strategy<Value = Tip> {
    (receivers(), 5u64..=300).prop_map(move |(to, tgas)| Tip { method: method.into(), to, tgas })
}

/// The same real WASM and broad input strategy find the detached transfer,
/// while the method which handles the promise result passes.
#[test]
fn a_search_finds_the_fire_and_forget_transfer() {
    let report =
        Harness::new(tip_sim()).search(plans(tips("tip")).len(1..=8)).seed(0x5eed).cases(32).run();
    let found = report.found().iter().find(|f| f.finding.check == builtin::UNHANDLED_FAILURE);
    let found = found.unwrap_or_else(|| panic!("{report}"));
    assert!(found.finding.message.contains("does not exist"), "{report}");
}

#[test]
fn a_search_passes_the_handled_transfer() {
    let report = Harness::new(tip_sim())
        .search(plans(tips("tip_checked")).len(1..=8))
        .seed(0x5eed)
        .cases(24)
        .run();
    report.assert_clean();
    report.assert_reached("jar.near::on_tip");
}

/// bob.near started with 1 NEAR, and each of `tips` tips paid 1 more.
fn bob_has_every_tip(sim: &Sim, tips: usize) -> Result<(), String> {
    let expected = Balance::from_near(1 + tips as u128);
    let actual = sim.balance("bob.near");
    if actual == expected { Ok(()) } else { Err(format!("bob has {actual}, expected {expected}")) }
}

/// alice.near tips bob.near through `tip_checked`. Its gas is pinned: a
/// tip starved of gas pays nothing, which isn't what these checks are
/// about.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct TipBob;

impl XOp for TipBob {
    fn submit(&self, sim: &mut Sim) -> Result<(), Skip> {
        let tip = sim.call("alice.near", "jar.near", "tip_checked").args("bob.near");
        tip.gas(Gas::from_teragas(100)).submit();
        Ok(())
    }
}

/// bob's tips: every one arrives once nothing is in flight, and each tip,
/// read from its own receipts, pays bob exactly 1 NEAR, overlap or not.
/// The world counts no tips: the at-rest check reads the case's ops.
fn accounted_tips() -> Harness<TipBob> {
    let mut sim = tip_sim();
    sim.create_account("bob.near", Balance::from_near(1));
    Harness::new(sim)
        .at_rest_ops("bob has every tip", |sim: &Sim, tips: &[OpDone<TipBob>]| {
            bob_has_every_tip(sim, tips.len())
        })
        .per_op("bob gets the tip", |d: &OpDone<TipBob>| {
            let got = d.effects().near_net("bob.near");
            let near = Balance::from_near(1).as_yoctonear() as i128;
            if got == near { Ok(()) } else { Err(format!("bob got {got}")) }
        })
}

/// This property belongs at rest: a tip submitted but not yet run hasn't
/// reached bob. Settled and overlapping tips reuse it.
#[test]
fn one_world_supports_settled_and_overlapping_calls() {
    let harness = accounted_tips();
    let settled = plans(Just(TipBob)).len(1..=6).gaps(Just(Gap::Settle));
    harness.search(settled).seed(0x5eed).cases(8).run().assert_clean();
    harness.search(plans(Just(TipBob)).len(2..=6)).seed(0x5eed).cases(8).run().assert_clean();
    let run = harness.run(&Plan::new().op(TipBob).blocks(0).op(TipBob));
    run.assert_clean();
    // At rest twice: the fixture, with no tips, and the end, with two.
    assert_eq!(run.judged()[0].ok, 2, "{run}");
}

/// A check after every block sees a yield's timeout pay bob, in a block
/// no transaction was submitted in.
#[test]
fn a_block_check_detects_a_yield_timeout_payment() {
    let mut sim = fuzz_sim();
    sim.create_account("bob.near", Balance::from_near(1));
    let callback = encode(&[Op::Transfer { to: "bob.near".into(), amount: NEAR }]);
    let program = encode(&[Op::YieldCreate { slot: 0, args: callback, gas: 10 * TGAS, weight: 0 }]);
    let park = Call::new("alice.near", "f.near", "run").args(program);
    let run = Harness::new(sim)
        .always("bob receives no timeout payment", |sim: &Sim| {
            if sim.balance("bob.near") == Balance::from_near(1) {
                Ok(())
            } else {
                Err("yield timeout paid bob")
            }
        })
        .run(&Plan::new().op(park).blocks(250));
    let found = run.assert_found("bob receives no timeout payment", "world");
    assert!(found.block > 200, "{run}");
}

/// Conservation runs while yields are parked, counting what each callback
/// holds, its deposit included, and the callback that waits on it: the
/// books balance once they park, once one is resumed and once the other
/// times out.
#[test]
fn conservation_holds_while_yields_are_parked() {
    let mut sim = fuzz_sim();
    let park = |id| {
        let cb = encode(&[Op::Results]);
        let deposit = 3 * NEAR;
        let y = Op::YieldCreateWithId { id, args: cb.clone(), deposit, gas: 10 * TGAS, weight: 0 };
        [y, Op::Then { args: cb, gas: 10 * TGAS, weight: 0 }]
    };
    let before = sim.balance("f.near");
    run_prog(&mut sim, &park(1)).assert_success();
    run_prog(&mut sim, &park(2)).assert_success();
    assert_eq!(sim.yields().len(), 2);
    assert!(sim.balance("f.near") < before.saturating_sub(Balance::from_near(5)), "the deposits");
    sim.findings().assert_none();
    let resume = Op::YieldResumeWithId { id: 1, payload: b"go".to_vec() };
    run_prog(&mut sim, &[resume]).assert_success();
    assert_eq!(sim.yields().len(), 1);
    sim.findings().assert_none();
    sim.advance_to(sim.yields()[0].expires_at);
    sim.settle();
    assert!(sim.yields().is_empty());
    sim.findings().assert_none();
}

// ---------------------------------------------------------------------------
// Handled vs unhandled
// ---------------------------------------------------------------------------

#[test]
fn detached_failed_transfer_is_unhandled() {
    let mut sim = tip_sim();
    let out = sim.call("alice.near", "jar.near", "tip").args(b"nobody.near".to_vec()).transact();
    out.assert_success();
    let vs = sim.findings();
    let [v] = &vs[..] else { panic!("{vs:?}") };
    assert_eq!(v.check, builtin::UNHANDLED_FAILURE);
    let u = unhandled(&out);
    assert_eq!(u.len(), 1, "{out}");
    assert_eq!(u[0].failure(), Some(Failure::AccountDoesNotExist("nobody.near".parse().unwrap())));
    assert_eq!(u[0].receiver_id.as_str(), "nobody.near");
    assert_eq!(failed(&out).len(), 1);
    assert!(out.to_string().contains("<-- UNHANDLED"), "{out}");
    let id = u[0].receipt_id.to_string();
    let want = format!(
        "nobody.near <- transfer 1.0000 NEAR ({}…): account nobody.near does not exist [burnt 0.22 Tgas]\n  \
         transaction status: SUCCESS; no callback receives this failure and the caller never sees it",
        &id[..6]
    );
    assert_eq!(v.message, want);
    assert_eq!(v.tx, Some(out.hash));
    assert_eq!(v.receipt_id, Some(u[0].receipt_id));
    // A successful tip has no failures, and `check` reports the first again.
    let out = sim.call("alice.near", "jar.near", "tip").args(b"alice.near".to_vec()).transact();
    assert!(failed(&out).is_empty(), "{out}");
    assert_eq!(txs(&sim.findings()), txs(&vs));
}

#[test]
fn failure_delivered_to_callback_is_handled() {
    let mut sim = tip_sim();
    let out =
        sim.call("alice.near", "jar.near", "tip_checked").args(b"nobody.near".to_vec()).transact();
    assert_eq!(out.assert_success().logs(), ["tip failed"]);
    assert_eq!(failed(&out).len(), 1);
    assert!(unhandled(&out).is_empty(), "{out}");
    sim.findings().assert_none();
}

/// A failure that ends the result path is the transaction's: a receipt's,
/// or the conversion's when the transaction is invalid.
#[test]
fn failure_on_the_result_path_is_handled() {
    let mut sim = tip_sim();
    let out = sim.call("alice.near", "jar.near", "no_such_method").transact();
    out.assert_failure();
    assert_eq!(failed(&out).len(), 1);
    assert!(unhandled(&out).is_empty(), "{out}");
    let out = sim.tx("alice.near", "jar.near").transfer(Balance::from_near(1_000)).transact();
    assert!(matches!(out.failure(), Some(Failure::InvalidTx(_))), "{out}");
    sim.findings().assert_none();
}

/// A transaction to its own signer runs its receipt locally, and the log
/// keeps no body for it: only the result path makes its failure handled.
#[test]
fn local_receipt_failure_is_handled() {
    let mut sim = tip_sim();
    let out = sim
        .call("jar.near", "jar.near", "tip")
        .args(b"alice.near".to_vec())
        .gas(Gas::from_gigagas(1))
        .transact();
    assert_eq!(out.failure(), Some(Failure::OutOfGas));
    assert!(out.receipts.iter().all(|r| !r.unhandled), "{out}");
    sim.findings().assert_none();
}

/// A gas refund to a signer that deleted its account in the meantime fails
/// and burns the unused gas. The signer's own doing: the call still
/// succeeds, `check` reports nothing, and conservation holds.
#[test]
fn a_failed_gas_refund_is_burnt_not_reported() {
    use near_contract_sim::nearcore::near_primitives::action::DeleteAccountAction;
    let mut sim = tip_sim();
    let tip = sim.call("alice.near", "jar.near", "tip").args(b"jar.near".to_vec()).submit();
    sim.step(); // converted: the tip runs in the next block and refunds alice's gas
    let delete = DeleteAccountAction { beneficiary_id: "jar.near".parse().unwrap() };
    sim.tx("alice.near", "alice.near").action(delete).transact().assert_success();
    assert!(sim.account("alice.near").is_none());
    let out = sim.outcome(tip);
    out.assert_success();
    let gone = Some(Failure::AccountDoesNotExist("alice.near".parse().unwrap()));
    let refunds: Vec<_> = out.receipts.iter().filter(|r| r.is_refund()).collect();
    assert!(!refunds.is_empty(), "{out:#}");
    assert!(refunds.iter().all(|r| r.failure() == gone && !r.unhandled), "{out:#}");
    sim.findings().assert_none();
}

/// A deleted account's balance goes to its beneficiary in a balance refund,
/// and nearcore never checks that the beneficiary exists: to a missing one
/// the refund fails and the balance is burnt. The transaction succeeds, and
/// only the unhandled failure tells.
#[test]
fn a_balance_lost_to_a_missing_beneficiary_is_unhandled() {
    use near_contract_sim::nearcore::near_primitives::action::DeleteAccountAction;
    let mut sim = tip_sim();
    let delete = DeleteAccountAction { beneficiary_id: "missing.near".parse().unwrap() };
    let out = sim.tx("alice.near", "alice.near").action(delete).transact();
    out.assert_success();
    let u = unhandled(&out);
    assert_eq!(u.len(), 1, "{out:#}");
    assert!(u[0].is_refund() && u[0].deposit > Balance::from_near(99), "{out:#}");
    assert_eq!(u[0].failure(), Some(Failure::AccountDoesNotExist("missing.near".parse().unwrap())));
    assert!(out.to_string().contains("(refund)"), "{out}");
    let vs = sim.findings();
    assert_eq!(txs(&vs), [out.hash]);
    let named = vs[0].message.starts_with("missing.near <- transfer ");
    assert!(named && vs[0].message.contains(" NEAR (refund) ("), "{}", vs[0].message);
}

/// token.near and receiver.near, each keeping its side of a transfer.
fn ledger_sim() -> Sim {
    let mut sim = Sim::new();
    sim.create_account("alice.near", Balance::from_near(100));
    sim.deploy("token.near", ledger_pair_wasm());
    sim.deploy("receiver.near", ledger_pair_wasm());
    sim
}

/// The token records a transfer and calls the receiver, which credits it,
/// then fails in its own callback. That failure reaches the token's resolve,
/// which refunds: every failure is handled, yet the token says nothing was
/// sent and the receiver says it was credited.
#[test]
fn a_handled_failed_callback_is_flagged() {
    let mut sim = ledger_sim();
    let out =
        sim.call("alice.near", "token.near", "send").args(b"receiver.near".to_vec()).transact();
    out.assert_success();
    assert_eq!(sim.state("token.near", b"sent"), None);
    assert_eq!(sim.state("receiver.near", b"credited"), Some(b"1".to_vec()));
    assert!(unhandled(&out).is_empty(), "{out}");
    let vs = sim.findings();
    let [v] = &vs[..] else { panic!("{vs:?}") };
    assert_eq!(v.check, builtin::FAILED_CALLBACK);
    let settle = out.receipt("settle").unwrap();
    assert_eq!(v.receipt_id, Some(settle.receipt_id));
    let on_transfer = out.receipt("on_transfer").unwrap();
    let want = format!(
        "receiver.near::settle ({}…): panicked: settle failed [{}]\n  transaction status: \
         SUCCESS; a callback failed: its contract keeps what it committed before the call it \
         waited on (in receiver.near::on_transfer ({}…): storage \"credited\"), while this \
         failure tells the caller the call failed",
        &settle.receipt_id.to_string()[..6],
        v.message.split_once(" [").unwrap().1.split_once(']').unwrap().0,
        &on_transfer.receipt_id.to_string()[..6],
    );
    assert_eq!(v.message, want);
}

/// A finding at or after a receipt its transaction had something forced on
/// is marked injected; the same finding with nothing forced isn't. A hold
/// forces no failure, so it excuses nothing. Sites name the failed receipt.
#[test]
fn a_finding_after_a_hold_is_marked_injected() {
    fn send(sim: &mut Sim) -> near_contract_sim::Tx<'_> {
        sim.call("alice.near", "token.near", "send").args(b"receiver.near".to_vec())
    }
    let found = |hold: bool| {
        let mut sim = ledger_sim();
        let tx = send(&mut sim);
        let tx = if hold { tx.inject(Receipts::index(0), Fault::Hold(1)) } else { tx };
        let out = tx.transact();
        assert_eq!(out.receipts.iter().any(|r| r.held.is_some()), hold, "{out}");
        let vs = sim.findings();
        let got: Vec<_> =
            vs.iter().map(|v| (v.check.clone(), v.site.to_string(), v.tx, v.injected)).collect();
        (got, out.hash)
    };
    for hold in [false, true] {
        let (got, tx) = found(hold);
        let want =
            (builtin::FAILED_CALLBACK.into(), "receiver.near::settle".into(), Some(tx), hold);
        assert_eq!(got, [want]);
    }
}

fn fuzz_sim() -> Sim {
    let mut sim = Sim::new();
    sim.create_account("alice.near", Balance::from_near(100));
    sim.deploy("f.near", fuzz_wasm());
    sim.deploy("g.near", fuzz_wasm());
    sim
}

fn run_prog(sim: &mut Sim, ops: &[Op]) -> near_contract_sim::TxOutcome {
    sim.call("alice.near", "f.near", "run").args(encode(ops)).transact()
}

fn call(to: &str, ops: &[Op], gas_t: u64) -> Op {
    Op::Call { to: to.into(), args: encode(ops), deposit: 0, gas: gas_t * TGAS, weight: 0 }
}

/// A returned promise takes its parent's place: returned by the
/// transaction's receipt, its failure is the transaction's failure; returned
/// by a receipt a callback waits on, it inherits that receipt's result
/// receivers, so the callback receives its failure.
#[test]
fn returned_promise_failure_is_handled() {
    let mut sim = fuzz_sim();
    let out = run_prog(&mut sim, &[call("g.near", &[Op::Panic], 10), Op::PromiseReturn]);
    out.assert_failure();
    assert!(unhandled(&out).is_empty(), "{out}");
    // g returns f's panicking call, whose failure then reaches f's callback.
    let returned = call("g.near", &[call("f.near", &[Op::Panic], 5), Op::PromiseReturn], 20);
    let then = Op::Then { args: encode(&[Op::Results]), gas: 10 * TGAS, weight: 0 };
    let out = run_prog(&mut sim, &[returned.clone(), then]);
    assert_eq!(out.assert_success().logs(), ["n1|x"]);
    assert!(unhandled(&out).is_empty(), "{out}");
    // Without the callback, nobody receives it.
    let out = run_prog(&mut sim, &[returned]);
    let u = unhandled(&out);
    assert_eq!(u.len(), 1, "{out}");
    assert_eq!(u[0].receiver_id.as_str(), "f.near");
    // Detached (not returned): unhandled, classified as a panic.
    let out = run_prog(&mut sim, &[call("g.near", &[Op::Panic], 10)]);
    out.assert_success();
    let u = unhandled(&out);
    assert_eq!(u.len(), 1);
    assert_eq!(u[0].failure(), Some(Failure::Panic("fuzz panic".into())));
}

/// `promise_and`: both results go to the joint callback.
#[test]
fn promise_and_delivers_every_failure() {
    let mut sim = fuzz_sim();
    let out = run_prog(
        &mut sim,
        &[
            call("g.near", &[Op::Panic], 10),
            call("g.near", &[Op::BurnGas(1_000_000)], 3),
            Op::And,
            Op::Then { args: encode(&[Op::Results]), gas: 10 * TGAS, weight: 0 },
        ],
    );
    out.assert_success();
    let f = failed(&out);
    assert_eq!(f.len(), 2, "{out}");
    assert!(f.iter().all(|r| !r.unhandled));
    sim.findings().assert_none();
}

/// Yield timeouts are not failures: the callback gets `PromiseResult::Failed`
/// and runs. A callback that then panics is unhandled unless the yield was
/// returned (then it's the transaction's result), and a failed callback
/// either way.
#[test]
fn yield_timeout_callbacks() {
    let mut sim = fuzz_sim();
    let cb = |ops: &[Op]| Op::YieldCreate { slot: 0, args: encode(ops), gas: 10 * TGAS, weight: 0 };
    // Detached yield whose callback is fine: nothing to report after timeout.
    let a = run_prog(&mut sim, &[cb(&[Op::Results])]).hash;
    // Detached yield whose callback panics.
    let b = run_prog(&mut sim, &[cb(&[Op::Panic])]).hash;
    // Returned yield whose callback panics.
    let c = run_prog(&mut sim, &[cb(&[Op::Panic]), Op::PromiseReturn]).hash;
    assert!(sim.outcome(c).is_pending());
    sim.findings().assert_none(); // nothing failed yet, and the parked callbacks hold their gas
    sim.advance_to(sim.height() + 210);
    assert!(failed(&sim.outcome(a)).is_empty());
    assert_eq!(unhandled(&sim.outcome(b)).len(), 1);
    assert!(sim.outcome(c).is_failure());
    assert!(unhandled(&sim.outcome(c)).is_empty());
    let vs = sim.findings();
    assert_eq!(txs(&vs), [b, c]);
    assert_eq!(
        vs.iter().map(|v| v.check.as_str()).collect::<Vec<_>>(),
        [builtin::UNHANDLED_FAILURE, builtin::FAILED_CALLBACK]
    );
}

/// `check` lists the unhandled failures in the order they ran, not by
/// transaction.
#[test]
fn check_reports_every_failure_in_order() {
    let mut sim = fuzz_sim();
    // a's yield callback panics at its timeout, long after b's call did.
    let cb = Op::YieldCreate { slot: 0, args: encode(&[Op::Panic]), gas: 10 * TGAS, weight: 0 };
    let a = run_prog(&mut sim, &[cb]).hash;
    let b = run_prog(&mut sim, &[call("g.near", &[Op::Panic], 10)]).hash;
    sim.advance_to(sim.height() + 210);
    assert_eq!(txs(&sim.findings()), [b, a]);
}

/// A callback that fails with nothing of its contract's to reconcile isn't
/// flagged: f's callback panics on g's failure, or after g wrote, and f
/// wrote nothing before. Once f writes before the call, the same panic is
/// flagged.
#[test]
fn a_failed_callback_is_flagged_only_after_its_contract_wrote() {
    let mut sim = fuzz_sim();
    let then = Op::Then { args: encode(&[Op::Panic]), gas: 10 * TGAS, weight: 0 };
    let out =
        run_prog(&mut sim, &[call("g.near", &[Op::Panic], 10), then.clone(), Op::PromiseReturn]);
    out.assert_failure();
    assert!(unhandled(&out).is_empty(), "{out}");
    // g writes, but the callback is f's, and f has nothing to reconcile.
    let write = Op::Write { key: b"k".to_vec(), value: b"v".to_vec() };
    let g_writes = call("g.near", std::slice::from_ref(&write), 10);
    run_prog(&mut sim, &[g_writes, then.clone(), Op::PromiseReturn]).assert_failure();
    sim.findings().assert_none();
    let out = run_prog(&mut sim, &[write, call("g.near", &[], 10), then, Op::PromiseReturn]);
    out.assert_failure();
    let vs = sim.findings();
    let [v] = &vs[..] else { panic!("{vs:?}") };
    assert_eq!((v.check.as_str(), v.tx), (builtin::FAILED_CALLBACK, Some(out.hash)));
}

/// The violation names what the contract committed: here a balance that
/// fell by the deposit it sent, then its storage, the first four of six
/// changes in all.
#[test]
fn a_failed_callback_names_what_its_contract_committed() {
    let mut sim = fuzz_sim();
    let mut ops: Vec<_> =
        (0..5).map(|i| Op::Write { key: format!("k{i}").into(), value: b"v".to_vec() }).collect();
    let pay =
        Op::Call { to: "g.near".into(), args: encode(&[]), deposit: 1, gas: 10 * TGAS, weight: 0 };
    ops.extend([pay, Op::Then { args: encode(&[Op::Panic]), gas: 10 * TGAS, weight: 0 }]);
    ops.push(Op::PromiseReturn);
    let out = run_prog(&mut sim, &ops);
    out.assert_failure();
    let vs = sim.findings();
    let [v] = &vs[..] else { panic!("{vs:?}") };
    let run = out.receipt("run").unwrap();
    let want = format!(
        "(in f.near::run ({}…): balance, storage \"k0\", storage \"k1\", storage \"k2\" and 2 \
         more), while",
        &run.receipt_id.to_string()[..6]
    );
    assert!(v.message.contains(&want), "{}", v.message);
}

// ---------------------------------------------------------------------------
// Conservation and snapshots
// ---------------------------------------------------------------------------

/// Random fuzz-contract scenarios (signed txs, transfers, account creation,
/// yields, clock moves): the supply ledger balances after every step.
#[test]
fn conservation_holds_on_random_scenarios() {
    let strat = scenario_strategy("c".into(), StepConfig { sim_only: true, depth: 2 }, 10);
    runner()
        .run(&strat, |sc| {
            let mut r = SimRunner::new(&sc);
            conservation(&r.sim).map_err(|vs| TestCaseError::fail(format!("{vs:?}")))?;
            for s in &sc.steps {
                r.step(s);
                conservation(&r.sim).map_err(|vs| TestCaseError::fail(format!("{s:?}: {vs:?}")))?;
            }
            Ok(())
        })
        .unwrap();
}

/// A receipt in flight carries value no account holds yet, so conservation
/// waits until nothing is in flight.
#[test]
fn conservation_waits_for_nothing_in_flight() {
    let mut sim = tip_sim();
    sim.call("alice.near", "jar.near", "tip").args(b"alice.near".to_vec()).submit();
    sim.step();
    assert!(!sim.is_idle() && sim.yields().is_empty());
    sim.findings().assert_none();
    sim.settle();
    sim.findings().assert_none();
}

#[test]
fn conservation_counts_sim_mints() {
    let mut sim = tip_sim();
    sim.set_balance("alice.near", Balance::from_near(7));
    sim.call("alice.near", "jar.near", "tip")
        .args(b"alice.near".to_vec())
        .transact()
        .assert_success();
    sim.findings().assert_none();
}

/// State records mint what they add, as every state write does: an account
/// (amount and locked), a funded gas key, and then an overwrite of the
/// account with less. Contract data mints nothing.
#[test]
fn conservation_counts_state_records() {
    use near_contract_sim::nearcore::near_primitives::account::{
        AccessKey, Account, AccountContract,
    };
    use near_contract_sim::{KeyType, SecretKey, StateRecord};
    let near = Balance::from_near;
    let account = |amount, locked| StateRecord::Account {
        account_id: "a.near".parse().unwrap(),
        account: Account::new(near(amount), near(locked), AccountContract::None, 0),
    };
    let mut gas_key = AccessKey::gas_key_full_access(1);
    gas_key.gas_key_info_mut().unwrap().balance = near(3);
    let pk = SecretKey::from_seed(KeyType::ED25519, "gk").public_key();
    let mut sim = Sim::new();
    sim.apply_state_records([
        account(10, 2),
        StateRecord::access_key("a.near".parse().unwrap(), pk, gas_key),
    ]);
    sim.findings().assert_none();
    sim.apply_state_records([account(4, 0)]);
    sim.set_state("a.near", b"k", Some(b"v"));
    sim.findings().assert_none();
}

/// `Sim: Clone` is a snapshot: both copies run on independently.
#[test]
fn clone_is_a_snapshot() {
    let mut a = tip_sim();
    let jar0 = a.balance("jar.near");
    let mut b = a.clone();
    a.call("alice.near", "jar.near", "tip")
        .args(b"alice.near".to_vec())
        .transact()
        .assert_success();
    assert_eq!(b.balance("jar.near"), jar0);
    assert!(a.balance("jar.near") < jar0);
    let before = b.balance("jar.near");
    b.call("alice.near", "jar.near", "tip")
        .args(b"alice.near".to_vec())
        .transact()
        .assert_success();
    assert_eq!(b.balance("jar.near"), a.balance("jar.near"));
    assert!(b.balance("jar.near") < before);
    // The original still reads its own state after the clone moved on.
    assert_eq!(a.view_state("jar.near", b""), vec![]);
    a.findings().assert_none();
    b.findings().assert_none();
}

/// The receipt metadata the checks read: transfer and call deposits,
/// attached gas, and a call's predecessor.
#[test]
fn receipt_outcomes_carry_metadata() {
    let mut sim = tip_sim();
    let out =
        sim.call("alice.near", "jar.near", "tip_checked").args(b"nobody.near".to_vec()).transact();
    let transfer = out.receipts.iter().find(|r| r.receiver_id.as_str() == "nobody.near").unwrap();
    assert_eq!(transfer.deposit, Balance::from_near(1));
    let cb = out.receipts.iter().find(|r| r.method_names == ["on_tip"]).unwrap();
    assert_eq!(cb.attached_gas, Gas::from_teragas(5));
    let out = sim
        .call("alice.near", "jar.near", "tip")
        .args(b"alice.near".to_vec())
        .gas(Gas::from_teragas(20))
        .deposit(Balance::from_near(2))
        .transact();
    let root = &out.receipts[0];
    assert_eq!(root.predecessor_id.as_str(), "alice.near");
    assert_eq!(root.method_names, ["tip"]);
    assert_eq!(root.attached_gas, Gas::from_teragas(20));
    assert_eq!(root.deposit, Balance::from_near(2));
}

/// Conservation through the protocol features that move NEAR outside plain
/// account balances: gas-key funding, gas paid from and refunded to a key,
/// withdrawal, deletion burning the key's rest, the gas a key pays for a
/// deposit the account can't cover, global-contract storage burnt at
/// publish, meta transactions (relayer pays gas, a failed inner deposit
/// goes back to the sender) and an ML-DSA signer.
#[test]
fn conservation_holds_across_protocol_features() {
    use near_contract_sim::{
        AccessKey, GlobalContractDeployMode, GlobalContractIdentifier, KeyType, SecretKey,
    };
    let near = Balance::from_near;
    let mut sim = Sim::new();
    sim.create_account("alice.near", near(100));
    sim.create_account("relayer.near", near(100));
    sim.deploy("c.near", fuzz_wasm());
    sim.create_account_with_key("pq.near", near(10), &SecretKey::from_seed(KeyType::MLDSA65, "pq"));
    conservation(&sim).unwrap();
    let step = |sim: &mut Sim, what: &str| {
        conservation(sim).unwrap_or_else(|vs| panic!("after {what}: {vs:?}"));
    };

    let gk = SecretKey::from_seed(KeyType::ED25519, "alice-gas");
    let pk = gk.public_key();
    sim.tx("alice.near", "alice.near")
        .add_key(pk.clone(), AccessKey::gas_key_full_access(2))
        .transfer_to_gas_key(pk.clone(), near(3))
        .transact()
        .assert_success();
    step(&mut sim, "funding a gas key");
    sim.call("alice.near", "c.near", "run")
        .args(encode(&[Op::Env(4)]))
        .gas(Gas::from_teragas(30))
        .sign_with(&gk)
        .nonce_index(0)
        .transact()
        .assert_success();
    step(&mut sim, "gas-key call");
    let out = sim
        .tx("alice.near", "relayer.near")
        .transfer(near(1_000))
        .sign_with(&gk)
        .nonce_index(1)
        .transact();
    assert!(out.is_failure());
    step(&mut sim, "gas-key tx with a deposit beyond the balance");
    sim.tx("alice.near", "alice.near")
        .withdraw_from_gas_key(pk.clone(), Balance::from_millinear(2_500))
        .transact()
        .assert_success();
    step(&mut sim, "withdraw_from_gas_key");
    sim.tx("alice.near", "alice.near").delete_key(pk).transact().assert_success();
    step(&mut sim, "deleting a gas key (burns its rest)");

    sim.tx("alice.near", "alice.near")
        .deploy_global(fuzz_wasm(), GlobalContractDeployMode::CodeHash)
        .transact()
        .assert_success();
    step(&mut sim, "deploy_global");
    sim.create_account("app.near", near(5));
    let code_hash = near_contract_sim::nearcore::near_primitives::hash::hash(&fuzz_wasm());
    sim.tx("app.near", "app.near")
        .use_global(GlobalContractIdentifier::CodeHash(code_hash))
        .transact()
        .assert_success();
    step(&mut sim, "use_global");

    sim.call("alice.near", "c.near", "run")
        .args(encode(&[Op::Env(4)]))
        .gas(Gas::from_teragas(30))
        .relay("relayer.near")
        .assert_success();
    step(&mut sim, "delegate");
    sim.call("alice.near", "c.near", "run")
        .args(encode(&[Op::Panic]))
        .gas(Gas::from_teragas(30))
        .deposit(near(1))
        .relay("relayer.near")
        .assert_success();
    step(&mut sim, "delegate with a failed inner deposit");

    sim.call("pq.near", "c.near", "run").args(encode(&[Op::Env(5)])).transact().assert_success();
    step(&mut sim, "ML-DSA signer");
}

/// A continuation whose receiver was redeployed after its transaction
/// began fails against the new code; "nobody told" says the code changed
/// and at which block: an upgrade under an in-flight chain.
/// Not when the same code was deployed again, nor when the transaction
/// created and deployed the account itself (a factory): it had no code
/// when the transaction began.
///
/// Verification boundary: the built-in finding's message, over actual
/// runs of the pinned runtime.
#[test]
fn nobody_told_after_a_redeploy_says_the_code_changed() {
    let no_run = wat::parse_str("(module (func (export \"other\")))").unwrap();
    let call = |program: &[Op]| Op::Call {
        to: "other.near".into(),
        args: encode(program),
        deposit: 0,
        gas: 10 * TGAS,
        weight: 0,
    };
    let message = |redeploy: Option<&[u8]>, call: Op| {
        let mut sim = Sim::new();
        sim.create_account("alice.near", Balance::from_near(100));
        sim.deploy("contract.near", fuzz_wasm());
        sim.deploy("other.near", fuzz_wasm());
        // The detached call reaches other.near two blocks on; the deploy
        // lands in between.
        sim.call("alice.near", "contract.near", "run").args(encode(&[call])).submit();
        if let Some(code) = redeploy {
            sim.tx("other.near", "other.near").deploy(code.to_vec()).submit();
        }
        sim.run_until(20, Sim::is_idle).expect("it settles");
        let found = sim.findings().into_iter().find(|f| f.check == builtin::UNHANDLED_FAILURE);
        found.map(|f| f.message).unwrap_or_default()
    };
    let redeployed = message(Some(&no_run), call(&[]));
    assert!(
        redeployed.contains("other.near's code changed since its transaction began"),
        "{redeployed}"
    );
    assert!(message(None, call(&[])).is_empty(), "the call succeeds on the old code");
    // The same code deployed again, and a call that fails on it anyway.
    let same = message(Some(&fuzz_wasm()), call(&[Op::Panic]));
    assert!(same.contains("fuzz panic") && !same.contains("code changed"), "{same}");
    // A factory: one transaction creates the account, deploys and calls it,
    // and its own continuation fails.
    let mut sim = Sim::new();
    sim.create_account("alice.near", Balance::from_near(100));
    let fails = Op::Call {
        to: "f.alice.near".into(),
        args: encode(&[Op::Panic]),
        deposit: 0,
        gas: 10 * TGAS,
        weight: 0,
    };
    let out = (sim.tx("alice.near", "f.alice.near").create_account())
        .transfer(Balance::from_near(10))
        .deploy(fuzz_wasm())
        .call("run")
        .args(encode(&[fails]))
        .transact();
    let found = sim.findings().into_iter().find(|f| f.check == builtin::UNHANDLED_FAILURE);
    let found = found.unwrap_or_else(|| panic!("the continuation fails: {out}"));
    assert!(!found.message.contains("code changed"), "{}", found.message);
}
