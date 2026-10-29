//! The batch specimen (`tests/res/batch.wat`): a keeper hands a batch of
//! `n` items to a worker on a fixed 20 Tgas budget, whatever `n` is, and
//! the worker's use grows with `n` (about 2.6 Tgas an item) until it runs
//! out, at 8 items: a budget the contract fixes, met by work an input
//! sizes. A curve along the batch's size finds the limit from a
//! batch of one, with nothing declared.

mod common;
use common::manifest_dir;
use near_contract_sim::explore::{Harness, Op, Plan, Reach, Resource, Skip};
use near_contract_sim::{Balance, Fixture, Gas, Sim};
use serde::{Deserialize, Serialize};

const KEEPER: &str = "keeper.near";
const WORKER: &str = "worker.near";
const ALICE: &str = "alice.near";
const BOB: &str = "bob.near";
const WORK: &str = "worker.near::work";
const TOUCH: &str = "worker.near::touch";

fn world() -> Sim {
    let wat = std::fs::read_to_string(manifest_dir().join("tests/res/batch.wat")).unwrap();
    let code = wat::parse_str(&wat).unwrap();
    Fixture::new()
        .account(ALICE, Balance::from_near(100))
        .account(BOB, Balance::from_near(100))
        .contract(KEEPER, code.clone())
        .contract(WORKER, code)
        .build()
}

// ---- everyday tests ----

/// The worker's use grows with the batch, at one budget: 7 items fit in
/// its 20 Tgas, 8 run out.
#[test]
fn a_batch_grows_the_workers_use_at_a_fixed_budget() {
    let mut sim = world();
    let mut used = vec![];
    for n in [1u64, 2, 4, 7, 8] {
        let out = sim.call(ALICE, KEEPER, "batch").args(n.to_le_bytes()).transact();
        let work = out.expect_receipt("work");
        assert_eq!(work.attached_gas, Gas::from_teragas(20), "{out}");
        used.push((n, work.gas_burnt, work.is_success()));
    }
    let tgas = |g: Gas| g.as_gas() as f64 / 1e12;
    let [(_, one, true), (_, two, true), (_, four, true), (_, _, true), (_, _, false)] = used[..]
    else {
        panic!("{used:?}");
    };
    assert!(tgas(two) - tgas(one) > 1.0 && tgas(four) > tgas(two), "{used:?}");
}

// ---- the audit harness ----

#[derive(Clone, Debug, Serialize, Deserialize)]
enum BatchOp {
    /// Alice's batch of `n` items.
    Batch(u64),
    /// `who` registers with the worker, through the keeper.
    Register(String),
    /// Alice calls the worker's `work(n)` herself, at the gas ceiling.
    Work(u64),
}

impl Op for BatchOp {
    fn submit(&self, sim: &mut Sim) -> Result<(), Skip> {
        match self {
            BatchOp::Batch(n) => sim.call(ALICE, KEEPER, "batch").args(n.to_le_bytes()).submit(),
            BatchOp::Register(who) => sim.call(who, KEEPER, "register").submit(),
            BatchOp::Work(n) => {
                let ceiling = sim.gas_ceiling();
                sim.call(ALICE, WORKER, "work").args(n.to_le_bytes()).gas(ceiling).submit()
            }
        };
        Ok(())
    }
}

fn harness() -> Harness<BatchOp> {
    Harness::new(world())
}

fn batches(sizes: &[u64]) -> Plan<BatchOp> {
    sizes.iter().fold(Plan::new(), |p, n| p.op(BatchOp::Batch(*n)))
}

/// From a batch of 1, the curve along the batch's size finds the worker's
/// own gas crossing its fixed 20 Tgas at 8 items, refined between 4 and 8,
/// with no declaration. Registering again, a branch under 1 Tgas apart,
/// leaves nothing open.
///
/// Verification boundary: which sizes run (the curve's ladder and
/// refinement), against actual simulator runs.
#[test]
fn a_batch_budget_crosses_along_its_size() {
    let audit = harness().curves(&[batches(&[1])]).run();
    let row = audit.assert_curve(WORK, Resource::ReceiptGas, "/Batch");
    assert_eq!(row.reach, Reach::Crossed { at: 8 }, "{row}");
    assert_eq!(row.limit, u128::from(Gas::from_teragas(20).as_gas()), "{row}");
    assert!(row.points.iter().any(|p| p.0 == 7), "refined to 7: {row}");

    let again = Plan::new().op(BatchOp::Register(BOB.into())).op(BatchOp::Register(BOB.into()));
    let registers = harness().curves(&[again]).run();
    let open: Vec<_> =
        registers.curves().iter().filter(|r| r.site == TOUCH && r.open().is_some()).collect();
    assert!(open.is_empty(), "{registers}");
}

/// A crossed row is a finding, with its replay, only where a check
/// fired in the crossing's own run; else it is listed as harmless. The
/// worker running out of gas is its caller's failed result, which no
/// built-in flags, so the plain curve's crossing is harmless; declared
/// `must_not_fail`, the same crossing is a finding whose replay the row
/// carries.
///
/// Verification boundary: a crossed row's verdict, over actual runs.
#[test]
fn a_crossing_is_a_finding_only_where_a_check_fired() {
    let plain = harness().curves(&[batches(&[1])]).run();
    let row = plain.assert_curve(WORK, Resource::ReceiptGas, "/Batch");
    assert_eq!(row.reach, Reach::Crossed { at: 8 }, "{row}");
    assert!(row.replay.is_none() && row.to_string().contains("harmless"), "{row}");
    assert!(plain.found().is_empty(), "{plain}");
    assert!(plain.to_string().contains("CROSSED at 8, harmless"), "{plain}");

    let judged = harness().must_not_fail(WORK).curves(&[batches(&[1])]).run();
    let row = judged.assert_curve(WORK, Resource::ReceiptGas, "/Batch");
    assert_eq!(row.reach, Reach::Crossed { at: 8 }, "{row}");
    let found = judged.assert_found(near_contract_sim::explore::builtin::MUST_NOT_FAIL, WORK);
    assert!(row.replay.is_some() && row.replay == found.replay, "{judged}");
    assert!(!row.to_string().contains("harmless"), "{row}");
}

/// A `PrepaidGas` row counts what the attached gas paid for: no refunds,
/// and not the entry's exec fees, which the signer prepaid on top. A
/// transaction that runs out of gas at the ceiling uses exactly its
/// limit, never more. The `work` call runs out in a step the curve doesn't
/// climb, so every point measures it.
///
/// Verification boundary: a curve row's use, over actual runs.
#[test]
fn a_transaction_out_of_gas_at_the_ceiling_uses_at_most_its_limit() {
    let witness = Plan::new().op(BatchOp::Batch(1)).op(BatchOp::Work(1000));
    let audit = harness().curve(&witness, 0, "/Batch").run();
    let row = audit.assert_curve(WORK, Resource::PrepaidGas, "/Batch");
    assert_eq!(row.limit, u128::from(world().gas_ceiling().as_gas()), "{row}");
    assert!(!row.points.is_empty() && row.points.iter().all(|p| p.1 <= row.limit), "{row}");
    assert!(row.points.iter().any(|p| p.1 == row.limit), "out of gas at the ceiling: {row}");
}
