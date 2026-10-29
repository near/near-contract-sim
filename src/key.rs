//! The storage keys near-sdk's collections write, to read a contract's
//! maps with [`Sim::state`](crate::Sim::state) or set them with
//! [`Sim::set_state`](crate::Sim::set_state). A map's prefix is its own
//! (`LookupMap::new(b"a")`, or the borsh form of a `StorageKey` variant);
//! the entry's key is the borsh form of the map's key, hashed as the map's
//! hasher hashes it.
//!
//! ```
//! use near_contract_sim::key;
//! let alice: near_contract_sim::AccountId = "alice.near".parse().unwrap();
//! // `LookupMap<AccountId, u128>` under prefix b"a": the prefix, then the
//! // id's borsh form (a u32 length, then its bytes).
//! assert_eq!(key::borsh(b"a", &alice), b"a\x0a\x00\x00\x00alice.near");
//! assert_eq!(key::borsh_keccak(b"a", &alice).len(), 1 + 32);
//! ```

use borsh::BorshSerialize;
use sha3::Digest;

fn encoded(key: &impl BorshSerialize) -> Vec<u8> {
    borsh::to_vec(key).expect("a key serializes with borsh")
}

/// `prefix`, then `key`'s borsh form: `near_sdk::store::LookupMap` and
/// `IterableMap` with the default hasher (`Identity`), and every map of
/// `near_sdk::collections`.
pub fn borsh(prefix: &[u8], key: &impl BorshSerialize) -> Vec<u8> {
    [prefix, &encoded(key)].concat()
}

/// `prefix`, then the keccak-256 of `key`'s borsh form: a map with the
/// `Keccak256` hasher.
pub fn borsh_keccak(prefix: &[u8], key: &impl BorshSerialize) -> Vec<u8> {
    [prefix, &sha3::Keccak256::digest(encoded(key))[..]].concat()
}

/// `prefix`, then the sha-256 of `key`'s borsh form: a map with the
/// `Sha256` hasher.
pub fn borsh_sha256(prefix: &[u8], key: &impl BorshSerialize) -> Vec<u8> {
    [prefix, near_primitives::hash::hash(&encoded(key)).as_bytes()].concat()
}
