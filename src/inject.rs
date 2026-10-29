//! Injection: force something on a receipt a transaction spawns
//! ([`Tx::inject`]: which [`Receipts`], what [`Fault`]), so the branches a
//! node rarely takes run under test. See the
//! [guide](https://github.com/near/near-contract-sim/blob/main/skills/near-contract-sim/references/faults.md).
//!
//! A fault changes what the runtime is given, never what it reports: the
//! outcome, refunds and promise results are nearcore's own for that receipt.
//!
//! [`Tx::inject`]: crate::Tx::inject

use crate::render::tgas;
use crate::sim::IntoAccountId;
use borsh::{BorshDeserialize, BorshSerialize};
use near_primitives::hash::CryptoHash;
use near_primitives::types::{AccountId, Gas};
use serde::{Deserialize, Serialize};
use std::fmt;

/// What to force on a spawned receipt ([`Tx::inject`](crate::Tx::inject)).
/// `PromiseFailed` and `ReturnValue` need a callback; `Hold`, `HoldUntil`
/// and `HoldResult` only delay, failing nothing.
#[non_exhaustive]
#[derive(
    Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, BorshSerialize, BorshDeserialize,
)]
pub enum Fault {
    /// Every call the callback waited on fails as with
    /// [`OutOfGas`](Self::OutOfGas) (deposit refunded, no promises made), and
    /// the callback reads `PromiseResult::Failed`. Needs a callback whose
    /// calls each left the same block and make a function call.
    PromiseFailed,
    /// The receipt's calls each run with 1 gas (nearcore refuses 0) and fail
    /// at contract load with [`Failure::OutOfGas`](crate::Failure::OutOfGas);
    /// the whole batch rolls back, a `Stake` included. A contract sees what
    /// running out partway shows, but this burns almost nothing; the rest is
    /// refunded as with [`Gas`](Self::Gas). Needs a function call.
    OutOfGas,
    /// The receipt's call runs with this much attached gas; the difference
    /// goes through the creator's gas refund, so balances are the chain's
    /// had the creator attached it. Panics if the refund can't cover more
    /// than the receipt had. Needs exactly one function call and at least 1
    /// gas.
    Gas(Gas),
    /// Each successful promise result the callback reads is these bytes, the
    /// callee's effects standing. A failed result stays failed, and a
    /// result from a receipt with no function call stays empty. At most
    /// `max_length_returned_data` bytes. Needs a callback.
    ReturnValue(Vec<u8>),
    /// The receipt is held back this many more blocks (at least 1), as
    /// nearcore's outgoing buffer can hold one: later receipts overtake it,
    /// and held ones go out in the order held. A callback still waits for
    /// all its results. Shown by
    /// [`ReceiptOutcome::held`](crate::ReceiptOutcome::held). Fits any
    /// receipt that leaves its block (a yield's callback never does); the
    /// [fidelity guide](https://github.com/near/near-contract-sim/blob/main/skills/near-contract-sim/references/fidelity.md)
    /// cites nearcore's ordering guarantees.
    Hold(u64),
    /// The receipt's result (its data receipt) is held back this many more
    /// blocks, as [`Hold`](Self::Hold) holds a receipt; a returned promise
    /// carries the hold on. Shown by
    /// [`ReceiptOutcome::result_held`](crate::ReceiptOutcome::result_held).
    /// Needs a receipt whose result goes to a callback.
    HoldResult(u64),
    /// The receipt is held back, as [`Hold`](Self::Hold) holds one, until
    /// the receipt these [`Receipts`] name has run, then goes out in the
    /// next block (behind any held before it): it runs after that one,
    /// however their blocks fall. They name one receipt of the same
    /// transaction, or of the one [`in_tx`](Receipts::in_tx) names: the
    /// first they select, or the [`nth`](Receipts::nth) counting every one
    /// they select, from that transaction's first spawned receipt on.
    /// Panics, as a fault that doesn't fit does, if
    /// that receipt already ran or is the held one, or if it hasn't run
    /// when nothing else is in flight (no transaction queued, no receipt
    /// incoming, delayed or buffered; a parked yield isn't, even one about
    /// to time out), the receipt going out then. Receipts held
    /// after it wait behind it, so one it waits for can't be among them.
    /// Shown by [`ReceiptOutcome::held`](crate::ReceiptOutcome::held).
    /// Fits any receipt.
    ///
    /// Forced. The sim shows this order is possible on one shard; it does
    /// not show that a network delays a receipt that long. Held receipts
    /// share one queue where a node keeps one per route (fidelity.md).
    HoldUntil(Receipts),
}

impl Fault {
    /// Its variant's name: a finding's identity holds it.
    pub(crate) fn kind(&self) -> &'static str {
        match self {
            Fault::PromiseFailed => "PromiseFailed",
            Fault::OutOfGas => "OutOfGas",
            Fault::Gas(_) => "Gas",
            Fault::ReturnValue(_) => "ReturnValue",
            Fault::Hold(_) => "Hold",
            Fault::HoldResult(_) => "HoldResult",
            Fault::HoldUntil(_) => "HoldUntil",
        }
    }

    /// Its slot: what a receipt runs with (0), when it leaves (1), when its
    /// result leaves (2). The first injection per slot wins.
    pub(crate) fn slot(&self) -> usize {
        match self {
            Self::PromiseFailed | Self::OutOfGas | Self::Gas(_) | Self::ReturnValue(_) => 0,
            Self::Hold(_) | Self::HoldUntil(_) => 1,
            Self::HoldResult(_) => 2,
        }
    }

    /// It delays, and fails nothing.
    pub(crate) fn is_hold(&self) -> bool {
        self.slot() > 0
    }
}

/// `Gas(5.00 Tgas)`, `ReturnValue(3 bytes)`, `Hold(2)`,
/// `HoldUntil(Receipts::method("m"))`.
impl fmt::Display for Fault {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::PromiseFailed => f.write_str("PromiseFailed"),
            Self::OutOfGas => f.write_str("OutOfGas"),
            Self::Gas(g) => write!(f, "Gas({})", tgas(*g)),
            Self::ReturnValue(v) => write!(f, "ReturnValue({} bytes)", v.len()),
            Self::Hold(n) => write!(f, "Hold({n})"),
            Self::HoldResult(n) => write!(f, "HoldResult({n})"),
            Self::HoldUntil(r) => write!(f, "HoldUntil({r})"),
        }
    }
}

/// Which spawned receipts an injection applies to: not the transaction's
/// first receipt (that is [`Tx::gas`](crate::Tx::gas)), refunds or data
/// receipts. Receipts count in the order they leave their blocks, which is
/// deterministic up to the first injection that changes something, so
/// [`index`](Self::index) names one receipt exactly.
#[must_use = "a selector does nothing until Tx::inject(selector, fault)"]
#[derive(
    Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, BorshSerialize, BorshDeserialize,
)]
pub struct Receipts {
    filter: Filter,
    nth: Option<usize>,
    /// An address ([`occurrence`](Receipts::occurrence)), not a sample.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    exact: bool,
    /// The transaction whose receipts it counts ([`in_tx`](Receipts::in_tx)),
    /// if not the injecting one's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    in_tx: Option<CryptoHash>,
}

#[derive(
    Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, BorshSerialize, BorshDeserialize,
)]
enum Filter {
    All,
    Callbacks,
    CrossContractCallbacks,
    Callback(String),
    Method(String),
    Call(AccountId, String),
    Receiver(AccountId),
    Action(String),
    Index(usize),
}

impl Receipts {
    fn of(filter: Filter) -> Self {
        Receipts { filter, nth: None, exact: false, in_tx: None }
    }

    /// The method it names, and the receiver if it names one.
    pub(crate) fn method_named(&self) -> Option<(Option<&AccountId>, &str)> {
        match &self.filter {
            Filter::Callback(m) | Filter::Method(m) => Some((None, m)),
            Filter::Call(a, m) => Some((Some(a), m)),
            _ => None,
        }
    }

    /// Every receipt the transaction spawns.
    pub fn all() -> Self {
        Self::of(Filter::All)
    }

    /// Every callback (`promise_then`, `promise_and`).
    pub fn callbacks() -> Self {
        Self::of(Filter::Callbacks)
    }

    /// Every callback whose calls all go to other accounts: a network fails
    /// those, rarely a contract's call to itself, so
    /// `explore::Faults::tolerable()` (the default) fails only these.
    pub fn cross_contract_callbacks() -> Self {
        Self::of(Filter::CrossContractCallbacks)
    }

    /// Every callback calling `method`.
    pub fn callback(method: &str) -> Self {
        Self::of(Filter::Callback(method.into()))
    }

    /// Every receipt calling `method`, on any receiver.
    pub fn method(method: &str) -> Self {
        Self::of(Filter::Method(method.into()))
    }

    /// Every receipt calling `method` on `receiver`.
    #[track_caller]
    pub fn call(receiver: impl IntoAccountId, method: &str) -> Self {
        Self::of(Filter::Call(receiver.into_account_id(), method.into()))
    }

    /// Every receipt to `id`.
    #[track_caller]
    pub fn receiver(id: impl IntoAccountId) -> Self {
        Self::of(Filter::Receiver(id.into_account_id()))
    }

    /// Every receipt with an action of this kind (`"Stake"`, `"Transfer"`,
    /// ..., as [`ReceiptOutcome::actions`](crate::ReceiptOutcome::actions)
    /// names them). Panics on a name that isn't nearcore's.
    #[track_caller]
    pub fn action(kind: &str) -> Self {
        assert!(
            ACTIONS.contains(&kind),
            "Receipts::action({kind:?}): not one of nearcore's actions, {}",
            ACTIONS.join(", ")
        );
        Self::of(Filter::Action(kind.into()))
    }

    /// The `n`th receipt the transaction spawns, from 0.
    pub fn index(n: usize) -> Self {
        Self::of(Filter::Index(n))
    }

    /// Only the `k`th (from 0) selected receipt the fault fits; receipts it
    /// doesn't fit aren't counted. Replaces an earlier `nth`.
    pub fn nth(mut self, k: usize) -> Self {
        self.nth = Some(k);
        self
    }

    /// The `k`th (from 0) receipt calling `method` on `receiver` that the
    /// fault fits: how scans and replays name a receipt, stable when a build
    /// spawns other receipts earlier.
    #[track_caller]
    pub fn occurrence(receiver: impl IntoAccountId, method: &str, k: usize) -> Self {
        Receipts { exact: true, ..Self::call(receiver, method).nth(k) }
    }

    /// The same receipts of transaction `hash` instead: what a
    /// [`Fault::HoldUntil`] in a later transaction (or one queued after it
    /// in the same block) waits for, as in
    /// `HoldUntil(Receipts::callback("on_deposit").in_tx(t1))`. Counts as
    /// [`HoldUntil`](Fault::HoldUntil) does, from `hash`'s first spawned
    /// receipt; never its first receipt, nor one congestion control held
    /// back. Only a `HoldUntil` takes it: submitting a transaction whose
    /// injection selects with it panics, as does one whose `hash` names no
    /// transaction submitted before it. One from before the sim was loaded
    /// from a snapshot ([`Sim::cached`](crate::Sim::cached), a
    /// [`Fixture`](crate::Fixture)) settled, so it is refused as already
    /// run; what a yield it parked spawns after the load isn't counted. A
    /// plan step names an earlier step's transaction only by its exact
    /// hash, which any change before it moves. To run a receipt of the
    /// earlier transaction after the later one's, hold it with
    /// [`Fault::Hold`] and submit the later one at an offset.
    ///
    /// Forced, as [`HoldUntil`](Fault::HoldUntil) is: a one-shard order,
    /// not a claim that a network delays a receipt that long.
    pub fn in_tx(mut self, hash: CryptoHash) -> Self {
        self.in_tx = Some(hash);
        self
    }

    /// The transaction [`in_tx`](Self::in_tx) names, if any.
    pub(crate) fn tx_of(&self) -> Option<CryptoHash> {
        self.in_tx
    }

    pub(crate) fn nth_of(&self) -> Option<usize> {
        self.nth
    }

    /// An `nth` that isn't an [`occurrence`](Self::occurrence): drawn.
    pub(crate) fn is_sample(&self) -> bool {
        self.nth.is_some() && !self.exact
    }

    /// Whether the filter (not `nth`) selects `r`, the `n`th spawned.
    pub(crate) fn matches(&self, n: usize, r: &crate::fault::Spawned) -> bool {
        let calls = |m: &String| r.methods.contains(m);
        match &self.filter {
            Filter::All => true,
            Filter::Callbacks => r.awaits > 0,
            Filter::CrossContractCallbacks => {
                r.awaits > 0 && r.awaits_from.iter().all(|a| *a != r.receiver)
            }
            Filter::Callback(m) => r.awaits > 0 && calls(m),
            Filter::Method(m) => calls(m),
            Filter::Call(a, m) => r.receiver == *a && calls(m),
            Filter::Receiver(a) => r.receiver == *a,
            Filter::Action(k) => r.actions.contains(k),
            Filter::Index(i) => *i == n,
        }
    }
}

/// nearcore's `Action` variants, as `AsRef<str>` names them.
const ACTIONS: [&str; 14] = [
    "CreateAccount",
    "DeployContract",
    "FunctionCall",
    "Transfer",
    "Stake",
    "AddKey",
    "DeleteKey",
    "DeleteAccount",
    "Delegate",
    "DeployGlobalContract",
    "UseGlobalContract",
    "DeterministicStateInit",
    "TransferToGasKey",
    "WithdrawFromGasKey",
];

/// The expression that builds it, `Receipts::callbacks().nth(1)`, but for
/// an [`in_tx`](Receipts::in_tx) hash, shown in base58.
impl fmt::Display for Receipts {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match (&self.filter, self.nth) {
            (Filter::Call(a, m), Some(k)) if self.exact => {
                write!(f, "Receipts::occurrence({:?}, {m:?}, {k})", a.as_str())?
            }
            (filter, nth) => {
                match filter {
                    Filter::All => write!(f, "Receipts::all()"),
                    Filter::Callbacks => write!(f, "Receipts::callbacks()"),
                    Filter::CrossContractCallbacks => {
                        write!(f, "Receipts::cross_contract_callbacks()")
                    }
                    Filter::Callback(m) => write!(f, "Receipts::callback({m:?})"),
                    Filter::Method(m) => write!(f, "Receipts::method({m:?})"),
                    Filter::Call(a, m) => write!(f, "Receipts::call({:?}, {m:?})", a.as_str()),
                    Filter::Receiver(a) => write!(f, "Receipts::receiver({:?})", a.as_str()),
                    Filter::Action(k) => write!(f, "Receipts::action({k:?})"),
                    Filter::Index(n) => write!(f, "Receipts::index({n})"),
                }?;
                if let Some(k) = nth {
                    write!(f, ".nth({k})")?
                }
            }
        }
        match self.in_tx {
            Some(h) => write!(f, ".in_tx({h})"),
            None => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fault::Spawned;

    fn spawned(methods: &[&str], awaits: usize) -> Spawned {
        Spawned {
            id: CryptoHash::default(),
            receiver: "vault.near".parse().unwrap(),
            methods: methods.iter().map(|m| m.to_string()).collect(),
            actions: methods.iter().map(|_| "FunctionCall".to_string()).collect(),
            attached: Gas::from_teragas(10),
            awaits,
            failable: true,
            awaits_no_call: false,
            sends: false,
            awaits_from: vec![],
        }
    }

    /// `callback` needs a receipt that waits on a result, `method` doesn't;
    /// `call` and `receiver` need the receiver, `index` the position.
    #[test]
    fn selectors_match() {
        let (cb, plain) = (spawned(&["resolve"], 1), spawned(&["resolve"], 0));
        let m = Receipts::method("resolve");
        assert!(m.matches(0, &cb) && m.matches(0, &plain));
        let c = Receipts::callback("resolve");
        assert!(c.matches(0, &cb) && !c.matches(0, &plain));
        assert!(Receipts::callbacks().matches(0, &cb) && !Receipts::callbacks().matches(0, &plain));
        let x = Receipts::cross_contract_callbacks();
        let from = |a: &str| Spawned { awaits_from: vec![a.parse().unwrap()], ..cb.clone() };
        assert!(x.matches(0, &from("token.near")) && !x.matches(0, &plain));
        assert!(!x.matches(0, &from("vault.near")), "a self-call isn't cross-contract");
        assert!(Receipts::all().matches(7, &plain));
        assert!(!Receipts::method("other").matches(0, &cb));
        assert!(Receipts::call("vault.near", "resolve").matches(0, &cb));
        assert!(!Receipts::call("token.near", "resolve").matches(0, &cb));
        assert!(Receipts::receiver("vault.near").matches(0, &spawned(&[], 0)));
        assert!(Receipts::index(2).matches(2, &cb) && !Receipts::index(2).matches(1, &cb));
        let stake = Spawned { methods: vec![], actions: vec!["Stake".into()], ..spawned(&[], 0) };
        assert!(Receipts::action("Stake").matches(0, &stake));
        assert!(!Receipts::action("Stake").matches(0, &cb));
        assert!(Receipts::action("FunctionCall").matches(0, &cb));
        let err = std::panic::catch_unwind(|| Receipts::action("Unstake")).unwrap_err();
        let msg = err.downcast_ref::<String>().unwrap();
        assert!(msg.starts_with("Receipts::action(\"Unstake\"): not one of nearcore's"), "{msg}");
    }

    #[test]
    fn receipts_display_as_their_constructor() {
        let shown = [
            Receipts::all(),
            Receipts::callbacks().nth(1),
            Receipts::cross_contract_callbacks(),
            Receipts::method("m"),
            Receipts::callback("c"),
            Receipts::call("a.near", "m"),
            Receipts::receiver("a.near"),
            Receipts::action("Stake"),
            Receipts::index(3).nth(0),
            Receipts::occurrence("a.near", "m", 1).in_tx(CryptoHash::default()),
            Receipts::call("a.near", "m").nth(1).in_tx(CryptoHash::default()),
        ]
        .map(|s| s.to_string());
        assert_eq!(
            shown,
            [
                "Receipts::all()",
                "Receipts::callbacks().nth(1)",
                "Receipts::cross_contract_callbacks()",
                r#"Receipts::method("m")"#,
                r#"Receipts::callback("c")"#,
                r#"Receipts::call("a.near", "m")"#,
                r#"Receipts::receiver("a.near")"#,
                r#"Receipts::action("Stake")"#,
                "Receipts::index(3).nth(0)",
                r#"Receipts::occurrence("a.near", "m", 1).in_tx(11111111111111111111111111111111)"#,
                r#"Receipts::call("a.near", "m").nth(1).in_tx(11111111111111111111111111111111)"#,
            ]
        );
    }

    /// Faults and selectors go through JSON and back unchanged, as a replay
    /// file stores them.
    #[test]
    fn faults_and_receipts_round_trip_through_serde() {
        let faults = [
            Fault::PromiseFailed,
            Fault::OutOfGas,
            Fault::Gas(Gas::from_teragas(5)),
            Fault::ReturnValue(vec![1, 2, 3]),
            Fault::Hold(2),
            Fault::HoldResult(3),
            Fault::HoldUntil(Receipts::occurrence("a.near", "m", 1)),
            Fault::HoldUntil(Receipts::callbacks().nth(1).in_tx(CryptoHash::hash_bytes(b"t1"))),
        ];
        let json = serde_json::to_string(&faults).unwrap();
        assert_eq!(serde_json::from_str::<Vec<Fault>>(&json).unwrap(), faults);
        let receipts = [
            Receipts::all(),
            Receipts::callbacks().nth(1),
            Receipts::cross_contract_callbacks().nth(2),
            Receipts::callback("c"),
            Receipts::method("m"),
            Receipts::call("a.near", "m"),
            Receipts::receiver("a.near"),
            Receipts::action("Stake"),
            Receipts::index(4),
        ];
        let json = serde_json::to_string(&receipts).unwrap();
        assert_eq!(serde_json::from_str::<Vec<Receipts>>(&json).unwrap(), receipts);
        // A selector without `in_tx` writes none, as before it existed.
        assert!(!json.contains("in_tx"), "{json}");
        // Borsh too: the log stores faults in `Sim::cached` snapshots, and
        // the variants before the holds keep their encoding.
        for f in &faults {
            assert_eq!(&Fault::try_from_slice(&borsh::to_vec(f).unwrap()).unwrap(), f);
        }
        assert_eq!(borsh::to_vec(&Fault::OutOfGas).unwrap(), [1]);
        assert_eq!(borsh::to_vec(&Fault::Hold(2)).unwrap(), [4, 2, 0, 0, 0, 0, 0, 0, 0]);
        assert_eq!(Fault::HoldUntil(Receipts::all()).to_string(), "HoldUntil(Receipts::all())");
    }

    /// What fits where: a callback takes the four faults, a call `OutOfGas`
    /// and a lower `Gas`, a batch of two calls no `Gas`; any receipt a
    /// `Hold` and a `HoldUntil`, and only one whose result goes to a
    /// callback a `HoldResult`.
    #[test]
    fn faults_fit() {
        let gas = |t| Fault::Gas(Gas::from_teragas(t));
        let fits = |s: &Spawned| {
            let all = [
                Fault::PromiseFailed,
                Fault::ReturnValue(vec![]),
                Fault::OutOfGas,
                gas(5),
                gas(10),
                Fault::Hold(1),
                Fault::HoldResult(1),
                Fault::HoldUntil(Receipts::all()),
            ];
            all.into_iter().filter(|f| s.fits(f)).map(|f| f.kind()).collect::<Vec<_>>()
        };
        let all = ["PromiseFailed", "ReturnValue", "OutOfGas", "Gas", "Hold", "HoldUntil"];
        assert_eq!(fits(&spawned(&["resolve"], 1)), all);
        let call = ["OutOfGas", "Gas", "Hold", "HoldResult", "HoldUntil"];
        assert_eq!(fits(&Spawned { sends: true, ..spawned(&["on_transfer"], 0) }), call);
        assert_eq!(fits(&spawned(&["a", "b"], 0)), ["OutOfGas", "Hold", "HoldUntil"]);
        assert_eq!(fits(&Spawned { failable: false, ..spawned(&["resolve"], 1) })[..2], all[1..3]);
        assert_eq!(fits(&spawned(&[], 0)), ["Hold", "HoldUntil"]);
        // A transfer's result is empty: bytes can't stand for it, none can.
        let tip = Spawned { awaits_no_call: true, ..spawned(&["on_tip"], 1) };
        assert!(tip.fits(&Fault::ReturnValue(vec![])) && !tip.fits(&Fault::ReturnValue(vec![1])));
        assert!(!spawned(&["x"], 0).fits(&Fault::Hold(0)));
    }
}
