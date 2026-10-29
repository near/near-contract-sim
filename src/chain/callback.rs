//! Receipts the simulator delivers: callbacks (`Tx::promise_results`) and
//! receipts from another predecessor (`Tx::predecessor`). The transaction
//! converts as usual, and on its way out of the conversion block its receipt
//! is rewritten.
//!
//! A callback becomes the receipt a `promise_then` makes: from the receiver
//! to itself, an `ActionV2` receipt waiting on one data id per result. A
//! data receipt per result follows it into the next block, so nearcore runs
//! it as any callback: it waits for its data, and the last data receipt
//! releases it. A receipt from another predecessor becomes the `ActionV2`
//! receipt that account's contract would send, its deposit refunded to the
//! signer who paid it. The transaction bought its gas and deposit, so the
//! supply ledger needs nothing.

use super::{Chain, Delivery};
use near_primitives::hash::{CryptoHash, hash};
use near_primitives::receipt::{
    ActionReceipt, ActionReceiptV2, DataReceipt, Receipt, ReceiptEnum, ReceiptV0,
};
use near_primitives::transaction::ExecutionOutcomeWithId;
use near_primitives::types::AccountId;

impl Chain {
    /// Rewrites the receipt of each transaction among `conversions` that is
    /// to be delivered as a callback, and sends its data receipts behind it.
    /// A refused transaction has no receipt and is forgotten.
    pub(super) fn rewrite_callbacks<'a>(
        &mut self,
        conversions: impl Iterator<Item = &'a ExecutionOutcomeWithId>,
        outgoing: &mut Vec<Receipt>,
    ) {
        for o in conversions {
            let Some(delivery) = self.callbacks.remove(&o.id) else { continue };
            let Some(&id) = o.outcome.receipt_ids.first() else { continue };
            let Some(at) = outgoing.iter().position(|r| *r.receipt_id() == id) else {
                panic!(
                    "transaction {}'s receipt was held back by congestion control, \
                     so it can't be delivered as a callback or from a predecessor",
                    o.id
                )
            };
            match delivery {
                Delivery::Callback(results) => {
                    let data = data_receipts(&outgoing[at], &results);
                    let input_data_ids = data.iter().map(data_id).collect();
                    as_v2(&mut outgoing[at], None, input_data_ids);
                    outgoing.splice(at + 1..at + 1, data);
                }
                Delivery::From(predecessor) => {
                    as_v2(&mut outgoing[at], Some(predecessor), vec![]);
                }
            }
            self.log.injected.insert(id);
        }
    }
}

/// Makes `r`, a transaction's receipt, the `ActionV2` receipt a contract
/// makes (nearcore's `function_call.rs`), waiting on `input_data_ids`.
/// From the receiver, with no refund override, it is what `promise_then`
/// on the receiver makes; from another `predecessor`, it refunds a failed
/// deposit to the signer, who paid it.
fn as_v2(r: &mut Receipt, predecessor: Option<AccountId>, input_data_ids: Vec<CryptoHash>) {
    let Receipt::V0(ReceiptV0 { predecessor_id, receiver_id, receipt, .. }) = r;
    let refund_to = predecessor.is_some();
    *predecessor_id = predecessor.unwrap_or_else(|| receiver_id.clone());
    // nearcore's `Receipt::from_tx` makes every transaction's receipt a
    // first-version `Action` one; only receipts a contract creates are V2.
    let ReceiptEnum::Action(a) = &*receipt else {
        unreachable!("a transaction converts into a first-version action receipt")
    };
    let ActionReceipt {
        signer_id,
        signer_public_key,
        gas_price,
        output_data_receivers,
        actions,
        ..
    } = a.clone();
    *receipt = ReceiptEnum::ActionV2(ActionReceiptV2 {
        refund_to: refund_to.then(|| signer_id.clone()),
        signer_id,
        signer_public_key,
        gas_price,
        output_data_receivers,
        input_data_ids,
        actions,
    });
}

/// One data receipt per result, from the receiver to itself, as a resumed
/// yield's are. Their ids and data ids derive from the callback's id.
fn data_receipts(callback: &Receipt, results: &[Option<Vec<u8>>]) -> Vec<Receipt> {
    let id = |what: &[u8], i: usize| {
        hash(&[callback.receipt_id().as_ref(), what, &(i as u64).to_le_bytes()].concat())
    };
    let receiver = callback.receiver_id();
    let each = results.iter().enumerate().map(|(i, data)| {
        Receipt::V0(ReceiptV0 {
            predecessor_id: receiver.clone(),
            receiver_id: receiver.clone(),
            receipt_id: id(b"injected receipt", i),
            receipt: ReceiptEnum::Data(DataReceipt {
                data_id: id(b"injected data", i),
                data: data.clone(),
            }),
        })
    });
    each.collect()
}

fn data_id(r: &Receipt) -> CryptoHash {
    match r.receipt() {
        ReceiptEnum::Data(d) => d.data_id,
        _ => unreachable!("a data receipt"),
    }
}
