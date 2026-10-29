//! Plans and the harness's settings are checked before the first case,
//! and every problem is listed, not the first.
//!
//! Verification boundary: the harness's refusal before any execution;
//! nothing runs, so no simulator behaviour is involved.

use near_contract_sim::explore::{Call, Faults, Harness, Plan, Verdict, plans};
use near_contract_sim::{Balance, Fault, Fixture, Receipts, Sim};
use proptest::strategy::Just;

const WAT: &str = r#"
(module
  (memory (export "memory") 1)
  (func (export "pair"))
  (func (export "on_pair")))
"#;

fn world() -> Sim {
    Fixture::new()
        .account("alice.near", Balance::from_near(10))
        .contract("bank.near", wat::parse_str(WAT).unwrap())
        .build()
}

fn pair() -> Call {
    Call::new("alice.near", "bank.near", "pair")
}

/// A gap set twice and a misspelled injection target: both listed, at
/// once, and nothing run.
#[test]
fn a_plan_with_two_mistakes_fails_at_once_listing_both() {
    let plan = Plan::new()
        .op(pair())
        .epochs(1)
        .epochs(4)
        .op(pair())
        .inject(Receipts::method("on_piar"), Fault::PromiseFailed);
    assert_eq!(plan.errors().len(), 1, "the builder keeps the error, never panics");
    let run = Harness::new(world()).run(&plan);
    assert_eq!(run.verdict(), Verdict::HarnessError, "{run}");
    assert!(run.outcomes().is_empty(), "nothing ran: {run}");
    let why = run.reason().unwrap_or_default();
    assert!(why.contains("already Epochs(1); Epochs(4) would replace it"), "{why}");
    assert!(why.contains("no contract in the fixture exports `on_piar`"), "{why}");

    // A scan reports it as the harness's error, not a finding.
    let audit = Harness::new(world()).scan(&[plan]).faults(Faults::none()).run();
    assert!(audit.found().is_empty(), "{audit}");
    assert!(audit.errors().iter().any(|e| e.contains("on_piar")), "{audit}");
}

/// Two faults on one step are both forced, not only the last.
#[test]
fn two_injects_on_one_step_are_both_kept() {
    let plan = Plan::new()
        .op(pair())
        .inject(Receipts::callback("on_pair"), Fault::PromiseFailed)
        .inject(Receipts::callback("on_pair"), Fault::OutOfGas);
    assert!(plan.errors().is_empty(), "{:?}", plan.errors());
    assert_eq!(plan.steps[0].inject.len(), 2);
}

/// The harness's own settings, an empty `must_not_fail` or
/// `anyone_may_call` and an excuse without a reason, are listed
/// together before a search's first case.
#[test]
fn a_search_lists_every_harness_problem_before_its_first_case() {
    let harness =
        Harness::new(world()).must_not_fail("").anyone_may_call(" ").excuse("unreached:*", " ");
    let audit = harness.search(plans(Just(pair()))).cases(4).seed(1).run();
    let error = audit.errors().first().cloned().unwrap_or_else(|| panic!("{audit}"));
    assert!(error.contains("must_not_fail(\"\") names no site"), "{error}");
    assert!(error.contains("anyone_may_call(\" \") names no site"), "{error}");
    assert!(error.contains("needs an item and why"), "{error}");
    assert!(audit.found().is_empty(), "{audit}");
}

/// `go` calls `bank.near::nope`, which bank.near doesn't export.
const CALLER: &str = r#"
(module
  (import "env" "promise_create"
    (func $create (param i64 i64 i64 i64 i64 i64 i64 i64) (result i64)))
  (memory (export "memory") 1)
  (data (i32.const 0) "bank.near")
  (data (i32.const 16) "nope")
  (func (export "go")
    (drop (call $create (i64.const 9) (i64.const 0) (i64.const 4) (i64.const 16)
                        (i64.const 0) (i64.const 0) (i64.const 64) (i64.const 10000000000000)))))
"#;

/// A search's sampled fault that lands on a receipt calling what its
/// receiver doesn't export (a stub's undeclared method, a missing one) is
/// skipped with its reason: the case runs without it, and the search goes
/// on. This one runs: the call fails as it would.
#[test]
fn a_sampled_fault_at_an_unexported_method_is_skipped_not_an_error() {
    let world = Fixture::new()
        .account("alice.near", Balance::from_near(10))
        .contract("bank.near", wat::parse_str(WAT).unwrap())
        .contract("caller.near", wat::parse_str(CALLER).unwrap())
        .build();
    let go = Call::new("alice.near", "caller.near", "go");
    let faults = Faults::new(Receipts::all(), [Fault::Hold(1)]).per_case(1.0);
    let h = Harness::new(world);
    let audit = h.search(plans(Just(go.clone())).faults(faults)).cases(4).seed(1).run();
    assert!(audit.errors().is_empty(), "{audit}");
    assert_eq!(audit.cases(), 4, "{audit}");
    let plan = Plan::new().call(go).inject(Receipts::all().nth(1), Fault::Hold(1)).refused();
    let run = h.run(&plan);
    assert_ne!(run.verdict(), Verdict::HarnessError, "{run}");
    assert!(run.to_string().contains("which its receiver doesn't export"), "{run}");
}
