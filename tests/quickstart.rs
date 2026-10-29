//! The everyday quickstart (`skills/near-contract-sim/references/quickstart.md`):
//! near-sdk's fungible token and a vault that holds it (`tests/contracts/vault`),
//! tested as unit tests would test them, with real cross-contract calls.
//!
//! Verification boundary: the docs (`tests/docs.rs` finds this file's
//! lines in the pages), run as tests over the pinned runtime.

use borsh::BorshDeserialize;
use near_contract_sim::Token;
use near_contract_sim::prelude::*;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::time::Duration;

const TOKEN: &str = "token.near";
const VAULT: &str = "vault.near";
const ALICE: &str = "alice.near";

/// A wasm `cargo near build` wrote to `tests/res/`.
fn wasm(name: &str) -> Vec<u8> {
    let path = format!("{}/tests/res/{name}", env!("CARGO_MANIFEST_DIR"));
    std::fs::read(&path).unwrap_or_else(|e| panic!("{path}: {e}: build the contract first"))
}

/// Built once per test binary, cloned per test.
fn world() -> Sim {
    Fixture::new()
        .account(ALICE, Balance::from_near(10))
        .contract(TOKEN, wasm("fungible_token.wasm"))
        .init("new_default_meta")
        .args_json(json!({ "owner_id": TOKEN, "total_supply": "1000" }))
        .contract(VAULT, wasm("vault.wasm"))
        .init("new")
        .args_json(json!({ "token_id": TOKEN }))
        .ft_register(TOKEN, [VAULT, ALICE])
        .ft_transfer(TOKEN, TOKEN, ALICE, 1000)
        .token(Token::nep141(TOKEN))
        .token(Token::view(VAULT, "shares_of"))
        .build()
}

/// A near-sdk `U128` view of one account.
fn u128_view(sim: &Sim, id: &str, method: &str, account_id: &str) -> u128 {
    sim.view(id, method, json!({ "account_id": account_id })).u128()
}

/// `withdraw`'s arguments, as the contract declares them.
#[derive(Serialize)]
struct Withdraw {
    amount: String,
}

/// One helper per method. Each returns the `Tx`, so a test can add a
/// fault or gas before it sends.
fn withdraw<'a>(sim: &'a mut Sim, who: &str, amount: u128) -> Tx<'a> {
    sim.call(who, VAULT, "withdraw")
        .args_json(Withdraw { amount: amount.to_string() })
        .deposit(Balance::from_yoctonear(1))
}

/// A deposit is the token's `ft_transfer_call` to the vault.
fn deposit<'a>(sim: &'a mut Sim, who: &str, amount: u128) -> Tx<'a> {
    sim.call(who, TOKEN, "ft_transfer_call")
        .args_json(json!({ "receiver_id": VAULT, "amount": amount.to_string(), "msg": "" }))
        .deposit(Balance::from_yoctonear(1))
}

#[test]
fn deposit_then_withdraw() {
    let mut sim = world();
    let d = deposit(&mut sim, ALICE, 100).transact();
    d.assert_success();
    assert_eq!(d.effects().token_net(TOKEN, ALICE), Ok(Delta::loss(100)));
    assert_eq!(d.effects().token_net(VAULT, ALICE), Ok(Delta::gain(100))); // shares

    let w = withdraw(&mut sim, ALICE, 40).transact();
    assert_eq!(w.u128(), 40, "{w}");
    assert_eq!(u128_view(&sim, TOKEN, "ft_balance_of", ALICE), 940);
    assert_eq!(u128_view(&sim, VAULT, "shares_of", ALICE), 60);
    sim.findings().assert_none();
}

#[test]
fn a_refused_withdraw_names_its_reason() {
    let mut sim = world();
    let out = withdraw(&mut sim, ALICE, 1).transact();
    assert_eq!(out.assert_failure(), Failure::Panic("not enough shares".into()));
}

/// Only the token may call `ft_on_transfer` (migrating.md): a call from
/// anyone else panics, matched by a fragment as `#[should_panic(expected =
/// ..)]` matches; one delivered from the token deposits.
#[test]
fn only_the_token_deposits() {
    let mut sim = world();
    let args = json!({ "sender_id": ALICE, "amount": "10", "msg": "" });
    let out = sim.call(ALICE, VAULT, "ft_on_transfer").args_json(&args).transact();
    out.assert_panic_contains("only the vault's token");
    let out =
        sim.call(ALICE, VAULT, "ft_on_transfer").args_json(&args).predecessor(TOKEN).transact();
    assert_eq!(out.u128(), 0, "{out}"); // the vault keeps all 10
    assert_eq!(u128_view(&sim, VAULT, "shares_of", ALICE), 10);
}

/// The payout fails, and the callback `on_withdraw` must give the shares
/// back.
#[test]
fn a_failed_payout_gives_the_shares_back() {
    let mut sim = world();
    deposit(&mut sim, ALICE, 100).transact().assert_success();
    let w = withdraw(&mut sim, ALICE, 40)
        .inject(Receipts::callback("on_withdraw"), Fault::PromiseFailed)
        .transact();
    assert_eq!(w.u128(), 0, "{w}");
    assert_eq!(w.effects().token_net(TOKEN, ALICE), Ok(Delta::ZERO));
    assert_eq!(w.effects().token_net(VAULT, ALICE), Ok(Delta::ZERO));
    sim.findings().assert_none();
}

/// `transact()` settles everything, so it would release the hold at once.
/// `submit()` and `run_until` stop while the payout is held, and a second
/// withdraw pays out first.
#[test]
fn a_withdraw_lands_while_a_payout_is_held() {
    let mut sim = world();
    deposit(&mut sim, ALICE, 100).transact().assert_success();
    let first = withdraw(&mut sim, ALICE, 40)
        .inject(Receipts::method("ft_transfer"), Fault::Hold(5))
        .submit();
    sim.run_until(10, |s| s.outcome(first).receipt("withdraw").is_some()).unwrap();
    let second = withdraw(&mut sim, ALICE, 60).transact(); // settles the held one too
    let first = sim.outcome(first);
    let paid_at = |out: &TxOutcome| out.expect_receipt("ft_transfer").block_height;
    assert!(paid_at(&second) < paid_at(&first), "{first}\n{second}");
    assert_eq!(u128_view(&sim, TOKEN, "ft_balance_of", ALICE), 1000);
}

/// `claim_rewards` pays once a day.
#[test]
fn rewards_once_a_day() {
    let mut sim = world();
    let claim = |sim: &mut Sim| sim.call(ALICE, VAULT, "claim_rewards").transact();
    claim(&mut sim).assert_success();
    assert!(claim(&mut sim).is_failure());
    sim.advance_by(Duration::from_secs(24 * 60 * 60));
    claim(&mut sim).assert_success();
    assert_eq!(u128_view(&sim, VAULT, "shares_of", ALICE), 20); // two claims of 10 shares
}

#[derive(Deserialize)]
struct Metadata {
    symbol: String,
    decimals: u8,
}

#[derive(BorshDeserialize)]
struct VaultState {
    token_id: String,
    _shares: Vec<u8>,
    total_shares: u128,
    _total_rewards: u128,
    _last_claim: Vec<u8>,
}

#[test]
fn typed_views_and_state() {
    let mut sim = world();
    deposit(&mut sim, ALICE, 100).transact().assert_success();
    let meta: Metadata = sim.view(TOKEN, "ft_metadata", ()).json();
    assert_eq!((meta.symbol.as_str(), meta.decimals), ("EXAMPLE", 24));
    let state: VaultState = sim.contract_state(VAULT);
    assert_eq!((state.token_id.as_str(), state.total_shares), (TOKEN, 100));
}

/// `testing_env!`'s `promise_results`: the callback alone, on a result the
/// test picks. It runs as the vault calling itself, so `#[private]` passes.
#[test]
fn the_callback_on_a_failed_result() {
    let mut sim = world();
    let out = sim
        .call(ALICE, VAULT, "on_withdraw")
        .args_json(json!({ "account_id": ALICE, "amount": "40" }))
        .promise_results([PromiseResult::Failed])
        .transact();
    assert_eq!(out.u128(), 0);
    assert_eq!(u128_view(&sim, VAULT, "shares_of", ALICE), 40);
}
