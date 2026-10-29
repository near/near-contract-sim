//! Failure semantics: rollback, deposit refunds, failed promises in callbacks.

mod common;
use common::*;
use near_contract_sim::{Balance, Failure, InvalidTxError, Sim};

fn setup() -> Sim {
    let mut sim = sim_with_users();
    sim.deploy("fail.near", failure_probe_wasm());
    sim.deploy("probe.near", probe_wasm());
    sim
}

#[test]
fn failed_receipt_state_is_rolled_back_and_deposit_refunded() {
    let mut sim = setup();
    let before = sim.balance("alice.near");
    let contract_before = sim.balance("fail.near");
    let out = sim
        .call("alice.near", "fail.near", "write_then_panic")
        .deposit(Balance::from_near(5))
        .transact();
    assert_eq!(out.failure(), Some(Failure::Panic("boom".into())), "{out}");
    // storage_write happened before the panic but was rolled back.
    assert!(sim.view_state("fail.near", b"").is_empty());
    // alice got her 5 NEAR back and paid exactly the burnt gas.
    let paid = before.as_yoctonear() - sim.balance("alice.near").as_yoctonear();
    assert_eq!(paid, out.tokens_burnt().as_yoctonear());
    // The contract did not keep the deposit (and since protocol 87 there is
    // no contract gas reward, so its balance is unchanged to the yocto).
    assert_eq!(sim.balance("fail.near"), contract_before);

    // Control: the same write without the panic persists.
    sim.call("alice.near", "fail.near", "write").transact().assert_success();
    assert_eq!(sim.view_state("fail.near", b""), vec![(b"key".to_vec(), b"value".to_vec())]);
}

#[test]
fn failed_cross_contract_call_callback_sees_failure_and_deposit_returns() {
    let mut sim = setup();
    let contract_before = sim.balance("fail.near");
    let out = sim.call("alice.near", "fail.near", "send_to_missing").transact();
    // The callback ran, observed PromiseResult::Failed, and its value is the tx result.
    assert_eq!(out.json::<String>(), "cb-failed");
    assert_eq!(out.logs(), ["cb-failed"]);
    let failed: Vec<_> = out.receipts.iter().filter(|r| r.failure().is_some()).collect();
    assert_eq!(failed.len(), 1);
    assert_eq!(failed[0].receiver_id.as_str(), "missing.near");
    let err = format!("{:?}", failed[0].status);
    assert!(err.contains("missing.near\") doesn't exist"), "{err}");
    // The 1 NEAR attached to the failed call was refunded to fail.near.
    let refund = out
        .receipts
        .iter()
        .filter(|r| r.is_refund() && r.receiver_id.as_str() == "fail.near")
        .count();
    assert!(refund >= 1);
    assert_eq!(sim.balance("fail.near"), contract_before);
}

#[test]
fn near_sdk_callback_sees_failure() {
    let mut sim = setup();
    let out = sim.call("alice.near", "probe.near", "call_missing").transact();
    out.assert_success();
    assert_eq!(sim.view("probe.near", "last", ()).json::<String>(), "cb-failed");
}

#[test]
fn rejected_transaction_insufficient_balance() {
    let mut sim = setup();
    sim.create_account("poor.near", Balance::from_yoctonear(1));
    let out = sim.call("poor.near", "probe.near", "ts").transact();
    // nearcore 2.14 records invalid transactions with an InvalidTxError outcome.
    assert!(out.receipts.is_empty());
    let failure = out.assert_failure();
    assert!(matches!(failure, Failure::InvalidTx(InvalidTxError::NotEnoughBalance { .. })));
    let want = "invalid transaction: Sender poor.near does not have enough balance";
    assert!(failure.to_string().starts_with(want), "{failure}");
}

/// Calls queued in one block each pay for their default 300 Tgas when they
/// convert, so a signer with little NEAR runs out after a few; the refusal
/// says so, rather than reading as a storage problem.
///
/// Verification boundary: the refused outcomes of actual queued submits.
#[test]
fn queued_calls_that_run_out_of_balance_name_the_prepaid_gas() {
    let mut sim = setup();
    sim.create_account("thin.near", Balance::from_millinear(200));
    let txs: Vec<_> = (0..20).map(|_| sim.call("thin.near", "probe.near", "ts").submit()).collect();
    sim.settle();
    let refused = txs.iter().map(|tx| sim.outcome(tx)).find(|o| o.receipts.is_empty());
    let refused = refused.expect("a call past the balance is refused");
    let failure = refused.assert_failure();
    assert!(
        matches!(
            failure,
            Failure::InvalidTx(
                InvalidTxError::NotEnoughBalance { .. }
                    | InvalidTxError::LackBalanceForState { .. }
            )
        ),
        "{failure}"
    );
    assert!(failure.to_string().contains("pays for all the gas it attaches"), "{failure}");
}
