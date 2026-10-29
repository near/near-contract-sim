//! Typed state patching on the token vault (`tests/res/vault.wasm`):
//! `contract_state` and `update_contract_state` on the root struct, and a
//! collection entry written with `set_state` under near-sdk's key.

mod common;
use borsh::{BorshDeserialize, BorshSerialize};
use common::{panic_message, res};
use near_contract_sim::{Balance, Fixture, Sim};
use serde_json::json;
use std::panic::{AssertUnwindSafe, catch_unwind};

const TOKEN: &str = "token.near";
const VAULT: &str = "vault.near";

/// The vault's root struct as a test declares it: its fields in order, each
/// `LookupMap` as its prefix.
#[derive(Debug, PartialEq, BorshSerialize, BorshDeserialize)]
struct VaultState {
    token_id: String,
    shares: Vec<u8>,
    total_shares: u128,
    total_rewards: u128,
    last_claim: Vec<u8>,
}

/// alice has deposited 100 tokens, so she holds 100 shares.
fn world() -> Sim {
    let mut sim = Fixture::new()
        .account("alice.near", Balance::from_near(10))
        .contract(TOKEN, res("fungible_token.wasm"))
        .init("new_default_meta")
        .args_json(json!({ "owner_id": TOKEN, "total_supply": "2000" }))
        .contract(VAULT, res("vault.wasm"))
        .init("new")
        .args_json(json!({ "token_id": TOKEN }))
        .ft_register(TOKEN, [VAULT, "alice.near"])
        .ft_transfer(TOKEN, TOKEN, "alice.near", 1000)
        .build();
    sim.call("alice.near", TOKEN, "ft_transfer_call")
        .args_json(json!({ "receiver_id": VAULT, "amount": "100", "msg": "" }))
        .deposit(Balance::from_yoctonear(1))
        .transact()
        .assert_success();
    sim
}

fn u128_view(sim: &Sim, method: &str, args: serde_json::Value) -> u128 {
    sim.view(VAULT, method, args).u128()
}

#[test]
fn contract_state_reads_the_root_struct() {
    let sim = world();
    let state: VaultState = sim.contract_state(VAULT);
    assert_eq!(state.token_id, TOKEN);
    assert_eq!((state.total_shares, state.total_rewards), (100, 0));
    assert_eq!((state.shares, state.last_claim), (b"s".to_vec(), b"c".to_vec()));
}

#[test]
fn update_contract_state_is_what_the_contract_reads() {
    let mut sim = world();
    sim.update_contract_state(VAULT, |s: &mut VaultState| s.total_shares += 1);
    assert_eq!(u128_view(&sim, "total_shares", json!({})), 101);
}

/// `update_contract_state` is `set_state` of the re-encoded struct: the same
/// trie, the same `storage_usage`, which moves with the length.
#[test]
fn update_contract_state_keeps_storage_usage_as_set_state_does() {
    let base = world();
    let usage = |sim: &Sim| sim.account(VAULT).unwrap().storage_usage;
    let longer = |s: &mut VaultState| {
        s.total_shares += 1;
        s.token_id = "a-much-longer-token.near".into();
    };

    let mut typed = base.clone();
    typed.update_contract_state(VAULT, longer);

    let mut raw = base.clone();
    let mut state = VaultState::try_from_slice(&raw.state(VAULT, b"STATE").unwrap()).unwrap();
    longer(&mut state);
    raw.set_state(VAULT, b"STATE", Some(&borsh::to_vec(&state).unwrap()));

    assert_eq!(usage(&typed), usage(&raw));
    assert_eq!(typed.state_root(), raw.state_root());
    let grown = "a-much-longer-token.near".len() - TOKEN.len();
    assert_eq!(usage(&typed), usage(&base) + grown as u64);
}

#[test]
fn a_missing_state_panics_naming_the_type() {
    let sim = world();
    let msg = panic_message(catch_unwind(AssertUnwindSafe(|| {
        sim.contract_state::<VaultState>("alice.near");
    })));
    assert!(msg.contains("alice.near has no \"STATE\" (0 bytes)"), "{msg}");
    assert!(msg.contains("VaultState"), "{msg}");
}

#[test]
fn an_undecodable_state_panics_with_its_length() {
    let sim = world();
    let len = sim.state(VAULT, b"STATE").unwrap().len();
    let msg = panic_message(catch_unwind(AssertUnwindSafe(|| {
        sim.contract_state::<(String, u8)>(VAULT);
    })));
    assert!(msg.contains(&format!("vault.near's \"STATE\" ({len} bytes)")), "{msg}");
    assert!(msg.contains("(alloc::string::String, u8)"), "{msg}");
}

/// A collection entry isn't in the root: near-sdk's `store::LookupMap`
/// keeps it under its prefix followed by the borsh-encoded key, its value
/// borsh-encoded too.
#[test]
fn a_lookup_map_entry_is_set_state_under_near_sdk_s_key() {
    let mut sim = world();
    let key = [b"s".as_slice(), &borsh::to_vec("bob.near").unwrap()].concat();
    sim.set_state(VAULT, &key, Some(&borsh::to_vec(&500u128).unwrap()));
    assert_eq!(u128_view(&sim, "shares_of", json!({ "account_id": "bob.near" })), 500);
    // The root's total is separate, and unchanged: keep it in step if the
    // contract relies on it.
    assert_eq!(u128_view(&sim, "total_shares", json!({})), 100);
}
