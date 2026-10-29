//! `Fixture`: the vault world as data, built once per test binary and wasm;
//! a rebuilt contract rebuilds it, and a failing step says which.

mod common;
use common::{panic_message, res, staking_pool_wasm};
use near_contract_sim::{Balance, CryptoHash, Fixture, Gas, Reply, Sim, Stub};
use serde_json::json;
use std::sync::atomic::{AtomicUsize, Ordering};

const TOKEN: &str = "token.near";
const VAULT: &str = "vault.near";

/// Accounts, both contracts, storage registration and 1000 tokens each.
/// Built once per test binary and wasm; each call returns a fresh clone.
fn vault_world(vault_wasm: &str) -> Sim {
    Fixture::new()
        .account("alice.near", Balance::from_near(10))
        .account("bob.near", Balance::from_near(10))
        .contract(TOKEN, res("fungible_token.wasm"))
        .init("new_default_meta")
        .args_json(json!({ "owner_id": TOKEN, "total_supply": "2000" }))
        .contract(VAULT, res(vault_wasm))
        .init("new")
        .args_json(json!({ "token_id": TOKEN }))
        .ft_register(TOKEN, [VAULT, "alice.near", "bob.near"])
        .ft_transfer(TOKEN, TOKEN, "alice.near", 1000)
        .ft_transfer(TOKEN, TOKEN, "bob.near", 1000)
        .build()
}

fn ft(sim: &Sim, who: &str) -> u128 {
    sim.view(TOKEN, "ft_balance_of", json!({ "account_id": who })).u128()
}

#[test]
fn the_vault_world_has_its_accounts_and_tokens() {
    let mut sim = vault_world("vault.wasm");
    assert_eq!(sim.balance("alice.near"), Balance::from_near(10));
    assert_eq!([ft(&sim, "alice.near"), ft(&sim, "bob.near"), ft(&sim, TOKEN)], [1000, 1000, 0]);
    let registered = |who| sim.view(TOKEN, "storage_balance_of", json!({ "account_id": who }));
    assert!(!registered(VAULT).json::<serde_json::Value>().is_null());
    let total = sim.view(VAULT, "total_shares", json!({})).json::<String>();
    assert_eq!(total, "0");
    // A token account the fixture created starts with 100 NEAR.
    assert!(sim.balance(TOKEN) < Balance::from_near(100));
    // The world works: alice deposits into the vault.
    sim.call("alice.near", TOKEN, "ft_transfer_call")
        .args_json(json!({ "receiver_id": VAULT, "amount": "100", "msg": "" }))
        .deposit(Balance::from_yoctonear(1))
        .transact()
        .assert_success();
    assert_eq!(ft(&sim, VAULT), 100);
}

/// `Fixture::gas` sets the gas of the last init or call step, as a
/// deployment script attaches it, and keys the setup.
/// Without it, a step attaches the transaction's 300 Tgas.
///
/// Verification boundary: the gas a fixture step's transaction attaches,
/// against its actual receipt.
#[test]
fn a_fixture_step_attaches_the_gas_it_is_given() {
    let attached = |gas: Option<Gas>| {
        let fixture = Fixture::new()
            .contract(TOKEN, res("fungible_token.wasm"))
            .init("new_default_meta")
            .args_json(json!({ "owner_id": TOKEN, "total_supply": "2000" }));
        let sim = match gas {
            Some(g) => fixture.gas(g).build(),
            None => fixture.build(),
        };
        let init = sim.outcome(sim.txs().last().unwrap());
        init.expect_receipt("new_default_meta").attached_gas
    };
    assert_eq!(attached(Some(Gas::from_teragas(20))), Gas::from_teragas(20));
    assert_eq!(attached(None), Gas::from_teragas(300));
    let after_account = std::panic::catch_unwind(|| {
        Fixture::new().account("alice.near", Balance::from_near(1)).gas(Gas::from_teragas(20))
    });
    assert!(panic_message(after_account).contains("the last step, an init or a call"));
}

static RUNS: AtomicUsize = AtomicUsize::new(0);

/// The first build runs the steps, or loads them from disk if an earlier
/// run of this test binary saved them; the second clones the first.
#[test]
fn a_second_build_is_the_first_one() {
    let fixture = Fixture::new()
        .account("alice.near", Balance::from_near(10))
        .contract(VAULT, res("vault.wasm"))
        .init("new")
        .args_json(json!({ "token_id": TOKEN }))
        .setup("count runs", |_| {
            RUNS.fetch_add(1, Ordering::SeqCst);
        });
    let first = fixture.build();
    let runs = RUNS.load(Ordering::SeqCst);
    assert!(runs <= 1);
    let second = fixture.build();
    assert_eq!(RUNS.load(Ordering::SeqCst), runs, "the second build ran the steps again");
    assert_eq!(second.state_root(), first.state_root());
}

/// The key holds the wasm bytes: the same steps with a rebuilt contract,
/// here one with an extra custom section, give a fresh setup with the new
/// code, never the cached old one.
#[test]
fn changed_wasm_bytes_rebuild() {
    let plain = res("vault.wasm");
    // A custom section (id 0): 3 bytes, the name "x" and the payload "1".
    let rebuilt = [plain.as_slice(), &[0, 3, 1, b'x', b'1']].concat();
    let world = |wasm: &[u8]| {
        Fixture::new()
            .contract(VAULT, wasm)
            .init("new")
            .args_json(json!({ "token_id": TOKEN }))
            .build()
    };
    let code = |sim: &Sim| sim.account(VAULT).unwrap().code_hash;
    let (a, b) = (world(&plain), world(&rebuilt));
    assert_eq!(code(&a), CryptoHash::hash_bytes(&plain));
    assert_eq!(code(&b), CryptoHash::hash_bytes(&rebuilt));
    assert_ne!(a.state_root(), b.state_root());
}

/// `config` edits the `Sim`'s config before the first step, so every step
/// runs with it, and the key holds it: two epoch lengths, two worlds.
#[test]
fn config_comes_before_the_steps_and_is_keyed() {
    let world = |epoch_length: u64| {
        Fixture::new()
            .config(|c| c.epoch_length = epoch_length)
            .account("alice.near", Balance::from_near(10))
            .setup("sees the config", move |sim| assert_eq!(sim.config.epoch_length, epoch_length))
            .build()
    };
    assert_eq!(world(100).config.epoch_length, 100);
    assert_eq!(world(200).config.epoch_length, 200);
}

/// The hazard `Fixture::setup` documents: its key is the step's name and
/// where it is written, not what the closure captures, so the same line
/// with another captured wasm gets the first one's setup.
#[test]
fn a_setup_step_is_keyed_by_its_name() {
    let plain = res("vault.wasm");
    // A custom section (id 0): 3 bytes, the name "x" and the payload "1".
    let rebuilt = [plain.as_slice(), &[0, 3, 1, b'x', b'1']].concat();
    let world = |wasm: Vec<u8>| {
        Fixture::new()
            .account(VAULT, Balance::from_near(10))
            .setup("deploy the vault by hand", move |sim| sim.deploy(VAULT, wasm.clone()))
            .build()
    };
    let code = |sim: &Sim| sim.account(VAULT).unwrap().code_hash;
    let first = code(&world(plain));
    assert_ne!(first, CryptoHash::hash_bytes(&rebuilt));
    assert_eq!(code(&world(rebuilt)), first, "the name is the key");
}

/// A call step sends what it says: nothing by default, raw bytes as they
/// are, or its args serialized as JSON or borsh. Raw bytes are never taken
/// for a JSON value, and an args editor after any other step panics.
#[test]
fn a_call_steps_args_are_sent_as_it_says() {
    let stored = |fixture: Fixture| {
        let sim = fixture.build();
        let entries = sim.view_state("pool.near", b"");
        let [(_, value)] = <[_; 1]>::try_from(entries).expect("one entry");
        value
    };
    let pool = || Fixture::new().contract("pool.near", staking_pool_wasm()).init("new");
    assert_eq!(stored(pool()), b"");
    assert_eq!(stored(pool().args(vec![0u8, 12, 255])), [0, 12, 255]);
    assert_eq!(stored(pool().args_json(json!({ "a": 1 }))), br#"{"a":1}"#);
    assert_eq!(stored(pool().args_borsh(7u32)), [7, 0, 0, 0]);
    let called = Fixture::new()
        .account("alice.near", Balance::from_near(10))
        .contract("pool.near", staking_pool_wasm())
        .call("alice.near", "pool.near", "new")
        .args_borsh("key");
    assert_eq!(stored(called.clone()), [3, 0, 0, 0, b'k', b'e', b'y']);
    // `deposit` attaches to the call it follows.
    let paid = called.clone().deposit(Balance::from_near(2)).build().balance("pool.near");
    assert_eq!(
        paid,
        called.clone().build().balance("pool.near").saturating_add(Balance::from_near(2))
    );
    // `Tx::args_borsh` sends the same bytes.
    let mut sim = called.build();
    sim.call("alice.near", "pool.near", "new").args_borsh("key2").transact().assert_success();
    assert_eq!(sim.view_state("pool.near", b"")[0].1, [4, 0, 0, 0, b'k', b'e', b'y', b'2']);
    let after_account = std::panic::catch_unwind(|| {
        Fixture::new().account("alice.near", Balance::from_near(1)).args_json(json!({}))
    });
    assert_eq!(
        panic_message(after_account),
        "Fixture::args_json edits the last step, an init or a call, and the last step is neither"
    );
}

#[test]
fn a_failing_init_names_the_step() {
    let fixture = Fixture::new()
        .contract(VAULT, res("vault.wasm"))
        .init("new")
        .args_json(json!({ "token": TOKEN }));
    let msg = panic_message(std::panic::catch_unwind(|| fixture.build()));
    assert!(msg.starts_with("Fixture step 2 of 2, init vault.near.new, failed:"), "{msg}");
    assert!(msg.contains("missing field `token_id`"), "{msg}");
}

#[test]
fn a_panicking_step_names_the_step() {
    let fixture = Fixture::new()
        .account("alice.near", Balance::from_near(10))
        .account("alice.near", Balance::from_near(10));
    let msg = panic_message(std::panic::catch_unwind(|| fixture.build()));
    assert!(
        msg.starts_with("Fixture step 2 of 2, account(alice.near, 10.00 NEAR), panicked:"),
        "{msg}"
    );
    assert!(msg.contains("account alice.near already exists"), "{msg}");
}

#[test]
fn a_stub_step_deploys_the_stub() {
    let world = |version: &str| {
        Fixture::new()
            .account("alice.near", Balance::from_near(10))
            .stub("verifier.near", Stub::new().on("version", Reply::json(version)))
            .build()
    };
    let version = |mut sim: Sim| {
        let out = sim.call("alice.near", "verifier.near", "version").transact();
        out.json::<String>()
    };
    // The replies are in the key too: a changed one is a fresh setup.
    assert_eq!(version(world("1.2.0")), "1.2.0");
    assert_eq!(version(world("2.0.0")), "2.0.0");
}
