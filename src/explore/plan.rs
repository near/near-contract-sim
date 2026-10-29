//! [`Plan`]: one case as data, and [`plans`], the strategy that generates
//! them.

use super::Call;
use super::faults::Faults;
use crate::inject::{Fault, Receipts};
use near_primitives::types::Gas;
use proptest::prelude::*;
use proptest::strategy::{NewTree, ValueTree};
use proptest::test_runner::TestRunner;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::fmt;
use std::ops::RangeInclusive;
use std::path::Path;

/// One case: steps in order, each an op (or a [`Call`]) with the blocks
/// after it, its gas and the faults to force. Build one by hand,
///
/// ```
/// # use near_contract_sim::explore::{Call, Plan};
/// # use near_contract_sim::{Fault, Gas, Receipts};
/// let withdraw = Call::new("alice.near", "vault.near", "withdraw");
/// let plan = Plan::new()
///     .op(withdraw.clone())
///     .blocks(0) // the next op goes into the same block
///     .op(withdraw)
///     .gas(Gas::from_teragas(40)) // on its unpinned calls
///     .inject(Receipts::callback("on_withdraw"), Fault::PromiseFailed);
/// assert_eq!(plan.ops().len(), 2);
/// ```
///
/// or generate them with [`plans`]. It saves and reloads ([`Plan::load`]).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(bound(serialize = "O: Serialize + Clone", deserialize = "O: DeserializeOwned"))]
pub struct Plan<O> {
    /// Steps played before the case, unjudged: its checks start from the
    /// world they leave.
    #[doc(hidden)]
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub setup: Vec<Step<O>>,
    /// What happens before the first step, if anything.
    #[doc(hidden)]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start: Option<Gap>,
    /// The steps, in order.
    #[doc(hidden)]
    pub steps: Vec<Step<O>>,
    /// What its builder refused (a gap set twice, `epochs(0)`); a run lists
    /// them before its first case. Not saved.
    #[serde(skip)]
    pub(crate) errors: Vec<String>,
    /// A witness meant to be refused ([`refused`](Self::refused)).
    #[serde(skip)]
    pub(crate) refused: bool,
    /// Gas margins its run judges (a curve's margin finding).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) margin: Vec<super::curves::margin::Margin>,
    /// Measure each just-enough step's gas band (a scan's caller-gas edge).
    #[serde(skip)]
    pub(crate) bands: bool,
    /// (account, key) siblings planted before the case, a byte each, to
    /// fork the trie along a written key's path (a margin's trie shaper).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) planted: Vec<(near_primitives::types::AccountId, Vec<u8>)>,
    /// Its callers are fresh: judge the storage the contract pays for them.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub(crate) callers: bool,
    /// The ops raw calls of a repeat or a tail copy, as JSON, each once.
    /// Only appended to, so dropping a step never moves one.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) sources: Vec<serde_json::Value>,
    /// The call the outsider sweep sent from `axis.near`, its last step.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) swept: Option<Swept>,
}

/// The call a scan's outsider sweep adds to a witness's earlier steps,
/// sent from `axis.near`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct Swept {
    /// Its site, `receiver::method`.
    pub(crate) site: String,
    /// Every witness saw only its own contract call it: judge
    /// [`SELF_ONLY`](super::builtin::SELF_ONLY) there.
    pub(crate) self_only: bool,
}

/// One step of a [`Plan`].
#[doc(hidden)]
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(
    bound(serialize = "O: Serialize + Clone", deserialize = "O: DeserializeOwned"),
    into = "StepRepr<O>",
    try_from = "StepRepr<O>"
)]
pub struct Step<O> {
    /// What it submits.
    pub act: Act<O>,
    /// The blocks after it.
    pub then: Gap,
    /// The gas every unpinned call of its transactions attaches, clamped to
    /// the max prepaid gas; `None` leaves them alone.
    pub gas: Option<StepGas>,
    /// Faults forced on its receipts, ahead of the op's own injections.
    pub inject: Vec<Inject>,
    /// What stubs answer its calls, over what the op binds.
    pub answers: Vec<StubAnswer>,
}

/// A stub's answer to a step's calls of `method`, over its reply
/// ([`Tx::stub_reply`](crate::Tx::stub_reply)).
#[doc(hidden)]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StubAnswer {
    /// The stub.
    pub stub: near_primitives::types::AccountId,
    /// Its method.
    pub method: String,
    /// What it returns.
    pub json: serde_json::Value,
}

impl<O: fmt::Debug> Step<O> {
    /// The step on one line, its act shown as `ran` if given (a copy's).
    pub(crate) fn line_as(&self, ran: Option<&str>) -> String {
        let mut s = ran.map_or_else(|| format!("{:?}", self.act), str::to_owned);
        if let Some(g) = &self.gas {
            s += &format!(", gas {g}");
        }
        for i in &self.inject {
            s += &format!(", forced {:?} at tx {} {:?}", i.fault, i.tx, i.at);
        }
        for a in &self.answers {
            s += &format!(", {}::{} answers {}", a.stub, a.method, a.json);
        }
        s
    }
}

/// What a [`Step`] submits.
#[doc(hidden)]
#[derive(Clone, Debug, PartialEq)]
pub enum Act<O> {
    /// One of the plan's ops.
    Op(O),
    /// A call of its own ([`Plan::call`]).
    Call(Call),
}

impl<O> Act<O> {
    /// The op, if it is one.
    pub fn op(&self) -> Option<&O> {
        if let Act::Op(op) = self { Some(op) } else { None }
    }

    /// Its name in coverage and curve points.
    pub(crate) fn name<W>(&self) -> String
    where
        O: super::Op<W>,
    {
        match self {
            Act::Op(op) => op.name(),
            Act::Call(c) => format!("{}::{}", c.receiver, c.method),
        }
    }
}

impl<O: Serialize> Act<O> {
    /// Its serde form.
    pub(crate) fn json(&self) -> Option<serde_json::Value> {
        match self {
            Act::Op(op) => serde_json::to_value(op).ok(),
            Act::Call(c) => serde_json::to_value(c).ok(),
        }
    }
}

impl<O: DeserializeOwned> Act<O> {
    /// An act of its kind (an op, a call) from `json`, if it deserializes.
    pub(crate) fn rebuild(&self, json: serde_json::Value) -> serde_json::Result<Act<O>> {
        Ok(match self {
            Act::Op(_) => Act::Op(serde_json::from_value(json)?),
            Act::Call(_) => Act::Call(serde_json::from_value(json)?),
        })
    }
}

/// A step as saved: `{"op": ..}` or `{"call": ..}` beside its settings.
#[derive(Serialize, Deserialize)]
#[serde(bound(serialize = "O: Serialize", deserialize = "O: DeserializeOwned"))]
struct StepRepr<O> {
    // Present is an op, even `null` (a unit struct).
    #[serde(default, skip_serializing_if = "Option::is_none", deserialize_with = "present")]
    op: Option<O>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    call: Option<Call>,
    then: Gap,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    gas: Option<StepGas>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    inject: Vec<Inject>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    answers: Vec<StubAnswer>,
}

fn present<'de, D: serde::Deserializer<'de>, T: Deserialize<'de>>(
    d: D,
) -> Result<Option<T>, D::Error> {
    T::deserialize(d).map(Some)
}

impl<O> From<Step<O>> for StepRepr<O> {
    fn from(s: Step<O>) -> Self {
        let (op, call) = match s.act {
            Act::Op(op) => (Some(op), None),
            Act::Call(call) => (None, Some(call)),
        };
        StepRepr { op, call, then: s.then, gas: s.gas, inject: s.inject, answers: s.answers }
    }
}

impl<O> TryFrom<StepRepr<O>> for Step<O> {
    type Error = &'static str;
    fn try_from(r: StepRepr<O>) -> Result<Self, Self::Error> {
        let act = match (r.op, r.call) {
            (Some(op), None) => Act::Op(op),
            (None, Some(call)) => Act::Call(call),
            _ => return Err("a step has an `op` or a `call`, one of them"),
        };
        Ok(Step { act, then: r.then, gas: r.gas, inject: r.inject, answers: r.answers })
    }
}

/// The gas a step gives its unpinned calls. Saved as a number for
/// `Exactly`, `{"LeastPlus": n}` otherwise.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum StepGas {
    /// The least with which each transaction succeeds
    /// ([`Tx::least_gas`](crate::Tx::least_gas)), plus this much: "a
    /// relayer attaches just enough", where a callback the result doesn't
    /// wait on can starve. One failing even at the most keeps its gas.
    LeastPlus(Gas),
    /// This much each.
    #[serde(untagged)]
    Exactly(Gas),
}

impl From<Gas> for StepGas {
    fn from(gas: Gas) -> Self {
        StepGas::Exactly(gas)
    }
}

/// `40.00 Tgas`, or `least + 1 gas`.
impl fmt::Display for StepGas {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StepGas::Exactly(g) => f.write_str(&crate::render::tgas(*g)),
            StepGas::LeastPlus(g) if g.as_gas() < 1_000_000 => {
                write!(f, "least + {} gas", g.as_gas())
            }
            StepGas::LeastPlus(g) => write!(f, "least + {}", crate::render::tgas(*g)),
        }
    }
}

/// What happens after a step, before the next one submits.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Gap {
    /// Blocks until nothing is queued or in flight, at most
    /// `config.max_blocks`: past that the case stops, incomplete.
    Settle,
    /// Exactly this many blocks; `Blocks(0)` overlaps the next step.
    Blocks(u16),
    /// Blocks until the next step converts `k` blocks from the next epoch
    /// start still ahead. The default gaps draw it (`k` in -2..=2) on
    /// epochs of 100 blocks or fewer.
    #[serde(alias = "Epoch")]
    EpochEdge(i8),
    /// Blocks until the next step converts at the `n`-th epoch start
    /// ahead: `Epochs(4)` waits out a four-epoch cooldown.
    Epochs(u8),
}

impl Gap {
    /// The blocks this gap steps after `height`; `None` for
    /// [`Gap::Settle`]. `pending`: transactions convert in the next block.
    pub(crate) fn blocks_from(self, height: u64, epoch_length: u64, pending: bool) -> Option<u64> {
        let target = |k: i128, after: i128| {
            let h = i128::from(height) + i128::from(pending);
            let len = i128::from(epoch_length.max(1));
            let m = ((h - k + len - 1).div_euclid(len)).max(1) + after;
            u64::try_from(m * len + k - i128::from(height))
                .expect("an epoch target behind the last block")
        };
        match self {
            Gap::Settle => None,
            Gap::Blocks(n) => Some(n.into()),
            Gap::EpochEdge(k) => Some(target(k.into(), 0)),
            Gap::Epochs(n) => Some(target(0, i128::from(n.max(1)) - 1)),
        }
    }

    pub(crate) fn blocks_on(self, sim: &crate::Sim) -> Option<u64> {
        let pending = sim.chain.queued().next().is_some();
        self.blocks_from(sim.height(), sim.config.epoch_length, pending)
    }
}

/// The longest epochs on which the default gaps draw [`Gap::EpochEdge`].
pub(crate) const SHORT_EPOCH: u64 = 100;

thread_local! {
    /// The running search's fixture epoch length (0: none), for the
    /// default gaps.
    static EPOCH_LENGTH: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// Runs `f` with `epoch_length` as the fixture's.
pub(crate) fn with_epoch_length<T>(epoch_length: u64, f: impl FnOnce() -> T) -> T {
    struct Restore(u64);
    impl Drop for Restore {
        fn drop(&mut self) {
            EPOCH_LENGTH.with(|c| c.set(self.0));
        }
    }
    let _restore = Restore(EPOCH_LENGTH.with(|c| c.replace(epoch_length)));
    f()
}

/// [`Gap::Settle`] or 0 to 2 blocks; on short epochs also
/// [`Gap::EpochEdge`] -2 to 2.
#[derive(Clone, Copy, Debug, Default)]
struct DefaultGaps;

impl Strategy for DefaultGaps {
    type Tree = Box<dyn ValueTree<Value = Gap>>;
    type Value = Gap;

    fn new_tree(&self, runner: &mut TestRunner) -> NewTree<Self> {
        let length = EPOCH_LENGTH.with(std::cell::Cell::get);
        let blocks = (0u16..=2).prop_map(Gap::Blocks);
        let epochs = (-2i8..=2).prop_map(Gap::EpochEdge);
        let gaps = if (1..=SHORT_EPOCH).contains(&length) {
            prop_oneof![Just(Gap::Settle), blocks, epochs].boxed()
        } else {
            prop_oneof![Just(Gap::Settle), blocks].boxed()
        };
        gaps.new_tree(runner)
    }
}

/// A fault forced on a step's transaction, as
/// [`Tx::inject`](crate::Tx::inject) forces it: a case where it applied
/// to none is incomplete. With [`Receipts::nth`] it is a sample: it goes
/// on the `k`th receipt it fits from this step on, or is dropped.
#[doc(hidden)]
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Inject {
    /// Which of the step's transactions, from 0.
    pub tx: u8,
    /// Which of its receipts.
    pub at: Receipts,
    /// What to force.
    pub fault: Fault,
}

/// `PromiseFailed at Receipts::callbacks().nth(0)`, ` (tx 1)` after.
impl fmt::Display for Inject {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} at {}", self.fault, self.at)?;
        match self.tx {
            0 => Ok(()),
            n => write!(f, " (tx {n})"),
        }
    }
}

impl<O> Default for Plan<O> {
    fn default() -> Self {
        Plan {
            setup: vec![],
            start: None,
            steps: vec![],
            errors: vec![],
            refused: false,
            margin: vec![],
            bands: false,
            planted: vec![],
            callers: false,
            sources: vec![],
            swept: None,
        }
    }
}

impl<O> Plan<O> {
    /// No steps.
    pub fn new() -> Self {
        Self::default()
    }

    /// Appends `op`, settling after it ([`Gap::Settle`]).
    pub fn op(self, op: O) -> Self {
        self.act(Act::Op(op))
    }

    /// Appends a call of its own, settling after it. Its signer is
    /// created, with 100 NEAR more than its deposit, if missing.
    pub fn call(self, call: Call) -> Self {
        self.act(Act::Call(call))
    }

    fn act(mut self, act: Act<O>) -> Self {
        let step = Step { act, then: Gap::Settle, gas: None, inject: vec![], answers: vec![] };
        self.steps.push(step);
        self
    }

    /// The gap after the last step (before the first, on an empty plan):
    /// `n` blocks, so `blocks(0)` overlaps the next. Twice, they add up.
    pub fn blocks(self, n: u16) -> Self {
        self.then(Gap::Blocks(n))
    }

    /// The gap after the last step: [`Gap::Epochs`]`(n)`, as in
    /// `.op(unstake).epochs(4).op(withdraw)`. `epochs(0)` is an error the
    /// plan keeps ([`Plan::errors`]).
    pub fn epochs(mut self, n: u8) -> Self {
        if n == 0 {
            let at = self.steps.len();
            self.errors.push(format!("epochs(0) after step {at} waits for nothing"));
            return self;
        }
        self.then(Gap::Epochs(n))
    }

    /// A step has one gap: blocks add up; another gap is an error the plan
    /// keeps.
    pub(crate) fn then(mut self, gap: Gap) -> Self {
        let n = self.steps.len();
        let (set, which) = match self.steps.last() {
            Some(step) => (Some(step.then), format!("after step {}", n - 1)),
            None => (self.start, "before the first step".to_owned()),
        };
        let combined = match (set, gap) {
            (None | Some(Gap::Settle), gap) => Ok(gap),
            (Some(Gap::Blocks(a)), Gap::Blocks(b)) => {
                a.checked_add(b).map(Gap::Blocks).ok_or_else(|| format!("{a} + {b} blocks {which}"))
            }
            (Some(old), new) => Err(format!(
                "the gap {which} is already {old:?}; {new:?} would replace it. A step has one \
                 gap: put an op between them, or wait in one gap"
            )),
        };
        match combined {
            Ok(gap) => match self.steps.last_mut() {
                Some(step) => step.then = gap,
                None => self.start = Some(gap),
            },
            Err(why) => self.errors.push(why),
        }
        self
    }

    /// A witness meant to be refused: its failed receipts aren't held
    /// against it. Otherwise a witness whose transaction fails is a
    /// harness error.
    pub fn refused(mut self) -> Self {
        self.refused = true;
        self
    }

    /// What its builder refused: a run lists them and runs nothing.
    pub fn errors(&self) -> &[String] {
        &self.errors
    }

    /// The last step's unpinned calls attach `gas`.
    #[track_caller]
    pub fn gas(mut self, gas: impl Into<StepGas>) -> Self {
        self.last("gas").gas = Some(gas.into());
        self
    }

    /// Forces `fault` on the receipts `at` selects of the last step's
    /// first transaction; several each apply. A hold of 0 is an error the
    /// plan keeps.
    #[track_caller]
    pub fn inject(mut self, at: Receipts, fault: Fault) -> Self {
        if let Fault::Hold(0) | Fault::HoldResult(0) = fault {
            let step = self.steps.len().saturating_sub(1);
            self.errors.push(format!("step {step}: inject {fault:?} holds nothing"));
            return self;
        }
        self.last("inject").inject.push(Inject { tx: 0, at, fault });
        self
    }

    #[track_caller]
    fn last(&mut self, what: &str) -> &mut Step<O> {
        match self.steps.last_mut() {
            Some(step) => step,
            None => panic!("Plan::{what} sets the last step's, and the plan has none: add one"),
        }
    }

    /// Each step's op; a [`Call`] step has none.
    pub fn ops(&self) -> Vec<Option<&O>> {
        self.steps.iter().map(|s| s.act.op()).collect()
    }
}

impl<O: Serialize + Clone> Plan<O> {
    /// Writes the plan to `path` as JSON. Panics naming the file if it
    /// can't.
    #[track_caller]
    pub fn save(&self, path: impl AsRef<Path>) {
        let path = path.as_ref();
        let json = serde_json::to_string_pretty(self).expect("a plan serializes");
        std::fs::write(path, json)
            .unwrap_or_else(|e| panic!("Plan::save({}): {e}", path.display()));
    }
}

impl<O: DeserializeOwned> Plan<O> {
    /// Reads a plan saved as JSON, bare or under `"plan"` (a replay file).
    /// Panics naming the file if it can't; [`from_json`](Self::from_json)
    /// returns why a text isn't a plan instead.
    #[track_caller]
    pub fn load(path: impl AsRef<Path>) -> Self {
        let path = path.as_ref();
        let text = std::fs::read_to_string(path)
            .unwrap_or_else(|e| panic!("Plan::load({}): {e}", path.display()));
        Self::from_json(&text).unwrap_or_else(|e| panic!("Plan::load({}): {e}", path.display()))
    }

    /// A plan from JSON text, as [`load`](Self::load) reads it, or why not
    /// (where `load` panics).
    pub fn from_json(text: &str) -> Result<Self, String> {
        let mut value: serde_json::Value =
            serde_json::from_str(text).map_err(|e| format!("not JSON: {e}"))?;
        if let Some(plan) = value.get_mut("plan") {
            value = plan.take();
        }
        with_zeros(value)
            .map_err(|e| format!("not a plan of these ops ({e}): an op renamed or removed?"))
    }
}

/// `value` as a `T` whose type has since gained fields, each filled with
/// a plain value.
fn with_zeros<T: DeserializeOwned>(value: serde_json::Value) -> serde_json::Result<T> {
    let ok = |v: &serde_json::Value| T::deserialize(v).map(drop).map_err(|e| e.to_string());
    let filled = super::axes::fill(&ok, &[], value.clone());
    T::deserialize(&filled.unwrap_or(value))
}

/// Plans of the ops `ops` generates, for
/// [`Harness::search`](super::Harness::search): by default 1 to 6 steps,
/// [`gas::mixed`](super::gas::mixed), and [`Faults::default`].
///
/// ```
/// # use near_contract_sim::explore::{Call, Faults, Plan, plans};
/// # use proptest::prelude::*;
/// let ops = Just(Call::new("alice.near", "vault.near", "withdraw"));
/// let strategy = plans(ops).len(2..=4).faults(Faults::none());
/// # let mut runner = proptest::test_runner::TestRunner::deterministic();
/// # let plan: Plan<Call> = strategy.new_tree(&mut runner).unwrap().current();
/// # assert!((2..=4).contains(&plan.steps.len()));
/// ```
pub fn plans<O: Clone + fmt::Debug + 'static>(ops: impl Strategy<Value = O> + 'static) -> Plans<O> {
    Plans {
        ops: ops.boxed(),
        len: 1..=6,
        gaps: DefaultGaps.boxed(),
        gas: super::gas::mixed().boxed(),
        faults: Faults::default(),
        repeats: true,
    }
}

/// The strategy [`plans`] returns.
#[derive(Clone)]
#[must_use]
pub struct Plans<O> {
    ops: BoxedStrategy<O>,
    len: RangeInclusive<usize>,
    gaps: BoxedStrategy<Gap>,
    gas: BoxedStrategy<Option<StepGas>>,
    faults: Faults,
    repeats: bool,
}

impl<O: Clone + fmt::Debug + 'static> Plans<O> {
    /// How many steps are drawn, 1 to 6 by default; a repeat can add 5.
    #[track_caller]
    pub fn len(mut self, len: RangeInclusive<usize>) -> Self {
        assert!(!len.is_empty(), "Plans::len({len:?}) is empty");
        self.len = len;
        self
    }

    /// The gap after each step; shrinking moves toward the first. A
    /// [`repeat`](Self::repeats) puts its copies in one block regardless.
    pub fn gaps(mut self, gaps: impl Strategy<Value = Gap> + 'static) -> Self {
        self.gaps = gaps.boxed();
        self
    }

    /// The gas each step's unpinned calls get (`None`: their own).
    pub fn gas<G: Into<StepGas> + fmt::Debug + 'static>(
        mut self,
        gas: impl Strategy<Value = Option<G>> + 'static,
    ) -> Self {
        self.gas = gas.prop_map(|g| g.map(Into::into)).boxed();
        self
    }

    /// What to inject, and in what share of cases.
    pub fn faults(mut self, faults: Faults) -> Self {
        self.faults = faults;
        self
    }

    /// Whether about one plan in eight repeats a step 2 to 6 times in one
    /// block; on by default.
    pub fn repeats(mut self, on: bool) -> Self {
        self.repeats = on;
        self
    }

    fn strategy(&self) -> BoxedStrategy<Plan<O>> {
        let step =
            (self.ops.clone(), self.gaps.clone(), self.gas.clone()).prop_map(|(op, then, gas)| {
                Step { act: Act::Op(op), then, gas, inject: vec![], answers: vec![] }
            });
        let steps = prop::collection::vec(step, self.len.clone());
        // A small range, so the fault's step shrinks to the first fast.
        let at = 0..(*self.len.end()).max(1);
        let repeat = if self.repeats {
            prop_oneof![
                7 => Just(None),
                1 => (0..(*self.len.end()).max(1), 2..=REPEAT_MAX).prop_map(Some),
            ]
            .boxed()
        } else {
            Just(None).boxed()
        };
        (steps, self.faults.strategy(), at, repeat)
            .prop_map(|(mut steps, inject, at, repeat)| {
                if let (Some(inject), false) = (inject, steps.is_empty()) {
                    let n = at % steps.len();
                    steps[n].inject.push(inject);
                }
                if let (Some((k, n)), false) = (repeat, steps.is_empty()) {
                    let k = k % steps.len();
                    let then = steps[k].then;
                    steps[k].then = Gap::Blocks(0);
                    let mut copies =
                        vec![Step { inject: vec![], answers: vec![], ..steps[k].clone() }; n - 1];
                    copies.last_mut().expect("n >= 2").then = then;
                    steps.splice(k + 1..k + 1, copies);
                }
                Plan { steps, ..Plan::default() }
            })
            .boxed()
    }
}

/// The most times [`plans`] repeats a step.
const REPEAT_MAX: usize = 6;

impl<O> fmt::Debug for Plans<O> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Plans").field("len", &self.len).finish_non_exhaustive()
    }
}

impl<O: Clone + fmt::Debug + 'static> Strategy for Plans<O> {
    type Tree = Box<dyn ValueTree<Value = Plan<O>>>;
    type Value = Plan<O>;

    fn new_tree(&self, runner: &mut TestRunner) -> NewTree<Self> {
        self.strategy().new_tree(runner)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plans_round_trip_through_json() {
        let plan = Plan::new()
            .op(1u8)
            .blocks(0)
            .op(2)
            .gas(Gas::from_teragas(40))
            .inject(Receipts::callbacks().nth(1), Fault::Hold(2))
            .inject(Receipts::method("m"), Fault::PromiseFailed)
            .call(Call::new("a.near", "b.near", "m").args_json(serde_json::json!({"n": 1})));
        let json = serde_json::to_string(&plan).unwrap();
        assert!(json.contains(r#"{"op":1,"then":{"Blocks":0}}"#), "{json}");
        assert_eq!(serde_json::from_str::<Plan<u8>>(&json).unwrap(), plan);
        let wrapped = format!(r#"{{"schema": 1, "plan": {json}}}"#);
        assert_eq!(Plan::<u8>::from_json(&wrapped).unwrap(), plan);
        assert_eq!(plan.steps[1].inject.len(), 2);
        // A step's gas: a number, or tagged.
        let least = plan.clone().gas(StepGas::LeastPlus(Gas::from_gas(7)));
        let json = serde_json::to_string(&least).unwrap();
        assert!(json.contains(r#""gas":40000000000000,"#), "{json}");
        assert!(json.contains(r#""gas":{"LeastPlus":7}"#), "{json}");
        assert_eq!(serde_json::from_str::<Plan<u8>>(&json).unwrap(), least);
    }

    /// An epoch gap steps to `k` from the next epoch start still ahead:
    /// on epochs of 10 (starts 1, 11, 21, ...), the next transaction
    /// converts in block start + k.
    #[test]
    fn an_epoch_gap_counts_to_the_next_start() {
        let blocks = |k, h| Gap::EpochEdge(k).blocks_from(h, 10, false).unwrap();
        assert_eq!(blocks(0, 0), 10);
        assert_eq!(blocks(-2, 0), 8);
        assert_eq!(blocks(2, 0), 12);
        assert_eq!(blocks(-2, 9), 9);
        assert_eq!(blocks(-1, 9), 0);
        assert_eq!(blocks(0, 10), 0);
        assert_eq!(blocks(0, 11), 9);
        let pending = |k, h| Gap::EpochEdge(k).blocks_from(h, 10, true).unwrap();
        assert_eq!(pending(0, 10), 10);
        assert_eq!(pending(0, 9), 1);
        for (h, pending) in [(0, false), (10, false), (10, true), (14, true)] {
            let one = Gap::Epochs(1).blocks_from(h, 10, pending);
            assert_eq!(one, Gap::EpochEdge(0).blocks_from(h, 10, pending), "h {h}");
            let four = Gap::Epochs(4).blocks_from(h, 10, pending).unwrap();
            assert_eq!(four, one.unwrap() + 30, "h {h}");
        }
    }

    #[test]
    fn a_gap_is_never_replaced_silently() {
        let plan = Plan::new().op(1u8).blocks(2).blocks(3).op(2).then(Gap::EpochEdge(0));
        assert_eq!(plan.steps[0].then, Gap::Blocks(5));
        assert_eq!(plan.steps[1].then, Gap::EpochEdge(0));
        let replaced = Plan::new().op(1u8).blocks(41).then(Gap::EpochEdge(0));
        let msg = &replaced.errors()[0];
        assert!(
            msg.starts_with("the gap after step 0 is already Blocks(41); EpochEdge(0)"),
            "{msg}"
        );
        assert_eq!(replaced.steps[0].then, Gap::Blocks(41));
        assert_eq!(Plan::<u8>::new().epochs(0).errors().len(), 1);
    }

    #[test]
    fn default_gaps_draw_epochs_only_on_short_epochs() {
        let draws = |length| {
            with_epoch_length(length, || {
                let mut runner = TestRunner::deterministic();
                (0..300)
                    .map(|_| DefaultGaps.new_tree(&mut runner).unwrap().current())
                    .collect::<Vec<Gap>>()
            })
        };
        assert!(!draws(43_200).iter().any(|g| matches!(g, Gap::EpochEdge(_))));
        let short = draws(SHORT_EPOCH);
        let epochs = short.iter().filter(|g| matches!(g, Gap::EpochEdge(_))).count();
        assert!((70..=130).contains(&epochs), "{epochs} of 300");
    }

    /// Lengths stay in range, but for a repeat, at most one step injects,
    /// and with no faults none does; a repeat shrinks away.
    #[test]
    fn generated_plans_follow_their_settings() {
        let mut runner = TestRunner::deterministic();
        let (mut injected, n) = (0, 400);
        for _ in 0..n {
            let plan = plans(0u8..4).len(2..=3).new_tree(&mut runner).unwrap().current();
            assert!((2..=3 + REPEAT_MAX - 1).contains(&plan.steps.len()));
            let k = plan.steps.iter().map(|s| s.inject.len()).sum::<usize>();
            assert!(k <= 1);
            injected += k;
            let none = plans(0u8..4).faults(Faults::none()).new_tree(&mut runner).unwrap();
            assert!(none.current().steps.iter().all(|s| s.inject.is_empty()));
        }
        assert!((60..=140).contains(&injected), "about a quarter: {injected} of {n}");
        let strategy = plans(0u32..1_000_000).len(1..=1).gaps(Just(Gap::Settle));
        for _ in 0..200 {
            let mut tree = strategy.new_tree(&mut runner).unwrap();
            if tree.current().steps.len() > 1 {
                while tree.simplify() {}
                assert_eq!(tree.current().steps.len(), 1, "shrinks to no repeat");
            }
        }
    }
}
