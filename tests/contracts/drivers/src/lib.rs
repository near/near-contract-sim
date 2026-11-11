//! Holdings: the specimen for near-contract-sim's max-gas drivers.
//! Built by `build.sh` with the `funded` feature as
//! `tests/res/drivers_funded.wasm`, and with `capped` as
//! `tests/res/drivers_capped.wasm`.
//!
//! A small, generic teaching contract, not a model of any deployed one.
//!
//! # The flows
//!
//! - `fund(account, amount)` sets what `account` may pledge in one call.
//! - `issue(ids)` issues ids; a pledge takes issued ids only ("not
//!   issued"): a creator, then its user.
//! - `pledge(holder, ids, amounts)` takes one amount per id, panicking
//!   "insufficient funds" when their sum is more than the caller's limit:
//!   it fails on the amounts' sum, never on the count. It takes up to
//!   [`MAX_DEPOSIT`] ids and records them in `on_pledged(holder, ids)`,
//!   whose budget is [`BASE_GAS`] + [`PLEDGE_GAS`] per id (weight 0).
//! - `capped` also refuses a pledge of [`CAP`] ids or more ("batch too
//!   long"): a cap on the count.
//!
//! # Where its gas goes
//!
//! `on_pledged` writes one key per id, so its cost grows with the ids. The
//! per-id budget is [`PLEDGE_GAS`]: 30 ids need more than the wallet's
//! 300 Tgas, so past 29 ids the ladder runs at the ceiling's gas, and 64
//! fit the 1 PGas ceiling.

use near_sdk::{env, near, require, AccountId, Gas, PanicOnDefault, Promise};

/// Longest an id may be.
pub const MAX_ID: usize = 64;
/// Most ids one issue or pledge may carry.
pub const MAX_DEPOSIT: usize = 64;
/// `on_pledged`'s budget before any per-id share.
pub const BASE_GAS: Gas = Gas::from_tgas(5);
/// `on_pledged`'s budget per id: 30 ids need more than the wallet's
/// 300 Tgas, 64 fit the 1 PGas ceiling.
#[cfg(feature = "funded")]
pub const PLEDGE_GAS: Gas = Gas::from_tgas(10);
/// The `capped` build refuses a pledge of this many ids or more.
#[cfg(feature = "capped")]
pub const CAP: usize = 32;

#[cfg(feature = "funded")]
fn funds(account: &str) -> u128 {
    env::storage_read(format!("f:{account}").as_bytes())
        .map(|v| u128::from_le_bytes(v.try_into().unwrap_or_else(|_| env::panic_str("corrupt"))))
        .unwrap_or(0)
}

#[cfg(feature = "funded")]
fn set_funds(account: &str, n: u128) {
    env::storage_write(format!("f:{account}").as_bytes(), &n.to_le_bytes());
}

#[near(contract_state)]
#[derive(PanicOnDefault)]
pub struct Holdings {
    /// The ledger `new` was given; nothing calls it since `settle` went.
    #[allow(dead_code)]
    ledger: AccountId,
}

#[near]
impl Holdings {
    #[init]
    pub fn new(ledger: AccountId) -> Self {
        Self { ledger }
    }

    /// Sets what `account` may pledge in one call.
    #[cfg(feature = "funded")]
    pub fn fund(&mut self, account: AccountId, amount: near_sdk::json_types::U128) {
        set_funds(account.as_str(), amount.0);
    }

    /// Issues `ids`: only issued ids may be pledged.
    #[cfg(feature = "funded")]
    pub fn issue(&mut self, ids: Vec<String>) {
        require!(!ids.is_empty() && ids.len() <= MAX_DEPOSIT, "issue size out of range");
        for id in &ids {
            require!(!id.is_empty() && id.len() <= MAX_ID, "id length out of range");
            env::storage_write(format!("i:{id}").as_bytes(), &[1]);
        }
    }

    /// Pledges `ids` to `holder`, one amount each, within the caller's
    /// limit; `on_pledged` records them.
    #[cfg(feature = "funded")]
    pub fn pledge(&mut self, holder: AccountId, ids: Vec<String>, amounts: Vec<near_sdk::json_types::U128>) -> Promise {
        #[cfg(feature = "capped")]
        require!(ids.len() < CAP, "batch too long");
        require!(!ids.is_empty() && ids.len() <= MAX_DEPOSIT, "pledge size out of range");
        require!(amounts.len() == ids.len(), "one amount per id");
        for id in &ids {
            require!(!id.is_empty() && id.len() <= MAX_ID, "id length out of range");
            require!(env::storage_has_key(format!("i:{id}").as_bytes()), "not issued");
        }
        let sum = amounts.iter().try_fold(0u128, |a, x| a.checked_add(x.0));
        if sum.is_none_or(|s| s > funds(env::predecessor_account_id().as_str())) {
            env::panic_str("insufficient funds");
        }
        let gas = Gas::from_gas(BASE_GAS.as_gas() + ids.len() as u64 * PLEDGE_GAS.as_gas());
        Self::ext(env::current_account_id()).with_static_gas(gas).with_unused_gas_weight(0).on_pledged(holder, ids)
    }
}

#[near]
impl Holdings {
    /// Records each pledged id under `holder`.
    #[cfg(feature = "funded")]
    #[private]
    pub fn on_pledged(&mut self, holder: AccountId, ids: Vec<String>) {
        for id in &ids {
            env::storage_write(format!("p:{holder}:{id}").as_bytes(), &[1]);
        }
    }
}
