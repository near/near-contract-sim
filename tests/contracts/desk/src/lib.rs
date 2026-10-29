//! A ticket desk: the specimen for near-contract-sim's must-not-fail
//! callbacks, headroom-guided search and trie-shaped gas. Built two ways by
//! `build.sh`: `tests/res/desk.wasm` (sized budget) and
//! `tests/res/desk_band.wasm` (feature `band`, a gas band).
//!
//! A small, generic teaching contract, not a model of any deployed one.
//!
//! # The books
//!
//! Each account holds a `balance` of NEAR it deposited and a `pending`
//! amount locked in a redemption that hasn't settled. `total` is the sum of
//! both over every account: the NEAR the desk owes.
//!
//! # The flows
//!
//! - `register()`: adds the caller's account, empty. Storage keyed by the
//!   caller's id, so anyone can add keys next to anyone else's.
//! - `deposit()`: credits the attached NEAR to the caller's balance.
//! - `redeem(tickets, amount)`: moves `amount` from the caller's balance to
//!   pending, asks the issuer to `issue` the tickets, and settles in
//!   `on_issued`, with a gas budget of its own (weight 0: no leftovers).
//! - `on_issued`: if the issuer issued, pays it the pending amount and logs
//!   one event naming every ticket; if it failed, gives the amount back to
//!   the balance. Either way pending drops by `amount`.
//!
//! # Why `on_issued` must not fail
//!
//! It is the only way out of pending. `redeem` already moved the amount,
//! so if `on_issued` fails, the amount stays locked for good: the user's
//! NEAR is neither refunded nor paid.
//!
//! # The budget
//!
//! The event `on_issued` logs lists every ticket, and logging costs gas per
//! byte, so the budget is sized for the batch's bytes.

//! # The gas band (`band`)
//!
//! `redeem` refuses less than [`GUARD`] of prepaid gas, then sends the
//! issuer's `deliver` and its own `release`, both with no gas of their own
//! and weight 1 (they share what is left), and returns that chain, so the
//! transaction succeeds only when `release` does. `release` does
//! [`RELEASE_ROUNDS`] rounds of hashing, then clears the pending amount.
//! Between the guard and the least gas the transaction succeeds at,
//! `deliver` runs and `release` runs out: the amount stays pending.

use near_sdk::json_types::U128;
use near_sdk::serde_json::json;
use near_sdk::store::LookupMap;
use near_sdk::{
    env, near, require, AccountId, Gas, GasWeight, NearToken, PanicOnDefault, Promise,
    PromiseError,
};

/// Most tickets one redemption may carry.
pub const MAX_TICKETS: usize = 64;
/// Longest a ticket id may be.
pub const MAX_TICKET_LEN: usize = 64;
/// Gas the issuer's `issue` gets (static, weight 0).
pub const GAS_FOR_ISSUE: Gas = Gas::from_tgas(10);

/// `on_issued`'s budget for a batch of `bytes` ticket bytes.
///
/// The base plus 1 Ggas per ticket byte, about 2.5 times what parsing and
/// logging a byte costs.
pub fn on_issued_gas(bytes: usize) -> Gas {
    Gas::from_gas(BASE_BUDGET.as_gas() + bytes as u64 * 1_000_000_000)
}

/// What `on_issued` gets before any per-byte share.
pub const BASE_BUDGET: Gas = Gas::from_gas(2_500_000_000_000);

/// Least prepaid gas `redeem` takes (band): not a whole Tgas.
#[cfg(feature = "band")]
pub const GUARD: Gas = Gas::from_ggas(GUARD_TGAS * 1000 + GUARD_GGAS);
#[cfg(feature = "band")]
const GUARD_TGAS: u64 = 54;
#[cfg(feature = "band")]
const GUARD_GGAS: u64 = 70;
/// Hashing rounds `release` does before it clears pending (band).
#[cfg(feature = "band")]
pub const RELEASE_ROUNDS: usize = 1470;

#[near(serializers = [borsh])]
#[derive(Clone, Default)]
pub struct Account {
    balance: u128,
    pending: u128,
}

#[near(contract_state)]
#[derive(PanicOnDefault)]
pub struct Desk {
    issuer: AccountId,
    accounts: LookupMap<AccountId, Account>,
    /// The NEAR the desk owes: every balance and every pending amount.
    total: u128,
}

#[near]
impl Desk {
    #[init]
    pub fn new(issuer: AccountId) -> Self {
        Self { issuer, accounts: LookupMap::new(b"a"), total: 0 }
    }

    /// Adds the caller's account, empty, if it has none.
    pub fn register(&mut self) {
        let who = env::predecessor_account_id();
        if !self.accounts.contains_key(&who) {
            self.accounts.insert(who, Account::default());
        }
    }

    /// Credits the attached NEAR to the caller's balance.
    #[payable]
    pub fn deposit(&mut self) {
        let amount = env::attached_deposit().as_yoctonear();
        require!(amount > 0, "attach a deposit");
        let who = env::predecessor_account_id();
        let mut account = self.accounts.get(&who).cloned().unwrap_or_default();
        account.balance += amount;
        self.accounts.insert(who, account);
        self.total += amount;
    }

    /// Locks `amount` of the caller's balance, asks the issuer to deliver
    /// `tickets`, and returns `release`'s outcome (band).
    #[cfg(feature = "band")]
    pub fn redeem(&mut self, tickets: Vec<String>, amount: U128) -> Promise {
        require!(env::prepaid_gas() >= GUARD, "redeem needs more gas");
        require!(
            !tickets.is_empty() && tickets.len() <= MAX_TICKETS,
            "ticket count out of range"
        );
        let amount = amount.0;
        require!(amount > 0, "redeem must be positive");
        let who = env::predecessor_account_id();
        let mut account = self.accounts.get(&who).cloned().unwrap_or_default();
        require!(account.balance >= amount, "not enough balance");
        account.balance -= amount;
        account.pending += amount;
        self.accounts.insert(who.clone(), account);
        Promise::new(self.issuer.clone())
            .function_call_weight(
                "deliver",
                json!({ "owner": who, "tickets": tickets }).to_string().into_bytes(),
                NearToken::from_yoctonear(0),
                Gas::from_gas(0),
                GasWeight(1),
            )
            .then(
                Self::ext(env::current_account_id())
                    .with_static_gas(Gas::from_gas(0))
                    .with_unused_gas_weight(1)
                    .release(who, U128(amount)),
            )
    }

    /// Clears a redemption's pending amount: paid out if the issuer
    /// delivered, else back to the balance (band).
    #[cfg(feature = "band")]
    #[private]
    pub fn release(&mut self, owner: AccountId, amount: U128) {
        let mut h = owner.as_bytes().to_vec();
        for _ in 0..RELEASE_ROUNDS {
            h = env::sha256(&h);
        }
        let amount = amount.0;
        let delivered = !matches!(env::promise_result_checked(0, 1024), Err(PromiseError::Failed));
        let mut account = self.accounts.get(&owner).cloned().unwrap_or_default();
        account.pending -= amount;
        if delivered {
            self.total -= amount;
        } else {
            account.balance += amount;
        }
        self.accounts.insert(owner, account);
    }

    /// Locks `amount` of the caller's balance and asks the issuer for
    /// `tickets`; `on_issued` settles.
    #[cfg(not(feature = "band"))]
    pub fn redeem(&mut self, tickets: Vec<String>, amount: U128) -> Promise {
        require!(
            !tickets.is_empty() && tickets.len() <= MAX_TICKETS,
            "ticket count out of range"
        );
        require!(tickets.iter().all(|t| t.len() <= MAX_TICKET_LEN), "ticket too long");
        let amount = amount.0;
        require!(amount > 0, "redeem must be positive");
        let who = env::predecessor_account_id();
        let mut account = self.accounts.get(&who).cloned().unwrap_or_default();
        require!(account.balance >= amount, "not enough balance");
        account.balance -= amount;
        account.pending += amount;
        self.accounts.insert(who.clone(), account);

        let bytes: usize = tickets.iter().map(String::len).sum();
        Promise::new(self.issuer.clone())
            .function_call_weight(
                "issue",
                json!({ "owner": who, "tickets": tickets }).to_string().into_bytes(),
                NearToken::from_yoctonear(0),
                GAS_FOR_ISSUE,
                GasWeight(0),
            )
            .then(
                Self::ext(env::current_account_id())
                    .with_static_gas(on_issued_gas(bytes))
                    .with_unused_gas_weight(0)
                    .on_issued(who, tickets, U128(amount)),
            )
    }

    /// Settles a redemption: pays the issuer and logs the tickets, or gives
    /// the amount back if the issuer failed.
    #[private]
    pub fn on_issued(&mut self, owner: AccountId, tickets: Vec<String>, amount: U128) {
        let amount = amount.0;
        let issued = !matches!(env::promise_result_checked(0, 1024), Err(PromiseError::Failed));
        let mut account = self.accounts.get(&owner).cloned().unwrap_or_default();
        account.pending -= amount;
        if issued {
            self.total -= amount;
            Promise::new(self.issuer.clone()).transfer(NearToken::from_yoctonear(amount)).detach();
            let event = json!({
                "standard": "desk",
                "version": "1.0.0",
                "event": "issued",
                "data": [{ "owner": owner, "amount": U128(amount), "tickets": tickets }],
            });
            env::log_str(&format!("EVENT_JSON:{event}"));
        } else {
            account.balance += amount;
        }
        self.accounts.insert(owner, account);
    }

    pub fn balance_of(&self, account_id: AccountId) -> U128 {
        U128(self.accounts.get(&account_id).map_or(0, |a| a.balance))
    }

    pub fn pending_of(&self, account_id: AccountId) -> U128 {
        U128(self.accounts.get(&account_id).map_or(0, |a| a.pending))
    }

    pub fn total(&self) -> U128 {
        U128(self.total)
    }
}
