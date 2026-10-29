//! `explore::Harness::run` on actual WASM: a case runs to its end (a
//! failing check doesn't keep the others from judging it), every distinct
//! failure is its own finding at its site, per-op
//! checks fire once, at completion,
//! on the op's own effects, and what couldn't be judged is counted. The
//! programmable fuzz contract and the token vault specimen are the
//! contracts; nothing here models the executor.
//!
//! Verification boundary: the executor's scheduling, judging and verdicts
//! over actual runs of the pinned runtime.

mod common;
mod explore_vault;
mod fuzz_common;

use fuzz_common::{Op as Fz, TGAS, encode, fuzz_wasm};
use near_contract_sim::explore::{
    Amount, Fail, Faults, Harness, Op, OpDone, Plan, Skip, Verdict, plans,
};
use near_contract_sim::{Balance, Delta, Fault, Gas, Receipts, Sim, builtin};
use proptest::prelude::*;
use proptest::strategy::ValueTree;
use proptest::test_runner::TestRunner;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::cell::RefCell;
use std::rc::Rc;

const NEAR: u128 = 1_000_000_000_000_000_000_000_000;

/// alice and bob, and the fuzz contract on contract.near and other.near.
fn fixture() -> Sim {
    let mut sim = Sim::new();
    sim.create_account("alice.near", Balance::from_near(100));
    sim.create_account("bob.near", Balance::from_near(100));
    // No code: a call to either fails, at a site that names it, as the
    // fixture has it (a plan's own name for an account the fixture lacks
    // reads `<new account>`).
    sim.create_account("missing-a.near", Balance::from_near(1));
    sim.create_account("missing-b.near", Balance::from_near(1));
    sim.deploy("contract.near", fuzz_wasm());
    sim.deploy("other.near", fuzz_wasm());
    sim
}

/// The fuzz contract's operations, and some that misbehave.
#[derive(Clone, Debug, Serialize, Deserialize)]
enum Prog {
    /// alice runs this program on contract.near.
    Run(Vec<Fz>),
    /// `who` runs it with a deposit.
    Pay { who: String, yocto: u128, program: Vec<Fz> },
    /// One call with its gas left unpinned and one pinned at 50 Tgas.
    TwoCalls,
    /// Skips itself.
    Nothing,
    /// Submits nothing, without skipping: judged at once.
    Quiet,
    /// One call, its gas pinned to this many Tgas if any: `Tx::gas(None)`
    /// leaves it unpinned.
    MaybePinned(Option<u64>),
    /// Produces blocks: a harness error.
    Transact,
    /// Skips after submitting: a harness error.
    SubmitThenSkip,
}

/// `who` calls contract.near's `run` with `program`.
fn run<'a>(sim: &'a mut Sim, who: &str, program: &[Fz]) -> near_contract_sim::Tx<'a> {
    sim.call(who, "contract.near", "run").args(encode(program))
}

impl Op for Prog {
    fn submit(&self, sim: &mut Sim) -> Result<(), Skip> {
        match self {
            Prog::Run(program) => {
                run(sim, "alice.near", program).submit();
            }
            Prog::Pay { who, yocto, program } => {
                run(sim, who, program).deposit(Balance::from_yoctonear(*yocto)).submit();
            }
            Prog::TwoCalls => {
                run(sim, "alice.near", &[]).submit();
                run(sim, "alice.near", &[Fz::Log("pinned".into())])
                    .gas(Gas::from_teragas(50))
                    .submit();
            }
            Prog::Nothing => return Err("nothing to do".into()),
            Prog::Quiet => {}
            Prog::MaybePinned(tgas) => {
                run(sim, "alice.near", &[]).gas(tgas.map(Gas::from_teragas)).submit();
            }
            Prog::Transact => {
                run(sim, "alice.near", &[]).transact().assert_success();
            }
            Prog::SubmitThenSkip => {
                run(sim, "alice.near", &[]).submit();
                return Err("too late".into());
            }
        }
        Ok(())
    }
}

fn write(key: &[u8]) -> Fz {
    Fz::Write { key: key.to_vec(), value: b"1".to_vec() }
}

/// A call whose failure nobody learns of: `missing` doesn't exist.
fn detached_call(missing: &str) -> Fz {
    Fz::Call { to: missing.into(), args: encode(&[]), deposit: 0, gas: 10 * TGAS, weight: 0 }
}

/// Calls `to`, then this contract's `then` program as the callback,
/// returning it.
fn call_then(to: &str, then: &[Fz]) -> Vec<Fz> {
    vec![
        Fz::Call { to: to.into(), args: encode(&[]), deposit: 0, gas: 10 * TGAS, weight: 0 },
        Fz::Then { args: encode(then), gas: 10 * TGAS, weight: 0 },
        Fz::PromiseReturn,
    ]
}

fn absent(key: &'static [u8]) -> impl Fn(&Sim) -> Result<(), String> {
    move |sim| match sim.state("contract.near", key) {
        Some(_) => Err(format!("{} was written", String::from_utf8_lossy(key))),
        None => Ok(()),
    }
}

/// Records what a per-op check saw, for the test to read after the run.
fn recorder<T: 'static>() -> Rc<RefCell<Vec<T>>> {
    Rc::new(RefCell::new(vec![]))
}

// ---- A case runs to its end ----

#[test]
fn a_check_failing_at_block_one_does_not_stop_later_ops() {
    let run = Harness::new(fixture())
        .always("no a", absent(b"a"))
        .at_rest("no b", absent(b"b"))
        .per_op("op ran", |_: &OpDone<Prog>| Ok::<_, Fail>(()))
        .run(&Plan::new().op(Prog::Run(vec![write(b"a")])).op(Prog::Run(vec![write(b"b")])));
    assert_eq!(run.verdict(), Verdict::Findings, "{run}");
    let a = run.assert_found("no a", "world");
    let b = run.assert_found("no b", "world");
    assert!(a.occurrences > 1, "every later block fails too, and is counted: {run}");
    assert!(b.block > a.block, "{run}");
    assert_eq!(run.judged()[2].ok, 2, "both ops ran and were judged: {run}");
    assert_eq!((run.judged()[0].failed, run.judged()[0].check.as_str()), (a.occurrences, "no a"));
    assert!(!a.injected && run.reason().unwrap().starts_with("no a at world and 1 more"), "{run}");
}

#[test]
fn every_failing_receipt_is_its_own_finding_at_its_site_and_op() {
    let run = Harness::new(fixture()).run(
        &Plan::new()
            .op(Prog::Run(vec![detached_call("missing-a.near")]))
            .op(Prog::Run(vec![detached_call("missing-b.near")])),
    );
    let a = run.assert_found(builtin::UNHANDLED_FAILURE, "missing-a.near::run");
    assert_eq!(a.site.to_string(), "missing-a.near::run (Run)");
    run.assert_found(builtin::UNHANDLED_FAILURE, "missing-b.near::run (Run)");
    assert_eq!(run.findings().len(), 2, "{run}");
    assert!(a.tx.is_some() && a.receipt_id.is_some());
    assert!(format!("{run}").contains("missing-a.near"), "the trace is drawn: {run}");
}

// ---- check names ----

#[test]
fn a_reused_check_name_is_a_harness_error() {
    let plan = Plan::new().op(Prog::Run(vec![]));
    let error = |h: Harness<Prog>| {
        let run = h.run(&plan);
        assert_eq!(run.verdict(), Verdict::HarnessError, "{run}");
        run.reason().unwrap().to_owned()
    };
    let twice = Harness::new(fixture()).always("x", absent(b"a")).at_rest("x", absent(b"b"));
    assert!(error(twice).contains("two checks are named \"x\""));
    let builtin_name = Harness::new(fixture()).always(builtin::CONSERVATION, absent(b"a"));
    assert!(error(builtin_name).contains("differ from the built-ins'"));
}

/// A check that fails on the untouched fixture is a harness error naming
/// it, however it is judged (after every block, at rest, at
/// the end): the fixture or the contract's initial state is wrong, and no
/// case can tell anything about that check. As a finding it had no op
/// and no receipt, and folded every later violation of the check into it.
#[test]
fn a_check_failing_on_the_fixture_is_a_harness_error() {
    let no_bob = |sim: &Sim| match sim.account("bob.near") {
        Some(_) => Err(String::from("bob.near exists")),
        None => Ok(()),
    };
    let plan = Plan::new().op(Prog::Run(vec![write(b"a")]));
    let harnesses = [
        Harness::new(fixture()).always("no bob", no_bob),
        Harness::new(fixture()).at_rest("no bob", no_bob),
        Harness::new(fixture()).at_end("no bob", no_bob),
    ];
    for h in harnesses {
        let run = h.run(&plan);
        assert_eq!(run.verdict(), Verdict::HarnessError, "{run}");
        let why = run.reason().unwrap();
        assert!(why.contains("\"no bob\" fails on the fixture: bob.near exists"), "{run}");
        assert!(run.outcomes().is_empty(), "no step ran: {run}");
    }
    let audit = Harness::new(fixture())
        .at_rest("no bob", no_bob)
        .scan(std::slice::from_ref(&plan))
        .faults(Faults::none())
        .run();
    assert!(audit.found().is_empty(), "{audit}");
    assert!(audit.errors().iter().any(|e| e.contains("fails on the fixture")), "{audit}");
}

/// Every case of a campaign starts from the same fixture, so a check
/// failing there stops the scan, the search and curves at once: one
/// error, one item, not one per case.
#[test]
fn a_fixture_failure_stops_the_campaign_with_one_error() {
    let no_bob = |sim: &Sim| match sim.account("bob.near") {
        Some(_) => Err(String::from("bob.near exists")),
        None => Ok(()),
    };
    let h = Harness::new(fixture()).at_rest("no bob", no_bob);
    let one = |w: &[u8]| Plan::new().op(Prog::Run(vec![write(w)]));
    let witnesses = [one(b"a"), one(b"b"), one(b"c")];
    let scan = h.scan(&witnesses).run();
    let curves = h.curves(&witnesses).run();
    let search = h.search(plans(Just(Prog::Run(vec![write(b"a")])))).cases(32).run();
    for audit in [scan, curves, search] {
        assert_eq!((audit.cases(), audit.errors().len()), (1, 1), "{audit}");
        assert!(audit.errors()[0].contains("\"no bob\" fails on the fixture"), "{audit}");
        let errors: Vec<_> = audit.todo().into_iter().filter(|t| t.id == "errors").collect();
        assert!(errors.len() == 1 && errors[0].why.starts_with("1 harness error"), "{audit}");
    }
}

// ---- forced excusal and the injected label ----

/// Writes `pending`, calls `other.near`, and removes `pending` in the
/// callback.
fn temporary_state() -> Vec<Fz> {
    let mut p = vec![write(b"pending")];
    p.extend(call_then("other.near", &[Fz::Remove { key: b"pending".to_vec() }]));
    p
}

/// A check after every block sees state a callback hasn't cleaned up yet;
/// one at rest waits for the callback. (Ported from the old runner's
/// `at_rest_waits_for_the_callback_but_block_checks_see_temporary_state`.)
#[test]
fn always_sees_temporary_state_but_at_rest_waits_for_the_callback() {
    let plan = Plan::new().op(Prog::Run(temporary_state()));
    Harness::new(fixture()).at_rest("no pending", absent(b"pending")).run(&plan).assert_clean();
    let run = Harness::new(fixture()).always("no pending", absent(b"pending")).run(&plan);
    run.assert_found("no pending", "world");
}

#[test]
fn a_starved_receipt_is_excused_but_what_it_caused_is_not() {
    // The starved callback's own out-of-gas is what the plan forced: no
    // built-in finding. That it never removed `pending` is a finding, and
    // labelled injected.
    let harness = Harness::new(fixture()).at_rest("no pending", absent(b"pending"));
    let plan =
        Plan::new().op(Prog::Run(temporary_state())).inject(Receipts::callbacks(), Fault::OutOfGas);
    let run = harness.run(&plan);
    assert_eq!(run.verdict(), Verdict::Findings, "{run}");
    assert!(run.assert_found("no pending", "world").injected, "{run}");
    assert_eq!(run.findings().len(), 1, "the starved callback is excused: {run}");
    // Unfaulted, the same plan is clean.
    let plain = Plan::new().op(Prog::Run(temporary_state()));
    harness.run(&plain).assert_clean();
}

#[test]
fn a_forced_callee_is_excused_but_the_callback_is_not() {
    // `PromiseFailed` starves the call to other.near (excused); the
    // callback that then fails after its contract wrote `pending` is a
    // finding at its own site.
    let mut program = vec![write(b"pending")];
    program.extend(call_then("other.near", &[Fz::Panic]));
    let plan =
        Plan::new().op(Prog::Run(program)).inject(Receipts::callbacks(), Fault::PromiseFailed);
    let run = Harness::new(fixture()).run(&plan);
    let f = run.assert_found(builtin::FAILED_CALLBACK, "contract.near::run (Run)");
    assert!(f.injected, "{run}");
    assert_eq!(run.findings().len(), 1, "other.near's starved call is excused: {run}");
}

// ---- per-op checks ----

#[test]
fn per_op_fires_at_completion_under_overlap_with_exact_effects() {
    let seen = recorder::<(String, i128, i128, i128, u64)>();
    let log = seen.clone();
    let harness = Harness::new(fixture()).per_op("record", move |d: &OpDone<Prog>| {
        let Prog::Pay { who, .. } = d.op() else { return Ok::<_, Fail>(()) };
        let e = d.effects();
        let net = |who| i128::try_from(e.near_net(who)).expect("fits an i128");
        let net = (net("alice.near"), net("bob.near"), net("contract.near"));
        // Its last receipt's block: world() would be the shared world.
        let receipts = d.outcomes().iter().flat_map(|o| &o.receipts);
        let done_at = receipts.map(|r| r.block_height).max().unwrap_or(0);
        log.borrow_mut().push((who.clone(), net.0, net.1, net.2, done_at));
        Ok(())
    });
    // alice pays 1 NEAR; bob pays 2 NEAR into a call with a callback, in
    // the same block, so bob's transaction is still running when alice's
    // completes.
    let alice = Prog::Pay { who: "alice.near".into(), yocto: NEAR, program: vec![] };
    let bob = Prog::Pay {
        who: "bob.near".into(),
        yocto: 2 * NEAR,
        program: call_then("other.near", &[]),
    };
    let run = harness.run(&Plan::new().op(alice.clone()).blocks(0).op(bob.clone()));
    run.assert_clean();
    let seen = seen.borrow();
    let n = |x: u128| x as i128;
    assert_eq!(seen.len(), 2, "each op judged once: {seen:?}");
    assert_eq!(seen[0].0, "alice.near");
    assert_eq!(
        (seen[0].1, seen[0].2, seen[0].3),
        (-n(NEAR), 0, n(NEAR)),
        "bob's overlap is not alice's"
    );
    assert_eq!((seen[1].1, seen[1].2, seen[1].3), (0, -n(2 * NEAR), n(2 * NEAR)));
    assert!(seen[0].4 < seen[1].4, "alice's fired first, while bob's ran: {seen:?}");

    // The same transactions by hand: each check fired at the very block its
    // transaction became complete.
    let mut sim = fixture();
    let a = sim
        .call("alice.near", "contract.near", "run")
        .args(encode(&[]))
        .deposit(Balance::from_yoctonear(NEAR))
        .submit();
    let b = sim
        .call("bob.near", "contract.near", "run")
        .args(encode(&call_then("other.near", &[])))
        .deposit(Balance::from_yoctonear(2 * NEAR))
        .submit();
    let complete = |sim: &mut Sim, tx| {
        while !sim.outcome(tx).is_complete() {
            sim.step();
        }
        sim.height()
    };
    assert_eq!(complete(&mut sim, a), seen[0].4);
    assert_eq!(complete(&mut sim, b), seen[1].4);
}

#[test]
fn a_skipped_op_is_not_judged_and_the_case_goes_on() {
    let harness = Harness::new(fixture()).per_op("ran", |_: &OpDone<Prog>| Ok::<_, Fail>(()));
    let run = harness.run(&Plan::new().op(Prog::Nothing).op(Prog::Run(vec![])));
    // A written plan's op that skipped never ran: incomplete, naming it,
    // and the case went on.
    assert_eq!(run.verdict(), Verdict::Incomplete, "{run}");
    assert!(run.findings().is_empty(), "{run}");
    let why = run.reason().unwrap();
    assert!(why.contains("skipped, so not judged: Nothing (step 0): nothing to do"), "{why}");
    assert_eq!(run.judged()[0].ok, 1, "only the op that ran: {run}");
    assert!(format!("{run}").contains("[skipped: nothing to do]"), "{run}");
    let run = harness.run(&Plan::new().op(Prog::Nothing));
    assert_eq!(run.verdict(), Verdict::Incomplete, "{run}");
    assert!(run.reason().unwrap().contains("every op skipped"), "{run}");
}

/// An op that skips itself after an earlier op's finding keeps the case
/// going, and the finding.
#[test]
fn a_skip_after_a_finding_keeps_the_finding() {
    let lost = Prog::Run(vec![detached_call("missing-a.near")]);
    let run = Harness::new(fixture()).run(&Plan::new().op(lost).op(Prog::Nothing));
    assert_eq!(run.verdict(), Verdict::Findings, "{run}");
    run.assert_found(builtin::UNHANDLED_FAILURE, "missing-a.near::run (Run)");
}

#[test]
fn an_op_that_produces_a_block_is_a_harness_error() {
    let harness = Harness::new(fixture()).always("no a", absent(b"a"));
    let run = harness.run(&Plan::new().op(Prog::Run(vec![write(b"a")])).op(Prog::Transact));
    assert_eq!(run.verdict(), Verdict::HarnessError, "{run}");
    let why = run.reason().unwrap();
    assert!(why.starts_with("Transact (step 1): Op::submit produced a block"), "{why}");
    assert!(!run.findings().is_empty(), "evidence before the error is kept: {run}");
    let run = harness.run(&Plan::new().op(Prog::SubmitThenSkip));
    assert_eq!(run.verdict(), Verdict::HarnessError, "{run}");
    assert!(
        run.reason().unwrap().contains("returned Skip(\"too late\") after submitting"),
        "{run}"
    );
}

#[test]
fn parked_yields_are_timed_out_at_the_end_and_their_op_judged() {
    let parked = vec![
        Fz::YieldCreate { slot: 0, args: encode(&[]), gas: 10 * TGAS, weight: 0 },
        Fz::PromiseReturn,
    ];
    let heights = recorder::<u64>();
    let log = heights.clone();
    let run = Harness::new(fixture())
        .always("anything", |_: &Sim| Ok::<_, String>(()))
        .at_rest("rest", |_: &Sim| Ok::<_, String>(()))
        .per_op("judged", move |d: &OpDone<Prog>| {
            log.borrow_mut().push(d.world().height());
            assert!(d.outcomes().iter().all(|o| o.is_complete()));
            Ok::<_, Fail>(())
        })
        .run(&Plan::new().op(Prog::Run(parked)));
    run.assert_clean();
    let heights = heights.borrow();
    assert_eq!(heights.len(), 1);
    assert!(heights[0] > 200, "judged once the yield timed out: {heights:?}");
    assert_eq!(run.judged()[0].ok as u64, run.blocks() + 1, "always ran after every block");
    assert!(run.blocks() < 20, "the idle heights were skipped: {run}");
    assert_eq!(run.judged()[1].ok, 2, "at rest only at the fixture and at the end: {run}");
}

/// Timing out a parked yield skips the idle heights, but its callback runs
/// at the height and time stepping block by block gives.
#[test]
fn a_parked_yields_timeout_runs_where_stepping_would_run_it() {
    let parked = vec![
        Fz::YieldCreate { slot: 0, args: encode(&[]), gas: 10 * TGAS, weight: 0 },
        Fz::PromiseReturn,
    ];
    let last_ran = |receipts: &[near_contract_sim::ReceiptOutcome]| {
        let r = receipts.iter().rfind(|r| r.refund.is_none()).expect("a receipt ran");
        (r.block_height, r.block_timestamp_ns)
    };
    let mut sim = fixture();
    let hash = run(&mut sim, "alice.near", &parked).submit();
    while !sim.yields().is_empty() || !sim.is_idle() {
        sim.step();
    }
    let stepped = last_ran(&sim.outcome(hash).receipts);
    let seen = recorder::<(u64, u64)>();
    let log = seen.clone();
    let run = Harness::new(fixture())
        .per_op("judged", move |d: &OpDone<Prog>| {
            log.borrow_mut().push(last_ran(&d.outcomes()[0].receipts));
            Ok::<_, Fail>(())
        })
        .run(&Plan::new().op(Prog::Run(parked)));
    run.assert_clean();
    assert!(run.blocks() < 20, "the idle heights were skipped: {run}");
    assert!(stepped.0 > 200, "the timeout ran: {stepped:?}");
    assert_eq!(*seen.borrow(), [stepped]);
}

/// A yield whose timeout callback yields again, four deep: timing them
/// out takes more than the block budget, so the op stays pending.
#[test]
fn an_op_still_pending_after_the_budget_is_not_judged() {
    let mut sim = fixture();
    sim.config.max_blocks = 5;
    let mut program = vec![];
    for _ in 0..4 {
        let args = encode(&program);
        program = vec![Fz::YieldCreate { slot: 0, args, gas: 20 * TGAS, weight: 1 }];
    }
    let run = Harness::new(sim)
        .per_op("judged", |_: &OpDone<Prog>| Ok::<_, Fail>(()))
        .run(&Plan::new().op(Prog::Run(program)));
    assert_eq!(run.verdict(), Verdict::Incomplete, "{run}");
    let why = run.reason().unwrap();
    assert!(why.contains("parked yields still waiting after 5 blocks"), "{why}");
    assert!(why.contains("pending, not judged: Run (step 0)"), "{why}");
    assert_eq!(run.judged()[0].ok, 0);
}

/// A case that runs out of its block budget still reports what it found,
/// and says why it stopped.
#[test]
fn a_finding_in_a_case_that_runs_out_of_budget_is_still_a_finding() {
    let mut sim = fixture();
    sim.config.max_blocks = 5;
    let mut program = vec![];
    for _ in 0..4 {
        let args = encode(&program);
        program = vec![Fz::YieldCreate { slot: 0, args, gas: 20 * TGAS, weight: 1 }];
    }
    program.insert(0, write(b"a"));
    let run =
        Harness::new(sim).always("no a", absent(b"a")).run(&Plan::new().op(Prog::Run(program)));
    assert_eq!(run.verdict(), Verdict::Findings, "{run}");
    run.assert_found("no a", "world");
    let why = run.reason().unwrap_or_default();
    assert!(why.contains("parked yields still waiting after 5 blocks"), "{run}");
}

#[test]
fn unobserved_is_counted_and_never_clean() {
    // token.near is never watched, so its effects can't be measured.
    let run = Harness::new(fixture())
        .per_op("tokens", |d: &OpDone<Prog>| {
            d.effects().token_net("token.near", "alice.near")?;
            Ok::<_, Fail>(())
        })
        .run(&Plan::new().op(Prog::Run(vec![])).op(Prog::Run(vec![])));
    assert_eq!(run.verdict(), Verdict::Incomplete, "{run}");
    let j = &run.judged()[0];
    assert_eq!((j.ok, j.failed, j.unjudged), (0, 0, 2), "{run}");
    assert!(run.reason().unwrap().contains("tokens not judged 2 time(s)"), "{run}");
    assert!(run.findings().is_empty(), "not judged is not a finding");
}

/// A check's panic is the harness's error, naming the check, never a
/// finding.
#[test]
fn a_panicking_check_is_a_harness_error() {
    let run = Harness::new(fixture())
        .per_op("one tx", |d: &OpDone<Prog>| {
            assert_eq!(d.outcomes().len(), 1, "submitted {} transactions", d.outcomes().len());
            Ok::<_, Fail>(())
        })
        .run(&Plan::new().op(Prog::TwoCalls));
    assert_eq!(run.verdict(), Verdict::HarnessError, "{run}");
    let why = run.reason().unwrap_or_default();
    assert!(why.contains("the check \"one tx\" panicked"), "{why}");
    assert!(why.contains("submitted 2 transactions") && run.findings().is_empty(), "{run}");
}

/// `OpDone::outcome` is the one transaction's outcome, `None` for an op
/// that submitted none (a check for every op can't assume one), and a
/// panic naming the op and the count for one that submitted several: the
/// check is a harness error, never a silent pass.
#[test]
fn outcome_is_none_for_no_transaction_and_panics_for_several() {
    let seen = recorder::<(String, Option<bool>)>();
    let log = seen.clone();
    let harness = Harness::new(fixture()).per_op("reads outcome", move |d: &OpDone<Prog>| {
        log.borrow_mut().push((d.name().to_owned(), d.outcome().map(|o| o.is_complete())));
        Ok::<_, Fail>(())
    });
    harness.run(&Plan::new().op(Prog::Run(vec![])).op(Prog::Quiet)).assert_clean();
    assert_eq!(*seen.borrow(), [("Run".to_owned(), Some(true)), ("Quiet".into(), None)]);
    let run = harness.run(&Plan::new().op(Prog::TwoCalls));
    assert_eq!(run.verdict(), Verdict::HarnessError, "{run}");
    let why = run.reason().unwrap_or_default();
    assert!(why.contains("TwoCalls submitted 2 transactions"), "{why}");
}

#[test]
fn a_keyed_failure_is_its_own_finding() {
    let run = Harness::new(fixture())
        .per_op("by signer", |d: &OpDone<Prog>| {
            let who = d.outcomes()[0].receipts[0].predecessor_id.to_string();
            Err::<(), _>(Fail::new("always").key(who))
        })
        .run(&Plan::new().op(Prog::Run(vec![])).op(Prog::Pay {
            who: "bob.near".into(),
            yocto: 1,
            program: vec![],
        }));
    run.assert_found("by signer", "alice.near");
    run.assert_found("by signer", "bob.near");
    assert_eq!(run.findings().len(), 2, "{run}");
    // A keyed per-op finding prints its op too, answers to the op's name,
    // and gives its key back.
    let f = run.assert_found("by signer", "Pay");
    assert_eq!((f.key(), f.site.to_string().as_str()), (Some("bob.near"), "bob.near (Pay)"));
    run.assert_found("by signer", "alice.near (Run)");
    // `found` is the same lookup, without the panic.
    assert_eq!(run.found_at("by signer", "bob.near").map(|f| &f.check), Some(&"by signer".into()));
    assert!(run.found_at("by signer", "carol.near").is_none());
}

/// The tip jar (`tests/res/tip_jar.wat`) at jar.near, alice and bob.
fn tip_jar() -> Sim {
    let mut sim = Sim::new();
    sim.create_account("alice.near", Balance::from_near(10));
    sim.create_account("bob.near", Balance::from_near(10));
    sim.deploy("jar.near", common::tip_jar_wasm());
    sim
}

/// alice has the jar tip bob 1 NEAR, from its own balance.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct Tip;

impl Op for Tip {
    fn submit(&self, sim: &mut Sim) -> Result<(), Skip> {
        sim.call("alice.near", "jar.near", "tip").args("bob.near").submit();
        Ok(())
    }
}

/// Per op: no account's NEAR moved, each account that moved a failure.
fn nothing_moves(d: &OpDone<Tip>) -> Result<(), Fail> {
    Fail::all(["jar.near", "bob.near"].into_iter().filter_map(|who| {
        let net = d.effects().near_net(who);
        (net != 0).then(|| Fail::new(format!("{who} moved {net}")).key(who))
    }))
}

/// How often `check` passed, failed, wasn't judged and was idle in `run`.
fn counts<O>(run: &near_contract_sim::explore::Run<O>, check: &str) -> (u32, u32, u32, u32) {
    let j = run.judged().iter().find(|j| j.check == check).unwrap();
    (j.ok, j.failed, j.unjudged, j.idle)
}

/// `Fail::all`: one judgement failing for two accounts is one failed
/// judgement and two findings, each at its key; a campaign keeps a row
/// for each.
///
/// Verification boundary: judging and finding identity, over actual runs.
#[test]
fn every_failure_of_one_judgement_is_a_finding() {
    let h = Harness::new(tip_jar()).per_op("nothing moves", nothing_moves);
    let plan = Plan::new().op(Tip);
    let run = h.run(&plan);
    run.assert_found("nothing moves", "jar.near");
    run.assert_found("nothing moves", "bob.near");
    assert_eq!(run.findings().len(), 2, "{run}");
    assert_eq!(counts(&run, "nothing moves"), (0, 1, 0, 0), "{run}");
    let audit = h.scan(&[plan]).faults(Faults::none()).run();
    for at in ["jar.near", "bob.near"] {
        let row = audit.found_at("nothing moves", at).unwrap_or_else(|| panic!("{audit}"));
        assert_eq!(row.finding.key(), Some(at), "a row of its own: {audit}");
    }
}

/// `Fail::all`'s failures at one key and cause are one finding, the
/// first, seen once: per op, and for a world check, where a repeat would
/// read as a later, different failure.
///
/// Verification boundary: finding identity within one judgement, over
/// actual runs.
#[test]
fn failures_of_one_judgement_at_one_site_are_one_finding() {
    let twice = || Fail::all([Fail::new("first"), Fail::new("second")]);
    let tipped = |sim: &Sim| sim.balance("bob.near") > Balance::from_near(10);
    let run = Harness::new(tip_jar())
        .per_op("twice per op", move |_: &OpDone<Tip>| twice())
        .at_rest("twice at rest", move |sim: &Sim| if tipped(sim) { twice() } else { Ok(()) })
        .per_op("twice with a cause", |_: &OpDone<Tip>| {
            let at_x = |m: &str| Fail::new(m).key("x");
            Fail::all([at_x("first"), at_x("second")]).map_err(|f| f.cause("c"))
        })
        .run(&Plan::new().op(Tip));
    let checks = [("twice per op", "Tip"), ("twice at rest", "world"), ("twice with a cause", "x")];
    for (check, at) in checks {
        assert_eq!(counts(&run, check).1, 1, "one failed judgement: {run}");
        let f = run.assert_found(check, at);
        assert_eq!((f.message.as_str(), f.occurrences), ("first", 1), "{run}");
    }
    // A cause on the result is every failure's: still one finding.
    let f = run.assert_found("twice with a cause", "x");
    assert_eq!(f.cause.as_deref(), Some("c"), "{run}");
    assert_eq!(run.findings().len(), 3, "{run}");
}

/// `Fail::all` of nothing is a pass; failures beside an element it
/// couldn't judge are a failed judgement; with no failure, the first
/// element not judged decides, else the first idle: an element that
/// couldn't be read is never masked by one with nothing at stake.
#[test]
fn fail_all_passes_on_nothing_and_fails_past_what_it_couldnt_judge() {
    let run = Harness::new(tip_jar())
        .per_op("nothing", |_: &OpDone<Tip>| Fail::all([]))
        .per_op("mixed", |_: &OpDone<Tip>| {
            Fail::all([Fail::not_judged("unread"), Fail::new("moved").key("bob.near")])
        })
        .per_op("idle first", |_: &OpDone<Tip>| {
            Fail::all([Fail::idle("nothing owed"), Fail::not_judged("unread")])
        })
        .per_op("idle only", |_: &OpDone<Tip>| {
            Fail::all([Fail::idle("nothing owed"), Fail::idle("nothing either")])
        })
        .run(&Plan::new().op(Tip));
    assert_eq!(counts(&run, "nothing"), (1, 0, 0, 0), "{run}");
    assert_eq!(counts(&run, "mixed"), (0, 1, 0, 0), "{run}");
    run.assert_found("mixed", "bob.near");
    let j = |check: &str| run.judged().iter().find(|j| j.check == check).unwrap();
    assert_eq!(counts(&run, "idle first"), (0, 0, 1, 0), "not judged outranks idle: {run}");
    assert_eq!(j("idle first").why.as_deref(), Some("unread"), "{run}");
    assert_eq!(counts(&run, "idle only"), (0, 0, 0, 1), "{run}");
    assert_eq!(j("idle only").idle_why.as_deref(), Some("nothing owed"), "{run}");
}

// ---- gas and injections ----

#[test]
fn explored_gas_reaches_unpinned_calls_only() {
    let gas = recorder::<Vec<Gas>>();
    let log = gas.clone();
    let harness = Harness::new(fixture()).per_op("gas", move |d: &OpDone<Prog>| {
        log.borrow_mut().push(d.outcomes().iter().map(|o| o.receipts[0].attached_gas).collect());
        Ok::<_, Fail>(())
    });
    let run = harness.run(&Plan::new().op(Prog::TwoCalls).gas(Gas::from_teragas(40)));
    run.assert_clean();
    assert_eq!(gas.borrow()[0], [Gas::from_teragas(40), Gas::from_teragas(50)]);
    // Unset, the call keeps its own 300 Tgas.
    harness.run(&Plan::new().op(Prog::TwoCalls)).assert_clean();
    assert_eq!(gas.borrow()[1], [Gas::from_teragas(300), Gas::from_teragas(50)]);
}

/// `Tx::gas(None)` pins nothing, so the step's gas reaches the call;
/// `Tx::gas(Some(g))` pins it.
#[test]
fn an_optional_gas_pins_only_when_set() {
    let gas = recorder::<Gas>();
    let log = gas.clone();
    let harness = Harness::new(fixture()).per_op("gas", move |d: &OpDone<Prog>| {
        log.borrow_mut().push(d.entries()[0].attached_gas);
        Ok::<_, Fail>(())
    });
    let at_40 = |op| harness.run(&Plan::new().op(op).gas(Gas::from_teragas(40))).assert_clean();
    at_40(Prog::MaybePinned(None));
    at_40(Prog::MaybePinned(Some(50)));
    assert_eq!(*gas.borrow(), [Gas::from_teragas(40), Gas::from_teragas(50)]);
}

/// `per_op_for` judges only the ops it accepts: the others add nothing to
/// its counts, so a check of one kind of op never passes once per other.
#[test]
fn a_per_op_check_for_one_variant_counts_only_that_variant() {
    let run = Harness::new(fixture())
        .per_op_for(
            |op| matches!(op, Prog::TwoCalls),
            "two calls",
            |d: &OpDone<Prog>| match d.outcomes().len() {
                2 => Ok(()),
                n => Err(format!("{n} transactions")),
            },
        )
        .per_op("every op", |_: &OpDone<Prog>| Ok::<_, Fail>(()))
        .run(&Plan::new().op(Prog::Run(vec![])).op(Prog::TwoCalls).op(Prog::Quiet));
    run.assert_clean();
    let counts: Vec<(u32, u32)> = run.judged().iter().map(|j| (j.ok, j.failed)).collect();
    assert_eq!(counts, [(1, 0), (3, 0)], "{run}");
}

/// `tests/res/flight_lock.wat` at lock.near, and alice.
fn flight_lock() -> Sim {
    let wasm = wat::parse_file(common::manifest_dir().join("tests/res/flight_lock.wat")).unwrap();
    let mut sim = Sim::new();
    sim.create_account("alice.near", Balance::from_near(10));
    sim.deploy("lock.near", wasm);
    sim
}

/// alice starts the lock, whose `finish` runs a block after `start`.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct Start;

impl Op for Start {
    fn submit(&self, sim: &mut Sim) -> Result<(), Skip> {
        sim.call("alice.near", "lock.near", "start").submit();
        Ok(())
    }
}

/// `OpDone::rest`: an op submitted with nothing in flight is at rest, one
/// in the block of another isn't, and one after the chain settled is.
#[test]
fn an_op_submitted_with_nothing_in_flight_is_at_rest() {
    let seen = recorder::<Vec<bool>>();
    let log = seen.clone();
    let h =
        Harness::new(flight_lock()).at_rest_ops("rest", move |_: &Sim, ops: &[OpDone<Start>]| {
            log.borrow_mut().push(ops.iter().map(OpDone::rest).collect());
            Ok::<_, Fail>(())
        });
    h.run(&Plan::new().op(Start).blocks(0).op(Start).op(Start)).assert_clean();
    assert_eq!(seen.borrow().last(), Some(&vec![true, false, true]));
}

/// What the harness keeps of each op, so the world needn't: the world just
/// before it submitted, and whether it ran alone. Overlap breaks
/// isolation, a skipped op doesn't, and an op with no transaction does.
/// An `at_rest_ops` check gets every op that submitted, skipped ones left
/// out, with the world now.
#[test]
fn the_harness_keeps_each_ops_world_before_and_whether_it_ran_alone() {
    let pay = |yocto| Prog::Pay { who: "alice.near".into(), yocto, program: vec![] };
    let seen = recorder::<(u128, bool)>();
    let log = seen.clone();
    let rests = recorder::<usize>();
    let rest_log = rests.clone();
    let run = Harness::new(fixture())
        .per_op("before and alone", move |d: &OpDone<Prog>| {
            let yocto = match d.op() {
                Prog::Pay { yocto, .. } => *yocto,
                _ => 0,
            };
            log.borrow_mut().push((yocto, d.isolated()));
            let (before, now) = (d.before(), d.world());
            // The world as submit found it: none of the op's transactions yet.
            let ours = d.outcomes().iter().map(|o| o.hash);
            if ours.clone().any(|h| before.txs().contains(&h)) {
                return Err("before() has the op's own transaction".to_owned());
            }
            if !ours.clone().all(|h| now.txs().contains(&h)) {
                return Err("world() lacks the op's transaction".to_owned());
            }
            Ok(())
        })
        .at_rest_ops("ops so far", move |sim: &Sim, ops: &[OpDone<Prog>]| {
            rest_log.borrow_mut().push(ops.len());
            match ops.iter().all(|d| d.world().height() == sim.height()) {
                true => Ok(()),
                false => Err("an op's world isn't the world now"),
            }
        })
        .run(
            &Plan::new()
                .op(pay(1))
                .op(pay(2))
                .blocks(0)
                .op(pay(3))
                .op(pay(4))
                .blocks(0)
                .op(Prog::Nothing)
                .op(pay(5))
                .blocks(0)
                .op(Prog::Quiet),
        );
    // The check read the shared world of ops not alone: those judgements
    // are not judged, and the run incomplete; no finding.
    assert!(run.findings().is_empty(), "{run}");
    assert_eq!(run.verdict(), Verdict::Incomplete, "{run}");
    let why = run.reason().unwrap();
    assert!(why.starts_with("before and alone not judged 4 time(s): overlapped"), "{why}");
    // Judged in completion order; Quiet at once, while pay(5) is queued.
    let want = [(1, true), (2, false), (3, false), (4, true), (0, false), (5, false)];
    assert_eq!(*seen.borrow(), want, "{run}");
    let rests = rests.borrow();
    assert_eq!((rests.first(), rests.last()), (Some(&0), Some(&6)), "{rests:?}");
    assert!(rests.contains(&4), "the skipped op is left out: {rests:?}");
}

/// A built-in finding is of its block: a transaction still running then
/// reads "pending at this block", not a bare PENDING. The account is one
/// the plan names and the fixture lacks, so its site reads `<new
/// account>`, and the message names it.
#[test]
fn a_finding_on_a_running_transaction_says_pending_at_this_block() {
    let run = Harness::<Prog>::new(fixture())
        .run(&Plan::new().op(Prog::Run(vec![detached_call("missing.near")])));
    let f = run.assert_found(builtin::UNHANDLED_FAILURE, "<new account>::run");
    assert!(f.message.contains("missing.near"), "{run}");
    assert!(f.message.contains("transaction status: pending at this block;"), "{run}");
    assert!(!f.message.contains("PENDING"), "{run}");
}

/// Every fault on any receipt of the op's transactions, as the per-op
/// check sees them.
fn faults_seen() -> (Harness<Prog>, Rc<RefCell<Vec<Fault>>>) {
    let faults = recorder::<Fault>();
    let log = faults.clone();
    let harness = Harness::new(fixture()).per_op("faults", move |d: &OpDone<Prog>| {
        let rs = d.outcomes().iter().flat_map(|o| &o.receipts);
        log.borrow_mut().extend(rs.filter_map(|r| r.fault.clone()));
        Ok::<_, Fail>(())
    });
    (harness, faults)
}

/// The default faults fail a callback's call to another
/// account, never a contract's call to itself.
#[test]
fn tolerable_faults_never_fail_a_contracts_call_to_itself() {
    let (harness, faults) = faults_seen();
    let self_call = Prog::Run(call_then("contract.near", &[]));
    let cross = Prog::Run(call_then("other.near", &[]));
    let fail = |op: &Prog, at: Receipts| {
        faults.borrow_mut().clear();
        let run = harness.run(&Plan::new().op(op.clone()).inject(at, Fault::PromiseFailed));
        (run, faults.borrow().clone())
    };
    let (run, seen) = fail(&self_call, Receipts::cross_contract_callbacks().nth(0));
    assert!(seen.is_empty(), "{seen:?}");
    run.assert_clean(); // a sample that fits nothing is the plain case
    let (_, seen) = fail(&self_call, Receipts::callbacks().nth(0));
    assert!(seen.contains(&Fault::PromiseFailed), "explicitly, it can: {seen:?}");
    let (_, seen) = fail(&cross, Receipts::cross_contract_callbacks().nth(0));
    assert!(seen.contains(&Fault::PromiseFailed), "{seen:?}");

    // Sampled: a fault in every case, never a failed self-call.
    let strategy = plans(Just(self_call)).len(1..=2).faults(Faults::tolerable().per_case(1.0));
    let mut runner = TestRunner::deterministic();
    faults.borrow_mut().clear();
    for _ in 0..24 {
        let plan = strategy.new_tree(&mut runner).unwrap().current();
        let run = harness.run(&plan);
        assert!(run.findings().is_empty(), "{run}");
    }
    let seen = faults.borrow();
    assert!(!seen.iter().any(|f| matches!(f, Fault::PromiseFailed | Fault::OutOfGas)), "{seen:?}");
}

#[test]
fn an_injection_the_plan_names_must_apply() {
    let plan = |at: Receipts| Plan::new().op(Prog::Run(vec![])).inject(at, Fault::Hold(1));
    let run = Harness::<Prog>::new(fixture()).run(&plan(Receipts::receiver("nobody.near")));
    assert_eq!(run.verdict(), Verdict::Incomplete, "{run}");
    assert!(run.reason().unwrap().contains("applied to no receipt"), "{run}");
    let sampled = plan(Receipts::receiver("nobody.near").nth(0));
    Harness::<Prog>::new(fixture()).run(&sampled).assert_clean();
}

/// Two injections with one fault: the broad one wins the step's only
/// receipt, and the other, aimed at nobody.near, applies to none. It must
/// not count as landed off the first one's receipt: the
/// case is incomplete, naming it.
///
/// Verification boundary: which injection the chain applied to which
/// receipt, judged per injection on an actual run.
#[test]
fn a_fault_counts_for_the_injection_that_won_it_only() {
    let plan = Plan::new()
        .op(Prog::Run(vec![detached_call("other.near")]))
        .inject(Receipts::all(), Fault::Hold(2))
        .inject(Receipts::receiver("nobody.near"), Fault::Hold(2));
    let run = Harness::<Prog>::new(fixture()).run(&plan);
    assert_eq!(run.verdict(), Verdict::Incomplete, "{run}");
    let why = run.reason().unwrap();
    assert!(why.contains("nobody.near") && why.contains("applied to no receipt"), "{why}");
}

/// `PromiseFailed` aimed at a receipt with no promise result to fail
/// (here a call to other.near, which waits on nothing) can't apply: the
/// step runs without it and the case is incomplete, saying why, as main's
/// `fault_scan` skipped it with its reason. Not an error, not silent.
///
/// Verification boundary: what the executor makes of an injection the
/// chain passes over, on an actual run.
#[test]
fn a_promise_failure_with_nothing_to_fail_says_why_it_was_passed_over() {
    let call = Fz::Call {
        to: "other.near".into(),
        args: encode(&[]),
        deposit: 0,
        gas: 10 * TGAS,
        weight: 0,
    };
    let plan = Plan::new()
        .op(Prog::Run(vec![call]))
        .inject(Receipts::receiver("other.near"), Fault::PromiseFailed);
    let run = Harness::<Prog>::new(fixture()).run(&plan);
    assert_eq!(run.verdict(), Verdict::Incomplete, "{run}");
    let why = run.reason().unwrap();
    assert!(why.contains("applied to no receipt: at other.near"), "{why}");
    assert!(why.contains("it waits on no promise"), "{why}");
}

// ---- sampled faults, placed as the case runs ----

/// A sampled fault goes to a receipt it fits, on the first step from its
/// own (then from the first) whose transaction spawns one: here step 1
/// spawns nothing, so step 0 takes it, at receipt 3 mod 2 = 1 (the call to
/// missing-b.near, by receiver, method and occurrence). The run's
/// plan names that receipt exactly and reruns the same way.
#[test]
fn a_sampled_fault_is_placed_on_a_receipt_it_fits() {
    let harness = Harness::new(fixture());
    let calls = Prog::Run(vec![detached_call("missing-a.near"), detached_call("missing-b.near")]);
    let plan = Plan::new()
        .op(calls)
        .op(Prog::Run(vec![write(b"k")]))
        .inject(Receipts::all().nth(3), Fault::Hold(1));
    let run = harness.run(&plan);
    let inject = |p: &Plan<Prog>, i: usize| -> Vec<String> {
        p.steps[i].inject.iter().map(ToString::to_string).collect()
    };
    assert_eq!(
        inject(run.plan(), 0),
        ["Hold(1) at Receipts::occurrence(\"missing-b.near\", \"run\", 0)"],
        "{run}"
    );
    assert!(inject(run.plan(), 1).is_empty(), "{run}");
    let held: Vec<String> = (run.outcomes().iter().flat_map(|o| &o.receipts))
        .filter(|r| r.held.is_some())
        .map(|r| r.receiver_id.to_string())
        .collect();
    assert_eq!(held, ["missing-b.near"], "{run}");
    let again = harness.run(run.plan());
    assert_eq!(format!("{:?}", again.plan()), format!("{:?}", run.plan()));
    assert_eq!(format!("{again}"), format!("{run}"));
}

/// A sampled fault that fits no receipt of the plan (an op that spawns
/// nothing) is dropped: the case runs plainly, is clean, not incomplete,
/// and a search counts it as having had no target.
#[test]
fn a_sampled_fault_nothing_fits_runs_plainly() {
    let harness = Harness::new(fixture());
    let quiet = Prog::Run(vec![write(b"k")]);
    let plan = Plan::new().op(quiet).inject(Receipts::all().nth(0), Fault::Hold(1));
    let run = harness.run(&plan);
    assert_eq!(run.verdict(), Verdict::Clean, "{run}");
    assert!(run.plan().steps[0].inject.is_empty(), "{run}");
    let shown = format!("{run}");
    assert!(
        shown.contains("no receipt fits step 0's Hold(1) at Receipts::all().nth(0)"),
        "{shown}"
    );
}

#[test]
fn a_plan_runs_the_same_way_twice() {
    let harness = Harness::new(fixture()).at_rest("no pending", absent(b"pending"));
    let plan = Plan::new()
        .op(Prog::Run(temporary_state()))
        .blocks(1)
        .op(Prog::Run(vec![detached_call("missing-a.near")]))
        .inject(Receipts::all(), Fault::Hold(2));
    let (a, b) = (harness.run(&plan), harness.run(&plan));
    assert_eq!(format!("{a:?}"), format!("{b:?}"));
    let saved = serde_json::to_string(&plan).unwrap();
    let again: Plan<Prog> = serde_json::from_str(&saved).unwrap();
    assert_eq!(format!("{:?}", harness.run(&again)), format!("{a:?}"));
}

// ---- the token vault (tests/explore_vault), with an op of two transactions ----

use explore_vault::{TOKEN, User, VAULT, VaultOp, vault_world, view};

/// per-op, NEAR: a withdraw costs at most its 1 yocto.
fn withdraw_costs_one_yocto(d: &OpDone<VaultOp>) -> Result<(), Fail> {
    let VaultOp::Withdraw { who, .. } = *d.op() else { return Ok(()) };
    let net = d.effects().near_net(who.id());
    if net >= -1 { Ok(()) } else { Err(format!("{} paid {} yocto", who.id(), -net).into()) }
}

/// The vault's checks, and one judged on every op.
fn vault_on(wasm: &str) -> Harness<VaultOp> {
    explore_vault::vault_on(wasm).per_op("a withdraw costs one yocto", withdraw_costs_one_yocto)
}

/// How many times the check `name` passed, and was not judged, in `run`.
fn judged<O: Op>(run: &near_contract_sim::explore::Run<O>, name: &str) -> (u32, u32) {
    let j = run.judged().iter().find(|j| j.check == name).unwrap();
    (j.ok, j.unjudged)
}

/// An op of two transactions: its effects merge both, the shares' supply
/// view included, under overlap with another op.
#[test]
fn vault_an_op_of_two_transactions_merges_their_token_moves() {
    let plan = Plan::new()
        .op(VaultOp::TwoDeposits { who: User::Alice, each: 30 })
        .blocks(0)
        .op(VaultOp::Deposit { who: User::Bob, amount: Amount::Exact(5) });
    vault_on("vault.wasm").run(&plan).assert_clean();
    let sixty = Harness::new(vault_world("vault.wasm")).per_op("sixty", |d: &OpDone<VaultOp>| {
        let VaultOp::TwoDeposits { .. } = d.op() else { return Ok(()) };
        let e = d.effects();
        let got =
            (e.token_net(TOKEN, "alice.near")?, e.token_supply_net(VAULT)?, d.outcomes().len());
        if got == (Delta::loss(60), Delta::gain(60), 2) {
            Ok(())
        } else {
            Err(Fail::new(format!("{got:?}")))
        }
    });
    let run = sixty.run(&plan);
    run.assert_clean();
    assert_eq!(judged(&run, "sixty"), (2, 0), "{run}");
}

/// Two deposits in flight together, neither isolated: a per-op check
/// reading only each one's own deltas (`effects`, its own receipts' token
/// moves) is judged for both. One reading the shared world (`world` less
/// `before`, which holds the other's deposit too) is not judged, whatever
/// it returns: the harness sees what it read, so a check that never asks
/// `isolated()` can't pass on a polluted reading. Were its Ok taken,
/// never-judged cases would count as ok.
#[test]
fn overlapping_ops_are_each_judged_on_their_own_deltas() {
    let plan = Plan::new()
        .op(VaultOp::Deposit { who: User::Alice, amount: Amount::Exact(30) })
        .blocks(0)
        .op(VaultOp::Deposit { who: User::Bob, amount: Amount::Exact(5) });
    let seen = Rc::new(RefCell::new(vec![]));
    let log = seen.clone();
    let total = |sim: &Sim| view(sim, VAULT, "total_shares", json!({}));
    let h = Harness::new(vault_world("vault.wasm"))
        .per_op("own deltas", move |d: &OpDone<VaultOp>| {
            let VaultOp::Deposit { amount: Amount::Exact(n), .. } = *d.op() else { return Ok(()) };
            let minted = d.effects().token_supply_net(VAULT)?;
            match minted == Delta::gain(n) {
                true => Ok(()),
                false => Err(Fail::new(format!("minted {minted:?} for {n}"))),
            }
        })
        .per_op("shared world", move |d: &OpDone<VaultOp>| {
            log.borrow_mut().push((d.isolated(), total(d.world()) - total(d.before())));
            Ok::<_, Fail>(())
        });
    let run = h.run(&plan);
    let count = |name: &str| {
        let j = run.judged().iter().find(|j| j.check == name).unwrap();
        (j.ok, j.unjudged)
    };
    assert_eq!(count("own deltas"), (2, 0), "{run}");
    assert_eq!(count("shared world"), (0, 2), "{run}");
    // The reason names the read, and what to read instead.
    let why = run.reason().unwrap_or_default();
    assert!(why.contains("overlapped: it read before() and world()"), "{run}");
    let instead = "read effects(), the op's own, or judge them together in at_rest_ops \
                   (OpDone::rest)";
    assert!(why.contains(instead), "{run}");
    // A campaign's summary prints it whole, the pointer too.
    let audit = h.scan(std::slice::from_ref(&plan)).faults(Faults::none()).run();
    let text = audit.to_string();
    let line = text.lines().find(|l| l.starts_with("not judged: shared world"));
    let line = line.unwrap_or_else(|| panic!("{text}"));
    assert!(line.contains(instead) && !line.contains(" ... "), "{line}");
    let seen = seen.borrow();
    assert!(seen.iter().all(|(isolated, _)| !isolated), "{seen:?}");
    assert!(seen.iter().any(|(_, shared)| *shared == 35), "the world holds both: {seen:?}");
}

/// The overlap rule sees a read through a clone too, and leaves an idle
/// verdict idle: a clone of `OpDone` shares what
/// the check read, and `Fail::idle` (nothing to judge) stays idle
/// whatever the check read.
#[test]
fn the_overlap_rule_sees_clones_and_keeps_idle() {
    let plan = Plan::new()
        .op(VaultOp::Deposit { who: User::Alice, amount: Amount::Exact(30) })
        .blocks(0)
        .op(VaultOp::Deposit { who: User::Bob, amount: Amount::Exact(5) });
    let total = |sim: &Sim| view(sim, VAULT, "total_shares", json!({}));
    let h = Harness::new(vault_world("vault.wasm"))
        .per_op("cloned", move |d: &OpDone<VaultOp>| {
            let _ = total(d.clone().world());
            Ok::<_, Fail>(())
        })
        .per_op("idle", move |d: &OpDone<VaultOp>| {
            let _ = total(d.world());
            Err::<(), _>(Fail::idle("nothing owed"))
        });
    let run = h.run(&plan);
    let count = |name: &str| {
        let j = run.judged().iter().find(|j| j.check == name).unwrap();
        (j.ok, j.unjudged, j.idle)
    };
    assert_eq!(count("cloned"), (0, 2, 0), "{run}");
    assert_eq!(count("idle"), (0, 0, 2), "{run}");
}

/// Idle and not judged keep their own reasons: idle first,
/// then not judged, the run's gap names why it couldn't judge, not why
/// nothing was at stake.
#[test]
fn idle_and_not_judged_keep_their_own_reasons() {
    let plan = Plan::new()
        .op(VaultOp::Deposit { who: User::Alice, amount: Amount::Exact(30) })
        .op(VaultOp::Deposit { who: User::Bob, amount: Amount::Exact(5) });
    let h = Harness::new(vault_world("vault.wasm")).per_op("split", |d: &OpDone<VaultOp>| match d
        .op()
    {
        VaultOp::Deposit { who: User::Alice, .. } => Err::<(), _>(Fail::idle("nothing owed yet")),
        _ => Err(Fail::not_judged("can't read the ledger")),
    });
    let run = h.run(&plan);
    let j = run.judged().iter().find(|j| j.check == "split").unwrap();
    assert_eq!((j.idle, j.unjudged), (1, 1), "{run}");
    assert_eq!(j.why.as_deref(), Some("can't read the ledger"), "{j:?}");
    assert_eq!(j.idle_why.as_deref(), Some("nothing owed yet"), "{j:?}");
    let reason = run.reason().unwrap_or_default();
    assert!(reason.contains("split not judged 1 time(s): can't read the ledger"), "{reason}");
}

#[test]
fn vault_overlapping_withdraws_with_amounts_read_at_submit() {
    let plan = Plan::new()
        .op(VaultOp::Deposit { who: User::Alice, amount: Amount::Exact(100) })
        .op(VaultOp::Withdraw { who: User::Alice, amount: Amount::Exact(40) })
        .blocks(0)
        .op(VaultOp::Withdraw { who: User::Alice, amount: Amount::All })
        .op(VaultOp::Withdraw { who: User::Bob, amount: Amount::AllMinus(1) });
    let run = vault_on("vault.wasm").run(&plan);
    assert!(run.findings().is_empty(), "{run}");
    assert!(format!("{run}").contains("[skipped: AllMinus(1) of a balance of 0]"), "{run}");
    let ran = judged(&run, "a withdraw costs one yocto").0;
    assert_eq!(ran, 3, "the deposit and two withdraws ran, one skipped: {run}");
}

/// A claim's notice fails by design (best-effort): a finding all the
/// same, which the write-up explains; nothing declares it away.
#[test]
fn vault_a_claims_failed_notice_is_a_finding() {
    let run = vault_on("vault.wasm").run(&Plan::new().op(VaultOp::Claim { who: User::Alice }));
    assert_eq!(run.verdict(), Verdict::Findings, "{run}");
    run.assert_found(builtin::UNHANDLED_FAILURE, "rewards.near::on_claim");
}

/// `trees()` draws every op's receipt trees, step by step, even when the
/// case is clean (its `Display` draws only findings' transactions).
#[test]
fn trees_draw_every_op_even_when_clean() {
    let plan = Plan::new().op(Prog::Run(vec![write(b"k")])).op(Prog::Nothing).op(Prog::Quiet);
    let run = Harness::new(fixture()).run(&plan);
    assert!(run.findings().is_empty(), "{run}");
    assert_eq!(run.verdict(), Verdict::Incomplete, "a skipped op: {run}");
    assert!(!run.to_string().contains("contract.near::run"), "{run}");
    let trees = run.trees();
    assert!(trees.starts_with("step 0: Run("), "{trees}");
    assert!(trees.contains("signed by alice.near"), "{trees}");
    assert!(trees.contains("contract.near::run"), "{trees}");
    assert!(trees.contains("step 1: Nothing [skipped: nothing to do]"), "{trees}");
    assert!(trees.contains("step 2: Quiet [no transaction]"), "{trees}");
}

// ---- causes ----

/// "a" is written by design, "b" is not: one check, two causes.
fn keys_by_cause(sim: &Sim) -> Result<(), Fail> {
    match (absent(b"b")(sim), absent(b"a")(sim)) {
        (Err(e), _) => Err(Fail::new(e).cause("b")),
        (_, Err(e)) => Err(Fail::new(e).cause("a, by design")),
        _ => Ok(()),
    }
}

/// One check's findings at one site stay apart by the cause it names:
/// two causes are two findings of an audit.
#[test]
fn a_checks_causes_are_separate_findings() {
    let a = Plan::new().op(Prog::Run(vec![write(b"a")]));
    let b = Plan::new().op(Prog::Run(vec![write(b"b")]));
    let harness = Harness::new(fixture()).at_rest("no keys", keys_by_cause);
    let audit = harness.scan(&[a, b]).faults(Faults::none()).run();
    let mut causes: Vec<_> = audit.found().iter().map(|f| f.finding.cause.clone()).collect();
    causes.sort();
    assert_eq!(causes, [Some("a, by design".to_owned()), Some("b".to_owned())], "{audit}");
}

/// A run keeps the world as its case left it: `world::<W>()` is the
/// harness's own world type.
///
/// Verification boundary: what a `Run` exposes of its end state, against
/// an actual run; nothing about the case changes.
#[test]
fn a_run_keeps_its_end_state() {
    let run = Harness::new(fixture()).run(&Plan::new().op(Prog::Run(vec![write(b"k")])));
    let sim = run.world::<Sim>().unwrap_or_else(|| panic!("a Sim: {run}"));
    assert_eq!(sim.state("contract.near", b"k").as_deref(), Some(&b"1"[..]), "{run}");
    assert!(run.world::<String>().is_none());
}

/// An op reads what earlier ops' transactions did from the `Sim` it
/// submits on: `sim.txs()` lists every transaction so far, setup
/// included, and `sim.outcome(hash)` gives each one's receipts.
///
/// Verification boundary: documentation of an existing API, exercised in
/// an actual run; no change to submission.
#[test]
fn an_op_reads_earlier_results_from_the_sim() {
    #[derive(Clone, Debug, Serialize, Deserialize)]
    enum Echo {
        Log(String),
        /// Logs what the last `run` call logged.
        Repeat,
    }
    impl Op for Echo {
        fn submit(&self, sim: &mut Sim) -> Result<(), Skip> {
            let said =
                match self {
                    Echo::Log(s) => s.clone(),
                    Echo::Repeat => {
                        let last =
                            sim.txs().iter().rev().map(|h| sim.outcome(h)).find_map(|o| {
                                o.receipt("run").and_then(|r| r.logs.first().cloned())
                            });
                        last.ok_or("nothing logged yet")?
                    }
                };
            sim.call("alice.near", "contract.near", "run").args(encode(&[Fz::Log(said)])).submit();
            Ok(())
        }
    }
    let plan = Plan::new().op(Echo::Log("hello".into())).op(Echo::Repeat);
    let run = Harness::new(fixture()).run(&plan);
    run.assert_clean();
    let logged: Vec<&str> = run.outcomes().iter().flat_map(|o| o.logs()).collect();
    assert_eq!(logged, ["hello", "hello"], "{}", run.trees());
}

/// What an op resolved to. A per-op check reads each of its
/// transactions' first receipts through `OpDone::entries`: receiver,
/// method, arguments and deposit, as the op's `submit` picked them.
#[test]
fn a_per_op_check_reads_what_the_op_resolved_to() {
    let seen = recorder::<(String, String, u128, usize)>();
    let record = seen.clone();
    let harness = Harness::new(fixture()).per_op("resolved", move |d: &OpDone<Prog>| {
        for r in d.entries() {
            let method = r.method().unwrap_or_default().to_owned();
            let args = r.args.first().map_or(0, Vec::len);
            record.borrow_mut().push((
                r.receiver_id.to_string(),
                method,
                r.deposit.as_yoctonear(),
                args,
            ));
        }
        Ok::<_, Fail>(())
    });
    let program = vec![Fz::Log("hi".into())];
    let pay = Prog::Pay { who: "bob.near".into(), yocto: 7, program: program.clone() };
    harness.run(&Plan::new().op(pay).op(Prog::TwoCalls)).assert_clean();
    let seen = seen.borrow();
    let expected_args = encode(&program).len();
    assert_eq!(seen[0], ("contract.near".into(), "run".into(), 7, expected_args), "{seen:?}");
    assert_eq!(seen.len(), 3, "one receipt per transaction: {seen:?}");
}

/// `OpDone::calls` lists every receipt the op's transactions
/// ran, a bystander the contract called included, in execution order;
/// `entries` only the first; `failed_on_value_path` the first failed
/// receipt that moves value.
///
/// Verification boundary: the per-op check API, over actual receipts.
#[test]
fn calls_lists_every_receipt_a_bystander_included() {
    let seen = recorder::<(Vec<String>, Vec<String>, Option<String>)>();
    let record = seen.clone();
    let harness = Harness::new(fixture()).per_op("reads calls", move |d: &OpDone<Prog>| {
        let who = |rs: Vec<&near_contract_sim::ReceiptOutcome>| -> Vec<String> {
            rs.iter().map(|r| r.receiver_id.to_string()).collect()
        };
        let failed = d.failed_on_value_path().map(|r| r.receiver_id.to_string());
        record.borrow_mut().push((who(d.calls()), who(d.entries()), failed));
        Ok::<_, Fail>(())
    });
    let pay = Fz::Call {
        to: "missing-b.near".into(),
        args: encode(&[]),
        deposit: 1,
        gas: 10 * TGAS,
        weight: 0,
    };
    let call_other = Fz::Call {
        to: "other.near".into(),
        args: encode(&[]),
        deposit: 0,
        gas: 10 * TGAS,
        weight: 0,
    };
    let run = harness.run(&Plan::new().op(Prog::Run(vec![call_other, pay])));
    let seen = seen.borrow();
    let (calls, entries, failed) = &seen[0];
    assert_eq!(entries, &["contract.near"], "{run}");
    assert_eq!(calls[0], "contract.near", "{calls:?}");
    assert!(calls.iter().any(|c| c == "other.near"), "the bystander: {calls:?}\n{run}");
    assert!(calls.iter().any(|c| c == "missing-b.near"), "{calls:?}");
    assert_eq!(failed.as_deref(), Some("missing-b.near"), "{run}");
}

/// A plan step can be a direct call whatever the plan's ops are
/// (`Plan::call`): its signer is created if the fixture lacks it, and it
/// runs and is judged like an op, named by its method.
#[test]
fn a_call_step_runs_from_a_signer_of_its_own() {
    use near_contract_sim::explore::Call;
    let seen = recorder::<String>();
    let log = seen.clone();
    let call = Call::new("fresh.near", "contract.near", "run").args(encode(&[write(b"c")]));
    let run = Harness::new(fixture())
        .at_rest("no c", absent(b"c"))
        .per_op("named", move |d: &OpDone<Prog>| {
            log.borrow_mut().push(d.name().to_owned());
            Ok::<_, Fail>(())
        })
        .run(&Plan::new().op(Prog::Run(vec![])).call(call));
    run.assert_found("no c", "world");
    let sim = run.world::<Sim>().unwrap();
    assert!(sim.account("fresh.near").is_some(), "{run}");
    assert_eq!(*seen.borrow(), ["Run"], "a call is no op of the harness: {run}");
    let saved = serde_json::to_string(run.plan()).unwrap();
    assert!(saved.contains(r#""call":{"signer":"fresh.near""#), "{saved}");
    let back: Plan<Prog> = serde_json::from_str(&saved).unwrap();
    assert_eq!(
        format!(
            "{:?}",
            Harness::new(fixture()).at_rest("no c", absent(b"c")).run(&back).findings()
        ),
        format!("{:?}", run.findings())
    );
}

/// An op that queues nothing but changes the world (here it takes
/// alice's NEAR outright) leaves the world at rest: the `at_rest` checks
/// judge it there, even with no block after it, where the check would
/// otherwise pass without judging. One in flight with others is no rest
/// point.
///
/// Verification boundary: when the executor judges `at_rest` checks,
/// over actual runs.
#[test]
fn an_op_that_queues_nothing_is_judged_at_rest() {
    #[derive(Clone, Debug, Serialize, Deserialize)]
    enum Take {
        All,
        Pay,
    }
    impl Op for Take {
        fn submit(&self, sim: &mut Sim) -> Result<(), Skip> {
            match self {
                Take::All => sim.set_balance("alice.near", Balance::ZERO),
                Take::Pay => {
                    sim.tx("bob.near", "alice.near").transfer(Balance::from_near(1)).submit();
                }
            }
            Ok(())
        }
    }
    let kept = |sim: &Sim| match sim.balance("alice.near").is_zero() {
        true => Err("alice lost her NEAR".to_owned()),
        false => Ok(()),
    };
    let harness = Harness::<Take>::new(fixture()).at_rest("alice keeps her NEAR", kept);
    let run = harness.run(&Plan::new().op(Take::All));
    assert_eq!(run.verdict(), Verdict::Findings, "{run}");
    run.assert_found("alice keeps her NEAR", "world");
    // In flight with a payment to her: no rest point until it lands.
    let overlapped = Plan::new().op(Take::Pay).blocks(0).op(Take::All);
    harness.run(&overlapped).assert_clean();
    // An `always` check judges it too.
    let always = Harness::<Take>::new(fixture()).always("alice keeps her NEAR", kept);
    always.run(&Plan::new().op(Take::All)).assert_found("alice keeps her NEAR", "world");
}

/// Writes contract.near's `x` (or, `Other`, `y`) directly, no transaction, or alice calls
/// it with an empty program.
#[derive(Clone, Debug, Serialize, Deserialize)]
enum Edit {
    Mark,
    Call,
    Other,
}

impl Op for Edit {
    fn submit(&self, sim: &mut Sim) -> Result<(), Skip> {
        match self {
            Edit::Mark => sim.set_state("contract.near", b"x", Some(b"1")),
            Edit::Other => sim.set_state("contract.near", b"y", Some(b"1")),
            Edit::Call => drop(run(sim, "alice.near", &[]).submit()),
        }
        Ok(())
    }
}

/// An op with no transaction takes effect at the checkpoint that first
/// judges the world after it, and breaks a world check failing there:
/// alone, at the fixture's height, though the fixture passed at that
/// height before it. Submitted while a call is in flight, its
/// checkpoint is the call's inclusion block, so a finding names both;
/// submitted before the call, it broke the check at its own checkpoint.
/// A harmless direct op before it, at the same height, is not named: the
/// check passed after it.
/// It was never a breaker: alone, a finding named no op, and with the
/// call, the call alone.
#[test]
fn an_op_with_no_transaction_breaks_at_its_checkpoint() {
    let h = Harness::new(fixture()).at_rest("x is never written", absent(b"x"));
    let ops = |plan: Plan<Edit>| -> Vec<String> {
        let audit = h.scan(&[plan]).faults(Faults::none()).run();
        let f = audit.assert_found("x is never written", "world");
        let path = f.replay.as_ref().unwrap_or_else(|| panic!("no replay: {audit}"));
        let saved: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        serde_json::from_value(saved["id"]["ops"].clone()).unwrap()
    };
    assert_eq!(ops(Plan::new().op(Edit::Mark)), ["Mark"]);
    assert_eq!(ops(Plan::new().op(Edit::Call).blocks(0).op(Edit::Mark)), ["Call", "Mark"]);
    assert_eq!(ops(Plan::new().op(Edit::Mark).blocks(0).op(Edit::Call)), ["Mark"]);
    assert_eq!(ops(Plan::new().op(Edit::Other).op(Edit::Mark)), ["Mark"]);
}

/// A per-op finding on an op with no transaction names that op:
/// its site is the op. Judged as a world finding, it named the ops that
/// took effect last since the check last passed: a call whose per-op
/// check passed later, or one completing at its checkpoint too.
#[test]
fn a_per_op_finding_on_an_op_with_no_transaction_names_it() {
    const CHECK: &str = "Mark writes nothing";
    let marks = |d: &OpDone<Edit>| match d.op() {
        Edit::Mark => Err(Fail::new("Mark wrote x")),
        _ => Ok(()),
    };
    let h = Harness::new(fixture()).per_op(CHECK, marks);
    for plan in [
        Plan::new().op(Edit::Mark),
        Plan::new().op(Edit::Call).blocks(0).op(Edit::Mark),
        Plan::new().op(Edit::Mark).blocks(0).op(Edit::Call),
        Plan::new().op(Edit::Other).op(Edit::Mark),
    ] {
        let audit = h.scan(&[plan]).faults(Faults::none()).run();
        let f = audit.assert_found(CHECK, "Mark");
        let path = f.replay.as_ref().unwrap_or_else(|| panic!("no replay: {audit}"));
        let saved: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        assert_eq!(saved["id"]["ops"], json!(["Mark"]), "{audit}");
    }
}
