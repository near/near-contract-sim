//! Pending-receipt reads: every action receipt that hasn't run yet, where it
//! waits. Read from the chain's queues and from state, never written.

use super::order::Wait;
use super::{Chain, action_receipt, method_names, yield_timeout};
use crate::effects::{PendingReceipt, PendingState};
use borsh::BorshDeserialize;
use near_primitives::receipt::Receipt;
use near_primitives::trie_key::col;
use near_primitives::types::PromiseYieldStatus;
use near_store::trie::receipts_column_helper::{
    DelayedReceiptQueue, ShardsOutgoingReceiptBuffer, TrieQueue,
};
use node_runtime::config::total_deposit;
use std::collections::HashMap;

impl Chain {
    /// See `Sim::pending_receipts`.
    pub(crate) fn pending(&self) -> Vec<PendingReceipt> {
        let mut out = vec![];
        for r in &self.incoming {
            self.push_pending(&mut out, r, PendingState::InFlight);
        }
        for (r, wait, _) in &self.holding {
            let state = match *wait {
                Wait::Blocks(blocks_left) => PendingState::Held { blocks_left },
                Wait::Until { tx, i } => {
                    PendingState::HeldUntil { receipt_id: self.target(&tx, i) }
                }
            };
            self.push_pending(&mut out, r, state);
        }
        let tu = self.trie_update();
        let delayed = DelayedReceiptQueue::load(&tu).expect("storage");
        for r in delayed.iter(&tu, false) {
            let r = r.expect("storage").into_receipt();
            self.push_pending(&mut out, &r, PendingState::Buffered);
        }
        let mut buffers = ShardsOutgoingReceiptBuffer::load(&tu).expect("storage");
        for shard in buffers.shards() {
            let buffer = buffers.to_shard(shard);
            for r in buffer.iter(&tu, false) {
                let r = r.expect("storage").into_receipt();
                self.push_pending(&mut out, &r, PendingState::Buffered);
            }
        }
        for r in self.stored(col::POSTPONED_RECEIPT) {
            self.push_pending(&mut out, &r, PendingState::AwaitingResults);
        }
        // A parked yield's callback is listed while its timeout is ahead
        // and no resume was sent; after either, its resume is in flight.
        let idx = near_store::get_promise_yield_indices(&tu).expect("storage");
        let expiry: HashMap<_, _> = (idx.first_index..idx.next_available_index)
            .filter_map(|i| yield_timeout(&tu, i))
            .map(|t| ((t.account_id, t.data_id), t.expires_at))
            .collect();
        let mut parked = vec![];
        for r in self.stored(col::PROMISE_YIELD_RECEIPT) {
            // A yield's callback waits on exactly one result, its data id.
            let Some(data_id) =
                action_receipt(&r).and_then(|a| a.input_data_ids().first().copied())
            else {
                continue;
            };
            let account = r.receiver_id().clone();
            let resumed = near_store::get_promise_yield_status(&tu, &account, data_id)
                .expect("storage")
                == Some(PromiseYieldStatus::ResumeInitiated);
            let state = match expiry.get(&(account, data_id)) {
                Some(&expires_at) if !resumed => PendingState::Yield { expires_at },
                _ => PendingState::AwaitingResults,
            };
            parked.push((state, r));
        }
        // In the order they time out.
        parked.sort_by_key(|(s, _)| match s {
            PendingState::Yield { expires_at } => *expires_at,
            _ => 0,
        });
        for (state, r) in parked {
            self.push_pending(&mut out, &r, state);
        }
        out
    }

    /// Every receipt stored under the state column `c`.
    fn stored(&self, c: u8) -> Vec<Receipt> {
        let trie = self.trie();
        let mut it = trie.disk_iter().expect("storage");
        it.seek_prefix([c]).expect("storage");
        it.map(|e| {
            let (_, v) = e.expect("storage");
            Receipt::try_from_slice(&v).expect("a receipt in state")
        })
        .collect()
    }

    /// Lists `r` as pending in `state`, unless it is a data receipt.
    fn push_pending(&self, out: &mut Vec<PendingReceipt>, r: &Receipt, state: PendingState) {
        let Some(a) = action_receipt(r) else { return };
        let id = *r.receipt_id();
        out.push(PendingReceipt {
            receipt_id: id,
            tx: self.log.origin.get(&id).copied().unwrap_or(id),
            predecessor_id: r.predecessor_id().clone(),
            receiver_id: r.receiver_id().clone(),
            method_names: method_names(a.actions()),
            deposit: total_deposit(a.actions()).expect("deposit overflow"),
            state,
        });
    }
}
