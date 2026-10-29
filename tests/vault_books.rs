//! `Books` on the vault: `vault_forgetful.wasm` keeps the tokens of a
//! withdraw whose payout failed and burns the shares anyway, so "kept,
//! owed to no one" fires there and not on `vault.wasm`; a key consumed
//! twice is found; a reader that panics on the fixture is refused.
//! The docs quote this file (`harness.md`, `explore.md`).
//!
//! Verification boundary: the derived checks judge actual runs.

mod common;
mod explore_vault;

use explore_vault::{User, VAULT, VaultOp, ft};
use near_contract_sim::explore::{Amount, Books, Faults, Harness, Plan};
use near_contract_sim::{Fault, Receipts, Sim};
use serde_json::json;
use std::collections::BTreeMap;

fn vault_books() -> Books<VaultOp> {
    let view = |sim: &Sim, m: &str| -> u128 { sim.view(VAULT, m, json!({})).u128() };
    Books::new()
        .liability("vault shares", move |sim: &Sim| {
            // Reward shares are unbacked by design.
            BTreeMap::from([("all".into(), view(sim, "total_shares") - view(sim, "total_rewards"))])
        })
        .asset("vault tokens", |sim: &Sim| ft(sim, VAULT))
        .backs("vault shares", "vault tokens")
}

fn deposit_then_withdraw() -> Plan<VaultOp> {
    Plan::new()
        .op(VaultOp::Deposit { who: User::Alice, amount: Amount::Exact(100) })
        .op(VaultOp::Withdraw { who: User::Alice, amount: Amount::Exact(40) })
}

#[test]
fn the_forgetful_vault_keeps_tokens_owed_to_no_one() {
    const KEPT: &str = "vault tokens is kept, owed to no one";
    let scan = |wasm| {
        Harness::new(explore_vault::vault_world(wasm))
            .books(vault_books())
            .scan(&[deposit_then_withdraw()])
            .faults(Faults::tolerable())
            .run()
    };
    let forgetful = scan("vault_forgetful.wasm");
    let found = forgetful.assert_found(KEPT, "world");
    assert!(found.finding.message.contains("40 kept"), "{forgetful}");
    let fixed = scan("vault.wasm");
    assert!(fixed.found_at(KEPT, "world").is_none(), "{fixed}");
}

/// Claiming rewards twice is a second use of one key.
#[test]
fn a_key_consumed_twice_is_found() {
    let books = vault_books().once("rewards claim", |d| match d.op() {
        VaultOp::Claim { who } => Some(who.id().to_owned()),
        _ => None,
    });
    let harness = Harness::new(explore_vault::vault_world("vault.wasm")).books(books);
    let once = Plan::new().op(VaultOp::Claim { who: User::Alice });
    let twice = once.clone().op(VaultOp::Claim { who: User::Alice });
    assert!(harness.run(&once).findings().iter().all(|f| f.check != "rewards claim at most once"));
    harness.run(&twice).assert_found("rewards claim at most once", "alice.near");
}

/// A reader that panics on the fixture panics at declaration, naming it.
#[test]
#[should_panic(expected = "Books: broken on the fixture")]
fn a_reader_that_panics_is_refused_at_declaration() {
    let books = Books::<VaultOp>::new()
        .liability("owed", |_: &Sim| BTreeMap::new())
        .asset("broken", |_: &Sim| panic!("no such view"))
        .backs("owed", "broken");
    let _ = Harness::new(explore_vault::vault_world("vault.wasm")).books(books);
}

/// One forced fault is one finding. Alice has left the token, so
/// the payout fails and `on_withdraw` must give her shares back; starving
/// it breaks the books (tokens kept for no one) and the declared
/// `must_not_fail`: one row, the other check in `also`, and either check
/// finds that row.
///
/// Verification boundary: how findings fold into rows, over actual runs.
#[test]
fn one_forced_fault_breaking_two_checks_is_one_finding() {
    let mut world = explore_vault::vault_world("vault.wasm");
    let left = |sim: &mut Sim| {
        let one = near_contract_sim::Balance::from_yoctonear(1);
        let all = json!({ "receiver_id": VAULT, "amount": "1000", "msg": "" });
        let token = explore_vault::TOKEN;
        sim.call("alice.near", token, "ft_transfer_call")
            .args_json(all)
            .deposit(one)
            .transact()
            .assert_success();
        let out = sim.call("alice.near", token, "storage_unregister").args_json(json!({}));
        out.deposit(one).transact().assert_success();
    };
    left(&mut world);
    let witness = Plan::new().op(VaultOp::Withdraw { who: User::Alice, amount: Amount::Exact(40) });
    let audit = Harness::new(world)
        .books(vault_books())
        .must_not_fail("on_withdraw")
        .scan(&[witness])
        .faults(Faults::new(Receipts::callback("on_withdraw"), [Fault::OutOfGas]))
        .run();
    assert_eq!(audit.found().len(), 1, "{audit}");
    let row = audit.assert_found("vault tokens is kept, owed to no one", "world");
    assert_eq!(row.also, ["must not fail at vault.near::on_withdraw (Withdraw)"], "{audit}");
    let folded = audit.assert_found(
        near_contract_sim::explore::builtin::MUST_NOT_FAIL,
        "vault.near::on_withdraw",
    );
    assert!(std::ptr::eq(row, folded), "{audit}");
    assert!(audit.to_string().contains("(+1 folded in)"), "{audit}");
}

/// A check whose every finding folded into another check's row has no
/// row of its own: `found()`'s rows alone say it never fired. Here one
/// forced fault breaks KEPT and a check of alice's deposit named as
/// backing, which ranks first (`report::kind` ranks a check named
/// `.. is backed by ..` with the books' backing); `found_at` finds KEPT
/// in that row's `folded`. On `vault.wasm` no check fires, in any row.
///
/// Verification boundary: the audit's API over actual runs; folds and
/// identities are unchanged.
#[test]
fn a_check_folded_into_another_row_is_found_there() {
    const KEPT: &str = "vault tokens is kept, owed to no one";
    const BACKED: &str = "alice's deposit is backed by the vault";
    let scan = |wasm| {
        Harness::new(explore_vault::vault_world(wasm))
            .books(vault_books())
            .at_rest(BACKED, |sim: &Sim| {
                let held = ft(sim, "alice.near") + explore_vault::shares(sim, "alice.near");
                if held >= 1000 { Ok(()) } else { Err(format!("alice holds {held} of her 1000")) }
            })
            .scan(&[deposit_then_withdraw()])
            .faults(Faults::tolerable())
            .run()
    };
    let audit = scan("vault_forgetful.wasm");
    assert!(audit.found().iter().all(|f| f.finding.check != KEPT), "no row of its own: {audit}");
    let row = audit.found_at(KEPT, "world").unwrap_or_else(|| panic!("{audit}"));
    assert_eq!(row.finding.check, BACKED, "{audit}");
    assert!(row.folded.iter().any(|f| f.check == KEPT), "{audit}");
    scan("vault.wasm").assert_clean();
}

/// alice deposits all her tokens and leaves the token: a withdraw's
/// payout to her then fails.
fn alice_left(wasm: &str) -> Sim {
    let mut world = explore_vault::vault_world(wasm);
    let one = near_contract_sim::Balance::from_yoctonear(1);
    let all = json!({ "receiver_id": VAULT, "amount": "1000", "msg": "" });
    let token = explore_vault::TOKEN;
    world
        .call("alice.near", token, "ft_transfer_call")
        .args_json(all)
        .deposit(one)
        .transact()
        .assert_success();
    let out = world.call("alice.near", token, "storage_unregister").args_json(json!({}));
    out.deposit(one).transact().assert_success();
    world
}

/// The books judge a raw call's effects as an op's: on the forgetful
/// vault a withdraw sent as `Plan::call` keeps the tokens of its failed
/// payout, and "kept, owed to no one" fires, as for the op. A plan of
/// raw calls alone is not clean where checks over ops judged nothing.
///
/// Verification boundary: which steps the derived and per-op checks
/// judge, over actual runs.
#[test]
fn a_raw_call_is_judged_by_the_books() {
    const KEPT: &str = "vault tokens is kept, owed to no one";
    let h = Harness::new(alice_left("vault_forgetful.wasm")).books(vault_books());
    let op = Plan::new().op(VaultOp::Withdraw { who: User::Alice, amount: Amount::Exact(40) });
    h.run(&op).assert_found(KEPT, "world");
    let call = near_contract_sim::explore::Call::new("alice.near", VAULT, "withdraw")
        .args_json(json!({ "amount": "40" }))
        .deposit(near_contract_sim::Balance::from_yoctonear(1));
    let run = h.run(&Plan::new().call(call));
    assert!(run.found_at(KEPT, "world").is_some_and(|f| f.message.contains("40 kept")), "{run}");
    let deposit = json!({ "receiver_id": VAULT, "amount": "100", "msg": "" });
    let deposit = near_contract_sim::explore::Call::new(
        "alice.near",
        explore_vault::TOKEN,
        "ft_transfer_call",
    )
    .args_json(deposit)
    .deposit(near_contract_sim::Balance::from_yoctonear(1));
    let per_op = explore_vault::harness(explore_vault::vault_world("vault.wasm"));
    let run = per_op.run(&Plan::new().call(deposit));
    assert_eq!(run.verdict(), near_contract_sim::explore::Verdict::Incomplete, "{run}");
    assert!(run.reason().unwrap_or_default().contains("raw call"), "{run}");
}
