//! Non-ed25519 keys: secp256k1 and ML-DSA-65 (post-quantum, protocol 85+).
//! Accounts can be created with them, keys of any type can be added to an
//! account, and each signs real transactions. ML-DSA-65 verification is
//! charged at transaction conversion.

mod common;
use common::*;
use near_contract_sim::nearcore::near_parameters::cost::SignatureKind;
use near_contract_sim::{AccessKey, KeyType, PublicKeyHandle, SecretKey, Sim};
use std::panic::{AssertUnwindSafe, catch_unwind};

const TYPES: [KeyType; 3] = [KeyType::ED25519, KeyType::SECP256K1, KeyType::MLDSA65];

fn short(t: KeyType) -> &'static str {
    match t {
        KeyType::ED25519 => "ed",
        KeyType::SECP256K1 => "k1",
        KeyType::MLDSA65 => "pq",
    }
}

/// One account per key type, same id length (`xx.near`), plus probe.near.
fn setup() -> Sim {
    let mut sim = Sim::new();
    sim.deploy("probe.near", probe_wasm());
    for t in TYPES {
        let id = format!("{}.near", short(t));
        sim.create_account_with_key(&id, near(10), &SecretKey::from_seed(t, &id));
    }
    sim
}

/// Bytes of near-sdk's `PublicKey` Debug output (`PublicKey { data: [..] }`),
/// which the probe's `pk()` returns for `env::signer_account_pk()`.
fn sdk_pk_bytes(debug: &str) -> Vec<u8> {
    let inner = &debug[debug.find('[').unwrap() + 1..debug.rfind(']').unwrap()];
    inner.split(',').map(|x| x.trim().parse().unwrap()).collect()
}

#[test]
fn ml_dsa_verification_is_charged_at_conversion() {
    let mut sim = setup();
    let costs = &sim.config.runtime_config().fees.signature_verification_costs;
    let ml_dsa = costs[SignatureKind::MlDsa65].gas.as_gas();
    // 0.1 Tgas at protocol 87 and 88; ed25519 and secp256k1 are free.
    assert_eq!(ml_dsa, 100_000_000_000);
    for k in [SignatureKind::Ed25519, SignatureKind::Secp256k1] {
        assert_eq!(costs[k].gas.as_gas(), 0);
    }
    let conv = |sim: &mut Sim, t: KeyType| {
        // `seed_len` (not `height`: its JSON length varies with the height).
        let out = sim.call(format!("{}.near", short(t)), "probe.near", "seed_len").transact();
        out.assert_success();
        (out.transaction.as_ref().unwrap().gas_burnt.as_gas(), out.gas_burnt().as_gas())
    };
    let (ed, k1, pq) = (
        conv(&mut sim, KeyType::ED25519),
        conv(&mut sim, KeyType::SECP256K1),
        conv(&mut sim, KeyType::MLDSA65),
    );
    assert_eq!(k1.0, ed.0);
    assert_eq!(pq.0, ed.0 + ml_dsa);
    // Only the conversion differs: the receipts cost the same.
    assert_eq!(pq.1 - pq.0, ed.1 - ed.0);
}

#[test]
fn add_keys_of_every_type_and_sign_with_them() {
    let mut sim = Sim::new();
    sim.deploy("probe.near", probe_wasm());
    sim.create_account("alice.near", near(10));
    let base = sim.account("alice.near").unwrap().storage_usage;
    let keys: Vec<SecretKey> =
        TYPES.iter().map(|t| SecretKey::from_seed(*t, &format!("extra-{}", short(*t)))).collect();
    let mut tx = sim.tx("alice.near", "alice.near");
    for k in &keys {
        tx = tx.add_key(k.public_key(), AccessKey::full_access());
    }
    tx.transact().assert_success();
    // 4 keys now; ML-DSA-65 is listed under its hash handle.
    let listed = sim.access_keys("alice.near");
    assert_eq!(listed.len(), 4);
    assert_eq!(listed.iter().filter(|(h, _)| matches!(h, PublicKeyHandle::MlDsa65(_))).count(), 1);
    // Trie storage: ML-DSA-65 costs a 33-byte handle, not the 1953-byte key.
    let suc = &sim.config.runtime_config().fees.storage_usage_config;
    let per_key = |len: u64| suc.num_extra_bytes_record + len + borsh_len_full_access();
    assert_eq!(
        sim.account("alice.near").unwrap().storage_usage - base,
        per_key(33) + per_key(65) + per_key(33)
    );
    for k in &keys {
        let pk: String = sim.call("alice.near", "probe.near", "pk").sign_with(k).transact().json();
        assert_eq!(sdk_pk_bytes(&pk), borsh::to_vec(&k.public_key()).unwrap());
        assert!(sim.access_key("alice.near", &k.public_key()).unwrap().nonce > 0);
    }
}

fn borsh_len_full_access() -> u64 {
    borsh::object_length(&AccessKey::full_access()).unwrap() as u64
}

#[test]
fn ml_dsa_needs_protocol_85() {
    let mut sim = Sim::new();
    sim.config.protocol_version = 84;
    sim.create_account("alice.near", near(10));
    let e = err(&sim
        .tx("alice.near", "alice.near")
        .add_key(SecretKey::from_seed(KeyType::MLDSA65, "x").public_key(), AccessKey::full_access())
        .transact());
    assert!(e.contains("PostQuantumSignatures"), "{e}");
    // Nor can one be written directly.
    let pq = SecretKey::from_seed(KeyType::MLDSA65, "x");
    let msg = "ml-dsa-65 keys need protocol 85 or later; config.protocol_version is 84";
    let set = catch_unwind(AssertUnwindSafe(|| sim.set_key("alice.near", &pq)));
    assert_eq!(panic_message(set), msg);
    let create =
        catch_unwind(AssertUnwindSafe(|| sim.create_account_with_key("pq.near", near(1), &pq)));
    assert_eq!(panic_message(create), msg);
    assert!(sim.account("pq.near").is_none());
    // secp256k1 predates all of this.
    sim.tx("alice.near", "alice.near")
        .add_key(
            SecretKey::from_seed(KeyType::SECP256K1, "x").public_key(),
            AccessKey::full_access(),
        )
        .transact()
        .assert_success();
}

#[test]
fn wrong_key_type_signature_is_rejected() {
    // Signing with a key the account doesn't have, of any type.
    let mut sim = setup();
    for t in TYPES {
        let other = SecretKey::from_seed(t, "not-registered");
        let e = err(&sim.call("ed.near", "probe.near", "height").sign_with(&other).transact());
        assert!(e.contains("AccessKeyNotFound"), "{t:?}: {e}");
    }
}
