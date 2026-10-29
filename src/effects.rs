//! Per-operation effects, read from one transaction's own receipts so
//! overlapping transactions don't blur them.

use crate::outcome::{ReceiptOutcome, Refund};
use crate::sim::{IntoAccountId, Sim};
use borsh::{BorshDeserialize, BorshSerialize};
use near_primitives::hash::CryptoHash;
use near_primitives::types::{AccountId, Balance};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fmt;

/// What one transaction did, from its own receipts only. NEAR is exact,
/// gas apart: the signer's balance changes by `near_net(signer)` less
/// `gas_paid()` (no gas key or staking in between). Tokens are measured
/// only if [watched](Sim::watch); any other is [`Unobserved`], never 0.
#[non_exhaustive]
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Effects {
    /// Value moves, in execution order. Self-moves are listed; `near_net`
    /// and `near_paid` leave them out.
    pub near: Vec<NearMove>,
    /// Watched tokens' balance changes, per receipt, in execution order.
    pub token_moves: Vec<TokenMove>,
    /// `storage_usage` changes, per receipt, in execution order.
    pub storage: Vec<StorageChange>,
    /// Its receipts that haven't run yet: a deposit they carry has left
    /// its sender and isn't credited yet.
    pub pending: Vec<PendingReceipt>,
    pub(crate) gas: Balance,
    pub(crate) queued: bool,
    /// Per watched token, `None` if its moves were observed, or why not.
    #[serde(with = "crate::outcome::pairs")]
    pub(crate) watched: HashMap<TokenKey, Option<String>>,
    pub(crate) supplied: HashSet<TokenKey>,
}

/// A watched token: its contract, and its id within a multi-token.
pub(crate) type TokenKey = (AccountId, Option<String>);

/// `wrap.near`, or `intents.near[nep141:wrap.near]` for a multi-token's.
pub(crate) fn token_label((contract, id): &TokenKey) -> String {
    match id {
        Some(id) => format!("{contract}[{id}]"),
        None => contract.to_string(),
    }
}

/// Which watched token: a contract (`"wrap.near"`), or a contract and a
/// token id (`("intents.near", "nep141:wrap.near")`).
pub trait WhichToken {
    /// The contract and the token id.
    fn which(self) -> (AccountId, Option<String>);
}

macro_rules! which_contract {
    ($($t:ty),*) => {$(
        impl WhichToken for $t {
            #[track_caller]
            fn which(self) -> (AccountId, Option<String>) {
                (self.into_account_id(), None)
            }
        }
    )*};
}
which_contract!(&str, String, &String, AccountId, &AccountId);

impl<A: IntoAccountId, B: AsRef<str>> WhichToken for (A, B) {
    #[track_caller]
    fn which(self) -> (AccountId, Option<String>) {
        (self.0.into_account_id(), Some(self.1.as_ref().to_owned()))
    }
}

/// One value move a receipt made.
#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NearMove {
    /// Who paid: the depositor, or the deleted account.
    pub from: AccountId,
    /// Who was credited.
    pub to: AccountId,
    /// How much, never 0.
    pub amount: Balance,
    /// Why it moved.
    pub kind: NearKind,
    /// The receipt that made it (the refund, for a refund).
    pub receipt_id: CryptoHash,
    /// The method a deposit's receipt called (a batch's first); `None`
    /// for a transfer or a refund.
    #[serde(default)]
    pub method: Option<String>,
}

/// Why NEAR moved.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum NearKind {
    /// A successful receipt's deposit: a call's, or a `Transfer`.
    Deposit,
    /// A failed receipt's deposit, refunded: nets to zero unless the
    /// refund goes elsewhere (`refund_to`).
    DepositRefund,
    /// A deleted account's balance, paid to its beneficiary.
    AccountDeletion,
}

/// A token balance change one receipt made, by the token's views just
/// before and after it. Never a 0.
#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, BorshSerialize, BorshDeserialize)]
pub struct TokenMove {
    /// The token contract.
    pub token: AccountId,
    /// The token id, for a multi-token.
    pub token_id: Option<String>,
    /// Whose balance changed; `None` for the total supply.
    pub holder: Option<AccountId>,
    /// After the receipt, less before it.
    pub delta: Delta,
    /// The receipt that changed it.
    pub receipt_id: CryptoHash,
}

impl TokenMove {
    pub(crate) fn key(&self) -> TokenKey {
        (self.token.clone(), self.token_id.clone())
    }
}

/// A signed change of a `u128` amount, exact from `-u128::MAX` to
/// `+u128::MAX`, where an `i128` difference would wrap. Compares with an
/// `i128` literal (`d == -5`); `token_net(..)? == Delta::gain(amount)`
/// needs no cast. `i128::try_from(d)` converts one, `Err(d)` out of
/// range; [`magnitude`](Self::magnitude) is its amount without the sign.
///
/// Arithmetic panics past ±`u128::MAX`, never wraps: one holder's net
/// always fits, a sum across holders may not, and in a check the panic is
/// a harness error, not a pass. Serializes as its decimal string.
#[derive(Clone, Copy, Default, PartialEq, Eq, Hash, BorshSerialize, BorshDeserialize)]
pub struct Delta {
    /// Negative; never with a zero magnitude.
    neg: bool,
    abs: u128,
}

impl Delta {
    /// No change.
    pub const ZERO: Delta = Delta { neg: false, abs: 0 };

    /// A gain of `amount`.
    pub const fn gain(amount: u128) -> Delta {
        Delta { neg: false, abs: amount }
    }

    /// A loss of `amount`.
    pub const fn loss(amount: u128) -> Delta {
        Delta::signed(true, amount)
    }

    /// Zero is never negative.
    const fn signed(neg: bool, abs: u128) -> Delta {
        Delta { neg: neg && abs != 0, abs }
    }

    /// `after` less `before`.
    pub const fn between(before: u128, after: u128) -> Delta {
        Delta::signed(after < before, after.abs_diff(before))
    }

    /// The amount, without its sign.
    pub const fn magnitude(self) -> u128 {
        self.abs
    }

    /// `self + other`, or `None` past ±`u128::MAX`.
    pub fn checked_add(self, other: Delta) -> Option<Delta> {
        if self.neg == other.neg {
            let abs = self.abs.checked_add(other.abs)?;
            return Some(Delta::signed(self.neg, abs));
        }
        // Opposite signs: the larger magnitude keeps its sign.
        let (big, small) = if self.abs >= other.abs { (self, other) } else { (other, self) };
        Some(Delta::signed(big.neg, big.abs - small.abs))
    }
}

impl From<i128> for Delta {
    fn from(n: i128) -> Delta {
        Delta::signed(n < 0, n.unsigned_abs())
    }
}

/// `Err` past `i128`'s range, with the delta.
impl TryFrom<Delta> for i128 {
    type Error = Delta;

    fn try_from(d: Delta) -> Result<i128, Delta> {
        match (d.neg, i128::try_from(d.abs)) {
            (false, Ok(n)) => Ok(n),
            (true, Ok(n)) => Ok(-n),
            (true, Err(_)) if d.abs == i128::MIN.unsigned_abs() => Ok(i128::MIN),
            _ => Err(d),
        }
    }
}

impl std::ops::Neg for Delta {
    type Output = Delta;

    fn neg(self) -> Delta {
        Delta::signed(!self.neg, self.abs)
    }
}

impl std::ops::Add for Delta {
    type Output = Delta;

    #[track_caller]
    fn add(self, other: Delta) -> Delta {
        self.checked_add(other)
            .unwrap_or_else(|| panic!("token delta {self} + {other} overflows ±u128::MAX"))
    }
}

impl std::ops::Sub for Delta {
    type Output = Delta;

    #[track_caller]
    fn sub(self, other: Delta) -> Delta {
        self.checked_add(-other)
            .unwrap_or_else(|| panic!("token delta {self} - {other} overflows ±u128::MAX"))
    }
}

impl std::ops::AddAssign for Delta {
    #[track_caller]
    fn add_assign(&mut self, other: Delta) {
        *self = *self + other;
    }
}

impl std::ops::SubAssign for Delta {
    #[track_caller]
    fn sub_assign(&mut self, other: Delta) {
        *self = *self - other;
    }
}

impl std::iter::Sum for Delta {
    #[track_caller]
    fn sum<I: Iterator<Item = Delta>>(iter: I) -> Delta {
        iter.fold(Delta::ZERO, |a, b| a + b)
    }
}

impl Ord for Delta {
    fn cmp(&self, other: &Delta) -> std::cmp::Ordering {
        match (self.neg, other.neg) {
            (false, false) => self.abs.cmp(&other.abs),
            (true, true) => other.abs.cmp(&self.abs),
            _ => other.neg.cmp(&self.neg),
        }
    }
}

impl PartialOrd for Delta {
    fn partial_cmp(&self, other: &Delta) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl PartialEq<i128> for Delta {
    fn eq(&self, other: &i128) -> bool {
        *self == Delta::from(*other)
    }
}

impl PartialOrd<i128> for Delta {
    fn partial_cmp(&self, other: &i128) -> Option<std::cmp::Ordering> {
        Some(self.cmp(&Delta::from(*other)))
    }
}

impl fmt::Display for Delta {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let sign = if self.neg { "-" } else { "" };
        write!(f, "{sign}{}", self.abs)
    }
}

impl fmt::Debug for Delta {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

impl Serialize for Delta {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for Delta {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Delta, D::Error> {
        let s = <String as Deserialize>::deserialize(d)?;
        let digits = s.strip_prefix('-');
        let abs: u128 = digits.unwrap_or(&s).parse().map_err(serde::de::Error::custom)?;
        Ok(Delta::signed(digits.is_some(), abs))
    }
}

/// A change of one account's `storage_usage` one receipt made, in bytes
/// (negative: freed), which its balance must cover at the storage price.
#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StorageChange {
    /// Whose storage.
    pub account: AccountId,
    /// After the receipt, less before it.
    pub bytes: i64,
    /// The receipt that changed it.
    pub receipt_id: CryptoHash,
}

/// A token effect that couldn't be measured: not watched (or watched
/// too late), a view that failed, or value moved to an unwatched holder
/// (the holders' changes don't add up to the supply's). Never a silent 0.
///
/// In an [`explore`](crate::explore) check, `?` on one makes the judgment
/// *not judged* and the case incomplete: no finding, no pass. Watch the
/// token (with [`Token::holders`] for unnamed payees), or match the `Err`.
#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Unobserved {
    /// The token asked about.
    pub token: AccountId,
    /// Why its effect couldn't be measured.
    pub why: String,
}

impl fmt::Display for Unobserved {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: unobserved: {}", self.token, self.why)
    }
}

impl std::error::Error for Unobserved {}

/// A token to measure per receipt ([`Sim::watch`]).
///
/// For each receipt that changes the contract's storage, the balance view
/// runs for every holder (every account in state, plus
/// [`holders`](Self::holders)) and the supply view once, just before and
/// after the receipt. With a supply view the holders' changes must add up
/// to the supply's, so a move to an unwatched id shows as [`Unobserved`],
/// not as a smaller number.
#[must_use = "a Token measures nothing until watched: sim.watch(token) or Fixture::token(token)"]
#[derive(
    Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, BorshSerialize, BorshDeserialize,
)]
pub struct Token {
    pub(crate) id: AccountId,
    /// A multi-token's ids, one watch each; empty for a single token.
    pub(crate) token_ids: Vec<String>,
    pub(crate) balance: String,
    pub(crate) arg: String,
    pub(crate) supply: Option<String>,
    pub(crate) holders: Vec<AccountId>,
    pub(crate) pure: bool,
    #[serde(default)]
    pub(crate) discover: bool,
}

impl Token {
    /// A NEP-141 token: `ft_balance_of` and `ft_total_supply`.
    pub fn nep141(id: impl IntoAccountId) -> Self {
        Self::view(id, "ft_balance_of").supply("ft_total_supply")
    }

    /// A contract whose view `balance_method({"account_id": ..})` returns a
    /// balance (a `U128` string or a number). No supply view by default.
    pub fn view(id: impl IntoAccountId, balance_method: &str) -> Self {
        Token {
            id: id.into_account_id(),
            balance: balance_method.to_string(),
            arg: "account_id".to_string(),
            token_ids: vec![],
            supply: None,
            holders: vec![],
            pure: false,
            discover: false,
        }
    }

    /// One token of a NEP-245 multi-token (`mt_balance_of`, `mt_supply`);
    /// [`Effects::token_net`] then takes `(id, token_id)`.
    pub fn nep245(id: impl IntoAccountId, token_id: &str) -> Self {
        Self::nep245_ids(id, [token_id])
    }

    /// Several token ids of one NEP-245 multi-token, each watched apart.
    ///
    /// ```
    /// # use near_contract_sim::Token;
    /// let ids = ["nep141:wrap.near", "nep141:usdc.near"];
    /// let watch = Token::nep245_ids("intents.near", ids).holders(["alice.near"]);
    /// ```
    pub fn nep245_ids(
        id: impl IntoAccountId,
        token_ids: impl IntoIterator<Item = impl Into<String>>,
    ) -> Self {
        let token = Self::view(id, "mt_balance_of").supply("mt_supply");
        Token { token_ids: token_ids.into_iter().map(Into::into).collect(), ..token }
    }

    /// A NEP-171 token, as counts per holder (`nft_supply_for_owner`): enough
    /// for "delivered" and "stranded" checks, not for which id.
    pub fn nep171(id: impl IntoAccountId) -> Self {
        Self::view(id, "nft_supply_for_owner").supply("nft_total_supply")
    }

    /// Also watches each token id a NEP-245 event of the contract's own
    /// receipts names, from that receipt on: ids minted mid-case. The
    /// balance before is 0 only if the id first appears in that receipt's
    /// own `mt_mint`; else it is [`Unobserved`] there. Holders events name
    /// are watched too.
    ///
    /// ```
    /// # use near_contract_sim::Token;
    /// let watch = Token::nep245_ids("mt.near", Vec::<String>::new()).discover();
    /// ```
    pub fn discover(mut self) -> Self {
        self.discover = true;
        self
    }

    /// The token id, once [`split`](Self::split).
    pub(crate) fn token_id(&self) -> Option<&String> {
        self.token_ids.first()
    }

    pub(crate) fn split(self) -> Vec<Token> {
        match self.token_ids.len() {
            0 | 1 => vec![self],
            _ => (self.token_ids.iter())
                .map(|t| Token { token_ids: vec![t.clone()], ..self.clone() })
                .collect(),
        }
    }

    pub(crate) fn key(&self) -> TokenKey {
        (self.id.clone(), self.token_id().cloned())
    }

    /// The total supply view, called with `{}` (or a multi-token's id).
    pub fn supply(mut self, method: &str) -> Self {
        self.supply = Some(method.to_string());
        self
    }

    /// The balance view's holder argument, `account_id` by default.
    pub fn arg(mut self, name: &str) -> Self {
        self.arg = name.to_string();
        self
    }

    /// Watches these ids that aren't accounts too.
    pub fn holders(mut self, ids: impl IntoIterator<Item = impl IntoAccountId>) -> Self {
        for id in ids.into_iter().map(IntoAccountId::into_account_id) {
            if !self.holders.contains(&id) {
                self.holders.push(id);
            }
        }
        self
    }

    /// Declares that its views read only the contract's storage and code
    /// (not the clock, block, randomness or NEAR balance), so measurements
    /// carry over between blocks, saving most views. Exact only if true: a
    /// balance that grows with time would land on the next receipt.
    pub fn pure(mut self) -> Self {
        self.pure = true;
        self
    }
}

/// A receipt that hasn't run yet. Data receipts aren't listed: the
/// callback waiting for them is.
#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingReceipt {
    /// The receipt's id.
    pub receipt_id: CryptoHash,
    /// The transaction it descends from.
    pub tx: CryptoHash,
    /// `system` for the runtime's refunds.
    pub predecessor_id: AccountId,
    /// The account it runs on.
    pub receiver_id: AccountId,
    /// The methods it calls, empty if it calls none.
    pub method_names: Vec<String>,
    /// The deposit it carries, left its sender and not credited yet.
    pub deposit: Balance,
    /// Where it waits.
    pub state: PendingState,
}

/// Where a pending receipt waits.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum PendingState {
    /// It runs in the next block.
    InFlight,
    /// A [`Fault::Hold`](crate::Fault::Hold) keeps it back at least
    /// `blocks_left` more blocks.
    Held {
        /// Blocks before it goes out, at least.
        blocks_left: u64,
    },
    /// A [`Fault::HoldUntil`](crate::Fault::HoldUntil) keeps it back until
    /// `receipt_id` has run.
    HeldUntil {
        /// The receipt it waits for; `None` until one its selector names
        /// leaves a block.
        receipt_id: Option<CryptoHash>,
    },
    /// A callback waiting for its promise results or a yield's resume.
    AwaitingResults,
    /// A yield's callback, parked until resumed or timed out.
    Yield {
        /// The height of the block that fires its timeout.
        expires_at: u64,
    },
    /// In the delayed queue or the outgoing buffer.
    Buffered,
}

impl fmt::Display for PendingState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InFlight => f.write_str("in flight"),
            Self::Held { blocks_left: 1 } => f.write_str("held, 1 more block"),
            Self::Held { blocks_left } => write!(f, "held, {blocks_left} more blocks"),
            Self::HeldUntil { receipt_id: Some(r) } => write!(f, "held until receipt {r} runs"),
            Self::HeldUntil { receipt_id: None } => {
                f.write_str("held until a receipt it names leaves a block and runs")
            }
            Self::AwaitingResults => f.write_str("awaiting promise results"),
            Self::Yield { expires_at } => write!(f, "parked yield, times out at #{expires_at}"),
            Self::Buffered => f.write_str("delayed or buffered in state"),
        }
    }
}

impl PendingReceipt {
    /// `receiver::method`, as the receipt tree names it ([`crate::outcome::site`]).
    pub(crate) fn label(&self) -> String {
        crate::outcome::site(&self.receiver_id, &self.method_names)
    }
}

impl Effects {
    /// One transaction's effects, its conversion first if it ran.
    pub(crate) fn of(
        transaction: Option<&ReceiptOutcome>,
        receipts: &[ReceiptOutcome],
        pending: Vec<PendingReceipt>,
        gas: Balance,
        (watched, supplied): (HashMap<TokenKey, Option<String>>, HashSet<TokenKey>),
    ) -> Self {
        let parent: HashMap<CryptoHash, &ReceiptOutcome> = transaction
            .into_iter()
            .chain(receipts)
            .flat_map(|r| r.receipt_ids.iter().map(move |c| (*c, r)))
            .collect();
        let mut near = vec![];
        for r in receipts.iter().filter(|r| r.is_success() && r.deposit > Balance::ZERO) {
            let (from, kind) = match r.refund {
                None => (r.predecessor_id.clone(), NearKind::Deposit),
                Some(Refund::Gas) => continue,
                // The failed receipt's deposit, from whoever attached it.
                Some(Refund::Deposit) => match parent.get(&r.receipt_id) {
                    Some(p) => (p.predecessor_id.clone(), NearKind::DepositRefund),
                    None => continue,
                },
                // The deleted account's balance.
                Some(Refund::AccountDeletion) => match parent.get(&r.receipt_id) {
                    Some(p) => (p.receiver_id.clone(), NearKind::AccountDeletion),
                    None => continue,
                },
            };
            near.push(NearMove {
                from,
                to: r.receiver_id.clone(),
                amount: r.deposit,
                kind,
                receipt_id: r.receipt_id,
                // A batch's deposit is the receipt's; its first call
                // names it.
                method: (kind == NearKind::Deposit)
                    .then(|| r.method_names.first().cloned())
                    .flatten(),
            });
        }
        Effects {
            near,
            token_moves: receipts.iter().flat_map(|r| r.token_moves.iter().cloned()).collect(),
            storage: receipts.iter().flat_map(|r| r.storage.iter().cloned()).collect(),
            pending,
            gas,
            queued: transaction.is_none(),
            watched,
            supplied,
        }
    }

    /// What `who` gained in yoctoNEAR (negative: lost), gas and self-moves
    /// excluded: a [`Delta`], as [`token_net`](Self::token_net) is.
    pub fn near_net(&self, who: impl IntoAccountId) -> Delta {
        let who = who.into_account_id();
        let moves = self.near.iter().filter(|m| m.from != m.to);
        moves
            .map(|m| match (m.to == who, m.from == who) {
                (true, _) => Delta::gain(m.amount.as_yoctonear()),
                (_, true) => Delta::loss(m.amount.as_yoctonear()),
                _ => Delta::ZERO,
            })
            .sum()
    }

    /// The value `from` moved to `to` (0 for the same account).
    pub fn near_paid(&self, from: impl IntoAccountId, to: impl IntoAccountId) -> Balance {
        let (from, to) = (from.into_account_id(), to.into_account_id());
        self.near
            .iter()
            .filter(|m| m.from == from && m.to == to && from != to)
            .fold(Balance::ZERO, |b, m| b.saturating_add(m.amount))
    }

    /// Bytes of storage `who` gained (negative: freed) so far: contract
    /// storage, keys and code. A byte count, not an amount, so an `i64`
    /// as [`StorageChange::bytes`] is; what the bytes added lock is
    /// `sim.storage_cost(u64::try_from(bytes).unwrap_or(0))`, nothing when
    /// storage was freed.
    pub fn storage_bytes(&self, who: impl IntoAccountId) -> i64 {
        let who = who.into_account_id();
        self.storage.iter().filter(|s| s.account == who).map(|s| s.bytes).sum()
    }

    /// The deposits that reached `who` from another account in receipts
    /// that succeeded, refunds left out, at any depth.
    pub fn near_deposited(&self, who: impl IntoAccountId) -> Balance {
        let who = who.into_account_id();
        (self.near.iter())
            .filter(|m| m.to == who && m.from != who && m.kind == NearKind::Deposit)
            .fold(Balance::ZERO, |b, m| b.saturating_add(m.amount))
    }

    /// What the signer paid for gas: the conversion's charge less the gas
    /// refunds that have run (pending ones count as paid). Not
    /// `tokens_burnt`, which differs under a refund penalty or a gas price
    /// change. 0 for a refused or queued transaction.
    pub fn gas_paid(&self) -> Balance {
        self.gas
    }

    /// `who`'s net change in a [watched](Sim::watch) `token` so far.
    /// `Err(Unobserved)` if it isn't watched or a move couldn't be
    /// measured: never a silent 0.
    pub fn token_net(
        &self,
        token: impl WhichToken,
        who: impl IntoAccountId,
    ) -> Result<Delta, Unobserved> {
        let token = token.which();
        self.observed(&token)?;
        Ok(self.token_sum(&token, Some(&who.into_account_id())))
    }

    fn token_sum(&self, token: &TokenKey, holder: Option<&AccountId>) -> Delta {
        let mine = |m: &&TokenMove| m.key() == *token && m.holder.as_ref() == holder;
        self.token_moves.iter().filter(mine).map(|m| m.delta).sum()
    }

    /// `token`'s total supply change; `Err(Unobserved)` where `token_net`
    /// is, and without a [`Token::supply`] view.
    pub fn token_supply_net(&self, token: impl WhichToken) -> Result<Delta, Unobserved> {
        let token = token.which();
        self.observed(&token)?;
        if !self.supplied.contains(&token) {
            let why = format!(
                "{} is watched with no supply view: see Token::supply",
                token_label(&token)
            );
            return Err(Unobserved { token: token.0, why });
        }
        Ok(self.token_sum(&token, None))
    }

    /// Converted, and every receipt it spawned has run, refunds and
    /// detached promises included.
    pub(crate) fn is_complete(&self) -> bool {
        !self.queued && self.pending.is_empty()
    }

    fn observed(&self, token: &TokenKey) -> Result<(), Unobserved> {
        let why = match self.watched.get(token) {
            Some(None) => return Ok(()),
            Some(Some(why)) => why.clone(),
            None => format!("{} is not a watched token", token_label(token)),
        };
        Err(Unobserved { token: token.0.clone(), why })
    }
}

impl Sim {
    /// Measures `token` per receipt from the next block on, replacing an
    /// earlier watch of the contract. Views run read-only: a watch changes
    /// no state, gas or outcome. A transaction that touched the token
    /// before the watch gets [`Unobserved`] for it.
    pub fn watch(&mut self, token: Token) {
        self.chain.watch(token);
    }

    /// Every receipt that hasn't run yet, of any transaction: in flight,
    /// held, buffered, awaiting results, parked; each group in run order.
    /// Queued transactions aren't receipts yet: see [`Sim::is_idle`].
    pub fn pending_receipts(&self) -> Vec<PendingReceipt> {
        self.chain.pending()
    }
}
