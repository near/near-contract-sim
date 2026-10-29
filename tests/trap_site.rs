//! A trap named by the function it is in (`tests/res/abort_ledger.wat`,
//! laid out as a Rust contract built with panic = "abort"): a failed check
//! aborts through the standard library with `unreachable`, and a run or a
//! scan's finding names the contract's function that called it, not the
//! abort, and not a function that had already returned.
//!
//! Verification boundary: the message of a failure in a run and a scan's
//! finding, from a rerun on an instrumented copy of the contract, over
//! actual runs on the pinned runtime. The original run's outcomes and gas
//! are what the verdict comes from.

mod common;
use common::manifest_dir;
use near_contract_sim::explore::{Harness, Op, Plan, Skip};
use near_contract_sim::{Balance, Fixture, Sim};
use serde::{Deserialize, Serialize};

const LEDGER: &str = "ledger.near";
const NAMED: &str =
    "An `unreachable` opcode was executed in `ledger::debit`, called from `withdraw`.";

fn world() -> Sim {
    let wat = std::fs::read_to_string(manifest_dir().join("tests/res/abort_ledger.wat")).unwrap();
    Fixture::new()
        .account("alice.near", Balance::from_near(10))
        .contract(LEDGER, wat::parse_str(&wat).unwrap())
        .build()
}

/// alice withdraws an amount, directly or through a call nothing waits on.
#[derive(Clone, Debug, Serialize, Deserialize)]
enum LedgerOp {
    Withdraw(u64),
    Relay(u64),
}

impl Op for LedgerOp {
    fn submit(&self, sim: &mut Sim) -> Result<(), Skip> {
        let (method, amount) = match *self {
            LedgerOp::Withdraw(a) => ("withdraw", a),
            LedgerOp::Relay(a) => ("relay", a),
        };
        sim.call("alice.near", LEDGER, method).args(amount.to_le_bytes()).submit();
        Ok(())
    }
}

#[test]
fn a_run_names_the_function_a_trap_is_in() {
    let harness = Harness::<LedgerOp>::new(world());
    let run = harness.run(&Plan::new().op(LedgerOp::Withdraw(500)));
    let out = &run.outcomes()[0];
    let failure = out.failure().expect("a withdraw of 500 fails").to_string();
    assert!(failure.ends_with(NAMED), "{failure}");
    assert!(out.receipts[0].failure().unwrap().to_string().ends_with(NAMED), "{run}");
    // The run's own outcome: no log of the copy's.
    assert!(out.receipts.iter().all(|r| r.logs.is_empty()), "{run}");

    // The fixture is the original: a plain withdraw still reads unnamed.
    let mut sim = world();
    let plain = sim.call("alice.near", LEDGER, "withdraw").args(500u64.to_le_bytes()).transact();
    let failure = plain.failure().unwrap().to_string();
    assert!(failure.ends_with("An `unreachable` opcode was executed."), "{failure}");
}

#[test]
fn a_scan_finding_names_the_function_a_trap_is_in() {
    let harness = Harness::<LedgerOp>::new(world());
    let audit = harness.scan(&[Plan::new().op(LedgerOp::Relay(5))]).run();
    let found = audit.found().iter().find(|f| f.finding.message.contains("unreachable"));
    let found = found.unwrap_or_else(|| panic!("{audit}"));
    assert!(found.finding.message.contains(NAMED), "{}", found.finding.message);
}

/// A replay of a trap's finding shows again on the build that saved it,
/// saved by a run or a scan: its identity is taken with
/// the trap unnamed on every path, so naming it never reads as fixed.
///
/// Verification boundary: replay identity over actual runs of a trapping
/// contract on the pinned runtime.
#[test]
fn a_trap_finding_s_replay_shows_again() {
    let dir = std::env::temp_dir().join(format!("trap-replay-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let harness = Harness::<LedgerOp>::new(world()).store(&dir);
    let audit = harness.scan(&[Plan::new().op(LedgerOp::Relay(5))]).run();
    for x in audit.found() {
        assert!(!harness.run(x.run.plan()).findings().is_empty(), "{}", x.run);
    }
    let all = harness.clone().store(&dir).regressions();
    assert!(!all.is_empty());
    for r in &all {
        let shows = matches!(r.status, near_contract_sim::explore::Status::Shows(_));
        assert!(shows && !r.by_site, "{r:?}");
    }
    let _ = std::fs::remove_dir_all(&dir);
}
