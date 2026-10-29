//! Token vault: the specimen contract for near-contract-sim's examples and
//! search acceptance tests. Built twice by `build.sh`:
//! `tests/res/vault.wasm` (fixed) and, with the `forgetful` feature,
//! `tests/res/vault_forgetful.wasm` (the planted bug).
//!
//! # Interface
//!
//! - `new(token_id)`: init; `token_id` is the NEP-141 token it holds.
//! - `ft_on_transfer(sender_id, amount, msg)`: a deposit, via the token's
//!   `ft_transfer_call`. Credits `amount` shares 1:1 to `sender_id` and
//!   returns `"0"` (nothing to refund). Only the token may call it.
//! - `withdraw(amount)`, 1 yocto: debits `amount` shares, then
//!   `token.ft_transfer(caller, amount)` and the callback `on_withdraw`,
//!   whose `U128` (the amount paid) is the transaction's result. Refuses
//!   more than credited deposits back (`total_shares - total_rewards`):
//!   see "Why a withdraw pays only credited tokens". **`forgetful`
//!   doesn't refuse.**
//! - `on_withdraw(account_id, amount)`, private: if the payout failed,
//!   gives the shares back. **`forgetful` doesn't**: the shares are gone and
//!   no tokens came out.
//! - `claim_rewards()`: once per 24 h per account (by `block_timestamp`),
//!   credits `REWARD` shares and adds them to `total_rewards`; rewards are
//!   not backed by tokens, so the vault owes `total_shares - total_rewards`
//!   in tokens. The fixed build pays reward shares only out of credited
//!   deposits, other users' included, by design. Then a detached,
//!   best-effort `rewards.near::on_claim` notification that nothing waits on: no `rewards.near` account exists
//!   in the examples, so it fails unhandled, benign by design.
//! - Views: `shares_of(account_id)`, `total_shares()`, `total_rewards()`,
//!   each a `U128` (a JSON string).
//!
//! `forgetful` is the vault before its audit, so it differs in both places
//! marked: it pays any withdraw it can debit, and forgets to give back the
//! shares of a payout that failed. Without the backing guard, a withdraw of
//! reward shares from a vault that holds no tokens fails its payout with no
//! fault forced, which is how a search reaches the forgotten re-credit
//! naturally.
//!
//! # State layout (borsh, key `STATE`)
//!
//! ```text
//! struct Vault {
//!     token_id: AccountId,               // String: u32 LE length + bytes
//!     shares: LookupMap<AccountId, u128>, // its prefix Box<[u8]>: u32 LE length + b"s"
//!     total_shares: u128,                // 16 bytes LE
//!     total_rewards: u128,               // 16 bytes LE
//!     last_claim: LookupMap<AccountId, u64>, // u32 LE length + b"c"
//! }
//! ```
//!
//! A `store::LookupMap` serializes only its prefix (its cache is
//! `#[borsh(skip)]`), and a `Box<[u8]>` encodes as a `Vec<u8>` does, so a
//! test can decode the root as `{ String, Vec<u8>, u128, u128, Vec<u8> }`.
//! Entries live at `b"s" ++ borsh(account_id)` (a `u128`) and
//! `b"c" ++ borsh(account_id)` (a `u64`, nanoseconds).
//!
//! # Why a withdraw pays only credited tokens
//!
//! The token moves a deposit into the vault before it calls
//! `ft_on_transfer`, so for a block or two the vault's token balance
//! includes tokens no share backs yet. If a withdraw spent them and that
//! `ft_on_transfer` then failed (it ran out of the gas the depositor
//! attached, say), NEP-141's `ft_resolve_transfer` would refund only what
//! the vault still holds, and the depositor would pay for the withdraw.
//! So `withdraw` refuses more than `total_shares - total_rewards`, the
//! tokens credited deposits brought in, before it debits anything. That
//! also means a withdraw of reward shares from a vault holding no credited
//! tokens refuses rather than failing its payout (in the fixed build).
//!
//! In the fixed build `total_shares >= total_rewards` always holds: a claim adds to both, and
//! a withdraw can't debit below `total_rewards`.
//!
//! # Why the fixed build is gas-safe
//!
//! The only state a withdraw commits before its payout is the debit, and
//! only `on_withdraw` can undo it. If the callback could run short of gas,
//! a withdraw would lose shares on the fixed build too, and a search that
//! explores the transaction's prepaid gas (100 to 300 Tgas, and 1 to 300
//! Tgas log-uniform) would rightly report it. So:
//!
//! - `on_withdraw` gets a static `GAS_FOR_ON_WITHDRAW` (10 Tgas, several
//!   times what it burns) and gas weight 0, so its budget never depends on
//!   what `withdraw` leaves over; `ft_transfer` gets a static 10 Tgas plus
//!   all the unused gas (weight 1).
//! - `withdraw` first requires `prepaid_gas >= MIN_WITHDRAW_GAS` (30 Tgas:
//!   both static budgets plus 10 Tgas for its own execution). Below that it
//!   panics before touching state, and the receipt rolls back whole. Even
//!   without the `require!`, a budget too small to attach the static gas
//!   makes the host call fail and roll back the receipt, so the `require!`
//!   is a clear message, and the margin for `withdraw`'s own burn, rather
//!   than the only guard.
//!
//! A receipt is atomic, so with these, every prepaid gas either fails the
//! withdraw before the debit or gives the callback its full 10 Tgas. Only a
//! fault that starves the callback receipt itself (an artificial
//! `Fault::Gas`/`OutOfGas` on `on_withdraw`) can lose shares on the fixed
//! build, which no prepaid gas can reach.
//!
//! The same holds for deposits: the token's `ft_transfer_call` reserves gas
//! for its own `ft_resolve_transfer`, and if `ft_on_transfer` fails the
//! token refunds the whole amount. `claim_rewards` commits its state and
//! schedules the notification in one receipt; if the notification can't
//! be attached the whole claim rolls back.

use near_sdk::json_types::U128;
use near_sdk::serde_json::json;
use near_sdk::store::LookupMap;
use near_sdk::{
    env, near, require, AccountId, Gas, GasWeight, NearToken, PanicOnDefault, Promise,
    PromiseError, PromiseOrValue,
};

/// Gas `ft_transfer` gets before the unused gas is shared out.
pub const GAS_FOR_FT_TRANSFER: Gas = Gas::from_tgas(10);
/// `on_withdraw`'s fixed budget (weight 0: it never gets leftovers).
pub const GAS_FOR_ON_WITHDRAW: Gas = Gas::from_tgas(10);
/// Least prepaid gas a withdraw accepts: both budgets plus its own run.
pub const MIN_WITHDRAW_GAS: Gas = Gas::from_tgas(30);
/// Gas for the best-effort `on_claim` notification.
pub const GAS_FOR_NOTIFY: Gas = Gas::from_tgas(5);
/// Shares a claim credits.
pub const REWARD: u128 = 10;
/// Least time between two claims of one account.
pub const CLAIM_PERIOD_NS: u64 = 24 * 60 * 60 * 1_000_000_000;
/// Who hears about claims (need not exist).
pub const REWARDS_ACCOUNT: &str = "rewards.near";

#[near(contract_state)]
#[derive(PanicOnDefault)]
pub struct Vault {
    token_id: AccountId,
    shares: LookupMap<AccountId, u128>,
    total_shares: u128,
    total_rewards: u128,
    last_claim: LookupMap<AccountId, u64>,
}

#[near]
impl Vault {
    #[init]
    pub fn new(token_id: AccountId) -> Self {
        Self {
            token_id,
            shares: LookupMap::new(b"s"),
            total_shares: 0,
            total_rewards: 0,
            last_claim: LookupMap::new(b"c"),
        }
    }

    /// NEP-141 receiver: a deposit of `amount` tokens from `sender_id`.
    pub fn ft_on_transfer(
        &mut self,
        sender_id: AccountId,
        amount: U128,
        #[allow(unused_variables)] msg: String,
    ) -> PromiseOrValue<U128> {
        require!(env::predecessor_account_id() == self.token_id, "only the vault's token deposits");
        require!(amount.0 > 0, "deposit must be positive");
        self.credit(&sender_id, amount.0);
        PromiseOrValue::Value(U128(0))
    }

    /// Burns `amount` of the caller's shares and pays out as many tokens.
    #[payable]
    pub fn withdraw(&mut self, amount: U128) -> Promise {
        near_sdk::assert_one_yocto();
        require!(
            env::prepaid_gas() >= MIN_WITHDRAW_GAS,
            format!("withdraw needs at least {} Tgas", MIN_WITHDRAW_GAS.as_tgas())
        );
        let amount = amount.0;
        require!(amount > 0, "withdraw must be positive");
        let who = env::predecessor_account_id();
        let held = self.shares_of(who.clone()).0;
        require!(held >= amount, "not enough shares");
        // Pay only from tokens credited deposits brought in. The vault's
        // token balance may include a deposit still in flight; if its
        // `ft_on_transfer` then fails, NEP-141 refunds at most what the
        // vault still holds, and that depositor would pay for this payout.
        // `forgetful` is the vault before its audit: it lacks this too.
        #[cfg(not(feature = "forgetful"))]
        require!(
            amount <= self.total_shares - self.total_rewards,
            "the vault holds too few credited tokens"
        );
        self.shares.insert(who.clone(), held - amount);
        self.total_shares -= amount;

        let args = json!({ "receiver_id": who, "amount": U128(amount) }).to_string();
        Promise::new(self.token_id.clone())
            .function_call_weight(
                "ft_transfer",
                args,
                NearToken::from_yoctonear(1),
                GAS_FOR_FT_TRANSFER,
                GasWeight(1),
            )
            .then(
                Self::ext(env::current_account_id())
                    .with_static_gas(GAS_FOR_ON_WITHDRAW)
                    .with_unused_gas_weight(0)
                    .on_withdraw(who, U128(amount)),
            )
    }

    /// The payout's result: on failure the fixed build gives the shares back.
    /// Returns the amount paid.
    #[private]
    pub fn on_withdraw(&mut self, account_id: AccountId, amount: U128) -> U128 {
        // Only a failed call means no tokens moved; any bytes are a success.
        let failed = matches!(env::promise_result_checked(0, 0), Err(PromiseError::Failed));
        if !failed {
            return amount;
        }
        #[cfg(not(feature = "forgetful"))]
        self.credit(&account_id, amount.0);
        #[cfg(feature = "forgetful")]
        let _ = &account_id;
        U128(0)
    }

    /// Once per 24 h: `REWARD` shares, then a best-effort notification.
    pub fn claim_rewards(&mut self) {
        let who = env::predecessor_account_id();
        let now = env::block_timestamp();
        if let Some(&last) = self.last_claim.get(&who) {
            require!(now >= last + CLAIM_PERIOD_NS, "already claimed in the last 24 hours");
        }
        self.last_claim.insert(who.clone(), now);
        self.credit(&who, REWARD);
        self.total_rewards += REWARD;

        let args = json!({ "account_id": who, "amount": U128(REWARD) }).to_string();
        Promise::new(REWARDS_ACCOUNT.parse().unwrap())
            .function_call("on_claim", args, NearToken::from_yoctonear(0), GAS_FOR_NOTIFY)
            .detach();
    }

    pub fn shares_of(&self, account_id: AccountId) -> U128 {
        U128(self.shares.get(&account_id).copied().unwrap_or(0))
    }

    pub fn total_shares(&self) -> U128 {
        U128(self.total_shares)
    }

    pub fn total_rewards(&self) -> U128 {
        U128(self.total_rewards)
    }
}

impl Vault {
    fn credit(&mut self, who: &AccountId, amount: u128) {
        let held = self.shares.get(who).copied().unwrap_or(0);
        self.shares.insert(who.clone(), held + amount);
        self.total_shares += amount;
    }
}
