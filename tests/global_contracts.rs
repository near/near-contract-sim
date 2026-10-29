//! Global contracts (NEP-591): publish code once, by code hash (immutable)
//! or under the publisher's account id (upgradable), then point any number
//! of accounts at it with `UseGlobalContract`, or create an account running
//! it with a NEP-616 state init.

mod common;
use common::*;
use near_contract_sim::nearcore::near_primitives::hash::hash;
use near_contract_sim::{CryptoHash, GlobalContractDeployMode, GlobalContractIdentifier, Sim};
use serde_json::json;

fn set_and_get(sim: &mut Sim, contract: &str, who: &str, msg: &str) -> Option<String> {
    sim.call(who, contract, "set_status")
        .args_json(json!({"message": msg}))
        .transact()
        .assert_success();
    sim.view(contract, "get_status", json!({"account_id": who})).json()
}

#[test]
fn by_hash_deploy_once_use_from_many_accounts() {
    let mut sim = sim_with_users();
    let wasm = status_wasm();
    let code_hash = hash(&wasm);
    sim.create_account("publisher.near", near(1_000));
    let before = sim.balance("publisher.near");
    let out = sim
        .tx("publisher.near", "publisher.near")
        .deploy_global(wasm.clone(), GlobalContractDeployMode::CodeHash)
        .transact();
    out.assert_success();
    // The publisher pays for global storage by burning tokens, not staking
    // them: its storage_usage doesn't change, the cost is in tokens_burnt.
    let fees = &sim.config.runtime_config().fees;
    let per_byte = fees.storage_usage_config.global_contract_storage_amount_per_byte;
    let storage_cost = sim.global_storage_cost(wasm.len() as u64).as_yoctonear();
    assert_eq!(storage_cost, per_byte.as_yoctonear() * wasm.len() as u128);
    let paid = before.as_yoctonear() - sim.balance("publisher.near").as_yoctonear();
    assert_eq!(paid, out.tokens_burnt().as_yoctonear());
    assert!(paid > storage_cost, "{paid} vs {storage_cost}");
    assert_eq!(sim.account("publisher.near").unwrap().code_hash, CryptoHash::default());
    let id = GlobalContractIdentifier::CodeHash(code_hash);
    assert_eq!(sim.global_contract_code(id.clone()), Some(wasm.clone()));

    // Two accounts use it; each keeps its own state.
    for (acc, msg) in [("app1.near", "one"), ("app2.near", "two")] {
        sim.create_account(acc, near(10));
        let storage = sim.account(acc).unwrap().storage_usage;
        sim.tx(acc, acc).use_global(id.clone()).transact().assert_success();
        let v = sim.account(acc).unwrap();
        assert_eq!(v.global_contract_hash, Some(code_hash));
        assert_eq!(v.global_contract_account_id, None);
        assert_eq!(v.code_hash, CryptoHash::default());
        // The account stores a reference, not the code.
        assert!(v.storage_usage - storage < 100, "{} -> {}", storage, v.storage_usage);
        assert_eq!(set_and_get(&mut sim, acc, "alice.near", msg).as_deref(), Some(msg));
    }
    let s: Option<String> =
        sim.view("app1.near", "get_status", json!({"account_id": "alice.near"})).json();
    assert_eq!(s.as_deref(), Some("one"));
}

#[test]
fn by_account_id_upgrade_reaches_every_user() {
    let mut sim = sim_with_users();
    sim.create_account("owner.near", near(1_000));
    sim.tx("owner.near", "owner.near")
        .deploy_global(status_wasm(), GlobalContractDeployMode::AccountId)
        .transact()
        .assert_success();
    let id = GlobalContractIdentifier::AccountId("owner.near".parse().unwrap());
    // A by-hash user of the same v1 code, for contrast.
    let v1_hash = GlobalContractIdentifier::CodeHash(hash(&status_wasm()));
    sim.create_account("hasher.near", near(1_000));
    sim.tx("hasher.near", "hasher.near")
        .deploy_global(status_wasm(), GlobalContractDeployMode::CodeHash)
        .transact()
        .assert_success();
    sim.create_account("pinned.near", near(1_000));
    sim.tx("pinned.near", "pinned.near").use_global(v1_hash).transact().assert_success();
    for acc in ["u1.near", "u2.near"] {
        sim.create_account(acc, near(1_000));
        sim.tx(acc, acc).use_global(id.clone()).transact().assert_success();
        let v = sim.account(acc).unwrap();
        assert_eq!(v.global_contract_account_id.as_ref().map(|a| a.as_str()), Some("owner.near"));
        assert_eq!(v.global_contract_hash, None);
        assert_eq!(set_and_get(&mut sim, acc, "bob.near", "v1").as_deref(), Some("v1"));
    }

    // Upgrade: publish different code under the same owner (the fuzz
    // contract, which keeps no state struct of its own).
    let v2 = res("fuzz_contract.wasm");
    sim.tx("owner.near", "owner.near")
        .deploy_global(v2.clone(), GlobalContractDeployMode::AccountId)
        .transact()
        .assert_success();
    assert_eq!(sim.global_contract_code(id.clone()), Some(v2));
    // borsh `vec![Op::Log("v2")]` in the fuzz contract's program encoding.
    let program = [1u8, 0, 0, 0, 3, 2, 0, 0, 0, b'v', b'2'];
    for acc in ["u1.near", "u2.near"] {
        // The users run the new code without redeploying anything...
        let out = sim.call("alice.near", acc, "run").args(program.to_vec()).transact();
        assert!(out.assert_success().logs().iter().any(|l| l.contains("v2")), "{out}");
        // ...the old methods are gone, and the old state is still there
        // (nothing migrates it).
        let e = err(&sim
            .call("alice.near", acc, "set_status")
            .args_json(json!({"message": "x"}))
            .transact());
        assert!(e.contains("MethodNotFound"), "{e}");
        assert!(!sim.view_state(acc, b"").is_empty());
    }
    // The by-hash user still runs v1.
    assert_eq!(set_and_get(&mut sim, "pinned.near", "bob.near", "still").as_deref(), Some("still"));
}

#[test]
fn switching_between_local_and_global_code() {
    let mut sim = sim_with_users();
    sim.create_account("owner.near", near(1_000));
    sim.tx("owner.near", "owner.near")
        .deploy_global(probe_wasm(), GlobalContractDeployMode::AccountId)
        .transact()
        .assert_success();
    sim.deploy("app.near", status_wasm());
    let local = sim.account("app.near").unwrap();
    assert_ne!(local.code_hash, CryptoHash::default());
    let id = GlobalContractIdentifier::AccountId("owner.near".parse().unwrap());
    sim.tx("app.near", "app.near").use_global(id).transact().assert_success();
    let v = sim.account("app.near").unwrap();
    assert_eq!(v.code_hash, CryptoHash::default());
    // Local code storage was released.
    assert!(v.storage_usage < local.storage_usage);
    let h: u64 = sim.call("alice.near", "app.near", "height").transact().json();
    assert!(h > 0);
    // And back to local code.
    sim.deploy("app.near", status_wasm());
    let v = sim.account("app.near").unwrap();
    assert_eq!(v.global_contract_account_id, None);
    assert_eq!(set_and_get(&mut sim, "app.near", "alice.near", "local").as_deref(), Some("local"));
}

#[test]
fn using_a_missing_global_contract_fails() {
    let mut sim = sim_with_users();
    for id in [
        GlobalContractIdentifier::CodeHash(hash(b"nope")),
        GlobalContractIdentifier::AccountId("nobody.near".parse().unwrap()),
    ] {
        let out = sim.tx("alice.near", "alice.near").use_global(id).transact();
        let e = err(&out);
        assert!(e.contains("GlobalContractDoesNotExist"), "{e}");
        assert_eq!(sim.account("alice.near").unwrap().global_contract_hash, None);
    }
    assert_eq!(sim.global_contract_code(GlobalContractIdentifier::CodeHash(hash(b"nope"))), None);
}

#[test]
fn create_and_use_in_one_transaction_then_call() {
    // A factory-style flow: create a sub-account and point it at a global
    // contract in one transaction.
    let mut sim = sim_with_users();
    sim.create_account("owner.near", near(1_000));
    sim.tx("owner.near", "owner.near")
        .deploy_global(status_wasm(), GlobalContractDeployMode::AccountId)
        .transact()
        .assert_success();
    sim.tx("alice.near", "sub.alice.near")
        .create_account()
        .transfer(near(5))
        .use_global(GlobalContractIdentifier::AccountId("owner.near".parse().unwrap()))
        .transact()
        .assert_success();
    assert_eq!(set_and_get(&mut sim, "sub.alice.near", "bob.near", "hi").as_deref(), Some("hi"));
}

/// A NEP-616 account's id derives from its state init, and a state init sent
/// to that id creates the account, running the global contract.
#[test]
fn a_state_init_creates_the_account_its_id_derives_from() {
    use near_contract_sim::Action;
    use near_contract_sim::GlobalContractDeployMode::CodeHash;
    use near_contract_sim::nearcore::near_primitives::action::DeterministicStateInitAction;
    use near_contract_sim::nearcore::near_primitives::deterministic_account_id::*;
    use near_contract_sim::nearcore::near_primitives::utils::derive_near_deterministic_account_id;
    let mut sim = sim_with_users();
    let wasm = status_wasm();
    let publish = sim.tx("alice.near", "alice.near").deploy_global(wasm.clone(), CodeHash);
    publish.transact().assert_success();

    let state_init = DeterministicAccountStateInit::V1(DeterministicAccountStateInitV1 {
        code: GlobalContractIdentifier::CodeHash(hash(&wasm)),
        data: Default::default(),
    });
    let id = derive_near_deterministic_account_id(&state_init);
    let init = DeterministicStateInitAction { state_init, deposit: near(1) };
    let init = Action::DeterministicStateInit(Box::new(init));
    sim.tx("alice.near", &id).action(init).transact().assert_success();
    assert_eq!(set_and_get(&mut sim, id.as_str(), "bob.near", "hi").as_deref(), Some("hi"));
}
