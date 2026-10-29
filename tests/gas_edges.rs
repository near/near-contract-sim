//! `edges` and `Tx::gas_edges` on generic demos: the 4-gas window where a
//! split attaches its child 0 gas (`tests/res/gas_split.wat`), found only
//! by a solved point; the gas where a settlement starves
//! (`tests/res/settle_starves.wat`), to 1 gas; the guard on a key that
//! holds burnt gas; and a log's size limit, to 1 byte.
//!
//! Verification boundary: edges over actual runs of the demos on the
//! pinned runtime, in this world.

mod common;
use common::{manifest_dir, panic_message};
use near_contract_sim::explore::{OutcomeShape, Region, Why, edges};
use near_contract_sim::{Balance, Failure, Fault, Fixture, Gas, Receipts, Sim, TxOutcome};
use std::panic::{AssertUnwindSafe, catch_unwind};

const ALICE: &str = "alice.near";
const SPLIT: &str = "split.near";
/// The reserve `split` keeps: from it to 3 gas above, the child gets 0.
const RESERVE: u64 = 9_876_543_210_123;

fn tgas(n: u64) -> Gas {
    Gas::from_teragas(n)
}

fn wat(name: &str) -> Vec<u8> {
    wat::parse_file(manifest_dir().join("tests/res").join(name)).unwrap()
}

/// alice, and `code` on `SPLIT`.
fn world(code: Vec<u8>) -> Sim {
    Fixture::new().account(ALICE, Balance::from_near(100)).contract(SPLIT, code).build()
}

/// `method` on `SPLIT` with `gas`, on a copy of `sim`.
/// A region's range, key and why, to compare.
fn shape<K: Clone>(r: &Region<K>) -> (std::ops::RangeInclusive<u64>, K, Why) {
    (r.range.clone(), r.key.clone(), r.why)
}

fn at(sim: &Sim, method: &str, gas: u64) -> TxOutcome {
    sim.clone().call(ALICE, SPLIT, method).gas(Gas::from_gas(gas)).transact()
}

/// The 4 gas where `split` attaches its child 0 and fails, with the
/// transaction succeeding on both sides, lie between grid reads 1 Tgas
/// apart that agree: `edges` alone misses them, and `gas_edges` finds
/// them exactly from the child's fitted line, as `Why::Solved`.
/// Under `OutcomeShape::of` the window starts where the child first
/// appears, so bisecting the grid reads on either side (no child, a child
/// out of gas) lands on it: found by the grid, with every region pinned.
#[test]
fn a_zero_gas_child_narrower_than_the_step_is_solved_exactly() {
    let sim = world(wat("gas_split.wat"));
    let ok = |gas| at(&sim, "split", gas).is_success();
    let window = RESERVE..=RESERVE + 3;
    assert_eq!(
        [RESERVE - 1, RESERVE, RESERVE + 3, RESERVE + 4].map(ok),
        [true, false, false, true]
    );
    let refused = at(&sim, "split", RESERVE);
    let Some(Failure::Action(kind)) = refused.failure() else { panic!("{refused}") };
    assert!(format!("{kind:?}").contains("FunctionCallZeroAttachedGas"), "{refused}");

    let height = sim.height();
    let found = sim.clone().call(ALICE, SPLIT, "split").gas_edges(
        tgas(1)..=tgas(300),
        tgas(1),
        TxOutcome::is_success,
    );
    assert_eq!(sim.height(), height, "nothing submitted");
    let fails: Vec<_> = found.iter().filter(|r| !r.key).map(shape).collect();
    assert_eq!(fails, [(window.clone(), false, Why::Solved)]);
    assert_eq!(found.len(), 3, "{found:?}");

    // Capped at 25 Tgas, the child's gas bends flat past 110 Tgas: the
    // window is still solved, from the line below the bend.
    let src = std::fs::read_to_string(manifest_dir().join("tests/res/gas_split.wat")).unwrap();
    let uncapped = "(global $cap i64 (i64.const -1))";
    assert!(src.contains(uncapped));
    let capped = src.replace(uncapped, "(global $cap i64 (i64.const 25000000000000))");
    let bent = world(wat::parse_str(capped).unwrap());
    let found = bent.clone().call(ALICE, SPLIT, "split").gas_edges(
        tgas(1)..=tgas(300),
        tgas(1),
        TxOutcome::is_success,
    );
    let fails: Vec<_> = found.iter().filter(|r| !r.key).map(shape).collect();
    assert_eq!(fails, [(window.clone(), false, Why::Solved)]);
    let flat = at(&bent, "split", tgas(200).as_gas());
    assert_eq!(flat.expect_receipt("child").attached_gas, tgas(25), "{flat}");

    // A third and a seventh, at reserves where the solved point lands a
    // few gas off: the window is n gas wide, and still solved.
    for (n, reserve) in [(3, 7_361_955_359_880), (7, 11_433_340_745_693)] {
        let wat = (src.replace(
            "(global $share i64 (i64.const 4))",
            &format!("(global $share i64 (i64.const {n}))"),
        ))
        .replace(&format!("(i64.const {RESERVE})"), &format!("(i64.const {reserve})"));
        let shared = world(wat::parse_str(wat).unwrap());
        assert!(!at(&shared, "split", reserve).is_success());
        let found = shared.clone().call(ALICE, SPLIT, "split").gas_edges(
            tgas(1)..=tgas(300),
            tgas(1),
            TxOutcome::is_success,
        );
        let fails: Vec<_> = found.iter().filter(|r| !r.key).map(shape).collect();
        let window = reserve..=reserve + n - 1;
        assert_eq!(fails, [(window, false, Why::Solved)], "1/{n}");
    }

    // The grid, its bisection and the radius alone: every read succeeds.
    let plain = edges(tgas(1).as_gas()..=tgas(300).as_gas(), tgas(1).as_gas(), 64, ok);
    assert_eq!(plain.len(), 1, "pinned miss: {plain:?}");

    let shapes = sim.clone().call(ALICE, SPLIT, "split").gas_edges(
        tgas(1)..=tgas(300),
        tgas(1),
        OutcomeShape::of,
    );
    let zero = "Action::NewReceiptValidationError::ActionsValidation::FunctionCallZeroAttachedGas";
    let shown: Vec<(u64, String, Why)> =
        shapes.iter().map(|r| (*r.range.start(), r.key.to_string(), r.why)).collect();
    let starved = "ok\n  split.near::split#0 ok\n  split.near::child#0 OutOfGas";
    let fed = "ok\n  split.near::split#0 ok\n  split.near::child#0 ok";
    let i = shown.iter().position(|r| r.1 == fed).unwrap_or_else(|| panic!("{shown:#?}"));
    assert_eq!(
        shown,
        [
            (tgas(1).as_gas(), "ok\n  split.near::split#0 ok".into(), Why::Grid),
            (RESERVE, format!("failed: {zero}\n  split.near::split#0 {zero}"), Why::Grid),
            (RESERVE + 4, starved.into(), Why::Grid),
            (shown[i].0, fed.into(), Why::Grid),
        ]
    );
}

/// The settlement's callback gets the unused gas by weight and starves
/// when the caller attaches just enough: `gas_edges` under
/// `OutcomeShape::of` finds the gas below which `on_pay` runs out, to 1 gas.
#[test]
fn the_gas_where_a_settlement_starves_is_exact_to_one() {
    let sim = world(wat("settle_starves.wat"));
    let starves = |out: &TxOutcome| {
        let on_pay = out.expect_receipt("on_pay");
        out.is_success() && matches!(on_pay.failure(), Some(Failure::OutOfGas))
    };
    let found = sim.clone().call(ALICE, SPLIT, "pay").gas_edges(
        tgas(1)..=tgas(300),
        tgas(10),
        OutcomeShape::of,
    );
    let i = (found.iter().position(|r| r.key.to_string().contains("on_pay#0 OutOfGas")))
        .unwrap_or_else(|| panic!("no starved region: {found:#?}"));
    let (starved, next) = (&found[i], &found[i + 1]);
    assert!(next.key.to_string().contains("on_pay#0 ok"), "{}", next.key);
    let edge = *next.range.start();
    assert_eq!(*starved.range.end() + 1, edge);
    let run = |gas| at(&sim, "pay", gas);
    assert!(starves(&run(edge - 1)), "{}", run(edge - 1));
    assert!(!starves(&run(edge)) && run(edge).is_success(), "{}", run(edge));
}

/// A key that holds the gas burnt changes every few gas where the child
/// runs out of what it was attached: the guard panics, naming two
/// neighbors, rather than bisect without end.
#[test]
fn a_key_with_gas_burnt_trips_the_guard() {
    let sim = world(wat("gas_split.wat"));
    let burnt = |out: &TxOutcome| {
        (out.is_success(), out.receipts.iter().map(|r| r.gas_burnt.as_gas()).sum::<u64>())
    };
    let from = Gas::from_gas(RESERVE + 1_000);
    let tx = sim.clone();
    let msg = panic_message(catch_unwind(AssertUnwindSafe(move || {
        let mut tx = tx;
        tx.call(ALICE, SPLIT, "split").gas_edges(from..=tgas(11), Gas::from_gigagas(100), burnt)
    })));
    assert!(msg.contains("moves with the knob") && msg.contains(" and "), "{msg}");
}

/// `edges` over a size: the longest log a call may write, to 1 byte, on
/// a contract that logs its input, read every 1,000 bytes.
#[test]
fn edges_find_a_size_limit_to_one_byte() {
    let logger = r#"(module
      (import "env" "input" (func $input (param i64)))
      (import "env" "read_register" (func $read_register (param i64 i64)))
      (import "env" "register_len" (func $register_len (param i64) (result i64)))
      (import "env" "log_utf8" (func $log_utf8 (param i64 i64)))
      (memory 1)
      (export "memory" (memory 0))
      (func (export "log")
        (call $input (i64.const 0))
        (call $read_register (i64.const 0) (i64.const 0))
        (call $log_utf8 (call $register_len (i64.const 0)) (i64.const 0))))"#;
    let sim = world(wat::parse_str(logger).unwrap());
    let limit = sim.config.runtime_config().wasm_config.limit_config.max_total_log_length;
    let found = edges(1..=40_000, 1_000, 64, |n| {
        let mut sim = sim.clone();
        sim.call(ALICE, SPLIT, "log").args(vec![b'x'; n as usize]).transact().is_success()
    });
    let found: Vec<_> = found.iter().map(shape).collect();
    assert_eq!(found, [(1..=limit, true, Why::Grid), (limit + 1..=40_000, false, Why::Grid)]);
}

/// `OutcomeShape::of` names each receipt by its site and count, a transfer
/// by its action, a failure by its variants and never its message, keeps
/// a deposit refund and what is pending, and no gas: two outcomes at
/// different gas with the same shape agree.
#[test]
fn an_outcome_shape_leaves_out_gas() {
    let twice = r#"(module
      (import "env" "current_account_id" (func $current_account_id (param i64)))
      (import "env" "read_register" (func $read_register (param i64 i64)))
      (import "env" "register_len" (func $register_len (param i64) (result i64)))
      (import "env" "input" (func $input (param i64)))
      (import "env" "panic_utf8" (func $panic_utf8 (param i64 i64)))
      (import "env" "promise_batch_create" (func $create (param i64 i64) (result i64)))
      (import "env" "promise_batch_action_function_call"
        (func $call (param i64 i64 i64 i64 i64 i64 i64)))
      (memory 1)
      (export "memory" (memory 0))
      (data (i32.const 0) "child")
      (func $child_call
        (call $call
          (call $create (call $register_len (i64.const 1)) (i64.const 128))
          (i64.const 5) (i64.const 0) (i64.const 0) (i64.const 0) (i64.const 64)
          (i64.const 5000000000000)))
      (func (export "twice")
        (call $current_account_id (i64.const 1))
        (call $read_register (i64.const 1) (i64.const 128))
        (call $child_call)
        (call $child_call))
      (func (export "child"))
      (func (export "fail")
        (call $input (i64.const 0))
        (call $read_register (i64.const 0) (i64.const 256))
        (call $panic_utf8 (call $register_len (i64.const 0)) (i64.const 256))))"#;
    let mut sim = world(wat::parse_str(twice).unwrap());
    sim.create_account("bob.near", Balance::from_near(1));
    let shape = |out: &TxOutcome| OutcomeShape::of(out).to_string();
    let out = sim.call(ALICE, SPLIT, "twice").gas(tgas(30)).transact();
    let more = sim.call(ALICE, SPLIT, "twice").gas(tgas(60)).transact();
    assert_eq!(OutcomeShape::of(&out), OutcomeShape::of(&more));
    assert_eq!(
        shape(&out),
        "ok\n  split.near::twice#0 ok\n  split.near::child#0 ok\n  split.near::child#1 ok"
    );
    let starved = (sim.call(ALICE, SPLIT, "twice").gas(tgas(30)))
        .inject(Receipts::method("child").nth(1), Fault::OutOfGas)
        .transact();
    assert!(shape(&starved).ends_with("child#1 OutOfGas, forced"), "{}", shape(&starved));
    let transfer = sim.tx(ALICE, "bob.near").transfer(Balance::from_near(1)).transact();
    assert_eq!(shape(&transfer), "ok\n  bob.near::Transfer#0 ok");
    // A call that fails with a deposit, and one still in flight.
    let mut split = world(wat("gas_split.wat"));
    let refused = split
        .call(ALICE, SPLIT, "split")
        .gas(Gas::from_gas(RESERVE))
        .deposit(Balance::from_near(1));
    let zero = "Action::NewReceiptValidationError::ActionsValidation::FunctionCallZeroAttachedGas";
    assert_eq!(
        shape(&refused.transact()),
        format!("failed: {zero}\n  split.near::split#0 {zero}\n  refund Deposit to alice.near")
    );
    // Two panics with different messages, and a method that doesn't
    // resolve, by its nested variants.
    let mut fail =
        |msg: &str| shape(&sim.call(ALICE, SPLIT, "fail").args(msg.as_bytes().to_vec()).transact());
    assert_eq!(fail("not enough"), "failed: Panic\n  split.near::fail#0 Panic");
    assert_eq!(fail("not 12345 enough at all"), fail("not enough"));
    let missing = "Action::FunctionCallError::MethodResolveError::MethodNotFound";
    assert_eq!(
        shape(&sim.call(ALICE, SPLIT, "nope").transact()),
        format!("failed: {missing}\n  split.near::nope#0 {missing}")
    );
    let hash = sim.call(ALICE, SPLIT, "twice").gas(tgas(30)).submit();
    sim.step();
    sim.step();
    let flight = sim.outcome(hash);
    assert!(
        shape(&flight).ends_with(&format!("\n  {} pending", flight.pending.len())),
        "{}",
        shape(&flight)
    );
    assert!(!flight.pending.is_empty(), "{flight}");
}
