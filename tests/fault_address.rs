//! Fault addresses that port between builds: a scan names the receipt it
//! starves by receiver, method and occurrence (`Receipts::occurrence`),
//! not by position, so its replay still starves the same receipt on a
//! build of `refund_ledger.wat` whose `send` first makes another call.
//! By position, it would land on that other call instead.
//!
//! Verification boundary: where a saved replay's fault lands, over actual
//! runs of two builds on the pinned runtime.

mod common;
use common::manifest_dir;
use near_contract_sim::explore::{Faults, Harness, Op, Plan, Skip, Status};
use near_contract_sim::{Balance, Fault, Fixture, Receipts, Reply, Sim, Stub};
use serde::{Deserialize, Serialize};

const LEDGER: &str = "ledger.near";
const RECEIVER: &str = "receiver.near";
const SETTLES: &str = "a refused send comes back";

/// The ledger, as written or with `send` first calling its own
/// `credit_of` (an extra receipt, earlier in the tree).
fn world(extra: bool) -> Sim {
    let mut wat =
        std::fs::read_to_string(manifest_dir().join("tests/res/refund_ledger.wat")).unwrap();
    if extra {
        let data = "(data (i32.const 16) \"resolve\")";
        let more =
            "\n  (data (i32.const 1300) \"ledger.near\")\n  (data (i32.const 1320) \"credit_of\")";
        let call = "(local.set $p (call $promise_create (local.get $to) (i64.const 1032)";
        let first = "(drop (call $promise_create (i64.const 11) (i64.const 1300) (i64.const 9) \
                     (i64.const 1320) (i64.const 0) (i64.const 0) (i64.const 64) \
                     (i64.const 5000000000000)))\n    ";
        assert!(wat.contains(data) && wat.contains(call));
        wat = wat.replace(data, &format!("{data}{more}")).replace(call, &format!("{first}{call}"));
    }
    let fund = |sim: &mut Sim| {
        sim.call("alice.near", LEDGER, "fund")
            .args(1000u64.to_le_bytes())
            .transact()
            .assert_success();
    };
    Fixture::new()
        .account("alice.near", Balance::from_near(100))
        .contract(LEDGER, wat::parse_str(&wat).unwrap())
        .stub(RECEIVER, Stub::new().on("on_receive", Reply::json("10")))
        .setup("fund alice", fund)
        .build()
}

/// alice sends 10 to the receiver.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct Send;

impl Op for Send {
    fn submit(&self, sim: &mut Sim) -> Result<(), Skip> {
        let args = [&10u64.to_le_bytes()[..], RECEIVER.as_bytes()].concat();
        sim.call("alice.near", LEDGER, "send").args(args).submit();
        Ok(())
    }
}

/// The receiver asks all 10 back, so alice ends where she began.
fn harness(extra: bool) -> Harness<Send> {
    Harness::new(world(extra)).at_rest(SETTLES, |sim: &Sim| {
        let bytes = sim.view_raw(LEDGER, "credit_of", b"alice.near".to_vec()).result.unwrap();
        match u64::from_le_bytes(bytes.try_into().unwrap()) {
            1000 => Ok(()),
            n => Err(format!("alice holds {n}")),
        }
    })
}

#[test]
fn a_replay_starves_the_same_receipt_on_a_build_with_an_extra_one() {
    let dir = std::env::temp_dir().join(format!("ncs-fault-address-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let starve = Faults::new(Receipts::method("resolve"), [Fault::OutOfGas]);
    let audit = harness(false).store(&dir).scan(&[Plan::new().op(Send)]).faults(starve).run();
    let found = audit.found().iter().find(|f| f.finding.check == SETTLES);
    let found = found.unwrap_or_else(|| panic!("{audit}"));
    let inject = &found.run.plan().steps[0].inject[0];
    assert_eq!(inject.at, Receipts::occurrence(LEDGER, "resolve", 0), "{audit}");
    assert_eq!(inject.at.to_string(), "Receipts::occurrence(\"ledger.near\", \"resolve\", 0)");

    // The saved replay, on the build with the extra receipt: still shows.
    let replays = harness(true).store(&dir).regressions();
    let at_check: Vec<_> = replays.iter().filter(|r| r.check == SETTLES).collect();
    assert!(!at_check.is_empty(), "{replays:?}");
    for r in &at_check {
        assert!(matches!(r.status, Status::Shows(_)), "{:?}: {:?}", r.path, r.status);
    }
    // By position, the original build's index lands on another receipt
    // of this one, and shows nothing.
    let n = index_of(&harness(false)).expect("a send calls resolve");
    let by_index = Faults::new(Receipts::index(n), [Fault::OutOfGas]);
    let moved = harness(true).scan(&[Plan::new().op(Send)]).faults(by_index).run();
    assert!(moved.found().iter().all(|f| f.finding.check != SETTLES), "{moved}");
    std::fs::remove_dir_all(&dir).unwrap();
}

/// The position of `resolve` among the receipts a send spawns.
fn index_of(h: &Harness<Send>) -> Option<usize> {
    let run = h.run(&Plan::new().op(Send));
    let out = run.outcomes().first()?;
    out.receipts.iter().skip(1).position(|r| r.method_names == ["resolve"])
}
