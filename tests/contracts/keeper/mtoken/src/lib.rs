//! A minimal multi-token for the keeper specimen. Built by
//! `../build.sh` as `tests/res/mtoken.wasm`, and with the `events` feature
//! as `tests/res/mtoken_events.wasm`.
//!
//! - `new(owner, ids)` mints [`SUPPLY`] of each of `ids` to the owner, so
//!   ids made at init exist only in the state the init left;
//! - `create(id)` (owner only) mints [`SUPPLY`] of a new id to the owner;
//! - `give(to, id, amount)` moves the caller's;
//! - `batch_transfer_call(receiver_id, token_ids, amounts, msg)` moves the
//!   caller's ids to `receiver_id`, calls its `on_transfer(sender_id,
//!   token_ids, amounts, msg)`, then `resolve`, which moves back what the
//!   receiver returned unused (all of it if the call failed).
//!
//! Balances are raw storage: `b:<account>:<id>` (u128, little-endian).
//!
//! With the `events` feature (`tests/res/mtoken_events.wasm`)
//! `create` and `give` log NEP-245 `mt_mint` / `mt_transfer` events, and
//! the standard views `mt_balance_of(account_id, token_id)` and
//! `mt_supply(token_id)` answer; both panic for an id never created, as a
//! token that keeps a registry of ids does.

use near_sdk::json_types::U128;
use near_sdk::{env, near, require, AccountId, Gas, GasWeight, NearToken, PanicOnDefault, Promise, PromiseError};

/// What `create` mints.
pub const SUPPLY: u128 = 1_000_000_000;
/// Gas `resolve` gets (static, weight 0).
pub const GAS_FOR_RESOLVE: Gas = Gas::from_tgas(20);

fn key(account: &str, id: &str) -> Vec<u8> {
    format!("b:{account}:{id}").into_bytes()
}

fn balance_in(account: &str, id: &str) -> u128 {
    env::storage_read(&key(account, id))
        .map(|v| u128::from_le_bytes(v.try_into().unwrap_or_else(|_| env::panic_str("corrupt"))))
        .unwrap_or(0)
}

fn set(account: &str, id: &str, amount: u128) {
    match amount {
        0 => {
            env::storage_remove(&key(account, id));
        }
        n => {
            env::storage_write(&key(account, id), &n.to_le_bytes());
        }
    }
}

#[cfg(feature = "events")]
fn supply_key(id: &str) -> Vec<u8> {
    format!("s:{id}").into_bytes()
}

/// Logs a NEP-245 event.
#[cfg(feature = "events")]
fn event(name: &str, data: near_sdk::serde_json::Value) {
    let e = near_sdk::serde_json::json!({
        "standard": "nep245", "version": "1.0.0", "event": name, "data": [data],
    });
    env::log_str(&format!("EVENT_JSON:{e}"));
}

fn move_balance_in(from: &str, to: &str, id: &str, amount: u128) {
    let have = balance_in(from, id);
    require!(have >= amount, "insufficient balance");
    set(from, id, have - amount);
    set(to, id, balance_in(to, id) + amount);
}

#[near(contract_state)]
#[derive(PanicOnDefault)]
pub struct MToken {
    owner: AccountId,
}

#[near]
impl MToken {
    #[init]
    pub fn new(owner: AccountId, ids: Vec<String>) -> Self {
        for id in &ids {
            require!(!id.is_empty() && id.len() <= 127, "id length out of range");
            set(owner.as_str(), id, SUPPLY);
        }
        Self { owner }
    }

    /// Mints [`SUPPLY`] of a new `id` to the owner.
    pub fn create(&mut self, id: String) {
        require!(env::predecessor_account_id() == self.owner, "only the owner creates");
        require!(!id.is_empty() && id.len() <= 127, "id length out of range");
        require!(balance_in(self.owner.as_str(), &id) == 0, "exists");
        set(self.owner.as_str(), &id, SUPPLY);
        #[cfg(feature = "events")]
        {
            env::storage_write(&supply_key(&id), &SUPPLY.to_le_bytes());
            event("mt_mint", near_sdk::serde_json::json!({
                "owner_id": self.owner, "token_ids": [id], "amounts": [SUPPLY.to_string()],
            }));
        }
    }

    /// Moves `amount` of the caller's `id` to `to`.
    pub fn give(&mut self, to: AccountId, id: String, amount: U128) {
        move_balance_in(env::predecessor_account_id().as_str(), to.as_str(), &id, amount.0);
        #[cfg(feature = "events")]
        event("mt_transfer", near_sdk::serde_json::json!({
            "old_owner_id": env::predecessor_account_id(), "new_owner_id": to,
            "token_ids": [id], "amounts": [amount],
        }));
    }

    /// Moves the caller's ids to `receiver_id` and tells it.
    pub fn batch_transfer_call(&mut self, receiver_id: AccountId, token_ids: Vec<String>, amounts: Vec<U128>, msg: String) -> Promise {
        require!(!token_ids.is_empty() && token_ids.len() == amounts.len(), "transfer size out of range");
        let sender = env::predecessor_account_id();
        for (id, amount) in token_ids.iter().zip(&amounts) {
            require!(amount.0 > 0, "zero amount");
            move_balance_in(sender.as_str(), receiver_id.as_str(), id, amount.0);
        }
        let args = near_sdk::serde_json::json!({
            "sender_id": sender,
            "token_ids": token_ids,
            "amounts": amounts,
            "msg": msg,
        });
        Promise::new(receiver_id.clone())
            .function_call_weight("on_transfer", args.to_string().into_bytes(), NearToken::from_yoctonear(0), Gas::from_gas(0), GasWeight(1))
            .then(
                Self::ext(env::current_account_id())
                    .with_static_gas(GAS_FOR_RESOLVE)
                    .with_unused_gas_weight(0)
                    .resolve(sender, receiver_id, token_ids, amounts),
            )
    }

    /// Moves back what the receiver returned unused.
    #[private]
    pub fn resolve(&mut self, sender: AccountId, receiver: AccountId, token_ids: Vec<String>, amounts: Vec<U128>) -> Vec<U128> {
        let unused: Vec<U128> = match env::promise_result_checked(0, 16 * 1024) {
            Ok(bytes) => near_sdk::serde_json::from_slice(&bytes).unwrap_or_else(|_| amounts.clone()),
            Err(PromiseError::Failed) => amounts.clone(),
            Err(_) => env::panic_str("promise result too long"),
        };
        let mut used = vec![];
        for (i, (id, amount)) in token_ids.iter().zip(&amounts).enumerate() {
            let back = unused.get(i).map_or(0, |u| u.0.min(amount.0)).min(balance_in(receiver.as_str(), id));
            if back > 0 {
                move_balance_in(receiver.as_str(), sender.as_str(), id, back);
            }
            used.push(U128(amount.0 - back));
        }
        used
    }

    /// `account`'s balance of `id`.
    pub fn balance_of(&self, account: AccountId, id: String) -> U128 {
        U128(balance_in(account.as_str(), &id))
    }

    /// NEP-245's balance view; panics for an id never created.
    #[cfg(feature = "events")]
    pub fn mt_balance_of(&self, account_id: AccountId, token_id: String) -> U128 {
        require!(env::storage_has_key(&supply_key(&token_id)), "no such token");
        U128(balance_in(account_id.as_str(), &token_id))
    }

    /// NEP-245's supply view; panics for an id never created.
    #[cfg(feature = "events")]
    pub fn mt_supply(&self, token_id: String) -> U128 {
        let v = env::storage_read(&supply_key(&token_id)).unwrap_or_else(|| env::panic_str("no such token"));
        U128(u128::from_le_bytes(v.try_into().unwrap_or_else(|_| env::panic_str("corrupt"))))
    }
}
