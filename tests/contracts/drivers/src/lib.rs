//! Holdings: the specimen for near-contract-sim's max-gas drivers.
//! Built by `build.sh` with the `funded` feature as
//! `tests/res/drivers_funded.wasm`, and with `capped` as
//! `tests/res/drivers_capped.wasm`.
//!
//! A small, generic teaching contract, not a model of any deployed one.
//!
//! # The books
//!
//! Each holder holds an ordered list of distinct ids. Raw storage, so the
//! keys read plainly:
//! - `n:<holder>`: how many entries (u32, little-endian);
//! - `h:<holder>:<index as 8 hex digits>`: the id at that place;
//! - `m:<holder>:<id>`: that id's place (u32, little-endian).
//!
//! # The flows
//!
//! - `deposit(holder, ids)`: appends each id `holder` doesn't hold yet;
//!   repeated ids merge into one entry.
//! - `settle(holder, ids)`: asks the ledger to `record` them, then removes
//!   them in `on_settled`, with a gas budget of its own: [`BASE_GAS`] +
//!   [`ITEM_GAS`] per id (weight 0: no leftovers).
//! - `on_settled(holder, ids)`: if the ledger recorded them, removes each
//!   id still held. Removing one shifts every later entry down one place,
//!   rewriting its entry and its place.
//!
//! # `funded` and `capped`
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
//! # Why `on_settled` must not fail
//!
//! The ledger has recorded the ids. If `on_settled` fails they stay held,
//! and can be settled again: the ledger counts them twice.
//!
//! # Where its gas goes
//!
//! Removing an id costs one shift per entry after it, so the callback's
//! cost per id grows with how many entries `holder` holds after that id,
//! not with `ids`. The per-id budget fits about [`FITS`] entries after it:
//! many distinct ids under one holder, settled from the front, run it out.
//! Repeating one id merges into one entry, ids spread over many holders
//! keep each list short, and a longer batch only adds budget.

use near_sdk::{env, near, require, AccountId, Gas, GasWeight, NearToken, PanicOnDefault, Promise, PromiseError};

/// Longest an id may be.
pub const MAX_ID: usize = 64;
/// Most ids one deposit may carry.
pub const MAX_DEPOSIT: usize = 64;
/// Most ids one settle may carry.
pub const MAX_BATCH: usize = 2;
/// Gas the ledger's `record` gets (static, weight 0).
pub const GAS_FOR_RECORD: Gas = Gas::from_tgas(5);
/// `on_settled`'s budget before any per-id share.
pub const BASE_GAS: Gas = Gas::from_tgas(5);
/// `on_settled`'s budget per id.
pub const ITEM_GAS: Gas = Gas::from_tgas(120);
/// `on_pledged`'s budget per id: 30 ids need more than the wallet's
/// 300 Tgas, 64 fit the 1 PGas ceiling.
#[cfg(feature = "funded")]
pub const PLEDGE_GAS: Gas = Gas::from_tgas(10);
/// The `capped` build refuses a pledge of this many ids or more.
#[cfg(feature = "capped")]
pub const CAP: usize = 32;
/// About how many entries after an id its budget covers (documentation).
pub const FITS: usize = 400;

/// `on_settled`'s budget for `n` ids: k + m n.
pub fn on_settled_gas(n: usize) -> Gas {
    Gas::from_gas(BASE_GAS.as_gas() + n as u64 * ITEM_GAS.as_gas())
}

fn count_key(holder: &str) -> Vec<u8> {
    format!("n:{holder}").into_bytes()
}

fn entry_key(holder: &str, i: u32) -> Vec<u8> {
    format!("h:{holder}:{i:08x}").into_bytes()
}

fn place_key(holder: &str, id: &str) -> Vec<u8> {
    format!("m:{holder}:{id}").into_bytes()
}

fn read_u32(key: &[u8]) -> Option<u32> {
    env::storage_read(key).map(|v| u32::from_le_bytes(v.try_into().unwrap_or_else(|_| env::panic_str("corrupt"))))
}

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

fn held(holder: &str) -> u32 {
    read_u32(&count_key(holder)).unwrap_or(0)
}

fn set_count(holder: &str, n: u32) {
    match n {
        0 => {
            env::storage_remove(&count_key(holder));
        }
        n => {
            env::storage_write(&count_key(holder), &n.to_le_bytes());
        }
    }
}

#[near(contract_state)]
#[derive(PanicOnDefault)]
pub struct Holdings {
    ledger: AccountId,
    settled: u64,
}

#[near]
impl Holdings {
    #[init]
    pub fn new(ledger: AccountId) -> Self {
        Self { ledger, settled: 0 }
    }

    /// Appends each id `holder` doesn't hold yet.
    pub fn deposit(&mut self, holder: AccountId, ids: Vec<String>) {
        require!(!ids.is_empty() && ids.len() <= MAX_DEPOSIT, "deposit size out of range");
        let h = holder.as_str();
        let mut n = held(h);
        for id in &ids {
            require!(!id.is_empty() && id.len() <= MAX_ID, "id length out of range");
            if env::storage_has_key(&place_key(h, id)) {
                continue;
            }
            env::storage_write(&entry_key(h, n), id.as_bytes());
            env::storage_write(&place_key(h, id), &n.to_le_bytes());
            n += 1;
        }
        set_count(h, n);
    }

    /// Asks the ledger to record `ids`; `on_settled` removes them.
    pub fn settle(&mut self, holder: AccountId, ids: Vec<String>) -> Promise {
        require!(!ids.is_empty() && ids.len() <= MAX_BATCH, "batch size out of range");
        for id in &ids {
            require!(env::storage_has_key(&place_key(holder.as_str(), id)), "not held");
        }
        let n = ids.len();
        let args = near_sdk::serde_json::json!({ "holder": holder, "ids": ids }).to_string().into_bytes();
        Promise::new(self.ledger.clone())
            .function_call_weight("record", args, NearToken::from_yoctonear(0), GAS_FOR_RECORD, GasWeight(0))
            .then(
                Self::ext(env::current_account_id())
                    .with_static_gas(on_settled_gas(n))
                    .with_unused_gas_weight(0)
                    .on_settled(holder, ids),
            )
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
    /// Removes the recorded ids still held, or leaves them if the ledger
    /// failed.
    #[private]
    pub fn on_settled(&mut self, holder: AccountId, ids: Vec<String>) {
        if matches!(env::promise_result_checked(0, 1024), Err(PromiseError::Failed)) {
            return;
        }
        let h = holder.as_str();
        for id in &ids {
            let Some(at) = read_u32(&place_key(h, id)) else { continue };
            let n = held(h);
            // Every later entry moves down one place.
            for i in at + 1..n {
                let moved = env::storage_read(&entry_key(h, i)).unwrap_or_else(|| env::panic_str("corrupt"));
                env::storage_write(&entry_key(h, i - 1), &moved);
                let moved = String::from_utf8(moved).unwrap_or_else(|_| env::panic_str("corrupt"));
                env::storage_write(&place_key(h, &moved), &(i - 1).to_le_bytes());
            }
            env::storage_remove(&entry_key(h, n - 1));
            env::storage_remove(&place_key(h, id));
            set_count(h, n - 1);
            self.settled += 1;
        }
    }

    /// Records each pledged id under `holder`.
    #[cfg(feature = "funded")]
    #[private]
    pub fn on_pledged(&mut self, holder: AccountId, ids: Vec<String>) {
        for id in &ids {
            env::storage_write(format!("p:{holder}:{id}").as_bytes(), &[1]);
        }
    }

    /// `holder`'s ids, in order.
    pub fn ids(&self, holder: AccountId) -> Vec<String> {
        let h = holder.as_str();
        (0..held(h))
            .map(|i| String::from_utf8(env::storage_read(&entry_key(h, i)).unwrap_or_default()).unwrap_or_default())
            .collect()
    }

    /// How many entries `holder` holds.
    pub fn count(&self, holder: AccountId) -> u32 {
        held(holder.as_str())
    }

    /// How many ids were settled.
    pub fn settled(&self) -> u64 {
        self.settled
    }
}
