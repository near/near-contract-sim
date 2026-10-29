//! A journal: the specimen for near-contract-sim's worst-case callback
//! gas. Built by `build.sh` as `tests/res/gasmax.wasm`.
//!
//! A small, generic teaching contract, not a model of any deployed one.
//!
//! # The books
//!
//! Each entry is an id and the bytes appended to it so far, kept in an
//! iterable map: a list of ids, and per id its bytes and its place in the
//! list.
//!
//! # The flows
//!
//! - `put(id, bytes)`: appends `bytes` (at most [`MAX_CHUNK`]) to the
//!   entry `id`, creating it at the end of the list if new.
//! - `remove(id)`: removes the entry. The last entry moves into its place,
//!   so its whole record is rewritten.
//! - `settle(ids)`: asks the ledger to `record` the entries, then removes
//!   them in `on_settled`, with a gas budget of its own:
//!   [`BASE_GAS`] + [`ITEM_GAS`] per id (weight 0: no leftovers).
//! - `on_settled(ids)`: if the ledger recorded them, removes each entry and
//!   counts it settled; if not, leaves them.
//!
//! # Why `on_settled` must not fail
//!
//! The ledger has recorded the entries. If `on_settled` fails they stay,
//! and can be settled again: the ledger counts them twice.
//!
//! # Where its gas goes
//!
//! Removing an entry that isn't last rewrites the last entry's whole
//! record (its new place in the list). So the callback's cost per id
//! depends on the stored state, not on `ids`: on how large the last
//! entries' records are when it runs. A record grows by at most
//! [`MAX_CHUNK`] a put, and the per-id budget fits a record of about
//! 24 KiB: a batch of any size over records that a few puts built
//! fits, and one over a record that many puts built doesn't.

use near_sdk::store::IterableMap;
use near_sdk::{
    env, near, require, AccountId, Gas, GasWeight, NearToken, PanicOnDefault, Promise,
    PromiseError,
};

/// Most bytes one put appends.
pub const MAX_CHUNK: usize = 1024;
/// Longest an id may be.
pub const MAX_ID: usize = 64;
/// Most ids one settle may carry.
pub const MAX_BATCH: usize = 16;
/// Gas the ledger's `record` gets (static, weight 0).
pub const GAS_FOR_RECORD: Gas = Gas::from_tgas(5);
/// `on_settled`'s budget before any per-id share.
pub const BASE_GAS: Gas = Gas::from_gas(1_500_000_000_000);
/// `on_settled`'s budget per id.
pub const ITEM_GAS: Gas = Gas::from_gas(2_000_000_000_000);

/// `on_settled`'s budget for `n` ids: k + m n.
pub fn on_settled_gas(n: usize) -> Gas {
    Gas::from_gas(BASE_GAS.as_gas() + n as u64 * ITEM_GAS.as_gas())
}

#[near(contract_state)]
#[derive(PanicOnDefault)]
pub struct Journal {
    ledger: AccountId,
    entries: IterableMap<String, Vec<u8>>,
    settled: u64,
}

#[near]
impl Journal {
    #[init]
    pub fn new(ledger: AccountId) -> Self {
        Self { ledger, entries: IterableMap::new(b"e"), settled: 0 }
    }

    /// Appends `bytes` to the entry `id`, creating it if new.
    pub fn put(&mut self, id: String, bytes: String) {
        require!(!id.is_empty() && id.len() <= MAX_ID, "id length out of range");
        require!(bytes.len() <= MAX_CHUNK, "chunk too long");
        match self.entries.get_mut(&id) {
            Some(record) => record.extend_from_slice(bytes.as_bytes()),
            None => {
                self.entries.insert(id, bytes.into_bytes());
            }
        }
    }

    /// Removes the entry `id`.
    pub fn remove(&mut self, id: String) {
        require!(self.entries.remove(&id).is_some(), "no such entry");
    }

    /// Asks the ledger to record `ids`; `on_settled` removes them.
    pub fn settle(&mut self, ids: Vec<String>) -> Promise {
        require!(!ids.is_empty() && ids.len() <= MAX_BATCH, "batch size out of range");
        for (i, id) in ids.iter().enumerate() {
            require!(self.entries.contains_key(id), "no such entry");
            require!(!ids[..i].contains(id), "an id twice");
        }
        let n = ids.len();
        let args = near_sdk::serde_json::json!({ "ids": ids }).to_string().into_bytes();
        Promise::new(self.ledger.clone())
            .function_call_weight(
                "record",
                args,
                NearToken::from_yoctonear(0),
                GAS_FOR_RECORD,
                GasWeight(0),
            )
            .then(
                Self::ext(env::current_account_id())
                    .with_static_gas(on_settled_gas(n))
                    .with_unused_gas_weight(0)
                    .on_settled(ids),
            )
    }

    /// Removes the recorded entries, or leaves them if the ledger failed.
    #[private]
    pub fn on_settled(&mut self, ids: Vec<String>) {
        if matches!(env::promise_result_checked(0, 1024), Err(PromiseError::Failed)) {
            return;
        }
        for id in &ids {
            if self.entries.remove(id).is_some() {
                self.settled += 1;
            }
        }
    }

    /// Every id, in list order.
    pub fn ids(&self) -> Vec<String> {
        self.entries.keys().cloned().collect()
    }

    /// The size of the entry `id`'s record, 0 if none.
    pub fn size_of(&self, id: String) -> u64 {
        self.entries.get(&id).map_or(0, |r| r.len() as u64)
    }

    /// How many entries were settled.
    pub fn settled(&self) -> u64 {
        self.settled
    }
}
