//! Accounts, keys and contract state written and read directly
//! (`create_account`, `set_key`, `set_state`, `state`, `state_records`) and
//! their storage accounting.

mod common;
use common::*;
use near_contract_sim::nearcore::near_primitives::action::DeleteAccountAction;
use near_contract_sim::{AccessKey, Balance, CryptoHash, KeyType, SecretKey, Sim, StateRecord};
use serde_json::json;
use std::panic::{AssertUnwindSafe, catch_unwind};

#[test]
fn account_view_and_set_state() {
    let mut sim = sim_with_users();
    sim.deploy("status.near", status_wasm());
    let acc = sim.account("status.near").unwrap();
    assert_ne!(acc.code_hash, CryptoHash::default());
    let usage = acc.storage_usage;
    sim.set_state("status.near", b"hello", Some(b"world"));
    assert_eq!(sim.view_state("status.near", b"hel"), vec![(b"hello".to_vec(), b"world".to_vec())]);
    assert_eq!(sim.state("status.near", b"hello"), Some(b"world".to_vec()));
    assert_eq!(sim.state("status.near", b"hel"), None);
    assert_eq!(sim.account("status.near").unwrap().storage_usage, usage + 40 + 5 + 5);
    sim.set_state("status.near", b"hello", None);
    assert!(sim.view_state("status.near", b"hel").is_empty());
    assert_eq!(sim.account("status.near").unwrap().storage_usage, usage);
    sim.set_balance("bob.near", Balance::from_near(7));
    assert_eq!(sim.balance("bob.near"), Balance::from_near(7));
    let missing = catch_unwind(AssertUnwindSafe(|| sim.view_state("nobody.near", b"")));
    assert_eq!(panic_message(missing), "account nobody.near does not exist");
    let missing = catch_unwind(AssertUnwindSafe(|| sim.state("nobody.near", b"hello")));
    assert_eq!(panic_message(missing), "account nobody.near does not exist");
}

/// `create_account` writes the account and key directly; its storage_usage
/// must match what the runtime charges, so deleting the only key through a
/// real `DeleteKey` action lands exactly on the bare-account size, for keys
/// of every type (an ML-DSA-65 key is stored under a hash handle, not the
/// full key).
#[test]
fn create_account_storage_usage_matches_runtime_accounting() {
    let mut sim = sim_with_users();
    sim.deploy("probe.near", probe_wasm());
    let bare = sim.config.runtime_config().fees.storage_usage_config.num_bytes_account;
    for (id, t) in [("pq.near", KeyType::MLDSA65), ("k1.near", KeyType::SECP256K1)] {
        sim.create_account_with_key(id, near(10), &SecretKey::from_seed(t, id));
        // The key signs real transactions.
        sim.call(id, "probe.near", "ts").transact().assert_success();
    }
    for who in ["pq.near", "k1.near", "bob.near"] {
        let pk = sim.secret_key(who).public_key();
        sim.tx(who, who).delete_key(pk).transact().assert_success();
        assert_eq!(sim.account(who).unwrap().storage_usage, bare, "{who}");
    }
}

/// `set_state` charges storage exactly like a contract's `storage_write`
/// (key + value + num_extra_bytes_record), including overwrites and deletes
/// of entries the contract wrote.
#[test]
fn set_state_storage_usage_matches_storage_write() {
    let mut sim = sim_with_users();
    sim.deploy("a.near", failure_probe_wasm());
    sim.deploy("b.near", failure_probe_wasm());
    let usage = |sim: &Sim, id: &str| sim.account(id).unwrap().storage_usage;
    let base = usage(&sim, "a.near");
    assert_eq!(base, usage(&sim, "b.near"));
    sim.call("alice.near", "a.near", "write").transact().assert_success(); // key -> value
    sim.set_state("b.near", b"key", Some(b"value"));
    assert_eq!(usage(&sim, "a.near"), usage(&sim, "b.near"));
    assert_eq!(sim.view_state("a.near", b""), sim.view_state("b.near", b""));
    // Overwrite with a longer, then a shorter value.
    sim.set_state("a.near", b"key", Some(b"a much longer value"));
    assert_eq!(usage(&sim, "a.near"), base + 40 + 3 + 19);
    sim.set_state("a.near", b"key", Some(b""));
    assert_eq!(usage(&sim, "a.near"), base + 40 + 3);
    // Deleting an entry the contract wrote, and deleting a missing one.
    sim.set_state("b.near", b"key", None);
    assert_eq!(usage(&sim, "b.near"), base);
    sim.set_state("b.near", b"missing", None);
    assert_eq!(usage(&sim, "b.near"), base);
    // The contract can still write afterward (storage accounting stays consistent).
    sim.call("alice.near", "b.near", "write").transact().assert_success();
    assert_eq!(usage(&sim, "b.near"), base + 40 + 3 + 5);
}

/// An entry `apply_state_records` wrote isn't in `storage_usage`, so
/// `set_state` replacing or deleting it takes the usage down to 0 at most.
#[test]
fn set_state_over_a_patched_entry_keeps_storage_usage_at_or_above_0() {
    let mut sim = Sim::new();
    sim.create_account("a.near", near(1));
    let usage = |sim: &Sim| sim.account("a.near").unwrap().storage_usage;
    let base = usage(&sim);
    let key = [1u8; 200];
    let patch = |sim: &mut Sim| {
        let account_id = "a.near".parse().unwrap();
        let data = StateRecord::Data {
            account_id,
            data_key: key.to_vec().into(),
            value: vec![2; 10].into(),
        };
        sim.apply_state_records([data]);
    };
    patch(&mut sim);
    assert_eq!(usage(&sim), base);
    sim.set_state("a.near", &key, Some(b"v"));
    assert_eq!(usage(&sim), base - 9);
    patch(&mut sim);
    sim.set_state("a.near", &key, None);
    assert_eq!(usage(&sim), 0);
    assert_eq!(sim.state("a.near", &key), None);
    // A shorter value lowers a usage near the top without overflowing.
    let Some(StateRecord::Account { account_id, mut account }) =
        sim.state_records(["a.near"]).into_iter().next()
    else {
        panic!("a.near's account record")
    };
    account.set_storage_usage(u64::MAX - 1);
    sim.apply_state_records([StateRecord::Account { account_id, account }]);
    patch(&mut sim);
    sim.set_state("a.near", &key, Some(b"v"));
    assert_eq!(usage(&sim), u64::MAX - 10);
}

/// `set_key` adds a missing key with the storage an `AddKey` action charges,
/// and signs with it from then on; a key the account has is only made the
/// signer.
#[test]
fn set_key_adds_the_key_an_add_key_would_and_signs_with_it() {
    let mut sim = sim_with_users();
    let sk = SecretKey::from_seed(KeyType::ED25519, "node");
    let mut added = sim.clone();
    let add = added.tx("bob.near", "bob.near").add_key(sk.public_key(), AccessKey::full_access());
    add.transact().assert_success();
    let usage = |sim: &Sim| sim.account("bob.near").unwrap().storage_usage;
    sim.set_key("bob.near", &sk);
    assert_eq!(usage(&sim), usage(&added));
    assert_eq!(sim.secret_key("bob.near"), sk);
    sim.tx("bob.near", "alice.near").transfer(near(1)).transact().assert_success();
    sim.set_key("bob.near", &sk);
    assert_eq!(usage(&sim), usage(&added));
    let missing = catch_unwind(AssertUnwindSafe(|| sim.set_key("nobody.near", &sk)));
    assert_eq!(panic_message(missing), "account nobody.near does not exist");
}

/// `state_records` gives what `apply_state_records` takes: a fresh chain
/// patched with them holds the same accounts, keys, code and storage, and
/// nothing of the accounts left out. Account records come first, and
/// gas-key nonces stay out: the copied gas key's start at 0, and it signs.
#[test]
fn state_records_move_accounts_to_another_chain() {
    let mut sim = sim_with_users();
    sim.deploy("status.near", status_wasm());
    let set = sim.call("alice.near", "status.near", "set_status");
    set.args_json(json!({"message": "hi"})).transact().assert_success();
    let gk = SecretKey::from_seed(KeyType::ED25519, "gas");
    let add = sim.tx("alice.near", "alice.near");
    let add = add.add_key(gk.public_key(), AccessKey::gas_key_full_access(2));
    add.transfer_to_gas_key(gk.public_key(), Balance::from_near(1)).transact().assert_success();
    let ids = ["alice.near", "status.near"];
    let records = sim.state_records(ids);
    let is_account = |r: &StateRecord| matches!(r, StateRecord::Account { .. });
    assert!(records[..2].iter().all(is_account) && !records[2..].iter().any(is_account));
    assert!(!records.iter().any(|r| matches!(r, StateRecord::GasKeyNonce { .. })));
    let mut other = Sim::new();
    other.apply_state_records(records);
    for id in ids {
        assert_eq!(other.account(id), sim.account(id));
        assert_eq!(other.access_keys(id), sim.access_keys(id));
        assert_eq!(other.view_state(id, b""), sim.view_state(id, b""));
    }
    assert!(other.account("bob.near").is_none());
    assert_eq!(other.gas_key_nonces("alice.near", &gk.public_key()), Some(vec![0, 0]));
    let tx = other.tx("alice.near", "alice.near").sign_with(&gk).nonce_index(1);
    tx.transfer(Balance::from_yoctonear(1)).transact().assert_success();
    let nonce = StateRecord::gas_key_nonce("alice.near".parse().unwrap(), gk.public_key(), 0, 7);
    other.apply_state_records([nonce]);
    assert_eq!(other.gas_key_nonces("alice.near", &gk.public_key()), Some(vec![7, 1]));
}

/// `Balance::MAX` is the account encoding's version marker, so no account
/// can hold it.
#[test]
fn balance_max_is_refused() {
    let mut sim = sim_with_users();
    let msg = "an account can't hold Balance::MAX (u128::MAX yocto)";
    let set = catch_unwind(AssertUnwindSafe(|| sim.set_balance("alice.near", Balance::MAX)));
    assert_eq!(panic_message(set), msg);
    let create = catch_unwind(AssertUnwindSafe(|| sim.create_account("max.near", Balance::MAX)));
    assert_eq!(panic_message(create), msg);
    assert!(sim.account("max.near").is_none());
    let most = Balance::from_yoctonear(u128::MAX - 1);
    sim.set_balance("alice.near", most);
    assert_eq!(sim.balance("alice.near"), most);
}

#[test]
fn access_keys_reads_every_page_without_gas_nonce_rows() {
    let mut sim = sim_with_users();
    assert!(sim.access_keys("nobody.near").is_empty());
    let id = "alice.near";
    let gas = SecretKey::from_seed(KeyType::ED25519, "gas-page").public_key();
    sim.tx(id, id)
        .add_key(gas.clone(), AccessKey::gas_key_full_access(3))
        .transact()
        .assert_success();
    assert_eq!(sim.gas_key_nonces(id, &gas).unwrap().len(), 3);
    let mut expected = sim.access_keys(id);
    for i in 2..=204 {
        let kind = [KeyType::ED25519, KeyType::SECP256K1, KeyType::MLDSA65][i % 3];
        let pk = SecretKey::from_seed(kind, &format!("page-{i}")).public_key();
        let key = AccessKey::full_access();
        sim.apply_state_records([StateRecord::access_key(
            id.parse().unwrap(),
            pk.clone(),
            key.clone(),
        )]);
        expected.push((pk.into(), key));
        if [99, 100, 204].contains(&i) {
            expected.sort_by(|a, b| a.0.cmp(&b.0));
            assert_eq!(sim.access_keys(id), expected, "{} keys", i + 1);
        }
    }
}

/// An account's key signs what a contract verifies off-chain-signed: raw
/// bytes, or a NEP-413 message, whose hash near-kit computes. The
/// signature is near_crypto's, and checks against the account's key.
#[test]
fn an_accounts_key_signs_a_nep413_message() {
    use near_kit::standards::nep413;
    let sim = sim_with_users();
    let params = nep413::SignMessageParams {
        message: r#"{"intents":[]}"#.to_string(),
        recipient: "verifier.near".to_string(),
        nonce: [7; 32],
        callback_url: None,
        state: None,
    };
    let hash = nep413::serialize_message(&params);
    let key = sim.secret_key("bob.near");
    let signature = key.sign(hash.as_ref());
    assert!(signature.verify(hash.as_ref(), &key.public_key()));
    // Raw ed25519 bytes, for a contract that takes them.
    let near_contract_sim::nearcore::near_crypto::Signature::ED25519(raw) = &signature else {
        panic!("bob's key is ed25519")
    };
    assert_eq!(raw.to_bytes().len(), 64);
}

/// A missing account gets one default, 100 NEAR, from every way of
/// deploying to it: `Sim::deploy`, `deploy_stub`, `Fixture::contract` and
/// `Fixture::stub`. Before, the fixture's contract got 100 and the rest
/// 1000. Each pays its deploy's gas out of it.
#[test]
fn every_deploy_creates_a_missing_account_with_one_balance() {
    use near_contract_sim::{Fixture, Reply, Stub};
    let stub = || Stub::new().on("f", Reply::json(()));
    let mut sim = Sim::new();
    sim.deploy("a.near", status_wasm());
    sim.deploy_stub("b.near", stub());
    let fixture = Fixture::new().contract("c.near", status_wasm()).stub("d.near", stub()).build();
    let near = |sim: &Sim, id: &str| sim.balance(id).as_yoctonear() / 10u128.pow(24);
    // What the deploy burnt is below 1 NEAR.
    for (sim, id) in
        [(&sim, "a.near"), (&sim, "b.near"), (&fixture, "c.near"), (&fixture, "d.near")]
    {
        assert_eq!(near(sim, id), 99, "{id}");
    }
}

/// A function-call key, added in one line, signs calls of its methods to
/// its receiver, and nothing else: another method, or a deposit, is
/// refused as nearcore refuses them.
#[test]
fn a_function_call_key_signs_its_methods_alone() {
    let mut sim = sim_with_users();
    sim.deploy("status.near", status_wasm());
    let key = SecretKey::from_seed(KeyType::ED25519, "alice's app");
    sim.tx("alice.near", "alice.near")
        .add_function_call_key(key.public_key(), "status.near", &["set_status"], near(1))
        .transact()
        .assert_success();
    let set = |sim: &mut Sim, method: &str, deposit| {
        sim.call("alice.near", "status.near", method)
            .args_json(json!({ "message": "hi" }))
            .deposit(deposit)
            .sign_with(&key)
            .transact()
    };
    set(&mut sim, "set_status", Balance::ZERO).assert_success();
    let refused = |out: near_contract_sim::TxOutcome| format!("{:?}", out.failure());
    assert!(refused(set(&mut sim, "get_status", Balance::ZERO)).contains("MethodNameMismatch"));
    let deposit = refused(set(&mut sim, "set_status", Balance::from_yoctonear(1)));
    assert!(deposit.contains("DepositWithFunctionCall"), "{deposit}");
    let stored = sim.access_key("alice.near", &key.public_key()).unwrap();
    let near_contract_sim::AccessKeyPermission::FunctionCall(p) = stored.permission else {
        panic!("{stored:?}")
    };
    assert_eq!(
        (p.receiver_id.as_str(), p.method_names),
        ("status.near", vec!["set_status".into()])
    );
    assert!(p.allowance.is_some_and(|a| a < near(1)), "the call's fee came out of it");
}

/// `create_subaccount` sends the transaction a parent sends, and the new
/// account signs straight away with the key it added.
#[test]
fn a_subaccount_signs_once_created() {
    let mut sim = sim_with_users();
    sim.deploy("status.near", status_wasm());
    let before = sim.balance("alice.near");
    let sub = sim.create_subaccount("alice.near", "app", near(5));
    assert_eq!(sub, "app.alice.near");
    assert_eq!(sim.balance(&sub), near(5));
    let tx = sim.outcome(sim.txs().last().unwrap());
    assert_eq!(tx.receipts[0].actions, ["CreateAccount", "Transfer", "AddKey"]);
    assert!(before.checked_sub(sim.balance("alice.near")).unwrap() > near(5));
    sim.call(&sub, "status.near", "set_status")
        .args_json(json!({ "message": "hi" }))
        .transact()
        .assert_success();
    // A parent can only create its own subaccounts.
    let other =
        catch_unwind(AssertUnwindSafe(|| sim.create_subaccount("bob.near", "x.alice", near(1))));
    assert!(panic_message(other).contains("CreateAccountNotAllowed"));
}

/// After a rotation deletes the key the simulator signs with, a call says
/// so, as for an account it holds no key for, and names the way out.
#[test]
fn a_rotated_key_is_named_not_refused_by_the_runtime() {
    let mut sim = sim_with_users();
    let (old, new) = (sim.secret_key("bob.near"), SecretKey::from_seed(KeyType::ED25519, "bob 2"));
    sim.tx("bob.near", "bob.near")
        .add_key(new.public_key(), AccessKey::full_access())
        .delete_key(old.public_key())
        .transact()
        .assert_success();
    let send = |sim: &mut Sim| sim.tx("bob.near", "alice.near").transfer(near(1)).transact();
    let stale = catch_unwind(AssertUnwindSafe(|| send(&mut sim)));
    assert_eq!(
        panic_message(stale),
        format!(
            "no key for bob.near: the key the simulator holds, {}, is no longer on chain (a \
             transaction deleted it); use Sim::set_key with a key it has, or Tx::sign_with",
            old.public_key()
        )
    );
    sim.set_key("bob.near", &new);
    send(&mut sim).assert_success();
    // A deleted account's transaction is still the runtime's to refuse.
    let delete = DeleteAccountAction { beneficiary_id: "alice.near".parse().unwrap() };
    sim.tx("bob.near", "bob.near").action(delete).transact().assert_success();
    let refused = send(&mut sim).failure();
    assert!(matches!(refused, Some(near_contract_sim::Failure::InvalidTx(_))), "{refused:?}");
}
