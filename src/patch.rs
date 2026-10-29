//! Typed state patching: `contract_state` and `update_contract_state`.
//!
//! A near-sdk contract keeps its root struct, borsh-encoded, under the
//! storage key `STATE`. These read and rewrite it as a type of the test's
//! own, whose fields mirror the contract's in order; a collection field is
//! what near-sdk stores for it there (a `LookupMap` only its prefix, a
//! `Vector` its length too), and its entries are separate keys, which
//! [`Sim::set_state`] writes.

use crate::sim::{IntoAccountId, Sim};
use borsh::{BorshDeserialize, BorshSerialize};
use std::any::type_name;

/// The key near-sdk stores a contract's root struct under.
const STATE: &[u8] = b"STATE";

impl Sim {
    /// The contract's root state, near-sdk's `STATE` key, borsh-decoded as
    /// `T`. The decode must use every byte. Panics, naming `T` and the
    /// stored length, if `id` has no `STATE` (no near-sdk contract, or none
    /// written yet: without `#[init]`, the first `&mut self` call writes it)
    /// or it doesn't decode; panics if `id` doesn't exist.
    #[track_caller]
    pub fn contract_state<T: BorshDeserialize>(&self, id: impl IntoAccountId) -> T {
        let id = id.into_account_id();
        let Some(raw) = self.state(&id, STATE) else {
            panic!(
                "{id} has no \"STATE\" (0 bytes) to read as {}: not a near-sdk contract, \
                 or none written yet (no init, or no `&mut self` call)",
                type_name::<T>()
            );
        };
        T::try_from_slice(&raw).unwrap_or_else(|e| {
            panic!(
                "{id}'s \"STATE\" ({} bytes) doesn't decode as {}: {e}. Its fields must be \
                 the contract's, in order, with each collection as the fields near-sdk \
                 stores for it (a LookupMap's prefix, a Vector's length and prefix)",
                raw.len(),
                type_name::<T>()
            )
        })
    }

    /// Reads the contract's root state as `T` (as
    /// [`contract_state`](Self::contract_state) does, with its panics), lets
    /// `edit` change it, and writes it back with [`set_state`](Self::set_state),
    /// which keeps the account's `storage_usage` right when the length
    /// changes. Directly in the trie: no transaction, no block.
    #[track_caller]
    pub fn update_contract_state<T: BorshSerialize + BorshDeserialize>(
        &mut self,
        id: impl IntoAccountId,
        edit: impl FnOnce(&mut T),
    ) {
        let id = id.into_account_id();
        let mut state = self.contract_state::<T>(&id);
        edit(&mut state);
        let bytes = borsh::to_vec(&state).expect("borsh-encoding the edited state");
        self.set_state(&id, STATE, Some(&bytes));
    }
}
