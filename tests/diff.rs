//! `Harness::diff`: two builds on the same plans, only the differences.
//! The forgetful vault (`vault_forgetful.wasm`) keeps the tokens of a
//! withdraw whose payout failed; `vault.wasm` refunds the shares.
//!
//! Verification boundary: each side is the ordinary executor on actual
//! simulator runs; the diff only compares them. No schedule changes.

mod common;
mod explore_vault;

use explore_vault::{User, VAULT, VaultOp, ft};
use near_contract_sim::Sim;
use near_contract_sim::explore::{Amount, Books, Faults, Harness, Plan};
use serde_json::json;
use std::collections::BTreeMap;

const KEPT: &str = "vault tokens is kept, owed to no one";

fn harness(wasm: &str) -> Harness<VaultOp> {
    harness_on(explore_vault::vault_world(wasm))
}

fn harness_on(world: Sim) -> Harness<VaultOp> {
    let view = |sim: &Sim, m: &str| -> u128 { sim.view(VAULT, m, json!({})).u128() };
    let books = Books::new()
        .liability("vault shares", move |sim: &Sim| {
            BTreeMap::from([("all".into(), view(sim, "total_shares") - view(sim, "total_rewards"))])
        })
        .asset("vault tokens", |sim: &Sim| ft(sim, VAULT))
        .backs("vault shares", "vault tokens");
    Harness::new(world).books(books)
}

fn deposit_then_withdraw() -> Plan<VaultOp> {
    Plan::new()
        .op(VaultOp::Deposit { who: User::Alice, amount: Amount::Exact(100) })
        .op(VaultOp::Withdraw { who: User::Alice, amount: Amount::Exact(40) })
}

/// On the plain plan the builds agree; on the forgetful build's finding,
/// as it ran (its payout failure pinned), only the forgetful behaviour
/// differs: the kept tokens, the books, and nothing about the deposit.
#[test]
fn the_forgetful_vault_differs_only_where_it_forgets() {
    let (fixed, forgetful) = (harness("vault.wasm"), harness("vault_forgetful.wasm"));
    let found = forgetful.scan(&[deposit_then_withdraw()]).faults(Faults::tolerable()).run();
    let kept = found.assert_found(KEPT, "world").run.plan().clone();

    let diff = fixed.diff(&forgetful, &[deposit_then_withdraw(), kept]);
    assert_eq!(diff.plans.len(), 1, "{diff}");
    assert_eq!(diff.plans[0].plan, 1, "{diff}");
    let lines: Vec<&str> = diff.lines().collect();
    let kept_line = format!("finding {KEPT} at world: none -> forced");
    assert!(lines.contains(&kept_line.as_str()), "{diff}");
    let owed = "books vault shares owed since step 0: +100 -> +60";
    assert!(lines.iter().any(|l| l.starts_with(owed)), "{diff}");
    assert!(lines.iter().all(|l| !l.contains("ft_on_transfer")), "the deposit agrees: {diff}");
    assert!(diff.plans[0].steps[1].contains("forced"), "the pinned fault prints: {diff}");
}

/// A build diffed against itself reports nothing.
#[test]
fn a_build_against_itself_differs_in_nothing() {
    let h = harness("vault_forgetful.wasm");
    let found = h.scan(&[deposit_then_withdraw()]).faults(Faults::tolerable()).run();
    let plans: Vec<Plan<VaultOp>> = found.found().iter().map(|f| f.run.plan().clone()).collect();
    assert!(!plans.is_empty());
    let diff = h.diff(&harness("vault_forgetful.wasm"), &plans);
    assert!(diff.is_empty(), "{diff}");
}

/// A reading that differs on the fixtures (here a donation the vault
/// holds on one side) prints once, before any plan; a plan that moves it
/// the same way on both sides prints nothing for it.
#[test]
fn a_difference_at_the_fixture_prints_once() {
    let mut donated = explore_vault::vault_world("vault.wasm");
    let to_vault = json!({ "receiver_id": VAULT, "amount": "7" });
    let gift = donated.call("bob.near", explore_vault::TOKEN, "ft_transfer").args_json(to_vault);
    gift.deposit(near_contract_sim::Balance::from_yoctonear(1)).transact().assert_success();
    let (plain, donated) = (harness("vault.wasm"), harness_on(donated));
    let plans = [deposit_then_withdraw(), deposit_then_withdraw(), Plan::new()];
    let diff = plain.diff(&donated, &plans);
    assert_eq!(diff.fixture, ["books vault tokens held: 0 -> 7"], "{diff}");
    assert!(diff.plans.is_empty(), "{diff}");
    assert_eq!(diff.to_string().matches("vault tokens held").count(), 1, "{diff}");
}
