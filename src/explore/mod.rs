//! Exploration: run a contract's operations as generated or written cases
//! (overlap, block gaps, chosen gas, forced faults), judge each with
//! checks, and measure what each receipt uses as its inputs grow:
//! [`Harness::scan`], [`Harness::search`], [`Harness::curves`], [`Books`]
//! and [`Harness::diff`]. Each returns an [`Audit`], whose
//! [`done`](Audit::done) refuses while an item is open. A clean audit
//! means no violation in what was tried; a check that couldn't judge
//! counts as not judged, never as a pass.
//!
//! **Experimental.** This module's API, reports and replay files change
//! more often than the rest of the crate; the CHANGELOG lists each break.
//!
//! ```
//! use near_contract_sim::explore::{Fail, Harness, Op, OpDone, Plan, Skip, Verdict};
//! use near_contract_sim::{Balance, Sim};
//! use serde::{Deserialize, Serialize};
//!
//! /// One operation: alice tips bob.
//! #[derive(Clone, Debug, Serialize, Deserialize)]
//! enum TipOp {
//!     Tip { yocto: u128 },
//! }
//!
//! impl Op for TipOp {
//!     fn submit(&self, sim: &mut Sim) -> Result<(), Skip> {
//!         let TipOp::Tip { yocto } = *self;
//!         if yocto == 0 {
//!             return Err("nothing to tip".into());
//!         }
//!         sim.tx("alice.near", "bob.near").transfer(Balance::from_yoctonear(yocto)).submit();
//!         Ok(())
//!     }
//! }
//!
//! /// Per op: bob got exactly the tip, whatever else is in flight.
//! fn bob_gets_the_tip(d: &OpDone<TipOp>) -> Result<(), Fail> {
//!     let TipOp::Tip { yocto } = *d.op();
//!     let got = d.effects().near_net("bob.near");
//!     if got == yocto as i128 { Ok(()) } else { Err(format!("bob got {got}, not {yocto}").into()) }
//! }
//!
//! let mut sim = Sim::new();
//! sim.create_account("alice.near", Balance::from_near(10));
//! sim.create_account("bob.near", Balance::from_near(10));
//! let harness = Harness::new(sim).per_op("bob gets the tip", bob_gets_the_tip);
//! // Two tips in the same block.
//! let plan = Plan::new().op(TipOp::Tip { yocto: 5 }).blocks(0).op(TipOp::Tip { yocto: 7 });
//! let run = harness.run(&plan);
//! run.assert_clean();
//! // An op that skips itself never ran: a run of a written plan says so.
//! let run = harness.run(&plan.op(TipOp::Tip { yocto: 0 }));
//! assert_eq!(run.verdict(), Verdict::Incomplete);
//! assert!(!run.is_clean());
//! ```

pub mod amount;
mod audit;
mod axes;
mod books;
mod check;
mod curves;
mod diff;
mod enumerate;
mod faults;
pub mod gas;
mod hook;
pub mod input;
#[cfg(test)]
mod memo_tests;
mod outsider;
mod plan;
mod progress;
mod report;
mod run;
mod scan;
mod search;
mod shrink;
mod store;

pub use crate::edges::{Region, Why, edges};
pub use crate::tx::OutcomeShape;
pub use amount::{Amount, AmountError};
pub use audit::{Audit, Found, Label, Shrink};
pub use axes::{Axes, Axis, AxisKind};
pub use books::{Books, Claim, LIVENESS_EPOCHS};
pub use check::{Harness, OpDone};
pub use curves::{CurveRow, Curves, Reach, Resource, trie_siblings};
pub use diff::{Diff, PlanDiff};
pub use enumerate::Enumerate;
pub use faults::Faults;
pub(crate) use hook::Hook;
pub use hook::submit_injected;
pub use input::{Pick, PickError};
pub use plan::{Act, Gap, Inject, Plan, Plans, Step, StepGas, StubAnswer, plans};
pub use report::{Curve, Paths, Report, Row, Todo};
pub use run::{Judged, Run, Verdict};
pub use scan::Scan;
pub use search::Search;
pub(crate) use store::Store;
pub use store::{Regression, Status};

/// The names of a harness's built-in checks: [`crate::builtin`]'s three
/// and five of a harness's own.
pub mod builtin {
    pub use crate::builtin::{CONSERVATION, FAILED_CALLBACK, UNHANDLED_FAILURE};
    /// A receipt [`Harness::must_not_fail`](super::Harness::must_not_fail)
    /// names failed, forced or not.
    pub const MUST_NOT_FAIL: &str = "must not fail";
    /// A callback kept too little of its gas along a curve's margin ascent.
    pub const MARGIN: &str = "gas margin";
    /// New callers made a contract stake storage worth over 10 times what
    /// they paid, along a curve's fresh-callers axis.
    pub const STORAGE_FOR_CALLERS: &str = "storage paid by the contract";
    /// Ops in flight together left the books' readings (each liability per
    /// holder, each asset's change, NEAR net of gas) where no serial order
    /// of the same ops leaves them, or one of them failed that succeeds in
    /// every serial order. On whenever [`Books`](super::Books) are declared;
    /// each order runs settled from the world the first of them was
    /// submitted to. A failed op that moved nothing besides gas may be
    /// left out of the order, if it fails in some serial order too. More
    /// than 3 ops in flight together, an order that can't run (an op
    /// skips), or an op that sends another call in an order (another
    /// caller or other arguments) is not judged, never a pass. A match says nothing about orders and
    /// offsets not tried.
    pub const SERIALIZABLE: &str = "outcome some serial order gives";
    /// A method every witness saw called only by its own contract (a call
    /// to itself, a callback) succeeded when a scan sent it directly from
    /// `axis.near`, after the witness's earlier steps, and changed that
    /// contract's state. It sees self-calls and callbacks that read no
    /// promise result, given state earlier steps left (not state their own
    /// transaction set just before them): a callback that reads a result
    /// fails when sent with none, so an open callback of that kind passes
    /// here (any contract could call it with results it picks; telling
    /// needs a relay that does).
    /// [`Harness::anyone_may_call`](super::Harness::anyone_may_call)
    /// declares a method public, which turns it off there.
    pub const SELF_ONLY: &str = "self-only method runs for an outsider";
    pub(crate) const ALL: [&str; 7] = [
        UNHANDLED_FAILURE,
        FAILED_CALLBACK,
        CONSERVATION,
        MUST_NOT_FAIL,
        MARGIN,
        STORAGE_FOR_CALLERS,
        SELF_ONLY,
    ];
}

/// What an audit harness names, in one import: [`crate::prelude`] and
/// this module's everyday types. Its `builtin` is [`builtin`], with all
/// eight names. [`input`]'s `Answer` and `Shape` stay under `input::`, so a
/// harness's own glob-imported `Answer` isn't ambiguous:
///
/// ```
/// mod ops {
///     pub enum Answer { Yes, No }
/// }
/// use near_contract_sim::explore::prelude::*;
/// use ops::*;
///
/// let _ = (Answer::Yes, input::Answer::Panic);
/// ```
pub mod prelude {
    pub use super::{
        Amount, Audit, Books, Fail, Faults, Found, Harness, Label, Op, OpDone, Pick, Plan, Report,
        Run, Skip, Status, StepGas, Verdict, builtin, input, plans,
    };
    pub use crate::prelude::*;
}

use crate::effects::Unobserved;
use crate::sim::{IntoAccountId, Sim};
use near_primitives::types::{AccountId, Balance, Gas};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::fmt::{self, Debug};

/// The version of the JSON files exploration writes; none reads as 0.
pub(crate) const FORMAT: u32 = 3;

/// What a harness clones per case: a [`Sim`], or a struct of your own
/// around one.
pub trait World: Clone + AsRef<Sim> + AsMut<Sim> + 'static {}
impl<W: Clone + AsRef<Sim> + AsMut<Sim> + 'static> World for W {}

/// One of your contract's operations: a serde value (usually an enum's)
/// that queues the transactions it stands for. A [`Plan`] is a list of
/// them, so it prints, saves and reloads. `W` is the [`World`].
///
/// An op holds its data, never an index into a table built at run time:
/// a replay is the plan's serde, and a scan moves an index as a number,
/// onto another row or past the table. A stub's reply is an
/// [`input::Answer`], an account an `AccountId`, a `String` or an enum of
/// yours, and gas a number field.
pub trait Op<W = Sim>: Clone + Debug + Serialize + DeserializeOwned + 'static {
    /// Queues this op's transactions with [`Tx::submit`](crate::Tx::submit).
    /// Producing a block here is a harness error: the plan's [`Gap`]s own
    /// block production. `Err(Skip)`, before submitting anything, skips
    /// the op; no per-op check judges it.
    ///
    /// What a later op needs from an earlier one (an id it returned), the
    /// earlier op records here, in a field of `W`, for the later to read.
    /// Give [`enumerate`](Harness::enumerate) a [`key`](Enumerate::key) that
    /// reads that field: the default reads only the audited contracts, so
    /// it merges states that differ there. A serial order that puts the
    /// later op first skips it, and that overlap is not judged.
    fn submit(&self, world: &mut W) -> Result<(), Skip>;

    /// Its name in findings and coverage: by default serde's (an enum's
    /// variant, a struct's type). A struct op, or one indexing a table,
    /// names every case alike, so its findings and flights fold under one
    /// name: use an enum, or override this.
    fn name(&self) -> String {
        serde_name(self)
    }

    /// Which of its arguments the environment controls: by default every
    /// number, list length and string ([`Axes::auto`]).
    fn axes(&self) -> Axes {
        Axes::auto()
    }

    /// Copy `k` (from 0) along a repeat or a tail: by default itself. An op
    /// that signs a nonce returns a fresh one here.
    fn copy(&self, k: usize) -> Self {
        let _ = k;
        self.clone()
    }

    /// Whether the contract refused this op though every receipt of its
    /// `outcomes` succeeded (it returned `false` or `{"success": false}`):
    /// by default never. [`builtin::SERIALIZABLE`] counts a refused op as
    /// failed, for its "refused only under interleaving" finding and for
    /// leaving a failed op out of the serial orders.
    fn refused(&self, outcomes: &[crate::TxOutcome]) -> bool {
        let _ = outcomes;
        false
    }
}

/// The stack a search, a scan and a run get: nested strategies recurse
/// once per level as they generate, shrink and drop, which a debug
/// build's 2 MiB test thread can't hold.
const STACK: usize = 256 << 20;
const RED_ZONE: usize = 32 << 20;

/// Runs `f` on a [`STACK`]-byte stack unless `RED_ZONE` is left already.
pub(crate) fn big_stack<T>(f: impl FnOnce() -> T) -> T {
    stacker::maybe_grow(RED_ZONE, STACK, f)
}

/// The name serde gives a value: an enum's variant, else the type's name.
pub(crate) fn serde_name<T: Serialize>(value: &T) -> String {
    let variant = |s: &str| {
        let ident = s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
        (s.starts_with(|c: char| c.is_ascii_uppercase()) && ident).then(|| s.to_owned())
    };
    let name = match serde_json::to_value(value) {
        Ok(serde_json::Value::String(s)) => variant(&s),
        Ok(serde_json::Value::Object(m)) if m.len() == 1 => {
            m.keys().next().and_then(|k| variant(k))
        }
        _ => None,
    };
    name.unwrap_or_else(|| {
        let full = std::any::type_name::<T>();
        let full = full.split('<').next().unwrap_or(full);
        full.rsplit("::").next().unwrap_or(full).to_owned()
    })
}

/// Why an op skipped itself ([`Op::submit`]): `"reason".into()`, or `?`
/// on an [`AmountError`] or an [`input::PickError`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Skip(pub String);

impl fmt::Display for Skip {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

macro_rules! from_text {
    ($to:ty, $make:expr, $($t:ty),*) => {
        $(impl From<$t> for $to {
            fn from(x: $t) -> Self { ($make)(x.to_string()) }
        })*
    };
}
from_text!(Skip, Skip, &str, String, AmountError, input::PickError);
from_text!(Fail, Fail::new, &str, String);

/// A check's failure: a message, and optionally a [`key`](Fail::key) and a
/// [`cause`](Fail::cause), each splitting findings. `String` and `&str`
/// convert into one; so does an [`Unobserved`], counted as *not judged*,
/// never as a pass.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Fail {
    pub(crate) message: String,
    pub(crate) key: Option<String>,
    pub(crate) cause: Option<String>,
    /// Why it couldn't judge: not a failure, never a pass.
    pub(crate) unjudged: Option<String>,
    /// With `unjudged`: nothing was at stake ([`Fail::idle`]).
    pub(crate) idle: bool,
    /// The check panicked: a harness error naming it, not a finding.
    pub(crate) panicked: bool,
    /// [`Fail::all`]'s other failures, each a finding of its own. A boxed
    /// slice keeps a `Fail` under clippy's `result_large_err` size.
    pub(crate) more: Box<[Fail]>,
}

impl Fail {
    /// A failure with this message.
    pub fn new(message: impl Into<String>) -> Self {
        let message = message.into();
        let (key, cause, unjudged) = (None, None, None);
        Fail { message, key, cause, unjudged, idle: false, panicked: false, more: Box::default() }
    }

    /// Every failure of one judgement, each its own finding at its
    /// [`key`](Self::key) and [`cause`](Self::cause); `Ok(())` if there
    /// are none. A check that walks holders returns
    /// `Fail::all(holders.filter_map(..))`, not its first failure. The
    /// judgement counts as failed once. If any element fails, those not
    /// judged or idle are passed over; if none does, the first not judged
    /// decides, else the first idle: an element that couldn't be read is
    /// never masked by one with nothing at stake. Give the failures
    /// distinct keys or causes: of several at one key and cause, only the
    /// first is a finding. [`cause`](Self::cause) on the result names
    /// every failure's cause; [`key`](Self::key) re-sites only the first.
    ///
    /// ```
    /// use near_contract_sim::explore::Fail;
    ///
    /// /// Each holder short of what she is owed, at her key.
    /// fn short(holders: &[(&str, u128, u128)]) -> Result<(), Fail> {
    ///     Fail::all(holders.iter().filter(|(_, owed, has)| has < owed).map(|(who, owed, has)| {
    ///         Fail::new(format!("{who} is short by {}", owed - has)).key(*who)
    ///     }))
    /// }
    ///
    /// assert_eq!(short(&[("alice.near", 5, 5)]), Ok(()));
    /// // Two findings, alice's and bob's; it prints the first.
    /// let both = short(&[("alice.near", 5, 2), ("bob.near", 3, 2)]).unwrap_err();
    /// assert_eq!(both.to_string(), "alice.near is short by 3");
    /// ```
    pub fn all(fails: impl IntoIterator<Item = Fail>) -> Result<(), Fail> {
        let (mut failed, mut unjudged) = (vec![], None);
        for mut f in fails {
            if f.unjudged.is_none() {
                let more = std::mem::take(&mut f.more);
                failed.push(f);
                failed.extend(more);
            } else if unjudged.as_ref().is_none_or(|u: &Fail| u.idle && !f.idle) {
                unjudged = Some(f);
            }
        }
        let mut failed = failed.into_iter();
        match failed.next() {
            Some(first) => Err(Fail { more: failed.collect(), ..first }),
            None => unjudged.map_or(Ok(()), Err),
        }
    }

    /// Not a failure: the check couldn't judge. Counted as *not judged*,
    /// never as a pass: the run is incomplete.
    pub fn not_judged(why: impl Into<String>) -> Self {
        let why = why.into();
        Fail { unjudged: Some(why.clone()), ..Fail::new(why) }
    }

    /// Nothing was at stake here (no claim open): idle, the run stays
    /// complete. A check only ever idle is an `unjudged:` to-do item.
    pub fn idle(why: impl Into<String>) -> Self {
        let why = why.into();
        Fail { idle: true, ..Fail::not_judged(why) }
    }

    /// Reports it at `key` instead of the check's site: per account, say.
    pub fn key(mut self, key: impl Into<String>) -> Self {
        self.key = Some(key.into());
        self
    }

    /// Names its cause: two causes are two findings, shrunk and saved apart.
    /// On a [`Fail::all`], every failure's: a cause is the judgement's.
    pub fn cause(mut self, cause: impl Into<String>) -> Self {
        let cause = cause.into();
        self.more.iter_mut().for_each(|f| f.cause = Some(cause.clone()));
        self.cause = Some(cause);
        self
    }
}

impl fmt::Display for Fail {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl From<Unobserved> for Fail {
    fn from(u: Unobserved) -> Self {
        Fail::not_judged(u.to_string())
    }
}

/// A built-in [`Op`] for one call, named by its method; its gas is
/// unpinned, so a plan step's gas reaches it. Any plan can hold one
/// ([`Plan::call`]).
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Call {
    pub(crate) signer: AccountId,
    pub(crate) receiver: AccountId,
    pub(crate) method: String,
    #[serde(with = "args")]
    pub(crate) args: Vec<u8>,
    pub(crate) deposit: Balance,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) gas: Option<Gas>,
    /// The op a repeat or a tail copies this call from, if it does.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) copies: Option<Box<Copied>>,
}

/// Copy `k` of the op a raw call of a repeat or a tail copies: its args
/// are what that copy makes the call carry, so a signed nonce is fresh
/// ([`Op::copy`]). The op is the plan's `sources[source]`, held once.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Copied {
    pub(crate) source: usize,
    pub(crate) k: usize,
    /// Its lists' items are made fresh first, as a step copy's are.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub(crate) items: bool,
    /// The call is the op's entry, from fresh callers: the accounts the op
    /// names (its payload's signer) are made fresh too.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub(crate) fresh: bool,
    /// For fresh callers, the witness step's signer: a field naming it
    /// names a caller.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) signer: Option<AccountId>,
}

/// `alice.near -> vault.near::withdraw({"amount":"1"})`, deposit and gas
/// if set.
impl Debug for Call {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (signer, receiver, method) = (&self.signer, &self.receiver, &self.method);
        match std::str::from_utf8(&self.args) {
            Ok(text) => write!(f, "{signer} -> {receiver}::{method}({text})")?,
            Err(_) => write!(f, "{signer} -> {receiver}::{method}({:?})", self.args)?,
        }
        if !self.deposit.is_zero() {
            write!(f, ", deposit {}", self.deposit.as_yoctonear())?;
        }
        self.gas.map_or(Ok(()), |g| write!(f, ", gas {g}"))
    }
}

/// A call's args in a plan file: JSON when they are (an axis edits them),
/// else bytes.
mod args {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    #[derive(Serialize, Deserialize)]
    #[serde(untagged)]
    enum Args {
        Json { json: serde_json::Value },
        Bytes { bytes: Vec<u8> },
    }

    pub(super) fn serialize<S: Serializer>(args: &[u8], s: S) -> Result<S::Ok, S::Error> {
        match serde_json::from_slice::<serde_json::Value>(args) {
            Ok(json) if !args.is_empty() => Args::Json { json }.serialize(s),
            _ => Args::Bytes { bytes: args.to_vec() }.serialize(s),
        }
    }

    pub(super) fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<u8>, D::Error> {
        Ok(match Args::deserialize(d)? {
            Args::Json { json } => serde_json::to_vec(&json).expect("JSON serializes"),
            Args::Bytes { bytes } => bytes,
        })
    }
}

impl Call {
    /// `signer` calls `method` on `receiver`, with no args and no deposit.
    #[track_caller]
    pub fn new(signer: impl IntoAccountId, receiver: impl IntoAccountId, method: &str) -> Self {
        Call {
            signer: signer.into_account_id(),
            receiver: receiver.into_account_id(),
            method: method.to_owned(),
            args: vec![],
            deposit: Balance::ZERO,
            gas: None,
            copies: None,
        }
    }

    /// The args as JSON, as [`Tx::args_json`](crate::Tx::args_json) does.
    #[track_caller]
    pub fn args_json(mut self, args: impl Serialize) -> Self {
        let json = serde_json::to_vec(&args).expect("JSON args");
        self.args = if json == b"null" { vec![] } else { json };
        self
    }

    /// The args, as bytes.
    pub fn args(mut self, args: impl Into<Vec<u8>>) -> Self {
        self.args = args.into();
        self
    }

    /// The attached deposit.
    pub fn deposit(mut self, deposit: Balance) -> Self {
        self.deposit = deposit;
        self
    }

    /// Pins its gas: a plan step's gas no longer reaches it.
    pub fn gas(mut self, gas: Gas) -> Self {
        self.gas = Some(gas);
        self
    }

    pub(crate) fn send(&self, sim: &mut Sim) {
        let mut tx = sim.call(&self.signer, &self.receiver, &self.method);
        tx = tx.args(self.args.clone()).deposit(self.deposit);
        if let Some(gas) = self.gas {
            tx = tx.gas(gas);
        }
        tx.submit();
    }
}

impl<W: AsMut<Sim>> Op<W> for Call {
    fn submit(&self, world: &mut W) -> Result<(), Skip> {
        self.send(world.as_mut());
        Ok(())
    }

    fn name(&self) -> String {
        self.method.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Clone, Debug, Serialize, Deserialize)]
    enum E {
        Unit,
        Tuple(u8),
        Named { a: u8 },
        Pair(u8, u8),
    }

    #[derive(Serialize)]
    struct Deposit {
        amount: u8,
    }

    #[test]
    fn op_names_are_serde_names() {
        assert_eq!(serde_name(&E::Unit), "Unit");
        assert_eq!(serde_name(&E::Tuple(1)), "Tuple");
        assert_eq!(serde_name(&E::Named { a: 1 }), "Named");
        assert_eq!(serde_name(&E::Pair(1, 2)), "Pair");
        assert_eq!(serde_name(&Deposit { amount: 1 }), "Deposit");
        assert_eq!(serde_name(&3u8), "u8");
        let call = Call::new("a.near", "b.near", "withdraw");
        assert_eq!(<Call as Op>::name(&call), "withdraw");
    }

    #[test]
    fn a_calls_json_args_read_as_json_and_round_trip() {
        let call = Call::new("a.near", "b.near", "m").args_json(serde_json::json!({"ids": ["x"]}));
        let text = serde_json::to_string(&call).unwrap();
        assert!(text.contains(r#""args":{"json":{"ids":["x"]}}"#), "{text}");
        assert_eq!(serde_json::from_str::<Call>(&text).unwrap(), call);
        let raw = Call::new("a.near", "b.near", "m").args(vec![0, 159]);
        let back: Call = serde_json::from_str(&serde_json::to_string(&raw).unwrap()).unwrap();
        assert_eq!(back, raw);
    }

    /// `Fail::all` flattens the failures of a nested one, passes over what
    /// couldn't judge beside a failure, and is `Ok` on nothing.
    #[test]
    fn fail_all_flattens_and_passes_over_what_couldnt_judge() {
        let k = |key: &str| Fail::new(key).key(key);
        let keys = |f: &Fail| {
            let all = std::iter::once(f).chain(f.more.iter());
            all.map(|f| f.key.clone().unwrap()).collect::<Vec<_>>()
        };
        let inner = Fail::all([k("a"), k("b")]).unwrap_err();
        let outer = Fail::all([Fail::idle("i"), inner, Fail::not_judged("u"), k("c")]).unwrap_err();
        assert_eq!(keys(&outer), ["a", "b", "c"]);
        assert!(outer.more.iter().all(|f| f.more.is_empty()));
        // Not judged outranks idle, in either order; idle alone is idle.
        let unjudged = |f: Fail| (f.unjudged, f.idle);
        let first = Fail::all([Fail::not_judged("u"), Fail::idle("i")]).unwrap_err();
        assert_eq!(unjudged(first), (Some("u".into()), false));
        let later = Fail::all([Fail::idle("i"), Fail::not_judged("u")]).unwrap_err();
        assert_eq!(unjudged(later), (Some("u".into()), false));
        let idle = Fail::all([Fail::idle("i"), Fail::idle("j")]).unwrap_err();
        assert_eq!(unjudged(idle), (Some("i".into()), true));
        assert_eq!(Fail::all(std::iter::empty()), Ok(()));
    }

    #[test]
    fn fails_convert() {
        let f: Fail = "short by 3".into();
        assert_eq!((f.message.as_str(), f.key.as_deref()), ("short by 3", None));
        assert_eq!(Fail::new("m").key("alice").key.as_deref(), Some("alice"));
        let u = Unobserved { token: "t.near".parse().unwrap(), why: "not watched".into() };
        let f: Fail = u.clone().into();
        assert_eq!(f.unjudged, Some(u.to_string()));
    }
}
