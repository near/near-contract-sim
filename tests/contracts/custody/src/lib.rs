//! An intents-inspired custodian: the specimen for near-contract-sim's
//! callback-gas and log-size search tests. Built three ways by `build.sh`:
//! `tests/res/custody.wasm` (fixed), `custody_gas_bug.wasm` (feature
//! `bug_gas`) and `custody_log_bug.wasm` (feature `bug_log`).
//!
//! It is a *self-contained teaching contract*, not a model of any deployed
//! one. In a small, readable form it reproduces two mechanisms a real
//! deposit-and-refund verifier can have, each arising from the same natural
//! cause — a callback whose work grows with its input, against a bound the
//! author did not size for it:
//!
//! 1. a resolver callback with a fixed gas budget while its per-position work
//!    grows with the number of positions (`bug_gas`);
//! 2. a refund event whose single log line grows with the positions and the
//!    memo, and can exceed the host's per-receipt log limit (`bug_log`).
//!
//! # The ledger
//!
//! It holds one NEP-141 token (`token_id`) as backing, and keeps an internal
//! ledger: each owner's `credit`, and the global `total_credit`. One real
//! token backs one unit of credit, so the invariant the audit checks is
//!
//! ```text
//! token.ft_balance_of(custody) == total_credit()
//! ```
//!
//! # The flows
//!
//! - **Deposit**: the token's `ft_transfer_call(custody, amount, msg)`. `msg`
//!   is JSON `{ receiver, ids: [String..], memo, notify }`. The custody
//!   credits `amount` to `receiver` and emits a *compact* deposit log (no
//!   ids). With `notify: false` it returns `"0"` (the token keeps all).
//!   With `notify: true` it calls `receiver::on_notify(shares)`, which
//!   answers how much of each position to refund, then chains its own
//!   `resolve_deposit`. The resolver does per-position work, burns the
//!   refunded credit, emits one `refund` event listing every position, and
//!   returns the total to refund as `ft_on_transfer`'s unused amount; the
//!   token refunds that much to the sender.
//! - **Withdraw**: `withdraw(amount)` debits the caller's credit, pays the
//!   tokens with `ft_transfer`, and in the callback `on_withdraw` re-credits
//!   if the payout failed. Gas-safe like the vault specimen (static callback
//!   gas, a minimum-gas guard), so it is sound on every build.
//! - **Transfer**: `transfer(to, amount)` moves credit between owners,
//!   internal bookkeeping only.
//!
//! # Where the books and the refund disagree
//!
//! A receipt is atomic, so a resolver that runs out of gas (`bug_gas`) or
//! traps on an over-long log (`bug_log`) rolls back its burn *and* returns a
//! failed promise. NEP-141 then refunds the sender the **whole** amount,
//! while the credit stays. The credit is now unbacked.
//!
//! # Why the fixed build is safe
//!
//! - The resolver's gas scales with the position count (`resolve_gas`), sized
//!   for the worst case (a full refund of every position), so it never runs
//!   short.
//! - The deposit checks, before crediting, that the worst-case refund log
//!   (every position refunded, the memo kept) fits the host limit, and panics
//!   otherwise. A rejected deposit rolls back whole: the token refunds
//!   everything and no credit is created, so the books stay backed.
//!
//! Each bug build drops exactly one of these guards.

use near_sdk::json_types::U128;
use near_sdk::serde::{Deserialize, Serialize};
use near_sdk::serde_json::{self, json};
use near_sdk::store::LookupMap;
use near_sdk::{
    env, near, require, AccountId, Gas, GasWeight, NearToken, PanicOnDefault, Promise,
    PromiseError, PromiseOrValue,
};

/// Most positions one deposit may carry.
pub const MAX_IDS: usize = 16;
/// Longest a single position id may be. Under the host's 2048-byte storage-key
/// limit, so a long single id never fails a write; `MAX_IDS * MAX_ID_LEN`
/// exceeds the log limit, so many valid positions (or a long memo) can still
/// overflow the refund log.
pub const MAX_ID_LEN: usize = 1400;
/// Longest memo a deposit may carry into the refund event.
pub const MAX_MEMO_LEN: usize = 20_000;
/// The host's per-receipt total log limit (nearcore `max_total_log_length`).
pub const MAX_TOTAL_LOG_LEN: usize = 16_384;
/// Rounds of per-position hashing the resolver does, standing in for the
/// per-token bookkeeping a real verifier performs. Makes the resolver's work
/// grow with the position count, independent of the trie's shape.
pub const WORK_ROUNDS: usize = 90;

/// Gas the notify receiver's `on_notify` gets (static, weight 0).
pub const GAS_FOR_ON_NOTIFY: Gas = Gas::from_tgas(10);
/// Gas the withdraw payout gets before unused gas is shared out.
pub const GAS_FOR_FT_TRANSFER: Gas = Gas::from_tgas(10);
/// `on_withdraw`'s fixed budget (weight 0: it never gets leftovers).
pub const GAS_FOR_ON_WITHDRAW: Gas = Gas::from_tgas(10);
/// Least prepaid gas a withdraw accepts: both budgets plus its own run.
pub const MIN_WITHDRAW_GAS: Gas = Gas::from_tgas(30);

/// The resolver's gas budget.
///
/// Fixed build: it scales with the position count, sized for burning every
/// position (the worst case a failed notify forces). `bug_gas` hands it a flat
/// budget sized for a couple of positions, blind to how many arrive, so a
/// many-position deposit starves it. `bug_log` gives it ample, scaling gas on
/// purpose: that build's only defect is the missing log-size check, so the
/// resolver must have the gas to reach the over-long log and trap on it (a
/// clean host error), never run out of gas first.
#[cfg(all(not(feature = "bug_gas"), not(feature = "bug_log")))]
pub fn resolve_gas(n: usize) -> Gas {
    // Covers hashing each position (~1.6 Tgas) and building the refund log,
    // which the deposit bounds to the host log limit (~5.7 Tgas for 16 KiB).
    Gas::from_tgas(12).checked_add(Gas::from_tgas(2 * n as u64)).unwrap()
}
#[cfg(feature = "bug_gas")]
pub fn resolve_gas(_n: usize) -> Gas {
    Gas::from_tgas(6)
}
#[cfg(feature = "bug_log")]
pub fn resolve_gas(n: usize) -> Gas {
    Gas::from_tgas(10).checked_add(Gas::from_tgas(4 * n as u64)).unwrap()
}

#[derive(Serialize, Deserialize)]
#[serde(crate = "near_sdk::serde")]
pub struct DepositMsg {
    pub receiver: AccountId,
    pub ids: Vec<String>,
    #[serde(default)]
    pub memo: String,
    #[serde(default)]
    pub notify: bool,
}

#[near(contract_state)]
#[derive(PanicOnDefault)]
pub struct Custody {
    token_id: AccountId,
    /// Each owner's credit, in backing-token units.
    credit: LookupMap<AccountId, u128>,
    /// The backing the books claim: the sum of every credit.
    total_credit: u128,
}

#[near]
impl Custody {
    #[init]
    pub fn new(token_id: AccountId) -> Self {
        Self { token_id, credit: LookupMap::new(b"c"), total_credit: 0 }
    }

    /// NEP-141 receiver: a deposit of `amount` from `sender_id`.
    pub fn ft_on_transfer(
        &mut self,
        sender_id: AccountId,
        amount: U128,
        msg: String,
    ) -> PromiseOrValue<U128> {
        require!(
            env::predecessor_account_id() == self.token_id,
            "only the custody's token deposits"
        );
        let amount = amount.0;
        require!(amount > 0, "deposit must be positive");

        let msg: DepositMsg = if msg.is_empty() {
            DepositMsg { receiver: sender_id, ids: vec![".".to_string()], memo: String::new(), notify: false }
        } else {
            serde_json::from_str(&msg).unwrap_or_else(|_| env::panic_str("bad deposit msg"))
        };
        let DepositMsg { receiver, ids, memo, notify } = msg;

        require!(!ids.is_empty() && ids.len() <= MAX_IDS, "position count out of range");
        for id in &ids {
            require!(!id.is_empty() && id.len() <= MAX_ID_LEN, "position id length out of range");
        }
        require!(memo.len() <= MAX_MEMO_LEN, "memo too long");

        // Split the deposit across the positions; the first takes the rest.
        let n = ids.len();
        let each = amount / n as u128;
        let shares: Vec<U128> = (0..n)
            .map(|i| U128(if i == 0 { amount - each * (n as u128 - 1) } else { each }))
            .collect();

        self.give(&receiver, amount);

        // Compact deposit log: no ids, so a large input never trips it here.
        env::log_str(&format!("deposit: {amount} to {receiver} across {n} position(s)"));

        if !notify {
            return PromiseOrValue::Value(U128(0));
        }

        // Fixed build only: refuse a deposit whose worst-case refund log
        // (every position refunded in full) would not fit the host limit. The
        // deposit then rolls back whole and the token refunds all.
        #[cfg(not(feature = "bug_log"))]
        {
            let worst = refund_log(&receiver, &ids, &shares, &memo);
            require!(worst.len() <= MAX_TOTAL_LOG_LEN, "refund log would exceed the host limit");
        }

        Promise::new(receiver.clone())
            .function_call_weight(
                "on_notify",
                json!({ "shares": shares }).to_string().into_bytes(),
                NearToken::from_yoctonear(0),
                GAS_FOR_ON_NOTIFY,
                GasWeight(0),
            )
            .then(
                Self::ext(env::current_account_id())
                    .with_static_gas(resolve_gas(n))
                    .with_unused_gas_weight(0)
                    .resolve_deposit(receiver, ids, shares, memo),
            )
            .into()
    }

    /// Settles a notified deposit: does per-position work, burns each
    /// position's refunded credit, emits one refund event, and returns the
    /// total to refund externally (`ft_on_transfer`'s unused amount). Private:
    /// only the chained call reaches it.
    #[private]
    pub fn resolve_deposit(
        &mut self,
        receiver: AccountId,
        ids: Vec<String>,
        shares: Vec<U128>,
        memo: String,
    ) -> U128 {
        // What the receiver asked to refund, per position. A failed or
        // malformed answer means refund everything (the worst case).
        let refund_all = || shares.iter().map(|s| s.0).collect::<Vec<u128>>();
        let requested: Vec<u128> = match env::promise_result_checked(0, 8192) {
            Ok(bytes) => serde_json::from_slice::<Vec<U128>>(&bytes)
                .ok()
                .filter(|v| v.len() == ids.len())
                .map(|v| v.into_iter().map(|u| u.0).collect())
                .unwrap_or_else(refund_all),
            Err(_) => refund_all(),
        };

        let mut refunded = Vec::with_capacity(ids.len());
        let mut total_refund: u128 = 0;
        for ((id, share), want) in ids.iter().zip(&shares).zip(&requested) {
            // Per-position work: a commitment hash standing in for the
            // per-token bookkeeping a real verifier does. This is what grows
            // with the position count and starves a flat gas budget.
            let mut h = id.clone().into_bytes();
            for _ in 0..WORK_ROUNDS {
                h = env::sha256(&h);
            }
            let refund = (*want).min(share.0);
            refunded.push(U128(refund));
            total_refund += refund;
        }
        if total_refund > 0 {
            self.take(&receiver, total_refund);
        }

        // One refund event for every position. On the bug_log build this can
        // exceed the host's per-receipt log limit and trap the whole receipt.
        env::log_str(&refund_log(&receiver, &ids, &refunded, &memo));

        U128(total_refund)
    }

    /// Debits the caller's credit and pays out as many tokens, re-crediting in
    /// the callback if the payout fails. Gas-safe on every build.
    #[payable]
    pub fn withdraw(&mut self, amount: U128) -> Promise {
        near_sdk::assert_one_yocto();
        require!(
            env::prepaid_gas() >= MIN_WITHDRAW_GAS,
            "withdraw needs at least 30 Tgas"
        );
        let amount = amount.0;
        require!(amount > 0, "withdraw must be positive");
        let who = env::predecessor_account_id();
        self.take(&who, amount);
        Promise::new(self.token_id.clone())
            .function_call_weight(
                "ft_transfer",
                json!({ "receiver_id": who, "amount": U128(amount) }).to_string().into_bytes(),
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

    /// The payout's result: re-credits on failure. Returns the amount paid.
    #[private]
    pub fn on_withdraw(&mut self, account_id: AccountId, amount: U128) -> U128 {
        let failed = matches!(env::promise_result_checked(0, 0), Err(PromiseError::Failed));
        if failed {
            self.give(&account_id, amount.0);
            return U128(0);
        }
        amount
    }

    /// Moves credit between owners. Internal bookkeeping, no tokens move.
    pub fn transfer(&mut self, to: AccountId, amount: U128) {
        let from = env::predecessor_account_id();
        require!(amount.0 > 0, "transfer must be positive");
        self.take(&from, amount.0);
        self.give(&to, amount.0);
    }

    pub fn credit_of(&self, account_id: AccountId) -> U128 {
        U128(self.credit.get(&account_id).copied().unwrap_or(0))
    }

    pub fn total_credit(&self) -> U128 {
        U128(self.total_credit)
    }
}

impl Custody {
    fn give(&mut self, who: &AccountId, amount: u128) {
        let held = self.credit.get(who).copied().unwrap_or(0);
        self.credit.insert(who.clone(), held + amount);
        self.total_credit += amount;
    }
    fn take(&mut self, who: &AccountId, amount: u128) {
        let held = self.credit.get(who).copied().unwrap_or(0);
        require!(held >= amount, "not enough credit");
        self.credit.insert(who.clone(), held - amount);
        self.total_credit -= amount;
    }
}

/// The refund event's log line, as emitted (and, at deposit time on the fixed
/// build, as measured for the worst case).
fn refund_log(receiver: &AccountId, ids: &[String], amounts: &[U128], memo: &str) -> String {
    let event = json!({
        "standard": "custody",
        "version": "1.0.0",
        "event": "refund",
        "data": [{ "receiver": receiver, "ids": ids, "amounts": amounts, "memo": memo }],
    });
    format!("EVENT_JSON:{event}")
}
