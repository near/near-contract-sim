//! An MPC world for simulator tests: near/mpc's production contract
//! (`mpc_contract_8397c0ae.wasm`, block-clock build) in Running state, with
//! one secp256k1 signing domain whose root key the test knows, so `respond`
//! can produce the signature the contract checks with `ecrecover`.
//!
//! A recipe, not API: `mod mpc_fixture;` from a test file. Provenance and the
//! build command are in `tests/res/mpc/README.md`.
//!
//! What `world()` builds, as near/mpc's sandbox `SandboxTestSetupBuilder::build`
//! does (`crates/contract/tests/sandbox/common.rs`):
//! - `mpc.near` initialized with `init_running`: domain 0 (`CaitSith`,
//!   `Sign`), a keyset at epoch 5 holding [`domain_public_key`], and
//!   [`PARTICIPANTS`] with governance threshold 2 (60 %, rounded up).
//! - Each participant then submits `Attestation::Mock(Valid)` signed with its
//!   own account key. `init_running` stores placeholder attestations whose
//!   account key is the TLS key (near/mpc TODO #1087), so `respond` would
//!   fail with `AttestationKeyMismatch` without this step.
//! - `user.near` with 100 NEAR.
#![allow(dead_code)]

use near_contract_sim::{Balance, Gas, KeyType, PendingYield, SecretKey, Sim, TxOutcome};
use serde_json::{Value, json};
use std::path::PathBuf;

/// The MPC contract account.
pub const MPC: &str = "mpc.near";
/// The tee-verifier account `init_running` trusts. Nothing is deployed
/// there: sign/respond never call it, and mock attestations are verified
/// synchronously.
pub const VERIFIER: &str = "tee-verifier.near";
/// The account that sends sign requests.
pub const USER: &str = "user.near";
/// The participants, in participant-id order.
pub const PARTICIPANTS: [&str; 3] = ["node0.near", "node1.near", "node2.near"];
/// One attestation-storage grant (`attestation_storage_fee_millinear`, 20 mNEAR).
pub const ATTESTATION_FEE: Balance = Balance::from_millinear(20);
/// `SIGN_DEPOSIT_YOCTONEAR`: what `sign` requires; any excess is refunded
/// by a detached transfer in the same receipt.
pub const SIGN_DEPOSIT: Balance = Balance::from_yoctonear(1);
/// The signing domain `world()` registers.
pub const DOMAIN_ID: u64 = 0;
/// The derivation path `sign_request` uses.
pub const PATH: &str = "test";

/// The domain's root secret key: test material, any scalar in `1..n`.
const ROOT_SK: [u8; 32] = [
    0x6d, 0x70, 0x63, 0x2d, 0x73, 0x69, 0x6d, 0x2d, 0x66, 0x69, 0x78, 0x74, 0x75, 0x72, 0x65, 0x2d,
    0x72, 0x6f, 0x6f, 0x74, 0x2d, 0x6b, 0x65, 0x79, 0x2d, 0x30, 0x30, 0x30, 0x30, 0x30, 0x30, 0x31,
];

/// The secp256k1 group order n, big-endian.
const N: [u8; 32] = [
    0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xfe,
    0xba, 0xae, 0xdc, 0xe6, 0xaf, 0x48, 0xa0, 0x3b, 0xbf, 0xd2, 0x5e, 0x8c, 0xd0, 0x36, 0x41, 0x41,
];

pub fn res(name: &str) -> Vec<u8> {
    let dir =
        std::env::var_os("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR unset: run under cargo");
    let p = PathBuf::from(dir).join("tests/res/mpc").join(name);
    std::fs::read(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display()))
}

/// near/mpc @ 8397c0ae, `mpc-contract` with default features (block clock).
pub fn mpc_wasm() -> Vec<u8> {
    res("mpc_contract_8397c0ae.wasm")
}

/// The domain's root secret key.
pub fn domain_secret_key() -> SecretKey {
    secp256k1_secret_key(&ROOT_SK)
}

/// The domain's root public key, as `init_running`'s keyset holds it.
pub fn domain_public_key() -> String {
    domain_secret_key().public_key().to_string()
}

/// A participant's TLS key (not its account key, which `Sim::create_account`
/// derives from the account id).
pub fn tls_public_key(node: &str) -> String {
    SecretKey::from_seed(KeyType::ED25519, &format!("tls:{node}")).public_key().to_string()
}

/// `init_running`'s arguments.
pub fn init_running_args() -> Value {
    let participants: Vec<Value> = PARTICIPANTS
        .iter()
        .enumerate()
        .map(|(i, p)| json!([p, i, {"url": "127.0.0.1", "tls_public_key": tls_public_key(p)}]))
        .collect();
    json!({
        "domains": [{"id": DOMAIN_ID, "protocol": "CaitSith", "reconstruction_threshold": 2, "purpose": "Sign"}],
        "next_domain_id": DOMAIN_ID + 1,
        "keyset": {"epoch_id": 5, "domains": [
            {"domain_id": DOMAIN_ID, "attempt": 0, "key": {"Secp256k1": {"near_public_key": domain_public_key()}}}
        ]},
        "parameters": {
            "participants": {"next_id": PARTICIPANTS.len(), "participants": participants},
            "threshold": 2,
        },
        "tee_verifier_account_id": VERIFIER,
        "init_config": null,
    })
}

/// The world described in the module docs.
pub fn world() -> Sim {
    let mut sim = Sim::new();
    sim.create_account(USER, Balance::from_near(100));
    for p in PARTICIPANTS {
        sim.create_account(p, Balance::from_near(10));
    }
    sim.deploy(MPC, mpc_wasm());
    sim.call(MPC, MPC, "init_running")
        .args_json(init_running_args())
        .gas(Gas::from_teragas(300))
        .transact()
        .assert_success();
    for p in PARTICIPANTS {
        submit_mock_attestation(&mut sim, p, json!("Valid")).assert_success();
    }
    sim
}

/// `submit_participant_info` with `Attestation::Mock(mock)` under `node`'s
/// TLS key, signed with `node`'s account key. `mock` is the `MockAttestation`
/// JSON: `"Valid"`, `"Invalid"`, or
/// `{"WithConstraints": {"expiry_timestamp_seconds": s, ..}}`.
pub fn submit_mock_attestation(sim: &mut Sim, node: &str, mock: Value) -> TxOutcome {
    sim.call(node, MPC, "submit_participant_info")
        .args_json(json!({
            "proposed_participant_attestation": {"Mock": mock},
            "tls_public_key": tls_public_key(node),
        }))
        .transact()
}

/// `available_attestation_grants` for `account`.
pub fn grants(sim: &Sim, account: &str) -> u64 {
    sim.view(MPC, "available_attestation_grants", json!({"account_id": account})).json()
}

/// `sign`'s arguments for an ECDSA request on domain 0 at [`PATH`]; the
/// 32-byte payload is `sha256(msg)`.
pub fn sign_request(msg: &[u8]) -> Value {
    let payload = near_primitives::hash::hash(msg);
    json!({"request": {"path": PATH, "payload_v2": {"Ecdsa": hex(payload.as_ref())}, "domain_id": DOMAIN_ID}})
}

/// The `SignatureRequest` a parked `sign` yield carries: its callback's
/// arguments are the one-element JSON tuple `(request,)`.
pub fn request_of(y: &PendingYield) -> Value {
    assert_eq!(
        y.callback_methods,
        ["return_signature_and_clean_state_on_success"],
        "not a sign yield"
    );
    let args: Vec<Value> = serde_json::from_slice(&y.callback_args[0]).unwrap();
    args.into_iter().next().unwrap()
}

/// A valid `SignatureResponse` for `request`: an ECDSA signature of its
/// payload under the root key tweaked by its tweak, as the nodes would
/// produce it.
pub fn signature_for(request: &Value) -> Value {
    let tweak: Vec<u8> = serde_json::from_value(request["tweak"].clone()).unwrap();
    let payload = unhex(request["payload"]["Ecdsa"].as_str().expect("an ECDSA payload"));
    let sk = secp256k1_secret_key(&add_mod_n(&ROOT_SK, &tweak.try_into().unwrap()));
    // `[key type tag, r (32), s (32), recovery id]`: libsecp256k1's
    // RFC 6979 signature, low-s, as `ecrecover`'s malleability check wants.
    let sig = borsh::to_vec(&sk.sign(&payload)).unwrap();
    assert_eq!(sig.len(), 66);
    let (r, s, v) = (&sig[1..33], &sig[33..65], sig[65]);
    let mut big_r = vec![0x02 + (v & 1)];
    big_r.extend_from_slice(r);
    json!({
        "scheme": "Secp256k1",
        "big_r": {"affine_point": hex(&big_r)},
        "s": {"scalar": hex(s)},
        "recovery_id": v,
    })
}

/// `node0.near` answers the parked sign yield `y` with a valid signature.
/// Returns `respond`'s outcome; [`Tx::transact`](near_contract_sim::Tx::transact)
/// also runs the resumed callback.
pub fn respond(sim: &mut Sim, y: &PendingYield) -> TxOutcome {
    respond_as(sim, PARTICIPANTS[0], y)
}

/// [`respond`] from `node`.
pub fn respond_as(sim: &mut Sim, node: &str, y: &PendingYield) -> TxOutcome {
    let request = request_of(y);
    let response = signature_for(&request);
    sim.call(node, MPC, "respond")
        .args_json(json!({"request": request, "response": response}))
        .transact()
}

fn secp256k1_secret_key(bytes: &[u8; 32]) -> SecretKey {
    format!("secp256k1:{}", bs58(bytes)).parse().unwrap()
}

/// `(a + b) mod n`, for `a < n` and `b < n` (the contract rejects a tweak
/// that isn't a scalar).
fn add_mod_n(a: &[u8; 32], b: &[u8; 32]) -> [u8; 32] {
    let mut sum = [0u8; 32];
    let mut carry = 0u16;
    for i in (0..32).rev() {
        let t = a[i] as u16 + b[i] as u16 + carry;
        sum[i] = t as u8;
        carry = t >> 8;
    }
    if carry == 1 || sum >= N {
        let mut borrow = 0i16;
        for i in (0..32).rev() {
            let t = sum[i] as i16 - N[i] as i16 - borrow;
            sum[i] = t.rem_euclid(256) as u8;
            borrow = (t < 0) as i16;
        }
    }
    sum
}

fn bs58(bytes: &[u8]) -> String {
    const ALPHABET: &[u8] = b"123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz";
    let mut digits: Vec<u8> = Vec::new(); // base 58, little-endian
    for &b in bytes {
        let mut carry = b as u32;
        for d in digits.iter_mut() {
            carry += (*d as u32) << 8;
            *d = (carry % 58) as u8;
            carry /= 58;
        }
        while carry > 0 {
            digits.push((carry % 58) as u8);
            carry /= 58;
        }
    }
    let zeros = bytes.iter().take_while(|&&b| b == 0).count();
    let ones = std::iter::repeat_n('1', zeros);
    ones.chain(digits.iter().rev().map(|&d| ALPHABET[d as usize] as char)).collect()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn unhex(s: &str) -> Vec<u8> {
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
}
