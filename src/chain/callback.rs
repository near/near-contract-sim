//! Receipts the simulator delivers: callbacks (`Tx::promise_results`) and
//! receipts from another predecessor (`Tx::predecessor`). The transaction
//! converts as usual, and on its way out of the conversion block its receipt
//! is rewritten.
//!
//! A callback becomes the receipt a `promise_then` makes: from the receiver
//! to itself, an action receipt waiting on one data id per result. A data
//! receipt per result follows it into the next block, so nearcore runs it as
//! any callback: it waits for its data, and the last data receipt releases
//! it. A receipt from another predecessor becomes the action receipt that
//! account's contract would send, its deposit refunded to the signer who
//! paid it. The transaction bought its gas and deposit, so the
//! supply ledger needs nothing.

use super::{Chain, Delivery};
use near_primitives::hash::{CryptoHash, hash};
use near_primitives::receipt::{DataReceipt, Receipt, ReceiptEnum, ReceiptV0};
use near_primitives::transaction::ExecutionOutcomeWithId;
use near_primitives::types::{AccountId, ProtocolVersion};

impl Chain {
    /// Rewrites the receipt of each transaction among `conversions` that is
    /// to be delivered as a callback, and sends its data receipts behind it.
    /// A refused transaction has no receipt and is forgotten.
    pub(super) fn rewrite_callbacks<'a>(
        &mut self,
        conversions: impl Iterator<Item = &'a ExecutionOutcomeWithId>,
        outgoing: &mut Vec<Receipt>,
        protocol_version: ProtocolVersion,
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
                    as_promise(&mut outgoing[at], None, input_data_ids, protocol_version);
                    outgoing.splice(at + 1..at + 1, data);
                }
                Delivery::From(predecessor) => {
                    as_promise(&mut outgoing[at], Some(predecessor), vec![], protocol_version);
                }
            }
            self.log.injected.insert(id);
        }
    }
}

/// Makes `r`, a transaction's receipt, the receipt a contract makes at
/// `protocol_version` (nearcore's `function_call.rs`: `ActionV3`, or
/// `ActionV2` before `PinReceiptFees`), waiting on `input_data_ids`. From
/// the receiver, with no refund override, it is what `promise_then` on the
/// receiver makes; from another `predecessor`, it refunds a failed deposit
/// to the signer, who paid it.
fn as_promise(
    r: &mut Receipt,
    predecessor: Option<AccountId>,
    input_data_ids: Vec<CryptoHash>,
    protocol_version: ProtocolVersion,
) {
    let Receipt::V0(ReceiptV0 { predecessor_id, receiver_id, receipt, .. }) = r;
    let refund_to = predecessor.is_some();
    *predecessor_id = predecessor.unwrap_or_else(|| receiver_id.clone());
    let a = match &*receipt {
        ReceiptEnum::Action(a) => a.into(),
        ReceiptEnum::ActionV3(a) => a.into(),
        _ => unreachable!("a transaction converts into an Action or ActionV3 receipt"),
    };
    let a: near_primitives::receipt::VersionedActionReceipt<'_> = a;
    let signer_id = a.signer_id().clone();
    *receipt = ReceiptEnum::new_action_receipt_from_promise(
        protocol_version,
        signer_id.clone(),
        refund_to.then_some(signer_id),
        a.authorization_context().into_owned(),
        a.gas_price(),
        a.output_data_receivers().to_vec(),
        input_data_ids,
        a.actions().to_vec(),
        false,
    );
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
