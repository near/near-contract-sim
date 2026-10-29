//! A first audit harness (`skills/near-contract-sim/references/audit.md`):
//! one file from a contract's wasm to a first scan and its summary. The
//! specimen is `tests/res/floor.wasm` (from `floor.wat`), a keeper that
//! pays out through a token's `transfer_call`, forwarding a caller-signed gas
//! floor; at the caller's least gas the floor loses the amount.
//!
//! Verification boundary: the docs (`tests/docs.rs` finds this file's
//! lines in the page), run as a test over an actual scan.

use near_contract_sim::explore::prelude::*;
use serde::{Deserialize, Serialize};

const KEEPER: &str = "keeper.near";
const TOKEN: &str = "token.near";
const ALICE: &str = "alice.near";
const HELD: u64 = 1000;

/// The world every case starts from: built once, cloned per case.
fn world() -> Sim {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/res/floor.wasm");
    let code = std::fs::read(path).unwrap();
    Fixture::new()
        .account(ALICE, Balance::from_near(100))
        .contract(KEEPER, code.clone())
        .contract(TOKEN, code)
        .setup("the token backs the keeper", |sim| {
            sim.call(KEEPER, TOKEN, "mint").args(HELD.to_le_bytes()).transact().assert_success();
        })
        .setup("alice's credit", |sim| {
            sim.call(ALICE, KEEPER, "fund").args(HELD.to_le_bytes()).transact().assert_success();
        })
        .build()
}

/// A u64 view. A near-sdk contract's views take and return JSON:
/// `sim.view(account, method, json!({..})).json::<T>()`.
fn read(sim: &Sim, account: &str, method: &str) -> u64 {
    let bytes = sim.view_raw(account, method, ALICE.as_bytes().to_vec()).result.unwrap();
    u64::from_le_bytes(bytes.try_into().unwrap())
}

/// The contract's value-moving operations, one variant each, every field
/// one a caller chooses.
#[derive(Clone, Debug, Serialize, Deserialize)]
enum KeeperOp {
    /// Withdraws `amount`, signing `min_gas` for the token's call.
    Withdraw { amount: u64, min_gas: Gas },
}

impl Op for KeeperOp {
    fn submit(&self, sim: &mut Sim) -> Result<(), Skip> {
        let KeeperOp::Withdraw { amount, min_gas } = *self;
        if amount == 0 || read(sim, KEEPER, "credit_of") < amount {
            return Err("nothing to withdraw".into());
        }
        let args = [amount.to_le_bytes(), min_gas.as_gas().to_le_bytes()].concat();
        sim.call(ALICE, KEEPER, "withdraw").args(args).submit();
        Ok(())
    }
}

/// The property: a withdraw moves credit into tokens, never loses it.
fn credit_and_tokens_add_up(sim: &Sim) -> Result<(), String> {
    let (credit, tokens) = (read(sim, KEEPER, "credit_of"), read(sim, TOKEN, "balance_of"));
    match credit + tokens {
        HELD => Ok(()),
        n => Err(format!("alice holds {credit} credit and {tokens} tokens, {n} of {HELD}")),
    }
}

fn harness() -> Harness<KeeperOp> {
    Harness::new(world())
        .audits([KEEPER])
        .at_rest("alice's credit and tokens add up", credit_and_tokens_add_up)
}

#[test]
fn first_harness() {
    let harness = harness();
    // One witness per flow, as a wallet signs it.
    let witnesses = [
        Plan::new().op(KeeperOp::Withdraw { amount: 10, min_gas: Gas::from_teragas(40) }),
        Plan::new().op(KeeperOp::Withdraw { amount: 10, min_gas: Gas::from_teragas(30) }),
    ];
    // Each witness runs clean as signed: a positive control.
    harness.run(&witnesses[0]).assert_clean();
    // Every fault at every receipt, and every edge, of each witness.
    let audit = harness.scan(&witnesses).run();
    println!("{}", audit.with_open_gaps("first harness: no curves or search yet"));
    // A finding with nothing forced, at an edge: the caller's least gas.
    assert!(audit.found().iter().any(|f| matches!(f.label, Label::ShownAt(_))), "{audit}");
}

/// NEP-413's signed message, as a wallet's `signMessage` signs it. An op
/// that submits a signed intent builds and signs it inside `submit`.
#[derive(borsh::BorshSerialize)]
struct Nep413Payload {
    message: String,
    nonce: [u8; 32],
    recipient: String,
    callback_url: Option<String>,
}

/// The NEP-413 signature: the tag 2^31 + 413, then the payload, both
/// borsh, hashed with sha256 and signed with the user's ed25519 key.
fn nep413_sign(key: &SecretKey, payload: &Nep413Payload) -> Vec<u8> {
    let mut bytes = (2u32.pow(31) + 413).to_le_bytes().to_vec();
    bytes.extend(borsh::to_vec(payload).unwrap());
    let hash = near_contract_sim::nearcore::near_primitives::hash::hash(&bytes);
    match key.sign(hash.as_ref()) {
        near_contract_sim::nearcore::near_crypto::Signature::ED25519(s) => s.to_bytes().to_vec(),
        other => panic!("not ed25519: {other:?}"),
    }
}

/// A signed payload inside an op, with its deadline and nonce taken from
/// the chain's clock, never the host's: a case replays the same way.
#[test]
fn a_signed_payload_with_a_deadline_from_the_chain_clock() {
    let sim = world();
    // The user's key: the sim re-exports near-crypto's `SecretKey`.
    let key = SecretKey::from_seed(KeyType::ED25519, ALICE);
    let deadline_ns = sim.timestamp_ns() + 60_000_000_000;
    let mut nonce = [0u8; 32];
    nonce[..8].copy_from_slice(&sim.timestamp_ns().to_le_bytes());
    let message = serde_json::json!({ "deadline": deadline_ns.to_string(), "amount": "10" });
    let payload = Nep413Payload {
        message: message.to_string(),
        nonce,
        recipient: KEEPER.into(),
        callback_url: None,
    };
    let signature = nep413_sign(&key, &payload);
    assert_eq!(signature.len(), 64);
    // A contract takes the key as `ed25519:<base58>`, as `PublicKey` prints.
    assert!(key.public_key().to_string().starts_with("ed25519:"));
}

/// The library's strategies are `Clone`: reuse one in several arms
/// without `.boxed()`.
#[test]
fn the_library_strategies_clone() {
    let picks = input::list(input::pick());
    let (gas, answer) = (input::gas(), input::answer());
    let _arms = (picks.clone(), picks, gas.clone(), gas, answer.clone(), answer);
}

/// The audit prelude names what a witness that picks, a saved report and a
/// regression test need: no import beyond it.
#[test]
fn the_audit_prelude_names_picks_reports_and_replays() {
    fn named(_: Option<(Amount, Pick, Report, Status<()>, Verdict)>) {}
    named(None);
}
