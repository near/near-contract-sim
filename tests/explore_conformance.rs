//! `explore::Harness::run` must preserve direct simulator execution: a plan
//! of ops and block gaps reaches the same state root after every block, and
//! the same transaction outcomes, as submitting the same transactions and
//! stepping the same blocks by hand.
//!
//! Verification boundary: the executor against direct submission on the
//! pinned runtime.

mod common;

use std::collections::BTreeMap;

use common::{sim_with_users, status_wasm};
use near_contract_sim::explore::{Harness, Op, Plan, Skip, Verdict};
use near_contract_sim::{CryptoHash, Sim};
use proptest::prelude::*;
use proptest::test_runner::{Config, RngSeed, TestRunner};
use serde::{Deserialize, Serialize};
use serde_json::json;

/// alice (0) or bob (1) sets their status message.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct SetStatus {
    actor: u8,
    message: String,
}

impl SetStatus {
    fn submit_to(&self, sim: &mut Sim) {
        let signer = ["alice.near", "bob.near"][self.actor as usize];
        sim.call(signer, "status.near", "set_status")
            .args_json(json!({"message": self.message}))
            .submit();
    }
}

impl Op for SetStatus {
    fn submit(&self, sim: &mut Sim) -> Result<(), Skip> {
        self.submit_to(sim);
        Ok(())
    }
}

fn remember_block(sim: &mut Sim, states: &mut BTreeMap<u64, CryptoHash>) {
    sim.step();
    states.insert(sim.height(), sim.state_root());
}

#[test]
fn plans_match_direct_submission_and_block_execution() {
    let mut fixture = sim_with_users();
    fixture.deploy("status.near", status_wasm());
    let commands = prop::collection::vec((0u8..2, "[a-z]{0,12}", 0u16..=3), 1..=6);
    let mut runner = TestRunner::new(Config {
        cases: 12,
        rng_seed: RngSeed::Fixed(0xc0ffee),
        failure_persistence: None,
        ..Config::default()
    });
    runner
        .run(&commands, |commands| {
            let mut direct = fixture.clone();
            let mut states = BTreeMap::from([(direct.height(), direct.state_root())]);
            let mut plan = Plan::new();
            for (actor, message, blocks) in &commands {
                let op = SetStatus { actor: *actor, message: message.clone() };
                op.submit_to(&mut direct);
                plan = plan.op(op).blocks(*blocks);
                for _ in 0..*blocks {
                    remember_block(&mut direct, &mut states);
                }
            }
            while !direct.is_idle() {
                remember_block(&mut direct, &mut states);
            }
            direct.findings().assert_none();
            let final_height = direct.height();
            let expected_outcomes: Vec<_> =
                direct.txs().iter().map(|tx| format!("{:?}", direct.outcome(tx))).collect();
            let run = Harness::new(fixture.clone())
                .always("same state as direct execution", move |sim: &Sim| {
                    if states.get(&sim.height()) == Some(&sim.state_root()) {
                        Ok(())
                    } else {
                        Err(format!("state diverged at block {}", sim.height()))
                    }
                })
                .at_rest("same complete outcomes as direct execution", move |sim: &Sim| {
                    if sim.height() != final_height {
                        return Ok(());
                    }
                    let actual: Vec<_> =
                        sim.txs().iter().map(|tx| format!("{:?}", sim.outcome(tx))).collect();
                    if actual == expected_outcomes {
                        Ok(())
                    } else {
                        Err("transaction outcomes diverged".to_owned())
                    }
                })
                .run(&plan);
            prop_assert_eq!(run.verdict(), Verdict::Clean, "{}", run);
            prop_assert_eq!(run.blocks(), final_height - fixture.height(), "{}", run);
            Ok(())
        })
        .unwrap();
}
