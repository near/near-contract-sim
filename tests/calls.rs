//! Calls, views and multi-action transactions through real signed
//! transactions: deploy, args, deposits and gas, cross-contract calls, the
//! fungible-token flow, several transactions in one block, and nonces.

mod common;
use common::*;
use near_contract_sim::{
    AccessKey, Balance, CryptoHash, ExecutionStatus, Failure, Gas, KeyType, NonceMode, SecretKey,
    Sim, Tx,
};
use serde_json::json;
use std::panic::{AssertUnwindSafe, catch_unwind};

fn minimal_wasm() -> Vec<u8> {
    wat::parse_str(
        r#"(module
            (import "env" "value_return" (func $value_return (param i64 i64)))
            (memory 1)
            (export "memory" (memory 0))
            (data (i32.const 0) "\"hello\"")
            (func (export "hello") (call $value_return (i64.const 7) (i64.const 0))))"#,
    )
    .unwrap()
}

#[test]
fn minimal_wasm_call() {
    let mut sim = sim_with_users();
    sim.deploy("minimal.near", minimal_wasm());
    let out = sim.call("alice.near", "minimal.near", "hello").transact();
    out.assert_success();
    assert_eq!(out.json::<String>(), "hello");
}

#[test]
fn deploy_and_call() {
    let mut sim = sim_with_users();
    sim.deploy("status.near", status_wasm());
    let out = sim
        .call("alice.near", "status.near", "set_status")
        .args_json(json!({"message": "Hello, NEAR!"}))
        .transact();
    out.assert_success();
    assert_eq!(out.logs(), ["alice.near set_status with message Hello, NEAR!"]);
    assert!(out.gas_burnt() > Gas::from_teragas(1));
    // The function-call receipt plus the gas refund receipt.
    assert_eq!(out.receipts.iter().filter(|r| !r.is_refund()).count(), 1);
    assert!(out.receipts.iter().any(|r| r.is_refund()));
}

#[test]
fn view_call() {
    let mut sim = sim_with_users();
    sim.deploy("status.near", status_wasm());
    sim.call("alice.near", "status.near", "set_status")
        .args_json(json!({"message": "Hello!"}))
        .transact()
        .assert_success();
    let v = sim.view("status.near", "get_status", json!({"account_id": "alice.near"}));
    assert_eq!(v.json::<Option<String>>(), Some("Hello!".into()));
    assert_eq!(v.logs(), vec!["get_status for account_id alice.near"]);
}

#[test]
fn storage_persistence() {
    let mut sim = sim_with_users();
    sim.deploy("status.near", status_wasm());
    for (who, msg) in [("alice.near", "Alice's message"), ("bob.near", "Bob's message")] {
        sim.call(who, "status.near", "set_status")
            .args_json(json!({"message": msg}))
            .transact()
            .assert_success();
    }
    for (who, msg) in [("alice.near", "Alice's message"), ("bob.near", "Bob's message")] {
        let v: Option<String> =
            sim.view("status.near", "get_status", json!({"account_id": who})).json();
        assert_eq!(v.as_deref(), Some(msg));
    }
    // Contract storage is visible through view_state.
    assert_eq!(sim.view_state("status.near", b"").len(), 3); // STATE + 2 records
}

#[test]
fn cross_contract_factorial() {
    let mut sim = sim_with_users();
    sim.deploy("factorial.near", xcc_wasm());
    let out =
        sim.call("alice.near", "factorial.near", "factorial").args_json(json!({"n": 3})).transact();
    out.assert_success();
    assert_eq!(out.json::<u32>(), 6);
    // factorial(3) -> factorial(2) -> factorial(1), then two callbacks.
    let calls: Vec<_> = out.receipts.iter().filter(|r| !r.is_refund()).collect();
    let methods: Vec<_> = calls.iter().map(|r| r.method_names.join(",")).collect();
    assert_eq!(
        methods,
        ["factorial", "factorial", "factorial", "factorial_mult", "factorial_mult"]
    );
    let logged = |m| out.logs().iter().any(|l| l.contains(m));
    assert!(logged("Multiplied 2") && logged("Multiplied 6"), "{out}");
    // Every receipt runs in a later block than its parent.
    let heights: Vec<_> = calls.iter().map(|r| r.block_height).collect();
    assert!(heights.windows(2).all(|w| w[0] < w[1]), "{heights:?}");
    // factorial(3) and factorial(2) give their unused gas to the calls they
    // make by weight (near-sdk's default), so they keep no headroom;
    // factorial(1) makes none and keeps some.
    assert_eq!((calls[0].headroom(), calls[1].headroom()), (Gas::ZERO, Gas::ZERO), "{out}");
    assert!(calls[2].headroom() > Gas::ZERO, "{out}");
}

#[test]
fn fluent_builder_deposit_and_gas() {
    let mut sim = sim_with_users();
    sim.deploy("status.near", status_wasm());
    let before = sim.balance("status.near");
    let out = sim
        .call("alice.near", "status.near", "set_status")
        .args_json(json!({"message": "paid"}))
        .deposit(Balance::from_near(1))
        .gas(Gas::from_teragas(50))
        .transact();
    out.assert_success();
    // Deposit arrived. Exactly: protocol 87 removed the 30% contract gas reward.
    assert_eq!(sim.balance("status.near"), before.checked_add(Balance::from_near(1)).unwrap());
    // Prepaid gas is respected: too little gas fails.
    let out = sim
        .call("alice.near", "status.near", "set_status")
        .args_json(json!({"message": "x"}))
        .gas(Gas::from_gigagas(100))
        .transact();
    assert_eq!(out.failure(), Some(Failure::OutOfGas), "{out}");
}

#[test]
fn fungible_token_flow() {
    let mut sim = sim_with_users();
    sim.deploy("ft.near", ft_wasm());
    sim.call("ft.near", "ft.near", "new_default_meta")
        .args_json(json!({"owner_id": "alice.near", "total_supply": "1000000"}))
        .transact()
        .assert_success();
    // bob must register storage first
    sim.call("bob.near", "ft.near", "storage_deposit")
        .args_json(json!({"account_id": "bob.near"}))
        .deposit(Balance::from_millinear(125))
        .transact()
        .assert_success();
    let out = sim
        .call("alice.near", "ft.near", "ft_transfer")
        .args_json(json!({"receiver_id": "bob.near", "amount": "250"}))
        .deposit(Balance::from_yoctonear(1))
        .transact();
    out.assert_success();
    assert_eq!(out.events()[0]["event"], "ft_transfer", "{out}");
    assert_eq!(out.events()[0]["data"][0]["amount"], "250");
    let bal = |sim: &Sim, who: &str| -> String {
        sim.view("ft.near", "ft_balance_of", json!({"account_id": who})).json()
    };
    assert_eq!(bal(&sim, "alice.near"), "999750");
    assert_eq!(bal(&sim, "bob.near"), "250");
    // Missing 1 yocto -> the contract panics.
    let out = sim
        .call("alice.near", "ft.near", "ft_transfer")
        .args_json(json!({"receiver_id": "bob.near", "amount": "1"}))
        .transact();
    let Some(Failure::Panic(m)) = out.failure() else { panic!("expected a panic\n{out}") };
    assert!(m.contains("Requires attached deposit of exactly 1 yoctoNEAR"), "{m}");
}

#[test]
fn call_args_variants_reach_the_contract() {
    let mut sim = sim_with_users();
    sim.deploy("env.near", env_wasm());
    let raw = sim.call("alice.near", "env.near", "echo").args(vec![1, 2, 3]).transact();
    assert_eq!(raw.value().unwrap(), [1, 2, 3]);
    let js = sim.call("alice.near", "env.near", "echo").args_json(json!([1])).transact();
    assert_eq!(js.json::<Vec<u8>>(), [1]);
    // As for a view, JSON args that are null send no input.
    let none = sim.call("alice.near", "env.near", "echo").args_json(()).transact();
    assert_eq!(none.value().unwrap(), b"");
}

/// `Tx` helpers produce the actions they name.
#[test]
fn tx_builder_actions() {
    let mut sim = sim_with_users();
    let sk = SecretKey::from_seed(KeyType::ED25519, "sub");
    let out = sim
        .tx("alice.near", "sub.alice.near")
        .create_account()
        .transfer(near(5))
        .add_key(sk.public_key(), AccessKey::full_access())
        .deploy(failure_probe_wasm())
        .call("hello")
        .gas(Gas::from_teragas(10))
        .transact();
    assert_eq!(out.json::<String>(), "hello");
    let acc = sim.account("sub.alice.near").unwrap();
    assert_ne!(acc.code_hash, CryptoHash::default());
    assert!(acc.amount <= near(5) && acc.amount > near(4));
    assert_eq!(acc.locked, Balance::ZERO);
    // The simulator holds no key for an account a transaction made.
    let keyless =
        panic_message(catch_unwind(AssertUnwindSafe(|| sim.secret_key("sub.alice.near"))));
    assert_eq!(
        keyless,
        "no key for sub.alice.near: the account has no access key the simulator holds (a \
         transaction or apply_state_records made it); use Sim::set_key or Tx::sign_with"
    );
    let ledger_only = panic_message(catch_unwind(AssertUnwindSafe(|| {
        sim.call("bob-in-a-ledger.near", "alice.near", "m").transact()
    })));
    assert!(
        ledger_only.starts_with(
            "no key for bob-in-a-ledger.near: no such account exists on chain (a name a contract"
        ),
        "{ledger_only}"
    );
    let alice = SecretKey::from_seed(KeyType::ED25519, "alice.near");
    assert_eq!(sim.secret_key("alice.near"), alice);
    assert_eq!(
        sim.outcome(out.hash).receipts.iter().find(|r| !r.is_refund()).unwrap().status,
        ExecutionStatus::SuccessValue(b"\"hello\"".to_vec())
    );
}

/// `args`, `args_json`, `deposit` and `gas` edit the last action, which
/// must be a call: the first call keeps its own, and the second its
/// default 300 Tgas. A call followed by a transfer refuses them.
#[test]
fn call_settings_edit_the_last_call() {
    let mut sim = sim_with_users();
    sim.deploy("env.near", env_wasm());
    let out = sim
        .tx("alice.near", "env.near")
        .call("echo")
        .args(vec![1])
        .gas(Gas::from_teragas(10))
        .call("echo")
        .args_json([2])
        .deposit(Balance::from_yoctonear(3))
        .transact();
    assert_eq!(out.value().unwrap(), b"[2]");
    let r = out.receipt("echo").unwrap();
    assert_eq!((r.attached_gas, r.deposit), (Gas::from_teragas(310), Balance::from_yoctonear(3)));
    let mut after_a_transfer = |edit: fn(Tx) -> Tx| {
        panic_message(catch_unwind(AssertUnwindSafe(|| {
            let _ = edit(sim.call("alice.near", "bob.near", "m").transfer(near(1)));
        })))
    };
    let refused = |edit: &str| format!("Tx::{edit} edits the last action, which isn't a call");
    assert_eq!(after_a_transfer(|t| t.args(vec![1])), refused("args"));
    assert_eq!(after_a_transfer(|t| t.args_json(1)), refused("args_json"));
    assert_eq!(after_a_transfer(|t| t.deposit(near(1))), refused("deposit"));
    assert_eq!(after_a_transfer(|t| t.gas(Gas::from_teragas(1))), refused("gas"));
}

#[test]
fn explicit_nonce_is_reserved_for_later_transactions() {
    let mut sim = sim_with_users();
    let pk = sim.secret_key("alice.near").public_key();
    let cur = sim.access_key("alice.near", &pk).unwrap().nonce;
    // Same block: the second tx must get a nonce above the explicit one.
    let a = sim.tx("alice.near", "bob.near").transfer(near(1)).nonce(cur + 5).submit();
    let b = sim.tx("alice.near", "bob.near").transfer(near(1)).submit();
    sim.settle();
    sim.outcome(a).assert_success();
    sim.outcome(b).assert_success();
    assert_eq!(sim.access_key("alice.near", &pk).unwrap().nonce, cur + 6);
}

/// The same transaction signed twice before a block has one hash, and it is
/// queued once, as in nearcore's mempool. At protocol 84, before chunks
/// dropped duplicates, a second copy would run, fail on its used nonce and
/// replace the first one's outcome.
#[test]
fn a_transaction_submitted_twice_is_queued_once() {
    let mut sim = Sim::new();
    sim.config.protocol_version = 84;
    sim.create_account("alice.near", near(10));
    sim.create_account("bob.near", near(10));
    let pk = sim.secret_key("alice.near").public_key();
    let nonce = sim.access_key("alice.near", &pk).unwrap().nonce + 1;
    let mut send = || sim.tx("alice.near", "bob.near").transfer(near(1)).nonce(nonce).submit();
    let h = send();
    assert_eq!(send(), h);
    sim.settle();
    sim.outcome(h).assert_success();
}

/// The next nonce is one above the key's nonce on chain and its queued
/// transactions, so one refused at conversion, even for a nonce far too
/// large, leaves no gap: strict nonce mode, which refuses gaps, takes the
/// next transaction.
#[test]
fn a_refused_transaction_leaves_no_nonce_gap() {
    let mut sim = sim_with_users();
    let strict = |sim: &mut Sim| {
        sim.tx("alice.near", "bob.near").transfer(near(1)).nonce_mode(NonceMode::Strict).transact()
    };
    let e = err(&sim.tx("alice.near", "bob.near").transfer(near(1_000_000)).transact());
    assert!(e.contains("NotEnoughBalance"), "{e}");
    strict(&mut sim).assert_success();
    let e = err(&sim.tx("alice.near", "bob.near").transfer(near(1)).nonce(u64::MAX / 2).transact());
    assert!(e.contains("NonceTooLarge"), "{e}");
    strict(&mut sim).assert_success();
}

/// nearcore 2.14 checks the nonce upper bound in the runtime
/// (`nonce < block_height * 1e6`), not only in the chain layer.
#[test]
fn nonce_upper_bound_is_enforced_by_the_runtime() {
    let mut sim = sim_with_users();
    sim.deploy("status.near", status_wasm());
    let pk = sim.secret_key("alice.near").public_key();
    // Re-adding alice's own key in block 1000, the pinned height, resets its
    // nonce to 999 * 1e6.
    sim.set_height(1_000);
    sim.tx("alice.near", "alice.near")
        .delete_key(pk.clone())
        .add_key(pk, AccessKey::full_access())
        .transact()
        .assert_success();
    let call = |sim: &mut Sim| {
        sim.call("alice.near", "status.near", "set_status")
            .args_json(json!({"message": "x"}))
            .transact()
    };
    // Moving the height back below the key's nonce range makes every nonce
    // "too large", exactly like the runtime would.
    sim.set_height(10);
    let e = err(&call(&mut sim));
    assert!(e.contains("NonceTooLarge"), "{e}");
    sim.set_height(2_000);
    call(&mut sim).assert_success();
}
