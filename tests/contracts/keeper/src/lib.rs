//! Keeper: the counterparty specimen for near-contract-sim's growth.
//! Built by `build.sh` as `tests/res/keeper.wasm`, with the `closed`
//! feature as `tests/res/keeper_closed.wasm`. With the `ping` feature (a
//! self-callback that touches no key), as `keeper_ping.wasm`, and with
//! `ping` and `hashed`, as `keeper_ping_hashed.wasm`.
//!
//! A small, generic teaching contract, not a model of any deployed one.
//!
//! # The books
//!
//! A token contract transfers ids to the keeper with a message; the keeper
//! credits them, per account, as entries of a list. Raw storage, so the
//! keys read plainly:
//! - `n:<account>`: how many entries (u32, little-endian);
//! - `l:<account>:<index as 8 hex digits>`: the entry at that place, its
//!   key `<token>:<id>`;
//! - `e:<account>:<token>:<id>`: that entry's place (u32) and balance
//!   (u128), little-endian.
//!
//! With `hashed`, every key is the SHA-256 of the plain key above, so no
//! account appears in plain bytes, and `on_transfer` takes the token's
//! `previous_owner_ids` too (a list as long as `token_ids`).
//!
//! # The flows
//!
//! - `on_transfer(sender_id, token_ids, amounts, msg)`, called by a token:
//!   credits each id (as `<token>:<id>`) to `msg.receiver` (else the
//!   sender), appending an entry for an id the account doesn't hold.
//!   `keeper.wasm` takes any caller as the token; `keeper_closed.wasm`
//!   only the token it was made with.
//! - With `msg.notify`, it calls the receiver's `on_transfer` with the ids
//!   and the notify text, then its own `on_resolve`, with a gas budget of
//!   its own: [`RESOLVE_BASE`] + [`RESOLVE_PER_ID`] per id (weight 0).
//! - `on_resolve`: what the receiver returned unused is refunded, to the
//!   token's caller through its return value. An entry refunded in full
//!   is removed: the list's *last* entry moves into the freed place, and
//!   that entry's place is rewritten.
//!
//! # `ping` (the `ping` feature)
//!
//! - `ping(account, ids)`: credits each id (as `ping:<id>`) to `account`,
//!   as `on_transfer` credits, then schedules its own `on_ping(account,
//!   n)` with a fixed budget of its own ([`ON_PING_GAS`], weight 0).
//! - `on_ping` hashes [`PING_ROUNDS`] rounds per id and logs: its use grows
//!   with n, and it reads and writes no key but `STATE`.
//!
//! # Why `on_resolve` must not fail
//!
//! The token has moved the ids to the keeper. If `on_resolve` fails, the
//! refund is lost: the token's resolver reads nothing back and keeps the
//! ids with the keeper, while the keeper still credits the receiver.
//!
//! # Where its gas goes
//!
//! Removing the last entry is cheap: it frees its own place. Removing one
//! in the middle moves the last entry: a read of its key, a write of it
//! into the freed place, and a rewrite of its place under its own (long)
//! key. So the resolver's cost per refunded id depends on what sits after
//! the deposited ids when it runs, and on those entries' key length: a
//! tail of long ids credited to the receiver *after* the deposit and
//! *before* the resolver runs makes every refund a middle removal of a
//! long key. Entries that were there before the deposit leave the
//! deposited ids last, and most refunds cheap.

use near_sdk::json_types::U128;
use near_sdk::serde::Deserialize;
use near_sdk::{env, near, require, AccountId, Gas, GasWeight, NearToken, PanicOnDefault, Promise, PromiseError, PromiseOrValue};

/// Longest an id may be.
pub const MAX_ID: usize = 127;
/// Most ids one transfer may carry.
pub const MAX_IDS: usize = 64;
/// Gas the receiver's `on_transfer` gets (static, weight 0).
pub const GAS_FOR_NOTIFY: Gas = Gas::from_tgas(10);
/// `on_resolve`'s budget before any per-id share.
pub const RESOLVE_BASE: Gas = Gas::from_tgas(3);
/// `on_resolve`'s budget per id.
pub const RESOLVE_PER_ID: Gas = Gas::from_ggas(650);

/// `on_ping`'s fixed budget (`ping`).
#[cfg(feature = "ping")]
pub const ON_PING_GAS: Gas = Gas::from_tgas(4);
/// Hashing rounds `on_ping` does per id (`ping`).
#[cfg(feature = "ping")]
pub const PING_ROUNDS: u32 = 12;

/// `on_resolve`'s share of the caller's unused gas: none.
pub const RESOLVE_WEIGHT: u64 = 0;

/// `on_resolve`'s budget for `n` ids: k + m n.
pub fn resolve_gas(n: usize) -> Gas {
    Gas::from_gas(RESOLVE_BASE.as_gas() + n as u64 * RESOLVE_PER_ID.as_gas())
}

/// A storage key: the plain key, or with `hashed` its SHA-256.
fn stored(plain: String) -> Vec<u8> {
    #[cfg(feature = "hashed")]
    return env::sha256(plain.as_bytes());
    #[cfg(not(feature = "hashed"))]
    plain.into_bytes()
}

fn count_key(account: &str) -> Vec<u8> {
    stored(format!("n:{account}"))
}

fn slot_key(account: &str, i: u32) -> Vec<u8> {
    stored(format!("l:{account}:{i:08x}"))
}

fn entry_key(account: &str, key: &str) -> Vec<u8> {
    stored(format!("e:{account}:{key}"))
}

fn len_of(account: &str) -> u32 {
    env::storage_read(&count_key(account))
        .map(|v| u32::from_le_bytes(v.try_into().unwrap_or_else(|_| env::panic_str("corrupt"))))
        .unwrap_or(0)
}

fn set_count(account: &str, n: u32) {
    match n {
        0 => {
            env::storage_remove(&count_key(account));
        }
        n => {
            env::storage_write(&count_key(account), &n.to_le_bytes());
        }
    }
}

/// An entry's place and balance.
fn entry_of(account: &str, key: &str) -> Option<(u32, u128)> {
    let v = env::storage_read(&entry_key(account, key))?;
    require!(v.len() == 20, "corrupt");
    let at = u32::from_le_bytes(v[..4].try_into().unwrap());
    let balance = u128::from_le_bytes(v[4..].try_into().unwrap());
    Some((at, balance))
}

fn set_entry(account: &str, key: &str, at: u32, balance: u128) {
    let mut v = at.to_le_bytes().to_vec();
    v.extend(balance.to_le_bytes());
    env::storage_write(&entry_key(account, key), &v);
}

/// What `msg` asks: the account credited, and the text to notify it with.
#[derive(Deserialize, Default)]
#[serde(crate = "near_sdk::serde")]
struct Msg {
    receiver: Option<AccountId>,
    notify: Option<String>,
}

#[near(contract_state)]
#[derive(PanicOnDefault)]
pub struct Keeper {
    /// The token `keeper_closed.wasm` takes transfers from.
    token: AccountId,
}

#[near]
impl Keeper {
    #[init]
    pub fn new(token: AccountId) -> Self {
        Self { token }
    }

    /// Credits `token_ids` from the calling token to `msg.receiver` (else
    /// the sender); with `msg.notify`, tells the receiver and refunds what
    /// it returns unused. Returns the amounts unused.
    #[cfg(not(feature = "hashed"))]
    pub fn on_transfer(
        &mut self,
        sender_id: AccountId,
        token_ids: Vec<String>,
        amounts: Vec<U128>,
        msg: String,
    ) -> PromiseOrValue<Vec<U128>> {
        self.credit(sender_id, token_ids, amounts, msg)
    }

    /// As without `hashed`, with the token's `previous_owner_ids` (one per
    /// id; not read beyond its length).
    #[cfg(feature = "hashed")]
    pub fn on_transfer(
        &mut self,
        sender_id: AccountId,
        previous_owner_ids: Vec<AccountId>,
        token_ids: Vec<String>,
        amounts: Vec<U128>,
        msg: String,
    ) -> PromiseOrValue<Vec<U128>> {
        require!(previous_owner_ids.len() == token_ids.len(), "transfer size out of range");
        self.credit(sender_id, token_ids, amounts, msg)
    }

    /// Refunds what the receiver returned unused (all of it if it failed),
    /// removing each entry refunded in full. Returns the refunds.
    #[private]
    pub fn on_resolve(
        &mut self,
        receiver: AccountId,
        token: AccountId,
        token_ids: Vec<String>,
        amounts: Vec<U128>,
    ) -> Vec<U128> {
        self.resolve(receiver, token, token_ids, amounts)
    }

    /// Credits `ids` to `account`, then pings itself.
    #[cfg(feature = "ping")]
    pub fn ping(&mut self, account: AccountId, ids: Vec<String>) -> Promise {
        let n = ids.len();
        require!(n > 0 && n <= MAX_IDS, "ping size out of range");
        let r = account.as_str();
        let mut len = len_of(r);
        for id in &ids {
            require!(!id.is_empty() && id.len() <= MAX_ID, "id length out of range");
            let key = format!("ping:{id}");
            match entry_of(r, &key) {
                Some((at, balance)) => set_entry(r, &key, at, balance + 1),
                None => {
                    env::storage_write(&slot_key(r, len), key.as_bytes());
                    set_entry(r, &key, len, 1);
                    len += 1;
                }
            }
        }
        set_count(r, len);
        Self::ext(env::current_account_id())
            .with_static_gas(ON_PING_GAS)
            .with_unused_gas_weight(0)
            .on_ping(account, n as u32)
    }

    /// Logs the ping; touches no key.
    #[cfg(feature = "ping")]
    #[private]
    pub fn on_ping(&mut self, account: AccountId, n: u32) {
        let mut h = account.as_bytes().to_vec();
        for _ in 0..n * PING_ROUNDS {
            h = env::sha256(&h);
        }
        env::log_str(&format!("pinged {account} with {n} id(s)"));
    }
}

impl Keeper {
    fn credit(
        &mut self,
        sender_id: AccountId,
        token_ids: Vec<String>,
        amounts: Vec<U128>,
        msg: String,
    ) -> PromiseOrValue<Vec<U128>> {
        let token = env::predecessor_account_id();
        #[cfg(feature = "closed")]
        require!(token == self.token, "only the token may transfer");
        let n = token_ids.len();
        require!(n > 0 && n <= MAX_IDS && amounts.len() == n, "transfer size out of range");
        let msg: Msg = near_sdk::serde_json::from_str(&msg).unwrap_or_default();
        let receiver = msg.receiver.unwrap_or_else(|| sender_id.clone());
        let r = receiver.as_str();
        let mut len = len_of(r);
        for (id, amount) in token_ids.iter().zip(&amounts) {
            require!(!id.is_empty() && id.len() <= MAX_ID, "id length out of range");
            require!(amount.0 > 0, "zero amount");
            let key = format!("{token}:{id}");
            match entry_of(r, &key) {
                Some((at, balance)) => set_entry(r, &key, at, balance + amount.0),
                None => {
                    env::storage_write(&slot_key(r, len), key.as_bytes());
                    set_entry(r, &key, len, amount.0);
                    len += 1;
                }
            }
        }
        set_count(r, len);
        let Some(notify) = msg.notify else {
            return PromiseOrValue::Value(vec![U128(0); n]);
        };
        let args = near_sdk::serde_json::json!({
            "sender_id": sender_id,
            "token_ids": token_ids,
            "amounts": amounts,
            "msg": notify,
        });
        Promise::new(receiver.clone())
            .function_call_weight(
                "on_transfer",
                args.to_string().into_bytes(),
                NearToken::from_yoctonear(0),
                GAS_FOR_NOTIFY,
                GasWeight(0),
            )
            .then(
                Self::ext(env::current_account_id())
                    .with_static_gas(resolve_gas(n))
                    .with_unused_gas_weight(RESOLVE_WEIGHT)
                    .on_resolve(receiver, token, token_ids, amounts),
            )
            .into()
    }

    fn resolve(
        &mut self,
        receiver: AccountId,
        token: AccountId,
        token_ids: Vec<String>,
        amounts: Vec<U128>,
    ) -> Vec<U128> {
        let unused: Vec<U128> = match env::promise_result_checked(0, 16 * 1024) {
            Ok(bytes) => near_sdk::serde_json::from_slice(&bytes).unwrap_or_else(|_| amounts.clone()),
            Err(PromiseError::Failed) => amounts.clone(),
            Err(_) => env::panic_str("promise result too long"),
        };
        let r = receiver.as_str();
        let mut refunds = vec![];
        for (i, (id, amount)) in token_ids.iter().zip(&amounts).enumerate() {
            let asked = unused.get(i).map_or(0, |u| u.0.min(amount.0));
            let key = format!("{token}:{id}");
            let Some((at, balance)) = entry_of(r, &key) else {
                refunds.push(U128(0));
                continue;
            };
            let refund = asked.min(balance);
            refunds.push(U128(refund));
            if refund == 0 {
                continue;
            }
            if refund < balance {
                set_entry(r, &key, at, balance - refund);
                continue;
            }
            // Refunded in full: the last entry moves into its place.
            let len = len_of(r);
            let last = len - 1;
            if at != last {
                let moved = env::storage_read(&slot_key(r, last)).unwrap_or_else(|| env::panic_str("corrupt"));
                env::storage_write(&slot_key(r, at), &moved);
                let moved = String::from_utf8(moved).unwrap_or_else(|_| env::panic_str("corrupt"));
                let (_, b) = entry_of(r, &moved).unwrap_or_else(|| env::panic_str("corrupt"));
                set_entry(r, &moved, at, b);
            }
            env::storage_remove(&slot_key(r, last));
            env::storage_remove(&entry_key(r, &key));
            set_count(r, last);
        }
        refunds
    }

}

#[near]
impl Keeper {
    /// `account`'s entries, in place order: (`<token>:<id>`, balance).
    pub fn entries(&self, account: AccountId) -> Vec<(String, U128)> {
        let a = account.as_str();
        (0..len_of(a))
            .map(|i| {
                let key = String::from_utf8(env::storage_read(&slot_key(a, i)).unwrap_or_default()).unwrap_or_default();
                let balance = entry_of(a, &key).map_or(0, |e| e.1);
                (key, U128(balance))
            })
            .collect()
    }

    /// How many entries `account` holds.
    pub fn count(&self, account: AccountId) -> u32 {
        len_of(account.as_str())
    }
}
