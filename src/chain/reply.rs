//! Stub replies bound to one transaction (`Tx::stub_reply`): what a stub's
//! method returns to the calls that descend from that transaction, while
//! every other caller gets the stub-wide reply (`Sim::stub_reply`).
//!
//! A stub reads its reply from its own storage. A bound reply goes under
//! the method's key plus `/` and the receipt's `env::random_seed()`, which
//! nearcore derives from the receipt id, the block height, the action's
//! index and the block's seed (`create_random_seed`), so it is unique to
//! one call in one block and the stub can compute it. Before each block,
//! the chain writes the key of every call it is waiting on, for that block;
//! after the block it removes them, which restores the trie exactly (it is
//! content-addressed). Writes go straight to the trie, like `set_state`,
//! without touching the account's storage usage, which the stub never
//! sees. nearcore still runs the stub; only its storage differs.

use super::{Chain, Config, action_receipt};
use near_primitives::action::Action;
use near_primitives::hash::CryptoHash;
use near_primitives::trie_key::TrieKey;
use near_primitives::types::AccountId;
use near_primitives::utils::{create_action_hash_from_receipt_id, create_random_seed};
use node_runtime::ApplyState;

/// A reply a transaction binds: the stub and its method, the storage key
/// of the method's reply (`reply/<i>`), and the encoded reply.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct BoundReply {
    pub stub: AccountId,
    pub method: String,
    pub key: Vec<u8>,
    pub value: Vec<u8>,
}

/// A receipt to a stub, from a transaction that binds a reply for it, not
/// yet run: each of its calls that a bound reply answers, by action index.
#[derive(Clone)]
pub(super) struct Awaiting {
    receipt: CryptoHash,
    stub: AccountId,
    calls: Vec<(usize, BoundReply)>,
}

impl Chain {
    /// Notes each receipt of the next block to a stub whose transaction
    /// binds a reply for one of its calls, unless already noted: once
    /// noted, it is waited on until it runs, delayed or held as it may be.
    pub(super) fn note_awaiting(&mut self) {
        if self.replies.is_empty() {
            return;
        }
        for r in &self.incoming {
            let id = *r.receipt_id();
            let Some(tx) = self.log.origin.get(&id) else { continue };
            let Some(bound) = self.replies.get(tx) else { continue };
            if self.awaiting.iter().any(|a| a.receipt == id) {
                continue;
            }
            let Some(a) = action_receipt(r) else { continue };
            let stub = r.receiver_id();
            let mut calls: Vec<(usize, BoundReply)> = vec![];
            for (i, action) in a.actions().iter().enumerate() {
                let Action::FunctionCall(fc) = action else { continue };
                let queue: Vec<&BoundReply> = bound
                    .iter()
                    .filter(|b| &b.stub == stub && b.method == fc.method_name)
                    .collect();
                let Some(last) = queue.last() else { continue };
                // The method's replies in the order its calls come; the
                // last answers the rest.
                let taken = self.answered.entry((*tx, stub.clone(), fc.method_name.clone()));
                let n = taken.or_default();
                calls.push((i, (*queue.get(*n).unwrap_or(last)).clone()));
                *n += 1;
            }
            if !calls.is_empty() {
                self.awaiting.push(Awaiting { receipt: id, stub: stub.clone(), calls });
            }
        }
    }

    /// Writes, for the block `apply_state` applies, the key of every call
    /// waited on. Returns the keys written, for [`unbind`](Self::unbind).
    pub(super) fn bind(&mut self, config: &Config, apply: &ApplyState) -> Vec<TrieKey> {
        if self.awaiting.is_empty() {
            return vec![];
        }
        let mut writes = vec![];
        for a in &self.awaiting {
            for (i, b) in &a.calls {
                let action = create_action_hash_from_receipt_id(&a.receipt, apply.block_height, *i);
                let seed = create_random_seed(action, apply.random_seed);
                let key = [b.key.as_slice(), b"/", &seed].concat();
                let tk = TrieKey::ContractData { account_id: a.stub.clone(), key };
                writes.push((tk, b.value.clone()));
            }
        }
        let keys = writes.iter().map(|(k, _)| k.clone()).collect();
        self.write(config, |tu| writes.into_iter().for_each(|(k, v)| tu.set(k, v)));
        keys
    }

    /// Removes the keys [`bind`](Self::bind) wrote, and stops waiting on
    /// the receipts that ran.
    pub(super) fn unbind(&mut self, config: &Config, keys: Vec<TrieKey>, ran: &[CryptoHash]) {
        if keys.is_empty() {
            return;
        }
        self.write(config, |tu| keys.into_iter().for_each(|k| tu.remove(k)));
        self.awaiting.retain(|a| !ran.contains(&a.receipt));
    }
}
