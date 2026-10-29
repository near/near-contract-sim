//! MPC-style tests: yields, the clock, stubs and faults on near/mpc's
//! production contract. The runnable example of the guides' mpc.md.
//!
//! Two worlds:
//! - `mpc_fixture::world()`: mpc @ 8397c0ae in Running state with one
//!   secp256k1 domain whose key the test knows, so `respond` can sign
//!   (`tests/mpc_fixture/mod.rs`, a recipe to copy, not crate API).
//! - `dcap_world()`: mpc @ 1d514bc4 and its real TEE verifier, with the
//!   DCAP quote fixture of `tests/mpc_tee.rs`, the clock inside the quote's
//!   collateral window, and one attestation grant prepaid.
//!
//! Measured: a sign parks one yield that times out 200 blocks after the
//! sign ran; the timeout fails the sign with "Request has timed out."; the
//! user pays exactly 1 yocto plus gas either way.

mod common;
mod mpc_fixture;

use common::near;
use mpc_fixture::{MPC, SIGN_DEPOSIT, USER, VERIFIER};
use near_contract_sim::explore::{Fail, Harness, Label, Op, OpDone, Plan, Skip, plans};
use near_contract_sim::{Balance, Failure, Fault, Gas, Receipts, Reply, SecretKey, Sim, Stub};
use proptest::prelude::*;
use serde::{Deserialize, Serialize};
use serde_json::json;

#[test]
fn a_sign_request_times_out_with_an_exact_bill() {
    let mut sim = mpc_fixture::world();
    let before = sim.balance(USER);
    let tx = sim
        .call(USER, MPC, "sign")
        .args_json(mpc_fixture::sign_request(b"hello"))
        .deposit(SIGN_DEPOSIT)
        .submit();
    sim.settle(); // until idle; the yield stays parked
    let [y] = <[_; 1]>::try_from(sim.yields()).expect("one parked yield");
    assert_eq!(y.callback_methods, ["return_signature_and_clean_state_on_success"]);
    assert!(!sim.outcome(tx).is_complete());

    // One block before expiry, a respond wins the race.
    let mut early = sim.clone();
    early.advance_to(y.expires_at - 1);
    mpc_fixture::respond(&mut early, &y).assert_success();
    early.settle();
    early.outcome(tx).assert_success();

    // Otherwise the timeout wins: no sleeps, no polling.
    let out = sim.expire(&y);
    assert_eq!(out.assert_failure(), Failure::Panic("Request has timed out.".into()));
    let paid = before.as_yoctonear() - sim.balance(USER).as_yoctonear();
    assert_eq!(paid, 1 + out.effects().gas_paid().as_yoctonear()); // the deposit and gas, to the yocto
    assert_eq!(out.effects().near_net(USER), -1); // the deposit stays with MPC
    assert!(sim.yields().is_empty() && sim.pending_receipts().is_empty());
}

// ---- the sign flow's heights, on the production contract ----
// Measured on mpc @ 8397c0ae and nearcore 2.14.0-rc.5: `sign` parks one
// yield whose `expires_at` is 200 blocks after the block its receipt ran
// in. A `respond` submitted after `advance_to(expires_at - 1)` runs at
// `expires_at + 1` and wins; one submitted after `advance_to(expires_at)`
// arrives after the timeout callback and fails.

/// A world with one `sign` parked: the sign tx and its yield.
fn parked() -> (Sim, near_contract_sim::CryptoHash, near_contract_sim::PendingYield) {
    let mut sim = mpc_fixture::world();
    let tx = (sim.call(USER, MPC, "sign"))
        .args_json(mpc_fixture::sign_request(b"hello"))
        .deposit(SIGN_DEPOSIT)
        .submit();
    sim.settle();
    let [y] = <[_; 1]>::try_from(sim.yields()).expect("one parked yield");
    (sim, tx, y)
}

#[test]
fn a_sign_resolves_or_times_out_at_exact_heights() {
    let (sim, tx, y) = parked();
    let sign = sim.outcome(tx).expect_receipt("sign").block_height;
    assert_eq!(y.expires_at, sign + 200);
    let mut early = sim.clone();
    early.advance_to(y.expires_at - 1);
    let r = mpc_fixture::respond(&mut early, &y);
    assert_eq!(r.expect_receipt("respond").block_height, y.expires_at + 1, "{r}");
    let mut sim = sim;
    let out = sim.expire(&y);
    let cb = out.expect_receipt("return_signature_and_clean_state_on_success");
    assert_eq!(cb.block_height, y.expires_at + 1, "{out}");
}

/// The timeout counts blocks, not time: on a paused clock the sign still
/// times out at its height, and every block of it reads the paused time.
#[test]
fn a_paused_clock_does_not_stop_the_timeout() {
    let (mut sim, tx, y) = parked();
    let ns = sim.timestamp_ns();
    sim.pause_clock_at(ns);
    let out = sim.expire(&y);
    assert_eq!(out.assert_failure(), Failure::Panic("Request has timed out.".into()));
    let cb = out.expect_receipt("return_signature_and_clean_state_on_success");
    assert_eq!(cb.block_height, y.expires_at + 1);
    let mut late = out.receipts.iter().filter(|r| r.block_height > y.expires_at);
    assert!(late.clone().count() >= 2 && late.all(|r| r.block_timestamp_ns == ns), "{out}");
    assert_eq!((out.hash, sim.timestamp_ns()), (tx, ns));
}

#[test]
fn a_respond_after_the_timeout_fired_loses() {
    let (mut sim, tx, y) = parked();
    sim.advance_to(y.expires_at);
    let r = mpc_fixture::respond(&mut sim, &y);
    let late = "This sign request has timed out, was completed, or never existed.";
    assert_eq!(r.assert_failure(), Failure::Panic(late.into()));
    assert_eq!(sim.outcome(tx).assert_failure(), Failure::Panic("Request has timed out.".into()));
}

/// Negative control for `respond`: the contract does check the signature.
#[test]
fn a_tampered_signature_is_rejected() {
    let (mut sim, tx, y) = parked();
    let request = mpc_fixture::request_of(&y);
    let mut response = mpc_fixture::signature_for(&request);
    response["recovery_id"] = json!(1 - response["recovery_id"].as_u64().unwrap());
    let r = sim
        .call(mpc_fixture::PARTICIPANTS[0], MPC, "respond")
        .args_json(json!({"request": request, "response": response}))
        .transact();
    assert_eq!(r.assert_failure(), Failure::Panic("The provided signature is invalid.".into()));
    assert!(sim.outcome(tx).is_pending());
    assert_eq!(sim.yields(), [y]);
}

// ---- the DCAP world: a real verifier, or a stub in its place ----

const PINNED_NOW_NS: u64 = 1_786_622_400_000_000_000; // inside the quote's collateral window
const NODE: &str = "fixture-node-a.near";

fn dcap(name: &str) -> Vec<u8> {
    common::res(&format!("mpc/{name}"))
}

/// mpc @ 1d514bc4 with its real verifier, the fixture's image hashes
/// allowed, and one attestation grant prepaid for `NODE`, whose key the
/// quote binds.
fn dcap_world() -> Sim {
    let mut sim = Sim::new();
    sim.set_timestamp_ns(PINNED_NOW_NS);
    sim.create_account("alice.near", near(1_000));
    sim.deploy(VERIFIER, dcap("tee_verifier.wasm"));
    sim.deploy(MPC, dcap("mpc_contract.wasm"));
    sim.call("alice.near", MPC, "init")
        .args(dcap("mpc_init_args.json"))
        .transact()
        .assert_success();
    sim.set_state(MPC, b"STATE", Some(&dcap("mpc_state_patched.borsh")));
    let key: SecretKey =
        String::from_utf8(dcap("near_account_secret_key")).unwrap().trim().parse().unwrap();
    sim.create_account_with_key(NODE, near(10), &key);
    sim.call("alice.near", MPC, "prepay_attestation_storage")
        .args(dcap("mpc_prepay_attestation_storage_args.json"))
        .deposit(Balance::from_millinear(20))
        .transact()
        .assert_success();
    sim
}

fn grants(sim: &Sim) -> u64 {
    sim.view(MPC, "available_attestation_grants", json!({ "account_id": NODE })).json()
}

fn submit_quote(sim: &mut Sim) -> near_contract_sim::Tx<'_> {
    sim.call(NODE, MPC, "submit_participant_info")
        .args(dcap("mpc_submit_participant_info_args.json"))
        .gas(Gas::from_teragas(300))
}

/// A stub in the verifier's place rejects the quote: the grant isn't
/// consumed, and the verifier saw exactly one call.
#[test]
fn a_rejected_quote_consumes_no_grant() {
    let mut sim = dcap_world();
    sim.deploy_stub(
        VERIFIER,
        Stub::new().on("verify_quote", Reply::panic("TCB collateral expired")),
    );
    assert_eq!(grants(&sim), 1);
    let out = submit_quote(&mut sim).transact();
    let failure = Failure::Panic("The TEE verifier did not answer the verify_quote call.".into());
    assert_eq!(out.assert_failure(), failure);
    let calls = sim.calls_to(VERIFIER);
    assert_eq!(
        (calls.len(), calls[0].method_name.as_str(), calls[0].succeeded),
        (1, "verify_quote", false)
    );
    assert_eq!(calls[0].gas, Gas::from_teragas(200));
    assert_eq!(grants(&sim), 1);
}

/// The same question against the real verifier: fail its receipt instead.
#[test]
fn an_unreachable_verifier_consumes_no_grant() {
    let mut sim = dcap_world();
    let out = submit_quote(&mut sim)
        .inject(Receipts::call(VERIFIER, "verify_quote"), Fault::OutOfGas)
        .transact();
    assert!(out.is_failure());
    assert_eq!(out.expect_receipt("verify_quote").fault, Some(Fault::OutOfGas));
    assert_eq!(grants(&sim), 1);
}

/// And the real verifier accepts the quote, consuming the grant, with
/// headroom left in the callback.
#[test]
fn a_verified_quote_consumes_the_grant() {
    let mut sim = dcap_world();
    let out = submit_quote(&mut sim).transact();
    out.assert_success();
    assert!(out.receipts.iter().all(|r| r.is_success()), "{out}");
    out.expect_receipt("verify_quote");
    out.assert_headroom("resolve_verification", Gas::from_teragas(10));
    assert_eq!(grants(&sim), 0);
    let tls_public_key = "ed25519:3FwWphCj47PA3D8ErKeVNzqn2ckAKrfuixrvvfGrDxUx";
    let att: serde_json::Value =
        sim.view(MPC, "get_attestation", json!({ "tls_public_key": tls_public_key })).json();
    assert!(!att.is_null());
}

// ---- a yield-based harness: sign and respond, in any order and overlap ----

/// MPC's two value-moving calls. `Respond` answers the `k`th parked sign
/// (modulo how many there are) and skips when none is parked.
#[derive(Clone, Debug, Serialize, Deserialize)]
enum MpcOp {
    Sign { msg: u8 },
    Respond { k: u8 },
}

impl Op for MpcOp {
    fn submit(&self, sim: &mut Sim) -> Result<(), Skip> {
        match *self {
            MpcOp::Sign { msg } => {
                sim.call(USER, MPC, "sign")
                    .args_json(mpc_fixture::sign_request(&[msg]))
                    .deposit(SIGN_DEPOSIT)
                    .submit();
            }
            MpcOp::Respond { k } => {
                let parked = sim.yields();
                let y = parked.get(k as usize % parked.len().max(1)).ok_or("no sign is parked")?;
                let request = mpc_fixture::request_of(y);
                let response = mpc_fixture::signature_for(&request);
                sim.call(mpc_fixture::PARTICIPANTS[0], MPC, "respond")
                    .args_json(json!({ "request": request, "response": response }))
                    .submit();
            }
        }
        Ok(())
    }
}

fn mpc_ops() -> impl Strategy<Value = MpcOp> {
    prop_oneof![
        (0u8..4).prop_map(|msg| MpcOp::Sign { msg }),
        (0u8..4).prop_map(|k| MpcOp::Respond { k }),
    ]
}

/// per-op, once the sign's yield resolved: a sign refused before it
/// parked kept nothing; one that parked kept exactly the 1 yocto deposit
/// and ended signed or timed out.
fn a_sign_costs_one_yocto(d: &OpDone<MpcOp>) -> Result<(), Fail> {
    let out = d.outcome().ok_or("a sign submits one transaction")?;
    let kept = i128::try_from(-d.effects().near_net(USER)).expect("fits an i128");
    let parked = out.receipt("return_signature_and_clean_state_on_success").is_some();
    let timed_out = out.failure() == Some(Failure::Panic("Request has timed out.".into()));
    match (parked, kept) {
        (false, 0) => Ok(()),
        (true, 1) if out.is_success() || timed_out => Ok(()),
        _ => Err(format!("a sign kept {kept} yocto\n{out}").into()),
    }
}

/// at_rest: no request stays pending once nothing is in flight.
fn no_parked_sign(sim: &Sim) -> Result<(), String> {
    match sim.yields().len() {
        0 => Ok(()),
        n => Err(format!("{n} sign(s) still parked")),
    }
}

fn is_sign(op: &MpcOp) -> bool {
    matches!(op, MpcOp::Sign { .. })
}

fn mpc() -> Harness<MpcOp> {
    Harness::new(mpc_fixture::world())
        .per_op_for(is_sign, "a sign costs one yocto", a_sign_costs_one_yocto)
        .at_rest("no sign stays parked", no_parked_sign)
}

/// Two signs in one block, one answered while the other times out at the
/// end of the case.
#[test]
fn two_overlapping_signs_one_answered() {
    let plan = Plan::new()
        .op(MpcOp::Sign { msg: 1 })
        .blocks(0)
        .op(MpcOp::Sign { msg: 2 })
        .blocks(3)
        .op(MpcOp::Respond { k: 1 })
        .refused(); // the unanswered sign's timeout fails, as meant
    let run = mpc().run(&plan);
    run.assert_clean();
    let judged =
        |name: &str| run.judged().iter().find(|j| j.check == name).map(|j| (j.ok, j.failed));
    assert_eq!(judged("a sign costs one yocto"), Some((2, 0))); // once per sign, not the Respond
    assert_eq!(judged("no sign stays parked"), Some((2, 0)));
}

#[test]
fn mpc_search() {
    let report = mpc().search(plans(mpc_ops()).len(1..=4)).cases(24).run();
    report.assert_reached("mpc.near::return_signature_and_clean_state_on_success");
    // Starved, the timeout handler fails by running out of gas instead of
    // panicking, and the signer keeps the 1 yocto: they pay less, never
    // more (a fixed 2 Tgas budget, 1.28 Tgas used, no input grows it).
    // Nothing else is found, and nothing unforced.
    let starved = |f: &near_contract_sim::explore::Found<MpcOp>| {
        matches!(&f.label, Label::Forced(how) if how.contains("fail_on_timeout"))
            && f.finding.check == "a sign costs one yocto"
    };
    assert!(report.found().iter().all(starved), "{report}");
}
