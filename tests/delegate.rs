//! Meta transactions (NEP-366): the sender signs a `DelegateAction`
//! off-chain, a relayer wraps it in its own transaction (`relayer ->
//! sender`) and pays for the gas and the deposits.

mod common;
mod fuzz_common;
use common::{err, near, panic_message};
use fuzz_common::{Op, encode, fuzz_wasm};
use near_contract_sim::nearcore::near_primitives::action::delegate::{
    DelegateActionV2, NonDelegateAction, VersionedDelegateActionPayload,
    VersionedSignedDelegateAction,
};
use near_contract_sim::nearcore::near_primitives::transaction::TransactionNonce;
use near_contract_sim::{
    AccessKey, AccessKeyPermission, Action, Balance, Failure, FunctionCallPermission, Gas, KeyType,
    NonceMode, SecretKey, Sim, TxOutcome, builtin,
};
use std::panic::{AssertUnwindSafe, catch_unwind};

const GAS: Gas = Gas::from_teragas(50);

/// alice (user), bob, relayer.near, c.near (fuzz contract).
fn setup() -> Sim {
    let mut sim = Sim::new();
    for id in ["alice.near", "bob.near", "relayer.near"] {
        sim.create_account(id, near(1_000));
    }
    sim.deploy("c.near", fuzz_wasm());
    sim
}

/// Program: log predecessor (`e4:`), signer (`e5:`) and attached deposit (`e7:`).
fn who_program() -> Vec<u8> {
    encode(&[Op::Env(4), Op::Env(5), Op::Env(7)])
}

/// A transfer from alice in strict nonce mode, which refuses a nonce gap.
fn strict(sim: &mut Sim) -> TxOutcome {
    sim.tx("alice.near", "bob.near")
        .transfer(Balance::from_yoctonear(1))
        .nonce_mode(NonceMode::Strict)
        .transact()
}

#[test]
fn relayer_pays_gas_and_inner_actions_run_as_the_sender() {
    let mut sim = setup();
    let alice_pk = sim.secret_key("alice.near").public_key();
    let (alice, relayer) = (sim.balance("alice.near"), sim.balance("relayer.near"));
    let nonce_before = sim.access_key("alice.near", &alice_pk).unwrap().nonce;

    let out =
        sim.call("alice.near", "c.near", "run").args(who_program()).gas(GAS).relay("relayer.near");
    out.assert_success();
    // Predecessor is the sender; the signer (and gas payer) is the relayer.
    assert_eq!(
        out.logs(),
        vec!["e4:alice.near", "e5:relayer.near", "e7:00000000000000000000000000000000"]
    );
    // The tx itself is relayer -> alice (the Delegate action runs on alice),
    // which then spawns alice -> c.near.
    let tx = out.transaction.as_ref().unwrap();
    assert_eq!(tx.receiver_id.as_str(), "relayer.near");
    let execs: Vec<&str> =
        out.receipts.iter().filter(|r| !r.is_refund()).map(|r| r.receiver_id.as_str()).collect();
    assert_eq!(execs, ["alice.near", "c.near"]);
    // Alice paid nothing; the relayer paid exactly what was burnt.
    assert_eq!(sim.balance("alice.near"), alice);
    assert_eq!(
        relayer.as_yoctonear() - sim.balance("relayer.near").as_yoctonear(),
        out.tokens_burnt().as_yoctonear()
    );
    // The delegate action consumed a nonce of alice's key.
    assert_eq!(sim.access_key("alice.near", &alice_pk).unwrap().nonce, nonce_before + 1);
    // Her next normal transaction still gets a fresh nonce.
    sim.call("alice.near", "c.near", "run").args(encode(&[])).transact().assert_success();
}

#[test]
fn relayer_funds_deposits_and_failed_deposits_go_back_to_the_sender() {
    let mut sim = setup();
    let (alice, bob, relayer) =
        (sim.balance("alice.near"), sim.balance("bob.near"), sim.balance("relayer.near"));
    // A transfer "from alice" is funded by the relayer.
    let out = sim.tx("alice.near", "bob.near").transfer(near(2)).relay("relayer.near");
    out.assert_success();
    assert_eq!(sim.balance("alice.near"), alice);
    assert_eq!(sim.balance("bob.near"), bob.checked_add(near(2)).unwrap());
    assert_eq!(
        relayer.as_yoctonear() - sim.balance("relayer.near").as_yoctonear(),
        near(2).as_yoctonear() + out.tokens_burnt().as_yoctonear()
    );

    // A failing call with a deposit: the deposit is refunded to the sender
    // (the predecessor), not to the relayer who paid it.
    let relayer = sim.balance("relayer.near");
    let out = sim
        .call("alice.near", "c.near", "run")
        .args(encode(&[Op::Panic]))
        .gas(GAS)
        .deposit(near(1))
        .relay("relayer.near");
    // Gotcha (nearcore semantics, same over RPC): the Delegate receipt does
    // not return its inner receipt, so the tx status is a success even
    // though the inner call failed. The failure is in the receipts, and
    // nobody learns of it: it is unhandled.
    out.assert_success();
    let failed: Vec<_> = out.receipts.iter().filter(|r| r.failure().is_some()).collect();
    assert_eq!(failed.len(), 1);
    assert_eq!(failed[0].receiver_id.as_str(), "c.near");
    assert_eq!(failed[0].failure(), Some(Failure::Panic("fuzz panic".into())));
    assert!(failed[0].unhandled, "{out}");
    let vs = sim.findings();
    let [v] = &vs[..] else { panic!("{vs:?}") };
    assert_eq!(v.check, builtin::UNHANDLED_FAILURE);
    assert_eq!(v.tx, Some(out.hash));
    assert_eq!(sim.balance("alice.near"), alice.checked_add(near(1)).unwrap());
    assert_eq!(
        relayer.as_yoctonear() - sim.balance("relayer.near").as_yoctonear(),
        near(1).as_yoctonear() + out.tokens_burnt().as_yoctonear()
    );
}

#[test]
fn expiry_and_nonce_checks() {
    let mut sim = setup();
    let prog = encode(&[Op::Log("x".into())]);

    let sign = |sim: &mut Sim, max_block_height| {
        sim.call("alice.near", "c.near", "run")
            .args(prog.clone())
            .gas(GAS)
            .sign_delegate(max_block_height)
    };
    let relay =
        |sim: &mut Sim, signed| sim.tx("relayer.near", "alice.near").action(signed).transact();

    // Expired: max_block_height is below the height the action runs at.
    let relayer = sim.balance("relayer.near");
    let h = sim.height();
    let expired = sign(&mut sim, h - 1);
    let out = relay(&mut sim, expired);
    assert!(err(&out).contains("DelegateActionExpired"), "{}", err(&out));
    // The relayer still pays for the attempt.
    assert!(sim.balance("relayer.near") < relayer);
    // The last valid height is inclusive: the relayer's tx is included in
    // block height() + 1, and the delegate runs in the block after.
    let h = sim.height();
    let last_block = sign(&mut sim, h + 2);
    relay(&mut sim, last_block).assert_success();

    // Replay: the same signed action relayed twice.
    let h = sim.height();
    let signed = sign(&mut sim, h + 100);
    relay(&mut sim, signed.clone()).assert_success();
    let e = err(&relay(&mut sim, signed));
    assert!(e.contains("DelegateActionInvalidNonce"), "{e}");

    // Nonce above the runtime's upper bound (height * 1e6, the height of the
    // block the delegate runs in). Once it ran, it holds nothing: alice's next
    // transaction takes the nonce after her key's.
    let too_large = sim_height_bound(&sim);
    let e = err(&sim
        .call("alice.near", "c.near", "run")
        .args(prog.clone())
        .gas(GAS)
        .nonce(too_large)
        .relay("relayer.near"));
    assert!(e.contains("DelegateActionNonceTooLarge"), "{e}");
    strict(&mut sim).assert_success();
}

fn sim_height_bound(sim: &Sim) -> u64 {
    (sim.height() + 2) * 1_000_000
}

/// A delegate signed here holds its nonce until it runs, even where it
/// fails, or expires. Until then the key's next transaction takes the nonce
/// after it: a gap, which strict nonce mode refuses.
#[test]
fn a_signed_delegate_holds_its_nonce_until_it_runs_or_expires() {
    let mut sim = setup();
    let one = Balance::from_yoctonear(1);
    let sign = |sim: &mut Sim, max_block_height| {
        sim.tx("alice.near", "bob.near").transfer(one).sign_delegate(max_block_height)
    };
    let h = sim.height();
    let signed = sign(&mut sim, h + 100);
    assert!(err(&strict(&mut sim)).contains("InvalidNonce"));
    // A relay refused at conversion never runs it.
    sim.create_account("poor.near", one);
    let e = err(&sim.tx("poor.near", "alice.near").action(signed.clone()).transact());
    assert!(e.contains("NotEnoughBalance"), "{e}");
    assert!(err(&strict(&mut sim)).contains("InvalidNonce"));
    // Relayed to the wrong account, it runs and fails.
    let e = err(&sim.tx("relayer.near", "bob.near").action(signed).transact());
    assert!(e.contains("DelegateActionSenderDoesNotMatchTxReceiver"), "{e}");
    strict(&mut sim).assert_success();
    // Held while a block can still run it, then released for good: a
    // rewind doesn't bring it back.
    let h = sim.height();
    sign(&mut sim, h + 2);
    sim.step();
    assert!(err(&strict(&mut sim)).contains("InvalidNonce"));
    sim.set_height(h);
    strict(&mut sim).assert_success();
}

/// A converted relay runs its delegate a block later. Until then the
/// sender's next delegate takes the nonce after it, so both run.
#[test]
fn a_delegate_in_flight_holds_its_nonce() {
    let mut sim = setup();
    let sign = |sim: &mut Sim| {
        let h = sim.height();
        sim.tx("alice.near", "bob.near").transfer(Balance::from_yoctonear(1)).sign_delegate(h + 100)
    };
    let first = sign(&mut sim);
    sim.tx("relayer.near", "alice.near").action(first.clone()).submit();
    sim.step();
    let second = sign(&mut sim);
    assert_eq!(second.delegate_action.nonce, first.delegate_action.nonce + 1);
    sim.settle();
    sim.tx("relayer.near", "alice.near").action(second).transact().assert_success();
}

#[test]
fn signature_and_key_checks() {
    let mut sim = setup();
    let prog = encode(&[Op::Log("x".into())]);

    // Signed by a key alice doesn't have.
    let stranger = SecretKey::from_seed(KeyType::ED25519, "stranger");
    let e = err(&sim
        .call("alice.near", "c.near", "run")
        .args(prog.clone())
        .gas(GAS)
        .sign_with(&stranger)
        .nonce(1)
        .relay("relayer.near"));
    assert!(e.contains("DelegateActionAccessKeyError") && e.contains("AccessKeyNotFound"), "{e}");

    // Tampered after signing: the signature no longer matches.
    let h = sim.height();
    let mut signed =
        sim.call("alice.near", "c.near", "run").args(prog.clone()).gas(GAS).sign_delegate(h + 100);
    signed.delegate_action.max_block_height += 1;
    let e = err(&sim.tx("relayer.near", "alice.near").action(signed).transact());
    assert!(e.contains("DelegateActionInvalidSignature"), "{e}");

    // Relayed to the wrong account (the outer receiver must be the sender).
    let signed =
        sim.call("alice.near", "c.near", "run").args(prog.clone()).gas(GAS).sign_delegate(h + 100);
    let e = err(&sim.tx("relayer.near", "bob.near").action(signed).transact());
    assert!(e.contains("DelegateActionSenderDoesNotMatchTxReceiver"), "{e}");

    // A function-call access key can sign delegate calls to its receiver
    // only.
    let fck = SecretKey::from_seed(KeyType::ED25519, "alice-fc");
    let permission = FunctionCallPermission {
        allowance: Some(near(1)),
        receiver_id: "c.near".into(),
        method_names: vec![],
    };
    let key = AccessKey { nonce: 0, permission: AccessKeyPermission::FunctionCall(permission) };
    sim.tx("alice.near", "alice.near").add_key(fck.public_key(), key).transact().assert_success();
    sim.call("alice.near", "c.near", "run")
        .args(prog.clone())
        .gas(GAS)
        .sign_with(&fck)
        .relay("relayer.near")
        .assert_success();
    let e = err(&sim
        .tx("alice.near", "bob.near")
        .transfer(Balance::from_yoctonear(1))
        .sign_with(&fck)
        .relay("relayer.near"));
    assert!(e.contains("DelegateActionAccessKeyError"), "{e}");
}

#[test]
fn ml_dsa_sender_costs_the_relayer_the_verification_fee() {
    let mut sim = setup();
    sim.create_account_with_key("pq.near", near(10), &SecretKey::from_seed(KeyType::MLDSA65, "pq"));
    // Same-length sender ids so only the signature scheme differs.
    sim.create_account("ed.near", near(10));
    let prog = encode(&[]);
    let conv = |sim: &mut Sim, who: &str| {
        let out = sim.call(who, "c.near", "run").args(prog.clone()).gas(GAS).relay("relayer.near");
        out.assert_success();
        out.transaction.unwrap().gas_burnt.as_gas()
    };
    let (ed, pq) = (conv(&mut sim, "ed.near"), conv(&mut sim, "pq.near"));
    let ml_dsa = sim.config.runtime_config().fees.signature_verification_costs
        [near_contract_sim::nearcore::near_parameters::cost::SignatureKind::MlDsa65]
        .gas
        .as_gas();
    assert!(ml_dsa > 0);
    // The inner ML-DSA signature is charged at conversion, but the Delegate
    // action's send fee also grows with the (1952-byte) key and 3309-byte
    // signature, so the difference is at least the verification cost.
    assert!(pq >= ed + ml_dsa, "ed {ed} pq {pq} ml-dsa {ml_dsa}");
}

/// `DelegateV2` (gas-key-aware meta transactions) shipped in protocol 85 and
/// was disabled again in 87 (`RejectDelegateV2`).
fn delegate_v2(sim: &Sim, gk: &SecretKey, nonce: TransactionNonce, actions: Vec<Action>) -> Action {
    let signer = near_contract_sim::nearcore::near_crypto::InMemorySigner::from_secret_key(
        "alice.near".parse().unwrap(),
        gk.clone(),
    );
    let payload = VersionedDelegateActionPayload::V2(DelegateActionV2 {
        sender_id: "alice.near".parse().unwrap(),
        receiver_id: "c.near".parse().unwrap(),
        actions: actions.into_iter().map(|a| NonDelegateAction::try_from(a).unwrap()).collect(),
        nonce,
        max_block_height: sim.height() + 100,
        public_key: gk.public_key(),
    });
    Action::DelegateV2(Box::new(VersionedSignedDelegateAction::sign(&signer, payload)))
}

fn run_call() -> Action {
    Action::FunctionCall(Box::new(
        near_contract_sim::nearcore::near_primitives::action::FunctionCallAction {
            method_name: "run".into(),
            args: encode(&[Op::Env(4)]),
            gas: GAS,
            deposit: Balance::ZERO,
        },
    ))
}

#[test]
fn delegate_v2_works_at_86_and_is_rejected_at_87() {
    for (pv, accepted) in [(86, true), (87, false)] {
        let mut sim = Sim::new();
        sim.config.protocol_version = pv;
        sim.create_account("alice.near", near(100));
        sim.create_account("relayer.near", near(100));
        sim.deploy("c.near", fuzz_wasm());
        let gk = SecretKey::from_seed(KeyType::ED25519, "alice-gas");
        sim.tx("alice.near", "alice.near")
            .add_key(gk.public_key(), AccessKey::gas_key_full_access(2))
            .transfer_to_gas_key(gk.public_key(), near(1))
            .transact()
            .assert_success();
        let n = sim.gas_key_nonces("alice.near", &gk.public_key()).unwrap()[1];
        let a = delegate_v2(
            &sim,
            &gk,
            TransactionNonce::from_nonce_and_index(n + 1, 1),
            vec![run_call()],
        );
        let out = sim.tx("relayer.near", "alice.near").action(a).transact();
        if accepted {
            assert_eq!(out.assert_success().logs(), ["e4:alice.near"]);
            // The gas key's nonce index 1 advanced; the relayer paid.
            assert_eq!(sim.gas_key_nonces("alice.near", &gk.public_key()).unwrap()[1], n + 1);
        } else {
            let e = err(&out);
            assert!(e.contains("RemovedProtocolFeature") && e.contains("DelegateV2"), "{e}");
        }
    }
}

#[test]
fn withdraw_from_gas_key_is_not_allowed_in_a_delegate_at_87() {
    let mut sim = setup();
    let pk = SecretKey::from_seed(KeyType::ED25519, "alice-gas").public_key();
    sim.tx("alice.near", "alice.near")
        .add_key(pk.clone(), AccessKey::gas_key_full_access(1))
        .transfer_to_gas_key(pk.clone(), near(1))
        .transact()
        .assert_success();
    let e = err(&sim
        .tx("alice.near", "alice.near")
        .withdraw_from_gas_key(pk, Balance::from_millinear(1))
        .relay("relayer.near"));
    assert!(e.contains("WithdrawFromGasKeyNotAllowedInDelegate"), "{e}");
}

/// `relay` signs a delegate valid for the next 100 blocks, through block
/// `height() + 100`: one whose relay is refused at conversion holds its
/// nonce until then.
#[test]
fn a_relayed_delegate_is_valid_for_100_blocks() {
    let mut sim = setup();
    let one = Balance::from_yoctonear(1);
    sim.create_account("poor.near", one);
    let h = sim.height();
    let e = err(&sim.tx("alice.near", "bob.near").transfer(one).relay("poor.near"));
    assert!(e.contains("NotEnoughBalance"), "{e}");
    let mut before = sim.clone();
    before.set_height(h + 99);
    assert!(err(&strict(&mut before)).contains("InvalidNonce"));
    sim.set_height(h + 100);
    strict(&mut sim).assert_success();
}

/// A delegate is signed with a plain nonce, with no nonce index or nonce
/// mode, and carries no delegate.
#[test]
fn a_delegate_has_a_plain_nonce_and_carries_no_delegate() {
    let mut sim = setup();
    let msg = panic_message(catch_unwind(AssertUnwindSafe(|| {
        sim.tx("alice.near", "bob.near").nonce_index(0).relay("relayer.near")
    })));
    assert_eq!(msg, "a delegate has no nonce index or nonce mode");
    let msg = panic_message(catch_unwind(AssertUnwindSafe(|| {
        sim.tx("alice.near", "bob.near").nonce_mode(NonceMode::Strict).sign_delegate(100)
    })));
    assert_eq!(msg, "a delegate has no nonce index or nonce mode");
    let inner = sim.tx("alice.near", "bob.near").sign_delegate(100);
    let msg = panic_message(catch_unwind(AssertUnwindSafe(|| {
        sim.tx("alice.near", "bob.near").action(inner).relay("relayer.near")
    })));
    assert_eq!(msg, "a delegate can't carry a delegate: IsDelegateAction");
}

// ---- a delegate's inner gas is pinned ----

use near_contract_sim::explore::{Harness, Op as XOp, Plan, Skip, StepGas};

/// c.near's delegate: it calls its own `run` with 300 Tgas, pinned.
fn inner_300(sim: &mut Sim) -> near_contract_sim::SignedDelegateAction {
    let h = sim.height();
    let call = sim.call("c.near", "c.near", "run").args(encode(&[]));
    call.gas(Gas::from_teragas(300)).sign_delegate(h + 100)
}

/// The relayer sends c.near's delegate and, beside it, a call of c.near's
/// `run` whose gas is unpinned.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
struct Relayed;

impl XOp for Relayed {
    fn submit(&self, sim: &mut Sim) -> Result<(), Skip> {
        let delegate = inner_300(sim);
        sim.tx("relayer.near", "c.near").action(delegate).call("run").args(encode(&[])).submit();
        Ok(())
    }
}

/// A delegate's inner calls count toward its transaction's gas, as
/// nearcore's `total_prepaid_gas` counts them: beside a delegate with an
/// inner 300 Tgas call, an unpinned call attaches at most the ceiling less
/// 300 Tgas, for `least_gas` and for a plan step's `StepGas::Exactly`.
/// Both gave it the whole ceiling, and nearcore refused the transaction
/// (`TotalPrepaidGasExceeded`).
///
/// Verification boundary: the Rust gas arithmetic of `Tx::least_gas` and a
/// plan step's gas, against the pinned runtime.
#[test]
fn a_delegates_inner_gas_counts_as_pinned() {
    let mut sim = setup();
    let ceiling = sim.gas_ceiling();
    let room = ceiling.checked_sub(Gas::from_teragas(300)).unwrap();
    // nearcore counts the inner call: the room is all the call can attach.
    let send = |sim: &mut Sim, gas: Gas| {
        let delegate = inner_300(sim);
        let tx = sim.tx("relayer.near", "c.near").action(delegate).call("run");
        tx.args(encode(&[])).gas(gas).transact()
    };
    send(&mut sim, room).assert_success();
    let over = send(&mut sim, room.checked_add(Gas::from_gas(1)).unwrap()).assert_failure();
    assert!(format!("{over:?}").contains("TotalPrepaidGasExceeded"), "{over:?}");
    // `least_gas` searches within the room.
    let delegate = inner_300(&mut sim);
    let tx = sim.tx("relayer.near", "c.near").action(delegate).call("run").args(encode(&[]));
    let least = tx.least_gas().unwrap_or_else(|out| panic!("it succeeds within the room: {out}"));
    assert!(least <= room, "{least} past {room}");
    // A step's gas is clamped to it.
    let plan = Plan::new().op(Relayed).gas(StepGas::Exactly(ceiling));
    let run = Harness::new(setup()).run(&plan);
    let out = &run.outcomes()[0];
    assert!(out.is_success(), "{run}");
    assert_eq!(out.receipts[0].attached_gas, room, "{out}");
}

/// The relayer sends c.near's delegate and, beside it, two calls of c.near's
/// `run` whose gas is unpinned.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
struct RelayedTwice;

impl XOp for RelayedTwice {
    fn submit(&self, sim: &mut Sim) -> Result<(), Skip> {
        let delegate = inner_300(sim);
        let tx = sim.tx("relayer.near", "c.near").action(delegate);
        tx.call("run").args(encode(&[])).call("run").args(encode(&[])).submit();
        Ok(())
    }
}

/// c.near's delegate and two calls of its `run` beside it, each pinned at
/// `gas`, or unpinned.
fn two(sim: &mut Sim, gas: Option<Gas>) -> near_contract_sim::Tx<'_> {
    let delegate = inner_300(sim);
    let tx = sim.tx("relayer.near", "c.near").action(delegate);
    tx.call("run").args(encode(&[])).gas(gas).call("run").args(encode(&[])).gas(gas)
}

/// Unpinned calls share what the pinned gas leaves of the ceiling: beside
/// a delegate's inner 300 Tgas, two unpinned calls attach at most half of
/// the ceiling less 300 Tgas each, for `least_gas` and for a plan step's
/// `StepGas::Exactly`. nearcore takes the transaction at that cap and
/// refuses it one gas above.
///
/// Verification boundary: the Rust gas arithmetic of `Tx::least_gas` and a
/// plan step's gas, against the pinned runtime.
#[test]
fn unpinned_calls_share_the_room_the_pinned_gas_leaves() {
    let mut sim = setup();
    let ceiling = sim.gas_ceiling();
    let half = Gas::from_gas(ceiling.checked_sub(Gas::from_teragas(300)).unwrap().as_gas() / 2);
    two(&mut sim, Some(half)).transact().assert_success();
    let over = two(&mut sim, Some(half.checked_add(Gas::from_gas(1)).unwrap())).transact();
    let over = over.assert_failure();
    assert!(format!("{over:?}").contains("TotalPrepaidGasExceeded"), "{over:?}");
    // `least_gas` searches within each call's share.
    let least = two(&mut sim, None).least_gas();
    let least = least.unwrap_or_else(|out| panic!("it succeeds within the share: {out}"));
    assert!(least <= half, "{least} past {half}");
    // A step's gas is clamped to it.
    let plan = Plan::new().op(RelayedTwice).gas(StepGas::Exactly(ceiling));
    let run = Harness::new(setup()).run(&plan);
    let out = &run.outcomes()[0];
    assert!(out.is_success(), "{run}");
    assert_eq!(out.receipts[0].attached_gas, Gas::from_gas(2 * half.as_gas()), "{out}");
}
