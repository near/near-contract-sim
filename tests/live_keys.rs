//! Generation from the current state (`tests/res/claim_queue.wat`): a
//! redeem pays only a claim queued under its id. An op that draws the id
//! never names a queued claim; one that draws a `Pick` and resolves it
//! when it submits, against the ids queued in the world then, always
//! does. A search reaches the redeem path that way within 64 cases.
//!
//! Verification boundary: plan generation and resolution against the
//! world at submit, over actual runs of the specimen.

mod common;
use common::manifest_dir;
use near_contract_sim::explore::input::{self, Pick};
use near_contract_sim::explore::{Harness, Op, OpDone, Skip, plans};
use near_contract_sim::{Balance, Fixture, Sim};
use proptest::prelude::*;
use serde::{Deserialize, Serialize};

const QUEUE: &str = "queue.near";
const REDEEMED: &str = "no claim is ever redeemed";

fn world() -> Sim {
    let wat = std::fs::read_to_string(manifest_dir().join("tests/res/claim_queue.wat")).unwrap();
    Fixture::new()
        .account("alice.near", Balance::from_near(10))
        .contract(QUEUE, wat::parse_str(&wat).unwrap())
        .build()
}

/// The ids queued now: those counted and not yet redeemed.
fn queued(sim: &Sim) -> Vec<u64> {
    let count = sim.view_raw(QUEUE, "count", vec![]).result.unwrap();
    let n = u64::from_le_bytes(count.try_into().unwrap());
    let key = |id: u64| [&b"q"[..], &id.to_le_bytes()].concat();
    (1..=n).filter(|&id| sim.state(QUEUE, &key(id)).is_some()).collect()
}

/// Queue a claim; redeem one by a drawn id; or redeem one of those
/// queued, picked when it submits.
#[derive(Clone, Debug, Serialize, Deserialize)]
enum QueueOp {
    Request,
    RedeemId(u64),
    Redeem(Pick),
}

impl Op for QueueOp {
    fn submit(&self, sim: &mut Sim) -> Result<(), Skip> {
        let id = match self {
            QueueOp::Request => {
                sim.call("alice.near", QUEUE, "request").submit();
                return Ok(());
            }
            QueueOp::RedeemId(id) => *id,
            QueueOp::Redeem(pick) => *pick.of(&queued(sim))?,
        };
        sim.call("alice.near", QUEUE, "redeem").args(id.to_le_bytes()).submit();
        Ok(())
    }
}

/// Fires once a redeem succeeds: the redeem path reached.
fn harness() -> Harness<QueueOp> {
    Harness::new(world()).per_op(REDEEMED, |d: &OpDone<QueueOp>| {
        let paid = d.entries().iter().any(|r| r.is_success() && r.method_names == ["redeem"]);
        match paid {
            true => Err("a claim was redeemed"),
            false => Ok(()),
        }
    })
}

#[test]
fn a_pick_resolved_against_the_world_reaches_the_redeem_path() {
    let request = Just(QueueOp::Request);
    let live = prop_oneof![request.clone(), input::pick().prop_map(QueueOp::Redeem)];
    let audit = harness().search(plans(live).len(1..=4)).seed(1).cases(64).run();
    audit.assert_found(REDEEMED, "Redeem");

    // Drawn ids: a redeem never names a queued claim.
    let drawn = prop_oneof![request, any::<u64>().prop_map(QueueOp::RedeemId)];
    let audit = harness().search(plans(drawn).len(1..=4)).seed(1).cases(64).run();
    assert!(audit.found().iter().all(|f| f.finding.check != REDEEMED), "{audit}");
}
