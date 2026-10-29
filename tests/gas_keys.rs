//! Gas keys (protocol 85+): access keys that carry their own NEAR balance
//! and `num_nonces` parallel nonces. Gas is paid from (and refunded to) the
//! key's balance; deposits still come from the account.

mod common;
use common::*;
use near_contract_sim::{
    AccessKey, AccessKeyPermission, Balance, FunctionCallPermission, Gas, KeyType, NonceMode,
    PublicKey, SecretKey, Sim, TxOutcome,
};
use serde_json::json;

fn gas_key(seed: &str) -> SecretKey {
    SecretKey::from_seed(KeyType::ED25519, seed)
}

fn setup() -> (Sim, SecretKey) {
    let mut sim = sim_with_users();
    sim.deploy("status.near", status_wasm());
    let gk = gas_key("alice-gas");
    sim.tx("alice.near", "alice.near")
        .add_key(gk.public_key(), AccessKey::gas_key_full_access(4))
        .transfer_to_gas_key(gk.public_key(), near(5))
        .transact()
        .assert_success();
    (sim, gk)
}

fn set_status(sim: &mut Sim, gk: &SecretKey, index: u16) -> TxOutcome {
    sim.call("alice.near", "status.near", "set_status")
        .args_json(json!({"message": format!("via {index}")}))
        .gas(GAS)
        .sign_with(gk)
        .nonce_index(index)
        .transact()
}

const GAS: Gas = Gas::from_teragas(30);

/// The balance of alice's gas key `pk`, or `None` if she has no such gas key.
fn key_balance(sim: &Sim, pk: &PublicKey) -> Option<Balance> {
    Some(sim.access_key("alice.near", pk)?.gas_key_info()?.balance)
}

#[test]
fn a_funded_gas_key_has_its_balance_and_parallel_nonces() {
    let (sim, gk) = setup();
    let ak = sim.access_key("alice.near", &gk.public_key()).unwrap();
    match &ak.permission {
        AccessKeyPermission::GasKeyFullAccess(info) => {
            assert_eq!(info.balance, near(5));
            assert_eq!(info.num_nonces, 4);
        }
        p => panic!("not a gas key: {p:?}"),
    }
    assert_eq!(key_balance(&sim, &gk.public_key()), Some(near(5)));
    let nonces = sim.gas_key_nonces("alice.near", &gk.public_key()).unwrap();
    assert_eq!(nonces.len(), 4);
    // Every index starts at the same height-derived nonce, like a new
    // access key: (height of the AddKey block - 1) * 1e6.
    assert!(nonces.iter().all(|n| *n == nonces[0] && n % 1_000_000 == 0), "{nonces:?}");
    // Not a gas key / no such key.
    let default_pk = sim.secret_key("alice.near").public_key();
    assert_eq!(sim.gas_key_nonces("alice.near", &default_pk), None);
    assert_eq!(key_balance(&sim, &default_pk), None);
    assert_eq!(sim.gas_key_nonces("alice.near", &gas_key("nope").public_key()), None);
    // access_keys lists both keys.
    assert_eq!(sim.access_keys("alice.near").len(), 2);
}

#[test]
fn gas_is_paid_from_the_key_and_refunded_to_it() {
    let (mut sim, gk) = setup();
    let pk = gk.public_key();
    let account_before = sim.balance("alice.near");
    let key_before = key_balance(&sim, &pk).unwrap();
    let nonces_before = sim.gas_key_nonces("alice.near", &pk).unwrap();

    let out = set_status(&mut sim, &gk, 2);
    out.assert_success();
    // The account's balance is untouched; the key paid exactly what was burnt
    // (unused prepaid gas came back to the key, not the account).
    assert_eq!(sim.balance("alice.near"), account_before);
    let key_after = key_balance(&sim, &pk).unwrap();
    assert_eq!(
        key_before.as_yoctonear() - key_after.as_yoctonear(),
        out.tokens_burnt().as_yoctonear()
    );
    // Only nonce index 2 advanced.
    let nonces = sim.gas_key_nonces("alice.near", &pk).unwrap();
    for (i, (a, b)) in nonces_before.iter().zip(&nonces).enumerate() {
        assert_eq!(*b, if i == 2 { a + 1 } else { *a }, "index {i}");
    }
    // The contract sees the account as signer; the status was stored.
    let s: Option<String> =
        sim.view("status.near", "get_status", json!({"account_id": "alice.near"})).json();
    assert_eq!(s.as_deref(), Some("via 2"));
}

#[test]
fn parallel_nonces_land_in_one_block() {
    let (mut sim, gk) = setup();
    let pk = gk.public_key();
    let before = sim.gas_key_nonces("alice.near", &pk).unwrap();
    // Four indices plus a second tx on index 0, all queued for one block.
    let mut hs = vec![];
    for i in [0u16, 1, 2, 3, 0] {
        hs.push(
            sim.call("alice.near", "status.near", "set_status")
                .args_json(json!({"message": "p"}))
                .gas(GAS)
                .sign_with(&gk)
                .nonce_index(i)
                .submit(),
        );
    }
    sim.step();
    let first = sim.height();
    sim.settle();
    for h in &hs {
        sim.outcome(*h).assert_success();
        assert_eq!(sim.outcome(*h).transaction.as_ref().unwrap().block_height, first);
    }
    let after = sim.gas_key_nonces("alice.near", &pk).unwrap();
    assert_eq!(after[0], before[0] + 2);
    for i in 1..4 {
        assert_eq!(after[i], before[i] + 1);
    }
}

#[test]
fn signing_failure_modes() {
    let (mut sim, gk) = setup();
    let pk = gk.public_key();
    let account_before = sim.balance("alice.near");

    // Index out of range.
    let e = err(&set_status(&mut sim, &gk, 4));
    assert!(e.contains("InvalidNonceIndex") && e.contains("num_nonces: 4"), "{e}");

    // A gas key must be used with a nonce index.
    let e = err(&sim
        .call("alice.near", "status.near", "set_status")
        .args_json(json!({"message": "x"}))
        .sign_with(&gk)
        .transact());
    assert!(e.contains("InvalidNonceIndex") && e.contains("tx_nonce_index: None"), "{e}");

    // A normal key can't be used with a nonce index.
    let e = err(&sim
        .call("alice.near", "status.near", "set_status")
        .args_json(json!({"message": "x"}))
        .nonce_index(0)
        .transact());
    assert!(e.contains("InvalidNonceIndex { tx_nonce_index: Some(0), num_nonces: 0 }"), "{e}");

    // Replaying an already used nonce of an index.
    set_status(&mut sim, &gk, 1).assert_success();
    let used = sim.gas_key_nonces("alice.near", &pk).unwrap()[1];
    let e = err(&sim
        .call("alice.near", "status.near", "set_status")
        .args_json(json!({"message": "x"}))
        .sign_with(&gk)
        .nonce_index(1)
        .nonce(used)
        .transact());
    assert!(e.contains("InvalidNonce"), "{e}");
    // ...while another index accepts the same nonce value.
    sim.call("alice.near", "status.near", "set_status")
        .args_json(json!({"message": "x"}))
        .gas(GAS)
        .sign_with(&gk)
        .nonce_index(3)
        .nonce(used)
        .transact()
        .assert_success();

    // None of the rejected txs cost the account anything (gas came from the key).
    assert_eq!(sim.balance("alice.near"), account_before);
}

#[test]
fn empty_gas_key_cannot_pay() {
    let mut sim = sim_with_users();
    sim.deploy("status.near", status_wasm());
    let gk = gas_key("empty");
    let key = AccessKey::gas_key_full_access(1);
    sim.tx("alice.near", "alice.near").add_key(gk.public_key(), key).transact().assert_success();
    let before = sim.balance("alice.near");
    let e = err(&set_status(&mut sim, &gk, 0));
    assert!(e.contains("NotEnoughGasKeyBalance"), "{e}");
    // The account's own balance never backs a gas key.
    assert_eq!(sim.balance("alice.near"), before);
}

#[test]
fn deposit_beyond_account_balance_still_charges_the_key() {
    let (mut sim, gk) = setup();
    let pk = gk.public_key();
    let account_before = sim.balance("alice.near");
    let key_before = key_balance(&sim, &pk).unwrap();
    let out = sim
        .tx("alice.near", "bob.near")
        .transfer(near(1_000))
        .sign_with(&gk)
        .nonce_index(0)
        .transact();
    let e = err(&out);
    assert!(e.contains("NotEnoughBalanceForDeposit"), "{e}");
    // Unlike a normal invalid tx, the conversion gas is burnt (from the key)
    // and the nonce advances.
    let burnt = out.tokens_burnt();
    assert!(burnt > Balance::ZERO);
    assert_eq!(key_balance(&sim, &pk).unwrap(), key_before.checked_sub(burnt).unwrap());
    assert_eq!(sim.balance("alice.near"), account_before);
}

#[test]
fn deposits_come_from_the_account() {
    let (mut sim, gk) = setup();
    let pk = gk.public_key();
    let (alice, bob) = (sim.balance("alice.near"), sim.balance("bob.near"));
    let key_before = key_balance(&sim, &pk).unwrap();
    let out =
        sim.tx("alice.near", "bob.near").transfer(near(3)).sign_with(&gk).nonce_index(0).transact();
    out.assert_success();
    assert_eq!(sim.balance("alice.near"), alice.checked_sub(near(3)).unwrap());
    assert_eq!(sim.balance("bob.near"), bob.checked_add(near(3)).unwrap());
    let paid = key_before.as_yoctonear() - key_balance(&sim, &pk).unwrap().as_yoctonear();
    assert_eq!(paid, out.tokens_burnt().as_yoctonear());
}

#[test]
fn fund_and_withdraw() {
    let (mut sim, gk) = setup();
    let pk = gk.public_key();
    let before = sim.balance("alice.near");
    // Top up from the account (default key pays gas).
    let out =
        sim.tx("alice.near", "alice.near").transfer_to_gas_key(pk.clone(), near(2)).transact();
    out.assert_success();
    assert_eq!(key_balance(&sim, &pk), Some(near(7)));
    let fee = out.tokens_burnt();
    assert_eq!(
        sim.balance("alice.near"),
        before.checked_sub(near(2)).unwrap().checked_sub(fee).unwrap()
    );

    // Withdraw, signed by the gas key itself: the key pays the gas and loses
    // the amount, the account gets exactly the amount.
    let before = sim.balance("alice.near");
    let out = sim
        .tx("alice.near", "alice.near")
        .withdraw_from_gas_key(pk.clone(), near(4))
        .sign_with(&gk)
        .nonce_index(1)
        .transact();
    out.assert_success();
    assert_eq!(sim.balance("alice.near"), before.checked_add(near(4)).unwrap());
    let key = key_balance(&sim, &pk).unwrap();
    assert_eq!(key.as_yoctonear(), near(3).as_yoctonear() - out.tokens_burnt().as_yoctonear());

    // Withdrawing more than the key holds fails in the receipt.
    let out =
        sim.tx("alice.near", "alice.near").withdraw_from_gas_key(pk.clone(), near(10)).transact();
    let e = err(&out);
    assert!(e.contains("InsufficientGasKeyBalance"), "{e}");
    assert_eq!(key_balance(&sim, &pk).unwrap(), key);
}

#[test]
fn add_key_with_a_balance_is_rejected() {
    let mut sim = sim_with_users();
    let gk = gas_key("rich");
    let mut ak = AccessKey::gas_key_full_access(2);
    ak.gas_key_info_mut().unwrap().balance = near(1);
    let e = err(&sim.tx("alice.near", "alice.near").add_key(gk.public_key(), ak).transact());
    assert!(e.contains("AddGasKeyWithNonZeroBalance"), "{e}");
    assert!(sim.access_key("alice.near", &gk.public_key()).is_none());
}

#[test]
fn function_call_gas_key_is_restricted() {
    let mut sim = sim_with_users();
    sim.deploy("status.near", status_wasm());
    sim.deploy("probe.near", probe_wasm());
    let gk = gas_key("fc");
    let permission = FunctionCallPermission {
        allowance: None,
        receiver_id: "status.near".into(),
        method_names: vec!["set_status".into()],
    };
    sim.tx("alice.near", "alice.near")
        .add_key(gk.public_key(), AccessKey::gas_key_function_call(2, permission))
        .transfer_to_gas_key(gk.public_key(), near(1))
        .transact()
        .assert_success();
    // Allowed method on the allowed receiver.
    set_status(&mut sim, &gk, 0).assert_success();
    // Wrong receiver.
    let e =
        err(&sim.call("alice.near", "probe.near", "ts").sign_with(&gk).nonce_index(1).transact());
    assert!(e.contains("ReceiverMismatch"), "{e}");
    // Wrong method.
    let e = err(&sim
        .call("alice.near", "status.near", "get_status")
        .args_json(json!({"account_id": "alice.near"}))
        .sign_with(&gk)
        .nonce_index(1)
        .transact());
    assert!(e.contains("MethodNameMismatch"), "{e}");
    // Not a function call.
    let e = err(&sim
        .tx("alice.near", "status.near")
        .transfer(near(1))
        .sign_with(&gk)
        .nonce_index(1)
        .transact());
    assert!(e.contains("RequiresFullAccess"), "{e}");
}

#[test]
fn deleting_a_gas_key_burns_a_small_balance_only() {
    let (mut sim, gk) = setup();
    let pk = gk.public_key();
    // 5 NEAR is above the 1 NEAR burn cap: refused.
    let e = err(&sim.tx("alice.near", "alice.near").delete_key(pk.clone()).transact());
    assert!(e.contains("GasKeyBalanceTooHigh"), "{e}");
    assert!(sim.access_key("alice.near", &pk).is_some());
    // Withdraw down to 0.5 NEAR, then delete: the rest is burnt, and the
    // nonces go with the key.
    sim.tx("alice.near", "alice.near")
        .withdraw_from_gas_key(pk.clone(), Balance::from_millinear(4_500))
        .transact()
        .assert_success();
    let storage_before = sim.account("alice.near").unwrap().storage_usage;
    let out = sim.tx("alice.near", "alice.near").delete_key(pk.clone()).transact();
    out.assert_success();
    // The leftover 0.5 NEAR shows up in the receipt's tokens_burnt.
    let receipt = out.receipts.iter().find(|r| !r.is_refund()).unwrap();
    assert!(receipt.tokens_burnt > Balance::from_millinear(500), "{}", receipt.tokens_burnt);
    assert!(receipt.tokens_burnt < Balance::from_millinear(501), "{}", receipt.tokens_burnt);
    assert!(sim.access_key("alice.near", &pk).is_none());
    assert_eq!(sim.gas_key_nonces("alice.near", &pk), None);
    assert!(sim.account("alice.near").unwrap().storage_usage < storage_before);
}

#[test]
fn strict_nonce_mode() {
    let mut sim = sim_with_users();
    sim.deploy("status.near", status_wasm());
    let pk = sim.secret_key("alice.near").public_key();
    let cur = sim.access_key("alice.near", &pk).unwrap().nonce;
    let call = |sim: &mut Sim, nonce: u64| {
        sim.call("alice.near", "status.near", "set_status")
            .args_json(json!({"message": "s"}))
            .nonce_mode(NonceMode::Strict)
            .nonce(nonce)
            .transact()
    };
    // Monotonic mode would accept a gap; strict mode doesn't.
    let e = err(&call(&mut sim, cur + 2));
    assert!(e.contains("InvalidNonce"), "{e}");
    call(&mut sim, cur + 1).assert_success();
    // A gas key's index in strict mode, too.
    let gk = gas_key("strict");
    sim.tx("alice.near", "alice.near")
        .add_key(gk.public_key(), AccessKey::gas_key_full_access(1))
        .transfer_to_gas_key(gk.public_key(), near(1))
        .transact()
        .assert_success();
    let n = sim.gas_key_nonces("alice.near", &gk.public_key()).unwrap()[0];
    let e = err(&sim
        .call("alice.near", "status.near", "set_status")
        .args_json(json!({"message": "s"}))
        .gas(GAS)
        .sign_with(&gk)
        .nonce_index(0)
        .nonce_mode(NonceMode::Strict)
        .nonce(n + 5)
        .transact());
    assert!(e.contains("InvalidNonce"), "{e}");
}

#[test]
fn gas_keys_need_protocol_85() {
    // 84 predates gas keys: the AddKey is refused.
    let mut sim = Sim::new();
    sim.config.protocol_version = 84;
    sim.create_account("alice.near", near(10));
    let gk = gas_key("old");
    let key = AccessKey::gas_key_full_access(1);
    let e = err(&sim.tx("alice.near", "alice.near").add_key(gk.public_key(), key).transact());
    assert!(
        e.contains("UnsupportedProtocolFeature { protocol_feature: \"GasKeys\", version: 84 }"),
        "{e}"
    );
}
