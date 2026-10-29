//! Execution outcomes returned by the simulator, assembled from the block
//! log.

use crate::chain::{
    Log, ViewAt, action_receipt, calls, is_gas_refund, method_names, prepaid_gas, priced_config,
};
use crate::effects::{Effects, PendingReceipt};
use crate::inject::Fault;
use crate::render::{failure_line, missing_method, tgas, trace, variant, verdict};
use crate::sim::Sim;
use near_parameters::{ActionCosts, RuntimeConfig};
use near_primitives::action::Action;
use near_primitives::errors::{
    ActionErrorKind, CompilationError, FunctionCallError, InvalidTxError, TxExecutionError,
};
use near_primitives::hash::CryptoHash;
use near_primitives::receipt::{DataReceiver, Receipt};
use near_primitives::transaction::ExecutionStatus;
use near_primitives::types::{AccountId, Balance, Gas};
use near_primitives::views::{ExecutionMetadataView, FinalExecutionStatus};
use node_runtime::config::{exec_fee, tx_cost};
use serde::de::DeserializeOwned;
use std::borrow::Borrow;
use std::collections::BTreeMap;
use std::fmt;
use std::ops::RangeBounds;
use std::sync::Arc;

/// A borsh value as hex in JSON: nearcore's execution status and data
/// receivers have no serde form of their own.
pub(crate) mod borsh_hex {
    use borsh::{BorshDeserialize, BorshSerialize};
    use serde::{Deserialize, Deserializer, Serializer, de::Error};

    pub fn serialize<T: BorshSerialize, S: Serializer>(v: &T, s: S) -> Result<S::Ok, S::Error> {
        let bytes = borsh::to_vec(v).map_err(serde::ser::Error::custom)?;
        s.serialize_str(&bytes.iter().map(|b| format!("{b:02x}")).collect::<String>())
    }

    pub fn deserialize<'de, T: BorshDeserialize, D: Deserializer<'de>>(
        d: D,
    ) -> Result<T, D::Error> {
        let hex = <String as Deserialize>::deserialize(d)?;
        let bytes: Result<Vec<u8>, _> = (0..hex.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(hex.get(i..i + 2).unwrap_or("zz"), 16))
            .collect();
        borsh::from_slice(&bytes.map_err(D::Error::custom)?).map_err(D::Error::custom)
    }
}

/// A map with tuple keys as a list of pairs, since JSON keys are strings.
pub(crate) mod pairs {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};
    use std::collections::HashMap;
    use std::hash::Hash;

    pub fn serialize<K: Serialize, V: Serialize, S: Serializer>(
        map: &HashMap<K, V>,
        s: S,
    ) -> Result<S::Ok, S::Error> {
        s.collect_seq(map)
    }

    pub fn deserialize<'de, K, V, D>(d: D) -> Result<HashMap<K, V>, D::Error>
    where
        K: Deserialize<'de> + Eq + Hash,
        V: Deserialize<'de>,
        D: Deserializer<'de>,
    {
        Ok(Vec::<(K, V)>::deserialize(d)?.into_iter().collect())
    }
}

/// `receiver::method`, a batch's methods joined by `,`, or the receiver
/// alone for a receipt that calls nothing: how the receipt tree labels a
/// receipt that only calls, and how faults and reports name a receipt. A
/// single call's label is what
/// [`Audit::assert_reached`](crate::explore::Audit::assert_reached)
/// takes. `::`, as `.` is part of account ids.
pub(crate) fn site(receiver: &AccountId, methods: &[String]) -> String {
    match methods {
        [] => receiver.to_string(),
        m => format!("{receiver}::{}", m.join(",")),
    }
}

/// What a runtime refund ([`ReceiptOutcome::refund`]) gives back.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum Refund {
    /// Prepaid gas the receipts didn't burn.
    Gas,
    /// A failed receipt's deposit, back to its sender (or `refund_to`).
    Deposit,
    /// A deleted account's balance, to its beneficiary.
    AccountDeletion,
}

/// Outcome of a single receipt (or of the transaction-to-receipt conversion).
#[non_exhaustive]
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ReceiptOutcome {
    /// Receipt id (or transaction hash for the conversion outcome).
    pub receipt_id: CryptoHash,
    /// The transaction it descends from.
    pub tx: CryptoHash,
    /// The account it ran on (the signer, for the conversion): nearcore's
    /// `executor_id`.
    pub receiver_id: AccountId,
    /// The predecessor (the signer, for the conversion).
    pub predecessor_id: AccountId,
    /// Its function calls' methods.
    pub method_names: Vec<String>,
    /// Each function call's arguments, as sent, one per `method_names`
    /// entry.
    pub args: Vec<Vec<u8>>,
    /// Each action's nearcore variant name. Empty for the conversion.
    pub actions: Vec<String>,
    /// Which refund it is, for the runtime's (predecessor `system`).
    pub refund: Option<Refund>,
    /// The block it ran in.
    pub block_height: u64,
    /// What `env::block_timestamp()` returned.
    pub block_timestamp_ns: u64,
    /// Everything it burnt, as the node reports it: `prepaid_exec_fees`
    /// plus what it burnt of `attached_gas`.
    pub gas_burnt: Gas,
    /// `gas_burnt` at the gas price.
    pub tokens_burnt: Balance,
    /// The logs it emitted.
    pub logs: Vec<String>,
    /// How it ended.
    #[serde(with = "borsh_hex")]
    pub status: ExecutionStatus,
    /// Receipts spawned by this one, refunds included.
    pub receipt_ids: Vec<CryptoHash>,
    /// Gas profile (cost name -> gas), as RPC reports it.
    pub gas_profile: BTreeMap<String, u64>,
    /// Gas attached to the receipt's function calls (0 for others).
    pub attached_gas: Gas,
    /// Deposit its actions carry; 0 for the conversion.
    pub deposit: Balance,
    /// The static exec fees inside `gas_burnt` (`new_action_receipt` and
    /// each action that ran). The sender prepaid them on top of the
    /// attached gas, so they never count against `attached_gas`.
    pub prepaid_exec_fees: Gas,
    /// It failed, and nobody learns of it: nearcore sends a failure to its
    /// output data receivers (from `promise_then`/`promise_and`, or
    /// inherited via `promise_return`), so it is handled if it has any, or
    /// if it ends the transaction's result path.
    ///
    /// A meta transaction's failed inner call is always unhandled (the
    /// `Delegate` receipt returns no promise); a gas refund never is
    /// (conservation counts what it burns); a failed balance refund is (its
    /// value is burnt). One a fault forced is still unhandled, but
    /// [`Sim::findings`] excuses it and the tree marks it `FORCED`.
    pub unhandled: bool,
    /// The simulator delivered it, which no node does: as a callback
    /// ([`Tx::promise_results`](crate::Tx::promise_results)), or from
    /// another predecessor ([`Tx::predecessor`](crate::Tx::predecessor)).
    pub delivered: bool,
    /// The fault that changed it ([`Tx::inject`](crate::Tx::inject)), the
    /// first if several did (`OutOfGas` for a call `PromiseFailed` failed).
    pub fault: Option<Fault>,
    /// How many blocks late a [`Fault::Hold`](crate::Fault::Hold) or
    /// [`Fault::HoldUntil`](crate::Fault::HoldUntil) sent it out. `None` if
    /// no hold did.
    pub held: Option<u64>,
    /// For a callback, the most blocks a
    /// [`Fault::HoldResult`](crate::Fault::HoldResult) kept back one of its
    /// results.
    pub result_held: Option<u64>,
    /// The changes it made to [watched](Sim::watch) tokens' balances.
    pub token_moves: Vec<crate::effects::TokenMove>,
    /// The accounts whose `storage_usage` it changed, and by how much.
    pub storage: Vec<crate::effects::StorageChange>,
    /// Where its result goes (see `unhandled`).
    #[serde(with = "borsh_hex")]
    pub(crate) output_data_receivers: Vec<DataReceiver>,
    pub(crate) input_data_ids: Vec<CryptoHash>,
    /// The attached gas and exec fees of the receipts it created, which
    /// nearcore counts as used but not burnt.
    pub(crate) forwarded: Gas,
    pub(crate) storage_ops: StorageOps,
    /// For a call of a method its contract doesn't export, the methods it
    /// does, for the trace.
    #[serde(default)]
    pub(crate) exports: Option<Vec<String>>,
}

/// A contract storage key a receipt wrote or removed.
#[non_exhaustive]
#[derive(
    Clone,
    Debug,
    PartialEq,
    Eq,
    Hash,
    borsh::BorshSerialize,
    borsh::BorshDeserialize,
    serde::Serialize,
    serde::Deserialize,
)]
pub struct WrittenKey {
    /// The account whose storage it is.
    pub account: AccountId,
    /// The key, in full.
    pub key: Vec<u8>,
    /// The value's length; `None` if removed.
    pub len: Option<usize>,
}

/// `0x6501000000` or `"STATE"`: the key, as text if it is printable.
impl fmt::Display for WrittenKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match std::str::from_utf8(&self.key) {
            Ok(s) if !s.contains(char::is_control) => write!(f, "{s:?}")?,
            _ => {
                write!(f, "0x{}", self.key.iter().map(|b| format!("{b:02x}")).collect::<String>())?
            }
        }
        match self.len {
            Some(n) => write!(f, " ({n} bytes)"),
            None => write!(f, " (removed)"),
        }
    }
}

/// What one receipt did to storage: counts from its gas profile (each
/// cost's gas over its unit cost), and the keys nearcore committed for it
/// (none for a failed receipt).
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[non_exhaustive]
pub struct StorageOps {
    /// Trie nodes it touched (read from the store) ...
    pub trie_nodes: u64,
    /// ... and read from the cache.
    pub cached_nodes: u64,
    /// `storage_read` calls.
    pub reads: u64,
    /// `storage_write` calls.
    pub writes: u64,
    /// `storage_remove` calls.
    pub removes: u64,
    /// `storage_has_key` calls.
    pub has_keys: u64,
    /// Bytes read: keys and values.
    pub read_bytes: u64,
    /// Bytes written: keys and values.
    pub written_bytes: u64,
    /// Bytes evicted: values writes replaced and removes returned.
    pub evicted_bytes: u64,
    /// The keys it wrote or removed.
    pub written: Vec<WrittenKey>,
}

impl StorageOps {
    pub(crate) fn of(
        profile: &BTreeMap<String, u64>,
        rc: &RuntimeConfig,
        written: Vec<WrittenKey>,
    ) -> StorageOps {
        use near_parameters::ExtCosts;
        let costs = &rc.wasm_config.ext_costs;
        let n = |c: ExtCosts| {
            let unit = costs.gas_cost(c).as_gas();
            let name = format!("{c:?}").to_uppercase();
            match unit {
                0 => 0,
                u => profile.get(&name).copied().unwrap_or(0) / u,
            }
        };
        StorageOps {
            trie_nodes: n(ExtCosts::touching_trie_node),
            cached_nodes: n(ExtCosts::read_cached_trie_node),
            reads: n(ExtCosts::storage_read_base),
            writes: n(ExtCosts::storage_write_base),
            removes: n(ExtCosts::storage_remove_base),
            has_keys: n(ExtCosts::storage_has_key_base),
            read_bytes: n(ExtCosts::storage_read_key_byte) + n(ExtCosts::storage_read_value_byte),
            written_bytes: n(ExtCosts::storage_write_key_byte)
                + n(ExtCosts::storage_write_value_byte),
            evicted_bytes: n(ExtCosts::storage_write_evicted_byte)
                + n(ExtCosts::storage_remove_ret_value_byte),
            written,
        }
    }
}

/// `read 2 (1.0 KiB), wrote 3 (32.1 KiB), evicted 31.0 KiB, 9 trie nodes
/// (+4 cached), 3 keys written`.
impl fmt::Display for StorageOps {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let kib = |n: u64| format!("{:.1} KiB", n as f64 / 1024.0);
        write!(
            f,
            "read {} ({}), wrote {} ({}), removed {}, evicted {}, {} trie nodes (+{} cached), {} \
             keys written",
            self.reads,
            kib(self.read_bytes),
            self.writes,
            kib(self.written_bytes),
            self.removes,
            kib(self.evicted_bytes),
            self.trie_nodes,
            self.cached_nodes,
            self.written.len()
        )
    }
}

impl ReceiptOutcome {
    /// One of the runtime's refunds.
    pub fn is_refund(&self) -> bool {
        self.refund.is_some()
    }

    /// It ran without failing: returned a value, or a promise.
    pub fn is_success(&self) -> bool {
        matches!(
            self.status,
            ExecutionStatus::SuccessValue(_) | ExecutionStatus::SuccessReceiptId(_)
        )
    }

    /// The value it returned, if it returned one rather than a promise.
    pub fn value(&self) -> Option<&[u8]> {
        match &self.status {
            ExecutionStatus::SuccessValue(v) => Some(v),
            _ => None,
        }
    }

    /// Why it failed, if it did.
    pub fn failure(&self) -> Option<Failure> {
        match &self.status {
            ExecutionStatus::Failure(e) => Some(Failure::new(e)),
            _ => None,
        }
    }

    /// Why it failed, on one line, as the trace shows it: a missing
    /// method with the methods its contract exports.
    pub(crate) fn failure_text(&self) -> Option<String> {
        let f = self.failure()?;
        // Only a missing method has exports.
        Some(match (&self.exports, self.method()) {
            (Some(exports), Some(m)) => missing_method(&self.receiver_id, m, exports),
            _ => failure_line(&f),
        })
    }

    /// Its [`site`]: what the receipt tree, reports and coverage name.
    pub(crate) fn site(&self) -> String {
        site(&self.receiver_id, &self.method_names)
    }

    /// The method that names it: the failing call's in a failed batch,
    /// else its first.
    pub fn method(&self) -> Option<&str> {
        self.failed_method().or(self.method_names.first()).map(String::as_str)
    }

    /// The method of the call it failed at: nearcore's `ActionError::index`
    /// counts every action, not only calls.
    pub(crate) fn failed_method(&self) -> Option<&String> {
        let ExecutionStatus::Failure(TxExecutionError::ActionError(e)) = &self.status else {
            return None;
        };
        let i = usize::try_from(e.index?).ok()?;
        if self.actions.get(i).map(String::as_str) != Some("FunctionCall") {
            return None;
        }
        let call = self.actions[..i].iter().filter(|a| *a == "FunctionCall").count();
        self.method_names.get(call)
    }

    /// Its one function call's arguments as JSON. An `Err` naming the
    /// receipt for a refund, a transfer or a batch, so a loop over every
    /// receipt can skip them, or naming it and the args that aren't a `T`.
    pub fn args_json<T: DeserializeOwned>(&self) -> Result<T, serde_json::Error> {
        use serde::de::Error;
        let at = self.site();
        let [args] = &self.args[..] else {
            let n = self.args.len();
            return Err(Error::custom(format!("{at} makes {n} function calls, not one")));
        };
        serde_json::from_slice(args)
            .map_err(|e| Error::custom(format!("{at}: {e}: {:?}", String::from_utf8_lossy(args))))
    }

    /// It waited on promise results: a `promise_then`/`promise_and`
    /// callback, a yield's callback, or one the simulator
    /// [`delivered`](Self::delivered).
    pub fn is_callback(&self) -> bool {
        !self.input_data_ids.is_empty()
    }

    /// What it did to storage.
    pub fn storage_ops(&self) -> StorageOps {
        self.storage_ops.clone()
    }

    /// The attached gas a successful receipt left unused: `attached_gas -
    /// (gas_burnt - prepaid_exec_fees) - forwarded`, since the exec fees
    /// were prepaid on top and forwarded gas isn't left over. The margin
    /// before "Exceeded the prepaid gas", unless its calls took the rest by
    /// weight (near-sdk's default): then 0, and their headroom shows the
    /// slack. 0 for a failed receipt (its discarded promises' use is
    /// unknown).
    pub fn headroom(&self) -> Gas {
        if !self.is_success() {
            return Gas::ZERO;
        }
        let ran = self.gas_burnt.saturating_sub(self.prepaid_exec_fees);
        self.attached_gas.saturating_sub(ran.saturating_add(self.forwarded))
    }
}

/// Why a receipt or transaction failed. The raw error stays in its status.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Failure {
    /// "Exceeded the prepaid gas": the receipt's attached gas ran out.
    OutOfGas,
    /// Hit the per-receipt burn limit (`max_gas_burnt`).
    GasLimitExceeded,
    /// The contract panicked, with its message.
    Panic(String),
    /// The receiver account doesn't exist.
    AccountDoesNotExist(AccountId),
    /// Refused at conversion: nothing ran.
    InvalidTx(InvalidTxError),
    /// Any other action error, as nearcore reports it (`LackBalanceForState`,
    /// a method that doesn't resolve, wasm that doesn't compile).
    Action(ActionErrorKind),
    /// The VM's message for any other execution error (a trap, a host
    /// limit).
    Host(String),
}

impl Failure {
    fn new(e: &TxExecutionError) -> Self {
        let a = match e {
            TxExecutionError::ActionError(a) => a,
            TxExecutionError::InvalidTxError(e) => return Self::InvalidTx(e.clone()),
        };
        match &a.kind {
            ActionErrorKind::FunctionCallError(f) => Self::call(f),
            ActionErrorKind::AccountDoesNotExist { account_id } => {
                Self::AccountDoesNotExist(account_id.clone())
            }
            kind => Self::Action(kind.clone()),
        }
    }

    /// The contract's own text: a panic's message (near-sdk's `require!`
    /// and `env::panic_str`), or the VM's for a trap or an abort (a Rust
    /// panic without a message hook). `None` for every other failure.
    pub fn message(&self) -> Option<&str> {
        match self {
            Self::Panic(m) | Self::Host(m) => Some(m),
            _ => None,
        }
    }

    /// nearcore's `MethodNotFound`: the contract exports no such method.
    pub(crate) fn is_missing_method(&self) -> bool {
        use near_primitives::errors::MethodResolveError::MethodNotFound;
        matches!(
            self,
            Self::Action(ActionErrorKind::FunctionCallError(
                FunctionCallError::MethodResolveError(MethodNotFound)
            ))
        )
    }

    /// A call's failure, a view's included.
    pub(crate) fn call(f: &FunctionCallError) -> Self {
        let FunctionCallError::ExecutionError(m) = f else {
            return Self::Action(ActionErrorKind::FunctionCallError(f.clone()));
        };
        if m.starts_with("Exceeded the prepaid gas") {
            Self::OutOfGas
        } else if m.starts_with("Exceeded the maximum amount of gas allowed") {
            Self::GasLimitExceeded
        } else if let Some(p) = m.strip_prefix("Smart contract panicked: ") {
            Self::Panic(p.to_string())
        } else {
            Self::Host(m.clone())
        }
    }
}

/// Names, in `e`, the function its `unreachable` trap is in.
pub(crate) fn name_trap(e: &mut TxExecutionError, name: &str) {
    if let TxExecutionError::ActionError(a) = e
        && let ActionErrorKind::FunctionCallError(FunctionCallError::ExecutionError(m)) =
            &mut a.kind
    {
        *m = crate::trap::named(m, name);
    }
}

const PREPAID: &str = " (a transaction pays for all the gas it attaches when it converts, 300 \
                       Tgas a call unless `.gas(..)` says less, and each one queued before it \
                       in the block has paid for its own)";

impl fmt::Display for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // nearcore rounds these to NEAR: the exact yocto here.
        let storage = |who: &dyn fmt::Display, amount: &Balance| {
            format!(
                "the account {who} wouldn't have enough balance to cover storage, required to \
                 have {} yoctoNEAR more",
                amount.as_yoctonear()
            )
        };
        match self {
            Self::OutOfGas => f.write_str("out of gas (Exceeded the prepaid gas)"),
            Self::GasLimitExceeded => f.write_str("out of gas (per-receipt burn limit)"),
            Self::Panic(m) => write!(f, "panicked: {m}"),
            Self::AccountDoesNotExist(a) => write!(f, "account {a} does not exist"),
            Self::InvalidTx(e @ InvalidTxError::NotEnoughBalance { .. }) => {
                write!(f, "invalid transaction: {}{PREPAID}", plain_ids(&e.to_string()))
            }
            Self::InvalidTx(InvalidTxError::LackBalanceForState { signer_id, amount }) => {
                write!(f, "invalid transaction: {}{PREPAID}", storage(signer_id, amount))
            }
            Self::InvalidTx(e) => write!(f, "invalid transaction: {}", plain_ids(&e.to_string())),
            Self::Action(kind @ ActionErrorKind::LackBalanceForState { account_id, amount }) => {
                write!(f, "{}: {}", variant(kind), storage(account_id, amount))
            }
            // nearcore's `Display` for these is their `Debug`; for the rest,
            // it writes account ids as `AccountId("x")`.
            Self::Action(ActionErrorKind::FunctionCallError(e)) => match e {
                FunctionCallError::CompilationError(CompilationError::CodeDoesNotExist {
                    account_id,
                }) => write!(f, "CodeDoesNotExist: {account_id} has no contract deployed"),
                FunctionCallError::CompilationError(c) => write!(f, "CompilationError: {c:?}"),
                FunctionCallError::MethodResolveError(m) => {
                    write!(f, "MethodResolveError: {}", variant(m))
                }
                FunctionCallError::LinkError { msg } => write!(f, "LinkError: {msg}"),
                e => write!(f, "{e:?}"),
            },
            Self::Action(kind) => write!(f, "{}: {}", variant(kind), plain_ids(&kind.to_string())),
            Self::Host(m) => f.write_str(m),
        }
    }
}

impl std::error::Error for Failure {}

/// `s` with nearcore's `AccountId("x")` written as `x`.
pub(crate) fn plain_ids(s: &str) -> String {
    const ID: &str = "AccountId(\"";
    let mut out = String::new();
    let mut rest = s;
    while let Some(i) = rest.find(ID) {
        let after = &rest[i + ID.len()..];
        let Some(j) = after.find("\")") else { break };
        out.push_str(&rest[..i]);
        out.push_str(&after[..j]);
        rest = &after[j + 2..];
    }
    out.push_str(rest);
    out
}

/// Outcome of a transaction. Its `Display` is the receipt tree, then a gas
/// table, cut to fit about 100 lines (like receipts in a row collapse;
/// a longer tree keeps its ends and first failure); `{:#}` shows all of
/// it: every receipt, the refunds and every log in full.
///
/// A failed call doesn't panic: assert on its outcome, with `"{out}"` in
/// the message for the tree.
///
/// ```
/// use near_contract_sim::prelude::*;
/// use serde_json::json;
///
/// let mut sim = Sim::new();
/// sim.create_account("alice.near", Balance::from_near(10));
/// sim.deploy("token.near", std::fs::read("tests/res/fungible_token.wasm").unwrap());
/// let init = json!({ "owner_id": "alice.near", "total_supply": "1000" });
/// sim.call("token.near", "token.near", "new_default_meta").args_json(init).transact().assert_success();
/// let bob = json!({ "account_id": "bob.near" });
/// let deposit = Balance::from_millinear(125); // the token's storage minimum
/// let register = sim.call("alice.near", "token.near", "storage_deposit").args_json(bob);
/// register.deposit(deposit).transact().assert_success();
/// let send = json!({ "receiver_id": "bob.near", "amount": "10" });
///
/// // near-sdk's `assert_one_yocto`: refused without 1 yoctoNEAR.
/// let out = sim.call("alice.near", "token.near", "ft_transfer").args_json(&send).transact();
/// out.assert_panic_contains("exactly 1 yoctoNEAR");
///
/// let out = sim
///     .call("alice.near", "token.near", "ft_transfer")
///     .args_json(&send)
///     .deposit(Balance::from_yoctonear(1))
///     .transact();
/// out.assert_success();
/// assert_eq!(out.events()[0]["event"], "ft_transfer", "{out}");
/// assert_eq!(out.effects().near_paid("alice.near", "token.near"), Balance::from_yoctonear(1));
/// assert!(out.gas_burnt() > Gas::ZERO);
/// ```
#[non_exhaustive]
#[must_use = "a failed transaction doesn't panic: assert on its outcome, e.g. .assert_success()"]
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct TxOutcome {
    /// The transaction hash, which [`Sim::outcome`] takes.
    pub hash: CryptoHash,
    /// The conversion's outcome; `None` while queued.
    pub transaction: Option<ReceiptOutcome>,
    /// Every receipt's outcome, in execution order, refunds included.
    pub receipts: Vec<ReceiptOutcome>,
    /// As RPC reports it: the result of the last receipt on its result
    /// path. It doesn't wait for receipts off that path: see
    /// [`is_complete`](Self::is_complete).
    pub status: FinalExecutionStatus,
    /// Its receipts that haven't run yet.
    pub pending: Vec<PendingReceipt>,
    pub(crate) effects: Effects,
}

impl TxOutcome {
    /// The receipt the transaction converted to, as signed.
    pub fn first_call(&self) -> Option<&ReceiptOutcome> {
        let id = self.transaction.as_ref()?.receipt_ids.first()?;
        self.receipts.iter().find(|r| r.receipt_id == *id)
    }

    /// Its result path ended in a value. A failed detached receipt doesn't
    /// change it: [`Sim::findings`] covers those.
    pub fn is_success(&self) -> bool {
        matches!(self.status, FinalExecutionStatus::SuccessValue(_))
    }

    /// Its result path ended in a failure, or it was refused at conversion.
    pub fn is_failure(&self) -> bool {
        matches!(self.status, FinalExecutionStatus::Failure(_))
    }

    /// Still queued, or its result path hasn't ended.
    pub fn is_pending(&self) -> bool {
        matches!(self.status, FinalExecutionStatus::NotStarted | FinalExecutionStatus::Started)
    }

    /// Every receipt it spawned has run, not just its result path: a
    /// success can be incomplete while a refund or detached promise waits.
    pub fn is_complete(&self) -> bool {
        self.effects.is_complete()
    }

    /// What it did, from its own receipts only.
    #[must_use]
    pub fn effects(&self) -> &Effects {
        &self.effects
    }

    /// Panics unless the transaction succeeded: the first line names the
    /// receipt that failed first, then the trace follows.
    #[track_caller]
    pub fn assert_success(&self) -> &Self {
        if !self.is_success() {
            panic!("expected success; {}{}", verdict(self), trace(self));
        }
        self
    }

    /// Why the transaction failed. Panics with the trace unless it did.
    #[track_caller]
    pub fn assert_failure(&self) -> Failure {
        match self.failure() {
            Some(f) => f,
            None => panic!("expected failure; {}{}", verdict(self), trace(self)),
        }
    }

    /// The panic or abort message ([`Failure::message`]) the transaction
    /// failed with, which must contain `fragment`, as
    /// `#[should_panic(expected = ..)]` matches. Panics with the trace
    /// otherwise.
    #[track_caller]
    pub fn assert_panic_contains(&self, fragment: &str) -> String {
        let got = match self.failure() {
            None => verdict(self),
            Some(f) => match f.message() {
                Some(m) if m.contains(fragment) => return m.to_string(),
                _ => {
                    // The first failure, if another receipt's.
                    let (why, first) = (failure_line(&f), verdict(self));
                    match first.ends_with(&why) {
                        true => format!("it failed with: {why}"),
                        false => format!("it failed with: {why}; {first}"),
                    }
                }
            },
        };
        panic!("expected a panic containing {fragment:?}; {got}{}", trace(self))
    }

    /// The return value as JSON. Panics with the trace unless the
    /// transaction succeeded with a `T`.
    #[track_caller]
    pub fn json<T: DeserializeOwned>(&self) -> T {
        match decode(self.bytes()) {
            Ok(t) => t,
            Err(e) => panic!("{e}{}", trace(self)),
        }
    }

    /// The return value as a `u128`: a near-sdk `U128` (`"40"`) or a JSON
    /// number. Panics with the trace unless the transaction succeeded with
    /// one.
    #[track_caller]
    pub fn u128(&self) -> u128 {
        let v: serde_json::Value = self.json();
        match u128_of(&v) {
            Ok(n) => n,
            Err(e) => panic!("{v} isn't a u128: {e}{}", trace(self)),
        }
    }

    /// The return value's bytes. Panics with the trace unless the
    /// transaction succeeded.
    #[track_caller]
    pub fn bytes(&self) -> &[u8] {
        self.assert_success().value().unwrap_or_default()
    }

    /// The return value, if the transaction succeeded.
    pub fn value(&self) -> Option<&[u8]> {
        match &self.status {
            FinalExecutionStatus::SuccessValue(v) => Some(v),
            _ => None,
        }
    }

    /// Why the transaction failed, if it did.
    pub fn failure(&self) -> Option<Failure> {
        match &self.status {
            FinalExecutionStatus::Failure(e) => Some(Failure::new(e)),
            _ => None,
        }
    }

    /// The first receipt that ran `method`.
    pub fn receipt(&self, method: &str) -> Option<&ReceiptOutcome> {
        self.receipts.iter().find(|r| r.method_names.iter().any(|m| m == method))
    }

    /// The first receipt that ran `method`. Panics with the trace if none did.
    #[track_caller]
    pub fn expect_receipt(&self, method: &str) -> &ReceiptOutcome {
        match self.receipt(method) {
            Some(r) => r,
            None => panic!("no receipt ran {method}{}", trace(self)),
        }
    }

    /// The [`headroom`](ReceiptOutcome::headroom) of the first receipt that
    /// ran `method`. Panics with the trace if none did, it failed, it left
    /// less than `min`, or it kept none while forwarding gas (weighted
    /// forwarding: judge its calls; the sim can't see weights).
    #[track_caller]
    pub fn assert_headroom(&self, method: &str, min: Gas) -> Gas {
        let r = self.expect_receipt(method);
        let left = r.headroom();
        let why = match r.failure() {
            Some(f) => format!("it failed: {f}"),
            None if left == Gas::ZERO && r.forwarded > Gas::ZERO => {
                "it kept no headroom and its unused gas went to its calls, as weighted \
                 forwarding does: assert on theirs; if their gas is fixed, it is at its own \
                 limit"
                    .into()
            }
            None if left < min => format!("it left {}", tgas(left)),
            None => return left,
        };
        panic!("expected {method} to leave {}, {why}{}", tgas(min), trace(self));
    }

    /// The logs of every receipt that succeeded, in execution order;
    /// [`all_logs`](Self::all_logs) adds failed receipts'.
    pub fn logs(&self) -> Vec<&str> {
        let ok = self.receipts.iter().filter(|r| r.failure().is_none());
        ok.flat_map(|r| r.logs.iter().map(String::as_str)).collect()
    }

    /// Every receipt's logs, failed receipts' included, in execution order.
    pub fn all_logs(&self) -> Vec<&str> {
        self.receipts.iter().flat_map(|r| r.logs.iter().map(String::as_str)).collect()
    }

    /// The NEP-297 events of [`logs`](Self::logs), each the whole object.
    /// Panics with the trace if one isn't JSON.
    #[track_caller]
    pub fn events(&self) -> Vec<serde_json::Value> {
        let mut out = vec![];
        for e in self.logs().into_iter().filter_map(|l| l.strip_prefix("EVENT_JSON:")) {
            match serde_json::from_str(e) {
                Ok(v) => out.push(v),
                Err(err) => panic!("EVENT_JSON:{e} isn't JSON: {err}{}", trace(self)),
            }
        }
        out
    }

    /// Gas burnt by the conversion and every receipt, refunds included: a
    /// refund receipt burns gas nobody is charged for (the gas table's
    /// "refunds (not charged)"), so `gas_burnt() * gas price` is above what
    /// the signer paid: that is `effects().gas_paid()`.
    pub fn gas_burnt(&self) -> Gas {
        self.transaction
            .iter()
            .chain(&self.receipts)
            .fold(Gas::ZERO, |g, r| g.saturating_add(r.gas_burnt))
    }

    /// Tokens burnt by the conversion and every receipt.
    pub fn tokens_burnt(&self) -> Balance {
        self.transaction
            .iter()
            .chain(&self.receipts)
            .fold(Balance::ZERO, |b, r| b.saturating_add(r.tokens_burnt))
    }
}

/// The total a receipt tried to log when it failed on the log limit, from
/// nearcore's `TotalLogLengthExceeded` message.
fn tried_to_log(r: &ReceiptOutcome) -> Option<u64> {
    let Some(Failure::Host(m)) = r.failure() else { return None };
    let rest = m.strip_prefix("The length of a log message ")?;
    rest.split_once(" exceeds the limit ")?.0.parse().ok()
}

impl Sim {
    /// The outcome of transaction `hash` so far. Panics on a hash that
    /// isn't a submitted transaction's.
    #[track_caller]
    pub fn outcome(&self, hash: impl Borrow<CryptoHash>) -> TxOutcome {
        let root = *hash.borrow();
        let log = self.chain.log();
        assert!(log.txs.contains_key(&root), "unknown transaction {root}");
        let mut outcomes = self.ran(root).into_iter();
        let transaction = outcomes.next();
        let receipts: Vec<ReceiptOutcome> = outcomes.collect();
        let pending: Vec<PendingReceipt> =
            self.chain.pending().into_iter().filter(|p| p.tx == root).collect();
        let gas = self.gas_paid(root, transaction.as_ref(), &receipts);
        let watched = log.token_verdicts(&receipts);
        let effects = Effects::of(transaction.as_ref(), &receipts, pending.clone(), gas, watched);
        TxOutcome {
            hash: root,
            transaction,
            receipts,
            status: self.tx_status(root),
            pending,
            effects,
        }
    }

    /// What of transaction `root` has run, conversion first; later blocks
    /// only add to the end.
    pub(crate) fn ran(&self, root: CryptoHash) -> Vec<ReceiptOutcome> {
        let mut executed = self.chain.log().outcomes(root);
        executed.sort();
        executed.into_iter().map(|at| self.receipt_outcome(at)).collect()
    }

    /// What the signer of `root` paid for gas: nearcore's `tx_cost` at the
    /// conversion's price, less the gas refunds that ran.
    fn gas_paid(
        &self,
        root: CryptoHash,
        transaction: Option<&ReceiptOutcome>,
        receipts: &[ReceiptOutcome],
    ) -> Balance {
        let Some(t) = transaction.filter(|t| t.is_success() && t.gas_burnt > Gas::ZERO) else {
            return Balance::ZERO;
        };
        let log = self.chain.log();
        let (b, _) = log.position(&root).expect("a converted transaction");
        let rc = priced_config(log.blocks[b].protocol_version, log.blocks[b].pricing);
        // The conversion burnt its gas at the block's price.
        let price = t.tokens_burnt.as_yoctonear() / u128::from(t.gas_burnt.as_gas());
        let cost = tx_cost(rc, &log.txs[&root].transaction, Balance::from_yoctonear(price))
            .expect("tx cost overflow")
            .gas_cost;
        receipts
            .iter()
            .filter(|r| r.refund == Some(Refund::Gas) && r.is_success())
            .fold(cost, |paid, r| paid.saturating_sub(r.deposit))
    }

    /// [`TxOutcome::status`] of the transaction `root`, without its outcomes.
    pub(crate) fn tx_status(&self, root: CryptoHash) -> FinalExecutionStatus {
        let log = self.chain.log();
        match log.status(&log.result_end(root)) {
            Some(ExecutionStatus::SuccessValue(v)) => FinalExecutionStatus::SuccessValue(v.clone()),
            Some(ExecutionStatus::Failure(e)) => FinalExecutionStatus::Failure(e.clone()),
            // Not converted yet.
            _ if log.position(&root).is_none() => FinalExecutionStatus::NotStarted,
            _ => FinalExecutionStatus::Started,
        }
    }

    /// Every receipt that ran in the blocks in `heights`, in execution
    /// order: `receipts_in(mark + 1..)` is what ran since `mark`.
    pub fn receipts_in(&self, heights: impl RangeBounds<u64>) -> Vec<ReceiptOutcome> {
        let log = self.chain.log();
        let blocks = log.blocks.iter().enumerate().filter(|(_, b)| heights.contains(&b.height));
        blocks
            .flat_map(|(b, block)| (0..block.outcomes.len()).map(move |pos| (b, pos)))
            .filter(|&at| !log.txs.contains_key(&log.at(at).1.id))
            .map(|at| self.receipt_outcome(at))
            .collect()
    }

    /// The bytes `r` logged (or tried to, past the limit), and the limit.
    pub(crate) fn log_bytes(&self, r: &ReceiptOutcome) -> (u64, u64) {
        let limit = self.config.runtime_config().wasm_config.limit_config.max_total_log_length;
        let logged = r.logs.iter().map(|l| l.len() as u64).sum::<u64>();
        (logged.max(tried_to_log(r).unwrap_or(0)), limit)
    }

    pub(crate) fn receipt_outcome(&self, at: (usize, usize)) -> ReceiptOutcome {
        let log = self.chain.log();
        let (block, e) = log.at(at);
        let (o, body) = (&e.outcome, log.receipts.get(&e.id));
        let receipt = body.and_then(|r| action_receipt(r));
        // A local receipt has no body: its transaction describes it.
        let tx = log.converted_from(&e.id);
        let actions = receipt.as_ref().map(|r| r.actions()).or(tx.map(|t| t.actions()));
        // Exec fees are priced by the config of the block that runs it.
        let rc = priced_config(block.protocol_version, block.pricing);
        let exec_fees = actions.map_or(vec![], |a| exec_fees(rc, a, &o.executor_id));
        let actions = actions.unwrap_or_default();
        let (attached_gas, deposit) = attached(actions);
        // Refunds are the runtime's own receipts, prepaid by nobody.
        let forwarded = o
            .receipt_ids
            .iter()
            .filter_map(|id| log.receipts.get(id))
            .filter(|r| !r.predecessor_id().is_system())
            .fold(Gas::ZERO, |g, r| g.saturating_add(prepaid(rc, r)));
        // A local receipt's predecessor, and the conversion's, is the signer.
        let signer = tx.or(log.txs.get(&e.id).map(|t| &t.transaction)).map(|t| t.signer_id());
        let predecessor_id =
            body.map(|r| r.predecessor_id()).or(signer).expect("a body or a transaction").clone();
        let gas_profile: BTreeMap<String, u64> = ExecutionMetadataView::from(o.metadata.clone())
            .gas_profile
            .unwrap_or_default()
            .into_iter()
            .map(|c| (c.cost, c.gas_used.as_gas()))
            .collect();
        let storage_ops = StorageOps::of(&gas_profile, rc, log.written(&e.id));
        let missing =
            matches!(&o.status, ExecutionStatus::Failure(f) if Failure::new(f).is_missing_method());
        // The code it ran against: the state its block applied on.
        let ran = ViewAt {
            height: block.height,
            timestamp_ns: block.timestamp_ns,
            protocol_version: block.protocol_version,
            root: block.prev_state_root,
        };
        let exports = missing.then(|| self.exports(ran, &o.executor_id)).flatten();
        ReceiptOutcome {
            receipt_id: e.id,
            tx: log.origin.get(&e.id).copied().unwrap_or(e.id),
            receiver_id: o.executor_id.clone(),
            refund: predecessor_id.is_system().then(|| refund_kind(log, at.0, &e.id, body)),
            predecessor_id,
            method_names: method_names(actions),
            args: calls(actions).map(|c| c.args.clone()).collect(),
            actions: actions.iter().map(|a| a.as_ref().to_string()).collect(),
            block_height: block.height,
            block_timestamp_ns: block.timestamp_ns,
            gas_burnt: o.gas_burnt,
            tokens_burnt: o.tokens_burnt,
            logs: o.logs.clone(),
            status: o.status.clone(),
            receipt_ids: o.receipt_ids.clone(),
            gas_profile,
            output_data_receivers: receipt
                .as_ref()
                .map_or(vec![], |a| a.output_data_receivers().to_vec()),
            input_data_ids: receipt.as_ref().map_or(vec![], |a| a.input_data_ids().to_vec()),
            forwarded,
            attached_gas,
            deposit,
            prepaid_exec_fees: charged_exec_fees(&exec_fees, &o.status),
            unhandled: log.unhandled.binary_search(&at).is_ok(),
            delivered: log.injected.contains(&e.id),
            fault: log.faulted.get(&e.id).cloned(),
            held: log.held.get(&e.id).copied(),
            result_held: receipt
                .as_ref()
                .and_then(|a| {
                    a.input_data_ids().iter().filter_map(|d| log.held_results.get(d)).max()
                })
                .copied(),
            token_moves: log.token_moves(&e.id),
            storage: log.storage_changes(&e.id),
            storage_ops,
            exports,
        }
    }

    /// The methods `id`'s contract exports at `at`, in name order; `None`
    /// without code.
    pub(crate) fn exports(&self, at: ViewAt, id: &AccountId) -> Option<Vec<String>> {
        Some(crate::trap::exports(&self.chain.code(&self.config, at, id)?))
    }
}

/// Which refund the runtime's receipt `id`, run in block `b`, is: gas by
/// its body, a deleted account's balance if its creator deleted one, else
/// a deposit.
fn refund_kind(log: &Log, b: usize, id: &CryptoHash, body: Option<&Arc<Receipt>>) -> Refund {
    if body.is_some_and(|r| is_gas_refund(r)) {
        return Refund::Gas;
    }
    let deletes =
        |actions: &[Action]| actions.iter().any(|a| matches!(a, Action::DeleteAccount(_)));
    // Its creator ran before it, almost always in the block before.
    let parent = log.blocks[..=b]
        .iter()
        .rev()
        .flat_map(|block| &block.outcomes)
        .find(|o| o.outcome.receipt_ids.contains(id));
    let deleted =
        parent.is_some_and(|p| match log.receipts.get(&p.id).and_then(|r| action_receipt(r)) {
            Some(a) => deletes(a.actions()),
            None => log.converted_from(&p.id).is_some_and(|t| deletes(t.actions())),
        });
    if deleted { Refund::AccountDeletion } else { Refund::Deposit }
}

fn attached(actions: &[Action]) -> (Gas, Balance) {
    actions.iter().fold((Gas::ZERO, Balance::ZERO), |(g, d), a| {
        (g.saturating_add(a.get_prepaid_gas()), d.saturating_add(a.get_deposit_balance()))
    })
}

/// `new_action_receipt`'s exec fee, then each action's.
fn exec_fees(rc: &RuntimeConfig, actions: &[Action], receiver: &AccountId) -> Vec<Gas> {
    let receipt = rc.fees.fee(ActionCosts::new_action_receipt).exec_fee();
    let each = actions.iter().map(|a| exec_fee(rc, a, receiver).expect("exec fee overflow"));
    std::iter::once(receipt).chain(each).map(|c| c.gas).collect()
}

/// What `r`'s creator prepaid for it: attached gas and exec fees.
fn prepaid(rc: &RuntimeConfig, r: &Receipt) -> Gas {
    action_receipt(r).map_or(Gas::ZERO, |a| prepaid_gas(rc, a.actions(), r.receiver_id()))
}

/// The part of `exec_fees` a receipt burnt: the runtime charges each
/// action's fee as it starts it, up to the first failing one; an oversized
/// promise input fails before any.
fn charged_exec_fees(exec_fees: &[Gas], status: &ExecutionStatus) -> Gas {
    let charged = match status {
        ExecutionStatus::Failure(TxExecutionError::ActionError(e)) => match (&e.kind, e.index) {
            (ActionErrorKind::TotalPromiseInputSizeExceeded { .. }, _) => 1,
            (_, Some(i)) => i as usize + 2,
            (_, None) => exec_fees.len(),
        },
        _ => exec_fees.len(),
    };
    exec_fees.iter().take(charged).fold(Gas::ZERO, |sum, g| sum.saturating_add(*g))
}

/// `bytes` as a JSON `T`, or why not: serde's error and the bytes, and a
/// pointer at `.u128()` for a near-sdk `U128` read as a number.
pub(crate) fn decode<T: DeserializeOwned>(bytes: &[u8]) -> Result<T, String> {
    serde_json::from_slice(bytes).map_err(|e| {
        let u128 = serde_json::from_slice::<String>(bytes)
            .is_ok_and(|s| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()));
        let hint = if u128 {
            "; near-sdk's U128 or U64 is a JSON string: read it with .u128()"
        } else {
            ""
        };
        format!("{e}: {:?}{hint}", String::from_utf8_lossy(bytes))
    })
}

/// A near-sdk `U128` (a JSON string of digits) or a JSON number as a `u128`.
pub(crate) fn u128_of(v: &serde_json::Value) -> Result<u128, std::num::ParseIntError> {
    v.as_str().map_or_else(|| v.to_string(), str::to_owned).parse()
}

#[cfg(test)]
mod tests {
    use super::*;
    use near_primitives::errors::{ActionError, MethodResolveError};

    fn action_err(kind: ActionErrorKind) -> TxExecutionError {
        TxExecutionError::ActionError(ActionError { index: Some(0), kind })
    }

    fn exec_err(m: &str) -> TxExecutionError {
        action_err(ActionErrorKind::FunctionCallError(FunctionCallError::ExecutionError(m.into())))
    }

    /// nearcore reports every VM host error, running out of gas included, as
    /// an `ExecutionError` message (`conversions.rs`): the messages are what
    /// `Failure` classifies.
    #[test]
    fn classifies_failures() {
        use Failure::*;
        let of = |e| Failure::new(&e);
        let missing =
            ActionErrorKind::AccountDoesNotExist { account_id: "x.near".parse().unwrap() };
        assert_eq!(of(action_err(missing)), AccountDoesNotExist("x.near".parse().unwrap()));
        assert_eq!(of(exec_err("Exceeded the prepaid gas.")), OutOfGas);
        assert_eq!(
            of(exec_err("Exceeded the maximum amount of gas allowed to burn per contract.")),
            GasLimitExceeded
        );
        assert_eq!(of(exec_err("Smart contract panicked: boom")), Panic("boom".into()));
        assert_eq!(of(exec_err("wasm trap")), Host("wasm trap".into()));
        let unresolved = FunctionCallError::MethodResolveError(MethodResolveError::MethodNotFound);
        let unresolved = ActionErrorKind::FunctionCallError(unresolved);
        assert_eq!(of(action_err(unresolved.clone())), Action(unresolved));
        let expired = ActionErrorKind::DelegateActionExpired;
        assert_eq!(of(action_err(expired.clone())), Action(expired));
        let invalid = TxExecutionError::InvalidTxError(InvalidTxError::InvalidSignature);
        assert_eq!(of(invalid), InvalidTx(InvalidTxError::InvalidSignature));
    }

    #[test]
    fn failure_display() {
        use Failure::*;
        let cases = [
            (OutOfGas, "out of gas (Exceeded the prepaid gas)"),
            (GasLimitExceeded, "out of gas (per-receipt burn limit)"),
            (Panic("boom".into()), "panicked: boom"),
            (AccountDoesNotExist("x.near".parse().unwrap()), "account x.near does not exist"),
            (
                InvalidTx(InvalidTxError::InvalidSignature),
                "invalid transaction: Transaction is not signed with the given public key",
            ),
            (
                Action(ActionErrorKind::DelegateActionExpired),
                "DelegateActionExpired: DelegateAction has expired",
            ),
            (
                Action(ActionErrorKind::LackBalanceForState {
                    account_id: "x.near".parse().unwrap(),
                    amount: Balance::from_yoctonear(1),
                }),
                "LackBalanceForState: the account x.near wouldn't have enough balance to cover \
                 storage, required to have 1 yoctoNEAR more",
            ),
            (Host("odd".into()), "odd"),
        ];
        for (c, s) in cases {
            assert_eq!(c.to_string(), s);
        }
        let poor = InvalidTxError::LackBalanceForState {
            signer_id: "x.near".parse().unwrap(),
            amount: Balance::from_near(2),
        };
        let want = "invalid transaction: the account x.near wouldn't have enough balance to \
                    cover storage, required to have 2000000000000000000000000 yoctoNEAR more \
                    (a transaction pays";
        assert!(InvalidTx(poor).to_string().starts_with(want));
    }

    /// `log_bytes` sums a receipt's logs; one that fails on the log limit
    /// counts what it tried to log.
    #[test]
    fn log_bytes_counts_what_a_receipt_tried_to_log() {
        use crate::{Reply, Stub};
        let mut sim = Sim::new();
        sim.create_account("alice.near", Balance::from_near(100));
        let stub = Stub::new()
            .on("logs", Reply::json(0).logs(vec!["x".repeat(1000); 3]))
            .on("over", Reply::json(0).logs(vec!["x".repeat(10_000); 2]));
        sim.deploy_stub("s.near", stub);
        let call = |sim: &mut Sim, m: &str| sim.call("alice.near", "s.near", m).transact();
        let out = call(&mut sim, "logs");
        assert_eq!(sim.log_bytes(&out.receipts[0]), (3000, 16 << 10));
        let out = call(&mut sim, "over");
        assert_eq!(
            out.receipts[0].failure().map(|f| f.to_string()).as_deref(),
            Some("The length of a log message 20000 exceeds the limit 16384")
        );
        assert_eq!(sim.log_bytes(&out.receipts[0]), (20_000, 16 << 10));
    }

    /// A callback `Tx::promise_results` delivers is a receipt the size of
    /// the one real promises deliver.
    #[test]
    fn an_injected_callback_is_the_size_of_a_delivered_one() {
        use crate::PromiseResult;
        let wat = crate::sim::tests::manifest_dir().join("tests/res/callback_probe.wat");
        let mut base = Sim::new();
        base.config.protocol_version = 84;
        base.create_account("alice.near", Balance::from_near(100));
        base.deploy("probe.near", wat::parse_file(wat).unwrap());
        let mut real = base.clone();
        let out = real.call("alice.near", "probe.near", "start").args(b"hello").transact();
        let want = out.expect_receipt("cb");
        let mut sim = base.clone();
        let out = sim
            .call("alice.near", "probe.near", "cb")
            .args(b"hello")
            .gas(Gas::from_teragas(20))
            .promise_results([PromiseResult::Successful(b"hello".to_vec())])
            .transact();
        let got = out.expect_receipt("cb");
        let size = |sim: &Sim, r: &ReceiptOutcome| {
            borsh::object_length(&sim.chain.log().receipts[&r.receipt_id]).expect("borsh")
        };
        assert_eq!(size(&sim, got), size(&real, want));
    }

    #[test]
    fn charged_exec_fees_stop_at_the_failing_action() {
        let fees: Vec<Gas> = [1, 10, 100, 1000].map(Gas::from_gas).to_vec();
        let failed = |index, kind| {
            ExecutionStatus::Failure(TxExecutionError::ActionError(ActionError { index, kind }))
        };
        let missing =
            || ActionErrorKind::AccountDoesNotExist { account_id: "x.near".parse().unwrap() };
        let charged = |s: &ExecutionStatus| charged_exec_fees(&fees, s).as_gas();
        assert_eq!(charged(&ExecutionStatus::SuccessValue(vec![])), 1111);
        assert_eq!(charged(&failed(Some(0), missing())), 11);
        assert_eq!(charged(&failed(Some(1), missing())), 111);
        // No index: the receipt failed after its actions (e.g. storage stake).
        assert_eq!(charged(&failed(None, missing())), 1111);
        let oversized = ActionErrorKind::TotalPromiseInputSizeExceeded { size: 2, limit: 1 };
        assert_eq!(charged(&failed(None, oversized)), 1);
        assert_eq!(charged_exec_fees(&[], &ExecutionStatus::SuccessValue(vec![])), Gas::ZERO);
    }
}

/// What `#[must_use]` catches: a dropped outcome, whose failure would
/// pass silently.
///
/// ```compile_fail
/// #![deny(unused_must_use)]
/// let mut sim = near_contract_sim::Sim::new();
/// sim.call("alice.near", "status.near", "set_status").transact();
/// ```
///
/// ```compile_fail
/// #![deny(unused_must_use)]
/// let sim = near_contract_sim::Sim::new();
/// sim.view("status.near", "get_status", ());
/// ```
///
/// ```compile_fail
/// #![deny(unused_must_use)]
/// near_contract_sim::Sim::new().findings();
/// ```
///
/// Each compiles once read:
///
/// ```no_run
/// #![deny(unused_must_use)]
/// let mut sim = near_contract_sim::Sim::new();
/// sim.call("alice.near", "status.near", "set_status").transact().assert_success();
/// sim.view("status.near", "get_status", ()).assert_success();
/// sim.findings().assert_none();
/// ```
#[cfg(doctest)]
struct MustUse;
