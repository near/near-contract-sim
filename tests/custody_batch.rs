//! The custody-batch specimen (`tests/res/custody_batch.wat`):
//! a custody pays a withdraw out through a batch call on a fixed 20 Tgas
//! budget, the batch's work grows with its length (about 2.6 Tgas an
//! item), and a batch that runs out of gas loses the amount. A deposit
//! reaches the same callee with its own batch.
//!
//! The curve here runs against actual simulator behavior. The audit's
//! to-do gate for it is covered in tests/report.rs.

mod common;
use common::manifest_dir;
use near_contract_sim::explore::{
    Audit, Found, Harness, Label, Op, Plan, Reach, Resource, Skip, Status,
};
use near_contract_sim::{Balance, Fixture, Sim};
use serde::{Deserialize, Serialize};

const CUSTODY: &str = "custody.near";
const MT: &str = "mt.near";
const ALICE: &str = "alice.near";
const WORK: &str = "mt.near::work";
/// Alice's credit before any case.
const HELD: u64 = 1000;
const CHECK: &str = "alice's credit and payouts add up";

fn world() -> Sim {
    let wat = std::fs::read_to_string(manifest_dir().join("tests/res/custody_batch.wat")).unwrap();
    let code = wat::parse_str(&wat).unwrap();
    Fixture::new()
        .account(ALICE, Balance::from_near(100))
        .contract(CUSTODY, code.clone())
        .contract(MT, code)
        .setup("alice's credit", |sim| {
            sim.call(ALICE, CUSTODY, "fund").args(HELD.to_le_bytes()).transact().assert_success();
        })
        .build()
}

fn read(sim: &Sim, account: &str, method: &str) -> u64 {
    let bytes = sim.view_raw(account, method, ALICE.as_bytes().to_vec()).result.unwrap();
    u64::from_le_bytes(bytes.try_into().unwrap())
}

#[derive(Clone, Debug, Serialize, Deserialize)]
enum CustodyOp {
    /// Withdraws `amount`, paid out by a batch over `ids`.
    Withdraw { amount: u64, ids: Vec<u64> },
    /// A deposit's batch over `ids`: it pays nothing, and reaches the
    /// same callee.
    Deposit { ids: Vec<u64> },
}

impl Op for CustodyOp {
    fn submit(&self, sim: &mut Sim) -> Result<(), Skip> {
        match self {
            CustodyOp::Withdraw { amount, ids } => {
                let n = ids.len() as u64;
                let args = [amount.to_le_bytes(), n.to_le_bytes()].concat();
                sim.call(ALICE, CUSTODY, "withdraw").args(args).submit();
            }
            CustodyOp::Deposit { ids } => {
                let n = ids.len() as u64;
                sim.call(ALICE, CUSTODY, "deposit").args(n.to_le_bytes()).submit();
            }
        }
        Ok(())
    }
}

fn harness() -> Harness<CustodyOp> {
    harness_on(world())
}

fn harness_on(world: Sim) -> Harness<CustodyOp> {
    Harness::new(world).at_rest(CHECK, |sim: &Sim| {
        let (credit, paid) = (read(sim, CUSTODY, "credit_of"), read(sim, MT, "paid_of"));
        match credit + paid {
            HELD => Ok(()),
            n => Err(format!("alice holds {credit} credit and was paid {paid}, {n} of {HELD}")),
        }
    })
}

fn ids(n: u64) -> Vec<u64> {
    (1..=n).collect()
}

fn withdraw(n: u64) -> Plan<CustodyOp> {
    Plan::new().op(CustodyOp::Withdraw { amount: 10, ids: ids(n) })
}

// ---- everyday tests ----

/// Seven items fit the batch's budget and pay alice; eight run out of
/// gas, and the amount is gone though the transaction succeeds.
#[test]
fn a_long_batch_loses_the_withdrawal() {
    for (n, paid) in [(7, 10), (8, 0)] {
        let mut sim = world();
        let args = [10u64.to_le_bytes(), (n as u64).to_le_bytes()].concat();
        let out = sim.call(ALICE, CUSTODY, "withdraw").args(args).transact();
        out.assert_success();
        assert_eq!(read(&sim, CUSTODY, "credit_of"), HELD - 10, "{out}");
        assert_eq!(read(&sim, MT, "paid_of"), paid, "n {n}: {out}");
    }
}

// ---- the curve ----

/// From a one-item withdraw, the curve along `ids` finds the batch's own
/// gas crossing its fixed 20 Tgas at 8 items, with no declaration, and
/// the crossing shows naturally at the custody's check (the world). A
/// default scan of the same plan forces that check at the same site; the
/// two causes stay two findings in one store, the shown one and the
/// forced one, told apart by identity.
///
/// Verification boundary: the curve's ladder and finding identity, over
/// actual simulator runs.
#[test]
fn a_batch_withdraw_crosses_along_its_ids() {
    let dir = std::env::temp_dir().join(format!("ncs-custody-batch-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let audit = harness().store(&dir);
    let grown = audit.curves(&[withdraw(1)]).run();
    let row = grown.assert_curve(WORK, Resource::ReceiptGas, "/Withdraw/ids");
    assert_eq!(row.reach, Reach::Crossed { at: 8 }, "{row}");
    let shown = |a: &Audit<CustodyOp>| {
        let at_world = |f: &&Found<CustodyOp>| f.finding.check == CHECK && !f.finding.injected;
        a.found().iter().find(at_world).map(|f| f.finding.site.to_string())
    };
    assert_eq!(shown(&grown).as_deref(), Some("world"), "{grown}");

    // The withdraw as written shows nothing at the check, only forced;
    // mt.near pays whoever calls `work`, so that call sent directly shows
    // it, and the forced variants fold into that row.
    let scan = audit.scan(&[withdraw(1)]).run();
    let direct = Label::ShownAt(format!("{WORK} sent directly from axis.near"));
    let at_check = || scan.found().iter().filter(|f| f.finding.check == CHECK);
    assert!(at_check().all(|f| f.finding.injected || f.label == direct), "{scan}");
    let forced = at_check().map(|f| usize::from(f.finding.injected) + f.also.len()).sum::<usize>();
    assert!(forced >= 1, "{scan}");

    let replays = audit.clone().store(&dir).regressions();
    let at_check: Vec<_> = replays.iter().filter(|r| r.check == CHECK).collect();
    let shows = at_check.iter().filter(|r| matches!(r.status, Status::Shows(_))).count();
    assert!(
        at_check.len() >= 2 && shows == at_check.len(),
        "{:?}",
        replays.iter().map(|r| &r.path).collect::<Vec<_>>()
    );
    std::fs::remove_dir_all(&dir).unwrap();
}
