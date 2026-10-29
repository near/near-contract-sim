//! The keeper specimen (`tests/contracts/keeper`, built as
//! `tests/res/keeper.wasm`, `keeper_closed.wasm` and `mtoken.wasm`).
//!
//! A token (`mtoken.near`) transfers ids to the keeper, which credits them
//! to a receiver as entries of a per-account list keyed `token:id` and,
//! with a notify message, tells the receiver and runs `on_resolve`, which
//! removes each id refunded in full. `keeper.wasm` takes any caller as the
//! token; `keeper_closed.wasm` only `mtoken.near`. The token's ids are
//! made at init, so no op can make another.
//!
//! Verification boundary: the axes' plan rewrites (repeats settled before
//! a step, a tail sent a block apart while a callback is held), against
//! actual simulator behavior.

mod common;
use common::res;
use near_contract_sim::explore::{Axis, AxisKind, Harness, Op, Plan, Skip};
use near_contract_sim::{Balance, Fixture, Reply, Sim, Stub};
use serde::{Deserialize, Serialize};
use serde_json::json;

const KEEPER: &str = "keeper.near";
const TOKEN: &str = "mtoken.near";
const ISSUER: &str = "issuer.near";
const USERS: [&str; 2] = ["alice.near", "bob.near"];
const RECEIVER: &str = "r0.near";

/// Every amount unused: the keeper caps each at what was sent.
fn refund_all() -> Reply {
    Reply::json(vec![u128::MAX.to_string(); 64])
}

fn world(keeper: &str) -> Sim {
    let mut f = Fixture::new().account(ISSUER, Balance::from_near(1000));
    for u in USERS {
        f = f.account(u, Balance::from_near(1000));
    }
    f = f
        .stub(RECEIVER, Stub::new().on("on_transfer", refund_all()))
        .contract(TOKEN, res("mtoken.wasm"))
        .init("new")
        .args_json(json!({ "owner": ISSUER, "ids": ["gold", "silver"] }))
        .contract(KEEPER, res(keeper))
        .init("new")
        .args_json(json!({ "token": TOKEN }));
    for id in ["gold", "silver"] {
        for u in USERS {
            let args = json!({ "to": u, "id": id, "amount": "1000000" });
            f = f.call(ISSUER, TOKEN, "give").args_json(args);
        }
    }
    f.build()
}

/// A deposit of `ids` through the token, crediting `to`; with `notify`,
/// telling it.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct Deposit {
    who: String,
    ids: Vec<String>,
    to: String,
    notify: bool,
}

impl Op for Deposit {
    fn submit(&self, sim: &mut Sim) -> Result<(), Skip> {
        let msg = match self.notify {
            true => json!({ "receiver": self.to, "notify": "hi" }),
            false => json!({ "receiver": self.to }),
        };
        sim.call(&self.who, TOKEN, "batch_transfer_call")
            .args_json(json!({
                "receiver_id": KEEPER,
                "token_ids": self.ids,
                "amounts": vec!["1"; self.ids.len()],
                "msg": msg.to_string(),
            }))
            .submit();
        Ok(())
    }

    fn name(&self) -> String {
        "Deposit".into()
    }
}

fn witness(notify: bool) -> Plan<Deposit> {
    let ids = vec!["gold".into()];
    Plan::new().op(Deposit { who: USERS[0].into(), ids, to: RECEIVER.into(), notify })
}

fn entries(sim: &Sim) -> Vec<String> {
    let entries: Vec<(String, String)> =
        sim.view(KEEPER, "entries", json!({ "account": RECEIVER })).json();
    entries.into_iter().map(|(k, _)| k).collect()
}

fn axis(h: &Harness<Deposit>, plan: &Plan<Deposit>, name: &str) -> Axis {
    let axes = h.axes(plan);
    let found = axes.iter().find(|(i, a)| *i == 0 && a.name == name);
    found.unwrap_or_else(|| panic!("no axis {name}: {axes:#?}")).1.clone()
}

/// The deposit's axes: its arguments, the caller's gas, a repeat of the
/// op and of the keeper's `on_transfer` the token called, and, with
/// notify, a tail of that call while `on_resolve` is held.
#[test]
fn a_deposit_has_argument_repeat_and_tail_axes() {
    let h = Harness::new(world("keeper.wasm"));
    let names = |plan| -> Vec<(String, AxisKind)> {
        h.axes(&plan).into_iter().map(|(_, a)| (a.name, a.kind)).collect()
    };
    let quiet = names(witness(false));
    for (name, kind) in [
        ("/ids", AxisKind::Len),
        ("/ids/*", AxisKind::Bytes),
        ("caller gas", AxisKind::CallerGas),
        ("repeat Deposit (step 0)", AxisKind::Repeat),
        ("repeat keeper.near::on_transfer", AxisKind::Repeat),
    ] {
        assert!(quiet.contains(&(name.into(), kind)), "{name}: {quiet:#?}");
    }
    // Without notify, no `on_resolve` to hold; the token's own `resolve`
    // takes a tail of deposits.
    assert!(!quiet.iter().any(|(n, _)| n.contains("on_resolve")), "{quiet:#?}");
    let token = "Deposit (step 0) while mtoken.near::resolve is held";
    assert!(quiet.contains(&(token.into(), AxisKind::HoldTail)), "{quiet:#?}");
    let tail = "keeper.near::on_transfer while keeper.near::on_resolve is held";
    assert!(names(witness(true)).contains(&(tail.into(), AxisKind::HoldTail)));
}

/// The keeper's `on_transfer`, as the token called it, repeated
/// 64 times from a signer of its own before the deposit: 64 fresh ids,
/// each credited (the open keeper takes any caller as the token), then
/// the deposit's own. The closed keeper refuses every copy.
#[test]
fn repeat_through_a_direct_on_transfer_builds_64_fresh_ids() {
    let h = Harness::new(world("keeper.wasm"));
    let plan = witness(false);
    let repeat = axis(&h, &plan, "repeat keeper.near::on_transfer");
    let grown = h.moved(&plan, 0, &repeat, 64).expect("it rebuilds");
    assert_eq!(grown.steps.len(), 65);
    let run = h.run(&grown);
    run.assert_clean();
    let keys = entries(run.world::<Sim>().unwrap());
    assert_eq!(keys.len(), 65, "{keys:?}");
    for k in 0..64 {
        assert!(keys.contains(&format!("axis.near:gold-{k}")), "{k}: {keys:?}");
    }
    assert!(keys.contains(&"mtoken.near:gold".to_owned()), "{keys:?}");

    let closed = Harness::new(world("keeper_closed.wasm"));
    let run = closed.run(&closed.moved(&plan, 0, &repeat, 64).unwrap());
    let keys = entries(run.world::<Sim>().unwrap());
    assert_eq!(keys, ["mtoken.near:gold"], "{run}");
}

/// A tail of 3 direct `on_transfer`s sent while the deposit's
/// `on_resolve` is held lands between the deposit's `on_transfer` and its
/// `on_resolve`; a list moved to 4 grows the amounts beside it, with
/// fresh ids the token doesn't have, so the token refuses it.
#[test]
fn a_tail_lands_between_the_call_and_its_held_callback() {
    let h = Harness::new(world("keeper.wasm"));
    let plan = witness(true);
    let tail = axis(&h, &plan, "keeper.near::on_transfer while keeper.near::on_resolve is held");
    let moved = h.moved(&plan, 0, &tail, 3).expect("it rebuilds");
    assert_eq!(moved.steps.len(), 4);
    let run = h.run(&moved);
    let height = |tx: usize, method: &str| {
        let r = run.outcomes()[tx].receipts.iter().find(|r| r.method_names == [method]);
        r.unwrap_or_else(|| panic!("{method} in tx {tx}: {}", run.trees())).block_height
    };
    let (called, resolved) = (height(0, "on_transfer"), height(0, "on_resolve"));
    for tx in 1..=3 {
        let at = height(tx, "on_transfer");
        assert!(called < at && at < resolved, "tx {tx} at {at}: {}", run.trees());
    }

    let len = axis(&h, &plan, "/ids");
    let four = h.moved(&plan, 0, &len, 4).unwrap();
    let Some(Deposit { ids, .. }) = four.steps[0].act.op() else { panic!() };
    assert_eq!(ids, &["gold", "gold-1", "gold-2", "gold-3"]);
    let run = h.run(&four);
    assert!(run.outcomes()[0].is_failure(), "{}", run.trees());
}
