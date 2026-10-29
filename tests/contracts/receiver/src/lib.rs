//! The custody specimen's notify receiver (`tests/res/receiver.wasm`).
//!
//! `on_notify` is what the custody calls after crediting a notified deposit.
//! It answers with how much of each position to refund. This one declines
//! half of every position, so a notified deposit keeps half its credit and
//! the custody refunds the other half to the sender. The amounts it returns
//! have the same length as the positions, so the resolver refunds exactly
//! that; a real failure here would make the resolver refund everything.

use near_sdk::json_types::U128;
use near_sdk::near;

#[near(contract_state)]
#[derive(Default)]
pub struct Receiver {}

#[near]
impl Receiver {
    /// Refund half of each position's share (rounding down).
    pub fn on_notify(&mut self, shares: Vec<U128>) -> Vec<U128> {
        shares.into_iter().map(|s| U128(s.0 / 2)).collect()
    }
}
