//! What a scan costs, and what keeps it bounded: an op's count tried at
//! the top a curve climbs to, not at its type's max; an edge that touches
//! neither gas nor a size run once, as the witness ran; one run per plan;
//! at-end checks judged once per case; and each variant's time in the
//! report. (A case past the timeout: `run.rs`'s unit tests.)
//!
//! Verification boundary: which variants a scan runs, over actual runs on
//! the pinned runtime.

mod common;

use near_contract_sim::explore::{Harness, Op, Plan, Skip};
use near_contract_sim::{Balance, Fixture, Sim};
use serde::{Deserialize, Serialize};
use std::time::{Duration, Instant};

/// alice pays bob `count` times, `tip` yocto each; or alice calls
/// `pay.near::pay`, `min_gas` unset by the witness; or `count` tips from
/// a world that holds 50 (`Held`: more skips).
#[derive(Clone, Debug, Serialize, Deserialize)]
enum Fan {
    Tips { count: u32, tip: u64 },
    Pay { amount: u64, min_gas: Option<u64> },
    Held { count: u32 },
}

impl Op for Fan {
    fn submit(&self, sim: &mut Sim) -> Result<(), Skip> {
        match *self {
            Fan::Tips { count, tip } => {
                for _ in 0..count {
                    sim.tx("alice.near", "bob.near")
                        .transfer(Balance::from_yoctonear(tip.into()))
                        .submit();
                }
            }
            Fan::Held { count } if count > 50 => return Err("the world holds 50".into()),
            Fan::Held { count } => Fan::Tips { count, tip: 1 }.submit(sim)?,
            Fan::Pay { amount, .. } => {
                sim.call("alice.near", "pay.near", "pay")
                    .args_json(serde_json::json!({ "amount": amount.to_string() }))
                    .submit();
            }
        }
        Ok(())
    }
}

fn world() -> Sim {
    let wasm = wat::parse_file(common::manifest_dir().join("tests/res/settle_starves.wat"));
    Fixture::new()
        .account("alice.near", Balance::from_near(1000))
        .account("bob.near", Balance::from_near(1))
        .contract("pay.near", wasm.unwrap())
        .build()
}

/// The op loops over `count`: a scan tries it at 0, 1 and the top a
/// curve climbs to (2048 from a witness of 1), never at `u32::MAX`, and
/// still tries `tip`, which only sets an amount, at its type's max.
#[test]
fn a_count_is_tried_at_a_curves_top_not_its_types_max() {
    let harness = Harness::<Fan>::new(world());
    let witness = Plan::new().op(Fan::Tips { count: 1, tip: 5 });
    let started = Instant::now();
    let audit = harness.scan(&[witness]).run();
    assert!(started.elapsed() < Duration::from_secs(120), "{:?}", started.elapsed());
    let names: Vec<String> = audit.report().timings.into_iter().map(|(n, _)| n).collect();
    let has = |n: &str| names.iter().any(|x| x == n);
    assert!(has("step 0, /Tips/count = 2048"), "{names:#?}");
    assert!(!names.iter().any(|n| n.contains(&u32::MAX.to_string())), "{names:#?}");
    assert!(has(&format!("step 0, /Tips/tip = {}", u64::MAX)), "{names:#?}");
}

/// A count the op skips at 99 (the world holds fewer) is still a size:
/// a probe that skips can't say a number isn't one, and at `u32::MAX` a
/// loop or an allocation never finishes. Tried at the curve's top.
#[test]
fn a_count_the_op_skips_past_is_still_bounded() {
    let harness = Harness::<Fan>::new(world());
    let audit = harness.scan(&[Plan::new().op(Fan::Held { count: 1 })]).run();
    let names: Vec<String> = audit.report().timings.into_iter().map(|(n, _)| n).collect();
    assert!(names.iter().any(|n| n == "step 0, /Held/count = 2048"), "{names:#?}");
    assert!(!names.iter().any(|n| n.contains(&u32::MAX.to_string())), "{names:#?}");
}

/// A number that touches neither gas nor a size runs once, as the
/// witness ran; one taken for gas (unset by the witness) also at the
/// least caller gas and the ceiling. A variant whose plan is the witness's
/// (300 Tgas as the witness signs it) doesn't run again.
#[test]
fn only_an_edge_that_touches_gas_or_a_size_is_crossed_with_the_callers_gas() {
    let harness = Harness::<Fan>::new(world());
    let pay = Plan::new().op(Fan::Pay { amount: 5, min_gas: None });
    let witness = pay.clone().gas(near_contract_sim::Gas::from_teragas(300));
    let audit = harness.scan(&[witness]).run();
    let names: Vec<String> = audit.report().timings.into_iter().map(|(n, _)| n).collect();
    let has = |n: &str| names.iter().any(|x| x == n);
    assert!(has("step 0, /Pay/amount = 0"), "{names:#?}");
    assert!(!names.iter().any(|n| n.starts_with("step 0, /Pay/amount = 0, caller")), "{names:#?}");
    assert!(has("step 0, /Pay/min_gas = 0, caller gas least + 0 gas"), "{names:#?}");
    assert!(has("step 0, /Pay/min_gas = 0, caller gas 1000.00 Tgas"), "{names:#?}");
    assert!(!has("step 0, caller gas 300.00 Tgas"), "the witness's own plan ran once: {names:#?}");
    assert!(has("step 0, caller gas 300.00 Tgas - 1 gas"), "{names:#?}");
    let mut sorted = names.clone();
    sorted.sort();
    sorted.dedup();
    assert_eq!(sorted.len(), names.len(), "each variant once");
    let times: Vec<f64> = audit.report().timings.iter().map(|t| t.1).collect();
    assert!(times.windows(2).all(|w| w[0] >= w[1]), "slowest first: {times:?}");
}

/// An at-end check is judged once per case, after everything settled; an
/// at-rest check at every rest point.
#[test]
fn an_at_end_check_is_judged_once_per_case() {
    let harness = Harness::<Fan>::new(world())
        .at_rest("rest", |_: &Sim| Ok::<(), String>(()))
        .at_end("end", |_: &Sim| Ok::<(), String>(()));
    let plan =
        Plan::new().op(Fan::Tips { count: 1, tip: 5 }).blocks(3).op(Fan::Tips { count: 1, tip: 5 });
    let run = harness.run(&plan);
    run.assert_clean();
    let judged = |name: &str| run.judged().iter().find(|j| j.check == name).unwrap().ok;
    assert_eq!(judged("end"), 1, "{run}");
    assert!(judged("rest") > 1, "{run}");
}
