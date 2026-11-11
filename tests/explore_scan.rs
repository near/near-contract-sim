//! `explore::Harness::scan` on actual WASM: each fault at each receipt it
//! fits, named exactly by `Receipts::index`; the caller's gas edges; and
//! the labels (shown, shown at an edge, forced).
//!
//! Verification boundary: the scan's variants and labels over actual runs
//! of the specimens on the pinned runtime.

mod common;
mod explore_vault;
mod fuzz_common;

use common::res;
use explore_vault::{User, VaultOp, deposit_then_withdraw, fails_after_ops, vault_on};
use near_contract_sim::explore::builtin;
use near_contract_sim::explore::{
    Amount, Call, Fail, Faults, Harness, Label, Op, Plan, Shown, Skip, Verdict,
};
use near_contract_sim::{Balance, Fault, Fixture, Receipts, Reply, Sim, Stub};
use serde::{Deserialize, Serialize};
use serde_json::json;

/// Alice deposits 100 tokens, then withdraws 40.
/// The forgetful build drops the shares of a withdraw whose payout fails:
/// the variant failing `on_withdraw`'s call to the token shows it, every
/// time, labeled forced, with the receipt named exactly. The fixed build
/// shows nothing.
#[test]
fn a_scan_finds_the_forgetful_payout_deterministically() {
    let scan = || {
        vault_on("vault_forgetful.wasm")
            .scan(&[deposit_then_withdraw()])
            .faults(Faults::tolerable())
            .run()
    };
    let audit = scan();
    let found = audit.assert_found("withdraw delivers what it debits", "Withdraw").unwrap_here();
    assert!(found.finding.injected, "{audit}");
    assert_eq!(
        found.label,
        Label::Forced("PromiseFailed at vault.near::on_withdraw".into()),
        "{audit}"
    );
    let inject = &found.run.plan().steps[1].inject[0];
    assert_eq!(inject.fault, Fault::PromiseFailed);
    assert_eq!(inject.at, Receipts::occurrence("vault.near", "on_withdraw", 0), "{inject}");
    assert!(found.finding.message.contains("gave up 40 shares and received 0 tokens"), "{audit}");
    assert!(audit.found().iter().all(|f| f.finding.injected), "{audit}");
    assert_eq!(scan().to_string(), audit.to_string(), "deterministic");
    let fixed =
        vault_on("vault.wasm").scan(&[deposit_then_withdraw()]).faults(Faults::tolerable()).run();
    assert!(fixed.found().is_empty(), "{fixed}");
}

/// The receipt tree names a call receipt by its site, `receiver::method`,
/// the form `assert_reached`, excuses and forced labels take: each label
/// copied from the tree is a receipt's site and passes `assert_reached`.
/// A forced fault's findings fold into one row by its label, named the
/// same way, so every forced label is one row's.
///
/// Verification boundary: the receipt tree's labels, and the scan's
/// coverage, forced labels and folds over actual runs.
#[test]
fn a_label_copied_from_the_tree_names_the_receipt_everywhere() {
    let h = vault_on("vault_forgetful.wasm");
    let run = h.run(&deposit_then_withdraw());
    let receipts = run.outcomes().iter().flat_map(|o| &o.receipts);
    let sites: Vec<String> = (receipts.filter(|r| r.method_names.len() == 1))
        .map(|r| format!("{}::{}", r.receiver_id, r.method_names[0]))
        .collect();
    assert!(sites.contains(&"vault.near::on_withdraw".to_owned()), "{sites:?}");
    // The word after a receipt line's `#height`: its label.
    let trees = run.trees();
    let copied: Vec<&str> = (trees.lines())
        .filter_map(|l| l.split_once('#')?.1.split_once(' ')?.1.split(' ').next())
        .filter(|label| label.contains("::"))
        .collect();
    for site in &sites {
        assert!(copied.contains(&site.as_str()), "{site} not in the tree:\n{trees}");
    }
    let audit = h.scan(&[deposit_then_withdraw()]).faults(Faults::tolerable()).run();
    for label in &copied {
        assert!(sites.contains(&label.to_string()), "{label}");
        audit.assert_reached(label);
    }
    let forced: Vec<&Label> =
        audit.found().iter().map(|f| &f.label).filter(|l| matches!(l, Label::Forced(_))).collect();
    let unique = forced.iter().enumerate().all(|(i, l)| !forced[..i].contains(l));
    assert!(unique, "a forced label's findings fold into one row: {audit}");
    let row = audit.assert_found("withdraw delivers what it debits", "Withdraw").unwrap_here();
    // The world check the same forced fault broke folded in.
    let folded = row.also.iter().any(|a| a.starts_with("the vault holds what it owes"));
    assert!(folded, "{audit}");
    let Label::Forced(at) = &row.label else { panic!("{audit}") };
    let site = at.split_once(" at ").unwrap().1;
    assert!(row.run.trees().contains(&format!(" {site} ")), "{site}:\n{}", row.run.trees());
}

/// A method that ran only as a view in the fixture's setup counts as
/// reached whether the fixture was built (cold) or loaded from the
/// snapshot cache (warm): the cache keeps what was viewed. It was
/// dropped, so a warm build listed the view as unreached.
///
/// Verification boundary: a scan's coverage over a fixture from
/// `Sim::cached`, cold and warm.
#[test]
fn coverage_is_the_same_from_a_built_and_a_cached_fixture() {
    let build = || {
        let mut sim = Sim::new();
        sim.create_account("alice.near", Balance::from_near(100));
        sim.deploy("status.near", res("status_message.wasm"));
        let set = json!({ "message": "hi" });
        sim.call("alice.near", "status.near", "set_status")
            .args_json(set)
            .transact()
            .assert_success();
        sim.view("status.near", "get_status", json!({ "account_id": "alice.near" }))
            .assert_success();
        sim
    };
    let runs = std::cell::Cell::new(0);
    let fixture = || {
        Sim::cached("viewed", || {
            runs.set(runs.get() + 1);
            build()
        })
    };
    let unreached = |sim: Sim| {
        let witness = Plan::new().op(Call::new("alice.near", "status.near", "set_status")
            .args_json(json!({ "message": "x" })));
        let audit = Harness::<Call>::new(sim).scan(&[witness]).faults(Faults::none()).run();
        audit.report().paths.unreached
    };
    let cold = unreached(build());
    fixture();
    // The second call loads, whether the first ran the setup or loaded.
    let warm = unreached(fixture());
    assert!(runs.get() <= 1, "the second fixture is loaded from the cache");
    assert!(!cold.contains(&"status.near::get_status".to_owned()), "{cold:?}");
    assert_eq!(cold, warm);
}

/// alice, and settle_starves.wat on pay.near.
fn pay_world() -> Sim {
    Fixture::new()
        .account("alice.near", Balance::from_near(100))
        .contract("pay.near", common::wat("settle_starves.wat"))
        .build()
}

/// A settlement that starves only when the caller attaches just enough:
/// the scan's caller-gas edge `least + 0` shows it, nothing forced; the
/// witness at its own gas is clean.
#[test]
fn the_least_caller_gas_starves_a_settlement_with_nothing_forced() {
    let harness = Harness::<Call>::new(pay_world());
    let witness = Plan::new().op(Call::new("alice.near", "pay.near", "pay"));
    harness.run(&witness).assert_clean();
    let audit = harness.scan(&[witness]).faults(Faults::edges()).run();
    let found = audit.assert_found(builtin::UNHANDLED_FAILURE, "pay.near::on_pay").unwrap_here();
    assert_eq!(found.label, Label::ShownAt("step 0, caller gas least + 0 gas".into()), "{audit}");
    assert!(!found.finding.injected, "{audit}");
}

const DESK: &str = "desk.near";
const ALICE: &str = "alice-ticket-holder.near";

#[derive(Clone, Debug, Serialize, Deserialize)]
enum DeskOp {
    Redeem { tickets: Vec<String>, amount: Amount },
}

impl Op for DeskOp {
    fn submit(&self, sim: &mut Sim) -> Result<(), Skip> {
        let DeskOp::Redeem { tickets, amount } = self;
        let balance: String = sim.view(DESK, "balance_of", json!({ "account_id": ALICE })).json();
        let n = amount.of(balance.parse().unwrap())?;
        if tickets.is_empty() || n == 0 {
            return Err("nothing to redeem".into());
        }
        sim.call(ALICE, DESK, "redeem")
            .args_json(json!({ "tickets": tickets, "amount": n.to_string() }))
            .submit();
        Ok(())
    }
}

/// A starved must-not-fail receipt is a finding, labeled forced: the
/// default scan starves `on_issued` on the fixed desk.
#[test]
fn a_starved_must_not_fail_receipt_is_a_forced_finding() {
    let world = Fixture::new()
        .account(ALICE, Balance::from_near(100))
        .stub("issuer.near", Stub::new().on("issue", Reply::json("ok")))
        .contract(DESK, res("desk.wasm"))
        .init("new")
        .args_json(json!({ "issuer": "issuer.near" }))
        .call(ALICE, DESK, "deposit")
        .deposit(Balance::from_near(50))
        .build();
    let tickets = (0..4).map(|i| format!("{i:08}")).collect();
    let witness = Plan::new().op(DeskOp::Redeem { tickets, amount: Amount::Exact(1000) });
    let harness = Harness::new(world).must_not_fail("on_issued");
    harness.run(&witness).assert_clean();
    let audit = harness.scan(&[witness]).run();
    let found = audit.assert_found(builtin::MUST_NOT_FAIL, "desk.near::on_issued").unwrap_here();
    assert_eq!(found.label, Label::Forced("OutOfGas at desk.near::on_issued".into()), "{audit}");
}

/// A witness shows its flow working, so one whose own receipt fails
/// (a withdraw of more than was deposited panics) is a harness error, in
/// a scan and in curves, and the audit isn't clean. The same witness
/// marked `Plan::refused()` is judged as the plan it is.
///
/// Verification boundary: how a scan and curves judge their witnesses,
/// over actual runs.
#[test]
fn a_witness_whose_receipt_fails_is_an_error_unless_meant_to_be_refused() {
    let over = Plan::new()
        .op(VaultOp::Deposit { who: User::Alice, amount: Amount::Exact(100) })
        .op(VaultOp::Withdraw { who: User::Alice, amount: Amount::Exact(400) });
    let h = vault_on("vault.wasm");
    let scan = h.scan(std::slice::from_ref(&over)).faults(Faults::none()).run();
    assert!(!scan.is_clean(), "{scan}");
    let failed = |e: &String| {
        e.starts_with("witness 0 (Deposit") && e.contains("failed at vault.near::withdraw")
    };
    assert!(scan.errors().iter().any(failed), "{scan}");
    let curves = h.curves(std::slice::from_ref(&over)).rungs(2).run();
    assert!(curves.errors().iter().any(failed), "{curves}");
    let refused = h.scan(&[over.clone().refused()]).faults(Faults::none()).run();
    assert!(refused.errors().is_empty() && refused.is_clean(), "{refused}");
    // The same rule for one plan run alone, with the same opt-out.
    let run = h.run(&over);
    assert_eq!(run.verdict(), Verdict::HarnessError, "{run}");
    assert!(run.reason().unwrap_or_default().contains("failed at vault.near::withdraw"), "{run}");
    h.run(&over.refused()).assert_clean();
}

/// One judgment of a failed witness, by `Harness::run` and a scan alike.
/// A failure no check of the harness's fired at is a harness error naming
/// the plan (its index in the scan, its steps) and the receipt, even where
/// another check found something: a Findings verdict doesn't hide it. A
/// failure where one did fire (here `must_not_fail`) is that finding, not
/// an error.
///
/// Verification boundary: how `Harness::run`, a scan and curves judge a
/// written witness, over actual runs.
#[test]
fn a_failed_witness_is_judged_once_by_run_and_scan() {
    let over = Plan::new()
        .op(VaultOp::Deposit { who: User::Alice, amount: Amount::Exact(100) })
        .op(VaultOp::Withdraw { who: User::Alice, amount: Amount::Exact(400) });
    // A check that fails on every case: alone, it makes the verdict Findings.
    let noisy = vault_on("vault.wasm").at_rest("fails by design", fails_after_ops("x"));
    let run = noisy.run(&over);
    assert_eq!(run.verdict(), Verdict::HarnessError, "{run}");
    let why = run.reason().unwrap();
    assert!(why.starts_with("the witness (Deposit"), "{why}");
    assert!(why.contains("; Withdraw") && why.contains("failed at vault.near::withdraw"), "{why}");
    // The checks that fired are named, and so is the way out.
    assert!(why.contains("fired: fails by design"), "{why}");
    assert!(why.contains("if this failure is the bug, mark the plan Plan::refused()"), "{why}");
    let scan = noisy.scan(&[deposit_then_withdraw(), over.clone()]).faults(Faults::none()).run();
    let named = |e: &String| {
        e.starts_with("witness 1 (Deposit")
            && e.contains("vault.near::withdraw")
            && e.contains("fired: fails by design")
    };
    assert_eq!(scan.errors().iter().filter(|e| named(e)).count(), 1, "{scan}");
    // Declared: the failure is the finding, in both.
    let judged = vault_on("vault.wasm").must_not_fail("vault.near::withdraw");
    let run = judged.run(&over);
    assert_eq!(run.verdict(), Verdict::Findings, "{run}");
    run.assert_found(builtin::MUST_NOT_FAIL, "vault.near::withdraw");
    let scan = judged.scan(std::slice::from_ref(&over)).faults(Faults::none()).run();
    assert!(scan.errors().is_empty(), "{scan}");
    scan.assert_found(builtin::MUST_NOT_FAIL, "vault.near::withdraw");
}

/// A run with findings gives a one-line reason: the first finding and how
/// many more. Only a clean run has none.
#[test]
fn a_run_with_findings_says_why_on_one_line() {
    let h = vault_on("vault.wasm").at_rest("fails by design", fails_after_ops("x"));
    let run = h.run(&deposit_then_withdraw());
    assert_eq!(run.verdict(), Verdict::Findings, "{run}");
    let why = run.reason().unwrap();
    assert!(why.starts_with("fails by design at world"), "{why}");
    assert!(!why.contains('\n'), "{why}");
    assert_eq!(vault_on("vault.wasm").run(&deposit_then_withdraw()).reason(), None);
}

/// `ping` calls `sink.near::absent`, a method the stub doesn't declare
/// (so the call fails), then its own `on_done`. The scan holds or fails
/// the receipts that exist, never one for a method its receiver lacks: no
/// harness error.
#[test]
fn a_scan_never_holds_a_method_its_stub_does_not_declare() {
    const PING: &str = r#"(module
      (import "env" "promise_create"
        (func $create (param i64 i64 i64 i64 i64 i64 i64 i64) (result i64)))
      (import "env" "promise_then"
        (func $then (param i64 i64 i64 i64 i64 i64 i64 i64 i64) (result i64)))
      (memory 1)
      (export "memory" (memory 0))
      (data (i32.const 0) "sink.nearabsentping.nearon_done")
      (func (export "ping")
        (drop (call $then
          (call $create (i64.const 9) (i64.const 0) (i64.const 6) (i64.const 9)
                        (i64.const 0) (i64.const 0) (i64.const 64) (i64.const 5000000000000))
          (i64.const 9) (i64.const 15) (i64.const 7) (i64.const 24)
          (i64.const 0) (i64.const 0) (i64.const 64) (i64.const 5000000000000))))
      (func (export "on_done")))"#;
    let world = Fixture::new()
        .account("alice.near", Balance::from_near(10))
        .contract("ping.near", wat::parse_str(PING).unwrap())
        .stub("sink.near", Stub::new().on("present", Reply::json(())))
        .build();
    let witness = Plan::new().op(Call::new("alice.near", "ping.near", "ping"));
    let audit = Harness::new(world).scan(&[witness]).run();
    assert!(audit.errors().is_empty(), "{:?}\n{audit}", audit.errors());
    assert!(audit.cases() > 1, "{audit}");
}

/// `go` writes `bad`.
const GOES: &str = r#"(module
  (import "env" "storage_write" (func $sw (param i64 i64 i64 i64 i64) (result i64)))
  (memory 1)
  (data (i32.const 0) "bad")
  (func (export "go")
    (drop (call $sw (i64.const 3) (i64.const 0) (i64.const 3) (i64.const 0) (i64.const 1)))))"#;

/// Calls `go` `times` times; with none to make, it skips, as it does
/// `after` a step that wrote nothing.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct Go {
    times: u64,
    after: bool,
}

impl Op for Go {
    fn submit(&self, sim: &mut Sim) -> Result<(), Skip> {
        if self.times == 0 {
            return Err(Skip("no call to make".into()));
        }
        if self.after && sim.state("c.near", b"bad").is_none() {
            return Err(Skip("nothing before it".into()));
        }
        for _ in 0..self.times.min(3) {
            sim.call("alice.near", "c.near", "go").submit();
        }
        Ok(())
    }
}

/// An edge whose op skips wasn't tried: the scan's edge at
/// step 0's `times = 0` is an `incomplete` entry, an open `incomplete:`
/// item and a summary line, and round-trips through the report's JSON.
/// Before, the edge ran as tried and nothing said otherwise. A later
/// op that skips because of an edge is what the edge did: no entry.
///
/// Verification boundary: the scan's verdict and report over actual runs.
#[test]
fn an_edge_its_op_skipped_is_incomplete_and_open() {
    let sim = Fixture::new()
        .account("alice.near", Balance::from_near(10))
        .contract("c.near", wat::parse_str(GOES).unwrap())
        .build();
    let h = Harness::<Go>::new(sim).at_rest("anything", |_: &Sim| Ok::<(), String>(()));
    // At step 0's `times = 0`, step 1 skips too, and step 2 runs.
    let go = |after| Go { times: 1, after };
    let plan = Plan::new().op(go(false)).op(go(true)).op(go(false));
    let audit = h.scan(&[plan]).faults(Faults::edges()).run();
    let edge =
        |e: &&(String, String)| e.1.contains("step 0, /times = 0: edge not tried: op skipped");
    let entry = audit.incomplete().iter().find(edge);
    let entry = entry.unwrap_or_else(|| panic!("{:#?}\n{audit}", audit.incomplete()));
    assert!(entry.1.contains("no call to make"), "{entry:?}");
    let id = format!("incomplete:{}", entry.0);
    assert!(audit.todo().iter().any(|t| t.id == id), "{:#?}", audit.todo());
    assert!(audit.to_string().lines().any(|l| l.starts_with("incomplete: ")), "{audit}");
    assert!(audit.is_clean(), "what was tried is clean: {audit}");
    // Step 1 skipping after step 0's edge is what the edge did: tried.
    let later = |e: &&(String, String)| e.1.contains(", step 0, ") && e.1.contains("(step 1)");
    assert_eq!(audit.incomplete().iter().find(later), None, "{audit}");
    let report = audit.report();
    let back = near_contract_sim::explore::Report::from_json(&report.json()).unwrap();
    assert!(back.incomplete.contains(entry), "{back:?}");
    assert!(back.todo.iter().any(|t| t.id == id));
}

/// Calls `go` `n` times, from 1 to 3; any other `n` skips.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct Few {
    n: u64,
}

impl Op for Few {
    fn submit(&self, sim: &mut Sim) -> Result<(), Skip> {
        if !(1..=3).contains(&self.n) {
            return Err(Skip(format!("{} calls is out of range", self.n)));
        }
        for _ in 0..self.n {
            sim.call("alice.near", "c.near", "go").submit();
        }
        Ok(())
    }
}

/// Skipped edges of one op on one axis are one `incomplete:` item, with
/// their count and first case, closed by one glob: before, each
/// edge was its own item, and an audit with dozens read as noise.
///
/// Verification boundary: the scan's to-do list over actual runs.
#[test]
fn one_op_s_skipped_edges_on_one_axis_are_one_item() {
    let sim = Fixture::new()
        .account("alice.near", Balance::from_near(10))
        .contract("c.near", wat::parse_str(GOES).unwrap())
        .build();
    let h = Harness::<Few>::new(sim).at_rest("anything", |_: &Sim| Ok::<(), String>(()));
    let plan = Plan::new().op(Few { n: 2 });
    let audit = h.clone().scan(std::slice::from_ref(&plan)).faults(Faults::edges()).run();
    let skipped = audit.incomplete().len();
    assert!(skipped >= 2, "several edges skip: {:#?}", audit.incomplete());
    let todo = audit.todo();
    let items: Vec<_> = todo.iter().filter(|t| t.id.starts_with("incomplete:")).collect();
    assert_eq!(items.len(), 1, "{items:#?}");
    assert!(items[0].why.starts_with(&format!("{skipped} case(s)")), "{:?}", items[0]);
    assert!(items[0].why.contains("out of range"), "{:?}", items[0]);
    assert_eq!(items[0].close, "an op that doesn't skip there, or a plan where it applies");
    let line = audit.to_string();
    let line = line.lines().find(|l| l.starts_with("incomplete: ")).expect("a summary line");
    assert!(line.contains(&format!("({skipped})")), "{line}");
    let excused = h.excuse("incomplete:Few*", "n is 1 to 3 by design");
    let audit = excused.scan(&[plan]).faults(Faults::edges()).run();
    assert!(!audit.todo().iter().any(|t| t.id.starts_with("incomplete:")), "{audit}");
}

/// A per-op check that reads the shared world while two ops overlap
/// judges nothing: the case's `incomplete:` item is named after the
/// check, not "the plan as written", and its close says how to judge it.
/// Each such check gets its own item, so excusing one leaves
/// the other open. A check not judged for its own reason is an item
/// closed by a plan where it can judge; a check that judged is none.
///
/// Verification boundary: the report's to-do ids and text over actual
/// runs.
#[test]
fn an_overlapped_check_names_its_incomplete_item() {
    let sim = Fixture::new()
        .account("alice.near", Balance::from_near(10))
        .contract("c.near", wat::parse_str(GOES).unwrap())
        .build();
    let h = Harness::<Go>::new(sim)
        .per_op("reads the world", |d| {
            let _ = d.world();
            Ok::<(), String>(())
        })
        .per_op("reads it too", |d| {
            let _ = d.world();
            Ok::<(), String>(())
        })
        .per_op("reads nothing", |_| Ok::<(), String>(()))
        .per_op("has no ledger", |_| Err::<(), _>(Fail::not_judged("no ledger here")));
    let go = Go { times: 1, after: false };
    let plan = Plan::new().op(go.clone()).blocks(0).op(go);
    let audit = h.scan(std::slice::from_ref(&plan)).faults(Faults::none()).run();
    let todo = audit.todo();
    let item = todo.iter().find(|t| t.id == "incomplete:reads the world not judged (overlapped)");
    let item = item.unwrap_or_else(|| panic!("{todo:#?}"));
    assert!(item.close.contains("effects()") && item.close.contains("at_rest_ops"), "{item:?}");
    assert!(item.close.contains("from the last whose OpDone::rest() is true"), "{item:?}");
    let own = todo.iter().find(|t| t.id == "incomplete:has no ledger not judged");
    let own = own.unwrap_or_else(|| panic!("{todo:#?}"));
    assert_eq!(own.close, "a plan where the check can judge", "{own:?}");
    assert!(!todo.iter().any(|t| t.id.contains("reads nothing")), "{todo:#?}");
    assert!(!todo.iter().any(|t| t.id.contains("the plan as written")), "{todo:#?}");
    let incomplete = todo.iter().filter(|t| t.id.starts_with("incomplete:"));
    assert_eq!(incomplete.count(), 3, "one item a check that didn't judge: {todo:#?}");
    let one = h.excuse("incomplete:reads the world*", "judged at rest elsewhere");
    let todo = one.scan(&[plan]).faults(Faults::none()).run().todo();
    let other = "incomplete:reads it too not judged (overlapped)";
    assert!(todo.iter().any(|t| t.id == other), "{todo:#?}");
}

/// An exported method one campaign never ran is open there, and its
/// close names the union that closes it: a union with a report
/// that reached it has no `unreached:` item.
///
/// Verification boundary: the report's to-do list over actual runs.
#[test]
fn an_unreached_method_closes_with_a_union_that_reached_it() {
    const TWO: &str = r#"(module
      (memory 1)
      (export "memory" (memory 0))
      (func (export "go"))
      (func (export "other")))"#;
    let sim = Fixture::new()
        .account("alice.near", Balance::from_near(10))
        .contract("two.near", wat::parse_str(TWO).unwrap())
        .build();
    let h = Harness::<Call>::new(sim);
    let call = |m: &str| Plan::new().op(Call::new("alice.near", "two.near", m));
    let one = h.scan(&[call("go")]).faults(Faults::none()).run().report();
    let item = one.todo.iter().find(|t| t.id == "unreached:two.near::other");
    let item = item.unwrap_or_else(|| panic!("{:#?}", one.todo));
    assert!(item.close.contains("Harness::union"), "{item:?}");
    let other = h.scan(&[call("other")]).faults(Faults::none()).run().report();
    let both = h.union(&[one, other]);
    assert!(!both.todo.iter().any(|t| t.id.starts_with("unreached:")), "{:#?}", both.todo);
}

/// A failed witness's error names the function its trap is in,
/// from `Harness::run` and from a campaign: `tests/res/abort_ledger.wat`
/// traps in `ledger::debit` on a withdraw of more than 100.
///
/// Verification boundary: the harness error's text over actual runs, the
/// trap named from a rerun on an instrumented copy.
#[test]
fn a_failed_witness_names_its_trap() {
    #[derive(Clone, Debug, Serialize, Deserialize)]
    struct Withdraw(u64);

    impl Op for Withdraw {
        fn submit(&self, sim: &mut Sim) -> Result<(), Skip> {
            sim.call("alice.near", "ledger.near", "withdraw").args(self.0.to_le_bytes()).submit();
            Ok(())
        }
    }

    let wat = std::fs::read_to_string(common::manifest_dir().join("tests/res/abort_ledger.wat"));
    let sim = Fixture::new()
        .account("alice.near", Balance::from_near(10))
        .contract("ledger.near", wat::parse_str(wat.unwrap()).unwrap())
        .build();
    let h = Harness::<Withdraw>::new(sim);
    let over = Plan::new().op(Withdraw(500));
    let named = "executed in `ledger::debit`, called from `withdraw`";
    let run = h.run(&over);
    assert_eq!(run.verdict(), Verdict::HarnessError, "{run}");
    assert!(run.reason().unwrap_or_default().contains(named), "{run}");
    let scan = h.scan(&[over]).faults(Faults::none()).run();
    assert!(scan.errors().iter().any(|e| e.contains(named)), "{:#?}", scan.errors());
}

/// `c.near` runs the fuzz contract; `u0.near` calls it.
fn race_world() -> Sim {
    let mut sim = Sim::new();
    sim.create_account("c.near", Balance::from_near(1_000));
    sim.deploy("c.near", fuzz_common::fuzz_wasm());
    sim.create_account("u0.near", Balance::from_near(100));
    sim
}

/// `c.near` sends itself `x` and `wrap` in one block, and `wrap` sends
/// itself `late`: `x` runs a block before `late`, and neither descends
/// from the other.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct Race;

impl Op for Race {
    fn submit(&self, sim: &mut Sim) -> Result<(), Skip> {
        use fuzz_common::{Op as Prog, TGAS, encode};
        // 10 Tgas for itself, and the gas of the calls it makes.
        let call = |then: &[Prog]| {
            let inner =
                then.iter().map(|op| if let Prog::Call { gas, .. } = op { *gas } else { 0 });
            let gas = 10 * TGAS + inner.sum::<u64>();
            Prog::Call { to: "c.near".into(), args: encode(then), deposit: 0, gas, weight: 0 }
        };
        let late = call(&[Prog::Log("late".into())]);
        let prog = [call(&[Prog::Log("x".into())]), call(&[late])];
        let tx = sim.call("u0.near", "c.near", "run").args(encode(&prog));
        tx.gas(near_contract_sim::Gas::from_teragas(200)).submit();
        Ok(())
    }
}

/// `Faults::reorder()`: each pair of one transaction's receipts
/// where neither descends from the other runs the other way round, the
/// first held with `HoldUntil` until the second has run, forced. The race
/// has two such pairs (`x` with `wrap`, `x` with `late`; `late` descends
/// from `wrap`), and a check that `x` runs before `late` fails only where
/// `x` waits for `late`. Without `reorder` nothing is held.
///
/// Verification boundary: which holds a scan schedules, over actual runs.
#[test]
fn reorder_runs_each_pair_of_unrelated_receipts_the_other_way() {
    let x_first = |d: &near_contract_sim::explore::OpDone<Race>| -> Result<(), String> {
        let ran = |line: &str| {
            let r = d.calls().into_iter().find(|r| r.logs.iter().any(|l| l == line));
            r.map(|r| r.block_height).ok_or(format!("no {line}"))
        };
        match ran("x")? < ran("late")? {
            true => Ok(()),
            false => Err("late ran before x".into()),
        }
    };
    let h = Harness::new(race_world()).per_op("x before late", x_first);
    let plan = Plan::new().op(Race);
    let audit = h.scan(std::slice::from_ref(&plan)).faults(Faults::reorder()).run();
    assert_eq!(audit.cases(), 3, "the witness and two reorders: {audit}");
    let f = audit.assert_found("x before late", "Race").unwrap_here();
    assert!(matches!(&f.label, Label::Forced(l) if l.contains("HoldUntil")), "{audit}");
    assert_eq!(audit.found().len(), 1, "{audit}");
    let none = h.scan(&[plan]).faults(Faults::none()).run();
    assert_eq!(none.cases(), 1, "{none}");
    none.assert_clean();
}

/// A chain of calls and callbacks (the vault pair's deposit: each receipt
/// descends from the one before by a call or a promise result) has no
/// pair to reorder: `reorder` adds no case a hold could only deadlock.
#[test]
fn reorder_holds_nothing_on_a_chain_of_callbacks() {
    let wasm = common::wat("vault_pair.wat");
    let mut sim = Sim::new();
    sim.create_account("alice.near", Balance::from_near(100));
    for c in ["token.near", "vault.near"] {
        sim.create_account(c, Balance::from_near(100));
        sim.deploy(c, wasm.clone());
    }
    sim.call("alice.near", "token.near", "mint")
        .args(100u64.to_le_bytes())
        .transact()
        .assert_success();
    let args = [&40u64.to_le_bytes()[..], b"vault.near"].concat();
    let deposit = Call::new("alice.near", "token.near", "transfer_call").args(args);
    let audit = Harness::<Call>::new(sim)
        .scan(&[Plan::new().call(deposit)])
        .faults(Faults::reorder())
        .run();
    assert_eq!(audit.cases(), 1, "{audit}");
    assert!(audit.errors().is_empty(), "{audit}");
}

/// An edge whose op skipped is no complete case: at `n = 0` (or past 3)
/// one op skips and the other runs, which the check judges, while it
/// can't judge any case of two calls or more, or of a call the sweep
/// sends from `axis.near`. Every case but those edges is incomplete, so
/// nothing was tried to its end: not clean.
/// Before, those edges counted as complete cases and the audit read
/// clean.
///
/// Verification boundary: the scan's verdict over actual runs.
#[test]
fn a_scan_whose_only_judged_cases_are_skipped_edges_is_not_clean() {
    let sim = Fixture::new()
        .account("alice.near", Balance::from_near(10))
        .contract("c.near", wat::parse_str(GOES).unwrap())
        .build();
    let fixture = sim.txs().len();
    // Judged after one call of alice's at most, and none sent from elsewhere.
    let h = Harness::<Few>::new(sim).at_rest("one call of alice's", move |s: &Sim| {
        let signer = |tx| s.outcome(tx).receipts.first().map(|r| r.predecessor_id.to_string());
        let signers: Vec<_> = s.txs()[fixture..].iter().map(signer).collect();
        if signers.len() <= 1 && signers.iter().flatten().all(|a| a == "alice.near") {
            Ok(())
        } else {
            Err(Fail::not_judged(format!("signed by {signers:?}")))
        }
    });
    let plan = Plan::new().op(Few { n: 1 }).op(Few { n: 1 });
    let audit = h.scan(&[plan]).faults(Faults::edges()).run();
    let skipped = audit.incomplete().iter().filter(|e| e.1.contains("edge not tried"));
    assert!(skipped.count() > 0, "{:#?}\n{audit}", audit.incomplete());
    assert!(audit.found().is_empty() && audit.errors().is_empty(), "{audit}");
    assert!(!audit.is_clean(), "no case tried to its end: {:#?}\n{audit}", audit.incomplete());
}

/// A resumed scan is clean, and done, as the whole campaign is: a saved
/// part's finding or harness error makes it not clean, as its summary
/// says, and a saved part whose every case was incomplete has no complete
/// case. A resume that runs no case of its own is clean if the saved part
/// is. A saved part's open item (an error is one) is open in `todo()`, so
/// `done()` is `Err`, as the summary lists it; `found_at` finds a saved
/// finding, by its own check and site or one folded into it. Before,
/// `is_clean`, `todo()`, `done()` and `found_at` read only the resumed
/// run's own part: a saved finding, error or open item read clean, done
/// and not found, and a clean campaign resumed with nothing left to run
/// read not clean.
///
/// Verification boundary: the scan's verdict over actual runs, resumed
/// from saves files this scan wrote, some with a planted finding, error,
/// item or count.
#[test]
fn a_resumed_scan_is_clean_only_if_its_saved_part_is() {
    let dir = std::env::temp_dir().join(format!("resumed-clean-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let sim = Fixture::new()
        .account("alice.near", Balance::from_near(10))
        .contract("c.near", wat::parse_str(GOES).unwrap())
        .build();
    let h = Harness::<Few>::new(sim).store(dir.join("found"));
    let h = h.at_rest("anything", |_: &Sim| Ok::<(), String>(()));
    // Its skipped edges excused: nothing else is open, so it is done.
    let h = h.excuse("incomplete:Few*", "n is 1 to 3 by design");
    let plan = Plan::new().op(Few { n: 2 });
    let path = dir.join("scan.json");
    let scan = || h.scan(std::slice::from_ref(&plan)).faults(Faults::edges()).saves(&path, 1);
    let whole = scan().run();
    assert!(whole.is_clean() && whole.cases() > 1, "{whole}");
    assert!(whole.done().is_ok(), "{whole}");
    let saved: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    let done = saved["done"].as_u64().unwrap();
    assert!(done > 1, "{saved}");
    // The saves file, `plant`ed, resumed with `left` variants to run.
    let resumed = |left: u64, plant: &dyn Fn(&mut serde_json::Value)| {
        let mut v = saved.clone();
        v["done"] = json!(done - left);
        plant(&mut v);
        std::fs::write(&path, v.to_string()).unwrap();
        scan().run()
    };
    let none = |_: &mut serde_json::Value| {};
    let ran = resumed(1, &none);
    assert!(ran.is_clean() && ran.cases() == 1, "one variant run, clean: {ran}");
    assert!(ran.done().is_ok(), "{ran}");
    let all_done = resumed(0, &none);
    assert!(all_done.is_clean() && all_done.cases() == 0, "nothing left to run: {all_done}");
    assert!(all_done.done().is_ok(), "{all_done}");
    let row = json!({
        "kind": "check", "check": "anything", "site": "c.near", "label": "shown",
        "message": "planted", "cases": 1, "steps": [], "replay": null, "id": "planted",
    });
    let mut row = row;
    // Found by its own check and site, and by those folded into it.
    let key = json!({ "Key": { "key": "c.near", "op": null } });
    row["found_by"] = json!([["anything", key], ["folded in", "World"]]);
    let found = resumed(1, &|v| v["findings"] = json!([row]));
    assert!(found.found().is_empty(), "the finding is the saved part's: {found}");
    assert!(found.to_string().contains("1 finding(s)"), "{found}");
    assert!(!found.is_clean(), "a saved finding: {found}");
    let in_saved = |check, at| match found.found_at(check, at) {
        Some(Shown::Saved(row)) => row.message == "planted",
        _ => false,
    };
    assert!(in_saved("anything", "c.near") && in_saved("folded in", "world"), "{found}");
    assert!(found.found_at("anything", "world").is_none(), "{found}");
    assert_eq!(found.assert_found("folded in", "world").here().map(|f| f.cases), None);
    let error = resumed(1, &|v| v["errors"] = json!(["planted: a harness error"]));
    assert!(error.errors().is_empty() && error.to_string().contains("planted"), "{error}");
    assert!(!error.is_clean(), "a saved error: {error}");
    let open = |a: &near_contract_sim::explore::Audit<Few>| {
        let ids: Vec<String> = a.todo().into_iter().map(|t| t.id).collect();
        assert_eq!(a.done().map(|_| ()).map_err(|t| t.len()), Err(ids.len()), "{a}");
        ids
    };
    assert_eq!(open(&error), ["errors"], "a saved error is open: {error}");
    // A saved part's open item, and nothing else.
    let item = json!({ "id": "planted", "why": "planted", "close": "planted" });
    let todo = resumed(1, &|v| v["todo"] = json!([item]));
    assert!(todo.is_clean() && todo.to_string().contains("TODO planted"), "{todo}");
    assert_eq!(open(&todo), ["planted"], "a saved item is open: {todo}");
    let with_gaps = todo.with_open_gaps("a test");
    assert!(with_gaps.starts_with("Coverage not achieved (a test): 1 item(s) open"), "{with_gaps}");
    let cases = saved["cases"].clone();
    let incomplete = resumed(0, &|v| v["incomplete_cases"] = cases.clone());
    assert!(!incomplete.is_clean(), "no complete case in the saved part: {incomplete}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// A scan resumed with nothing left to run finds what its saved part
/// found, as its summary prints it: `found_at` gives the saved row, which
/// keeps no case. Before, `found_at` read only the resumed run's own
/// part, and `found_at(..).is_none()` passed on a finding the summary
/// listed.
///
/// Verification boundary: actual runs, resumed from the saves file the
/// scan wrote.
#[test]
fn a_resumed_scan_finds_its_saved_part_s_findings() {
    let dir = std::env::temp_dir().join(format!("resumed-found-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let sim = Fixture::new()
        .account("alice.near", Balance::from_near(10))
        .contract("c.near", wat::parse_str(GOES).unwrap())
        .build();
    let h = Harness::<Few>::new(sim).store(dir.join("found"));
    let h = h.at_rest("never bad", |s: &Sim| match s.state("c.near", b"bad") {
        Some(_) => Err("wrote bad".to_owned()),
        None => Ok(()),
    });
    let path = dir.join("scan.json");
    let plan = Plan::new().op(Few { n: 1 });
    let scan = || h.scan(std::slice::from_ref(&plan)).faults(Faults::edges()).saves(&path, 1);
    let whole = scan().run();
    assert!(matches!(whole.assert_found("never bad", "world"), Shown::Here(_)), "{whole}");
    let resumed = scan().run();
    assert!(resumed.found().is_empty() && resumed.cases() == 0, "{resumed}");
    let Some(Shown::Saved(row)) = resumed.found_at("never bad", "world") else {
        panic!("the saved part's finding: {resumed}")
    };
    assert_eq!(row.message, "wrote bad", "{resumed}");
    assert!(!resumed.is_clean(), "{resumed}");
    let _ = std::fs::remove_dir_all(&dir);
}
