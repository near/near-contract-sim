//! [`Curves`]: what each receipt uses as an axis of a witness moves. Each
//! axis climbs a ladder from the witness's value, fourfold; wherever the
//! receipts reached or their outcomes change between two rungs, it is
//! refined by halving, and where a use bends, once. Each (site, [`Resource`], axis) row
//! has its measured maximum and slope, and stands against its limit
//! ([`Reach`]) as far as the axis moved: observed, never extrapolated.
//!
//! Design rules, each with a regression:
//! - C1: a row whose site's receipt the axis didn't change (its args, its
//!   storage ops, its use) is [`Reach::NoEffect`], never a gap.
//! - C2: a refusal is an [`Reach::EntryCap`] only if the largest passing
//!   rung reached the site and the refused one still fails at the same
//!   receipt, the same way, with the step's numbers at 1; else the row
//!   is [`Reach::Stopped`], open.
//! - C3: only [`Reach::Crossed`], a run that hit the limit, can be a
//!   finding: one where a check fired in that run, with its replay; one
//!   where none did is listed as harmless.
//! - C4: a row whose axis moved no further than [`SMALL`] is open.

use super::audit::Audit;
use super::axes::{Axis, AxisKind, How, act_json, act_max, arg_axes, axes_of, moved, moved_with};
use super::plan::{Plan, StepGas};
use super::report::Todo;
use super::run::Run;
use super::store::Saves;
use super::{Harness, Op, World, gas, progress};
use crate::outcome::{Failure, ReceiptOutcome, TxOutcome};
use near_primitives::errors::ActionErrorKind;
use near_primitives::hash::CryptoHash;
use near_primitives::types::Gas;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::PathBuf;
use std::time::Instant;

pub(crate) mod margin;
pub use margin::trie_siblings;

/// An axis moved no further than this is a gap (C4).
pub(crate) const SMALL: u128 = 8;
/// A ladder climbs to 2^(RUNGS - 1) times its start by default.
pub(crate) const RUNGS: u32 = 12;
/// Runs refinement takes per axis, at most.
const REFINES: u32 = 8;
/// A repeat or a tail climbs no higher than this.
const COPIES: u128 = 64;
/// yoctoNEAR a byte of storage stakes.
const BYTE: u128 = 10_000_000_000_000_000_000;

/// What a receipt uses, against a limit.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum Resource {
    /// The gas it burnt running, against its attached gas; what it
    /// attached to the receipts it made is a separate series
    /// ([`CurveRow::forwarded`]), never its own use.
    ReceiptGas,
    /// The gas the whole transaction burnt out of its attached gas, at its
    /// entry, against the ceiling one transaction may attach: every
    /// receipt but the refunds, less the entry's exec fees, which the
    /// signer prepaid on top.
    PrepaidGas,
    /// Bytes it logged, against the host's limit.
    LogBytes,
    /// Storage it added that its own account staked (yoctoNEAR, less any
    /// deposit it got), against that account's free balance.
    StoragePaid,
}

/// How a row stands against its limit.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Reach {
    /// A run at `at` hit the limit (C3): a finding if a check fired in
    /// that run ([`CurveRow::replay`] is its replay), else harmless.
    Crossed {
        /// The axis value.
        at: u128,
    },
    /// Below the limit as far as [`CurveRow::moved_to`].
    Below,
    /// The axis couldn't move past [`CurveRow::moved_to`]: why. Open.
    Stopped(String),
    /// The contract refuses the axis at `cap` and above (C2): the evidence.
    EntryCap {
        /// The least value refused.
        cap: u128,
        /// The passing value, the refusal, and the control.
        evidence: String,
    },
    /// The next rung's step (not a copy) was refused whole, nothing it did
    /// staying (a balance spent, an id past its length), or a fresh
    /// caller's copy of an op naming no account was refused (its signer
    /// can't be varied): a bound, closed. The refusal.
    AtBound(String),
    /// Moving the axis didn't change this site's receipt (C1).
    NoEffect,
    /// A copy skipped: the op found too little left in the world for it
    /// (an [`Amount`](super::Amount) of a spent balance). Closed: why.
    Exhausted(String),
}

/// One site's use of one resource along one axis of one witness step.
#[derive(Clone, Debug)]
pub struct CurveRow<O> {
    /// `receiver::method`.
    pub site: String,
    /// What it uses.
    pub resource: Resource,
    /// The witness step the axis moves.
    pub step: usize,
    /// What moved.
    pub axis: Axis,
    /// `(axis value, use)`, in order of the value.
    pub points: Vec<(u128, u128)>,
    /// For [`Resource::ReceiptGas`], the gas it attached to the receipts
    /// it made, at each point.
    pub forwarded: Vec<(u128, u128)>,
    /// The limit at the last point.
    pub limit: u128,
    /// The limit at each point.
    pub(crate) limits: Vec<u128>,
    /// Its use per unit of the axis, first point to last (observed).
    pub slope: f64,
    /// How it stands against its limit.
    pub reach: Reach,
    /// The furthest the axis moved with the site reached, in this curve's
    /// runs or any other case counted ([`Curves::with`]).
    pub moved_to: u128,
    /// Points from other cases past this curve's own: `(axis value, use)`.
    pub elsewhere: Vec<(u128, u128)>,
    /// The plan at the crossing, else at `moved_to`: replay it with
    /// [`Harness::run`].
    pub plan: Plan<O>,
    /// The replay file of the finding a check showed in the crossing's
    /// own run; `None` for a harmless crossing.
    pub replay: Option<PathBuf>,
    /// The steps of `plan` as its run ran them.
    pub(crate) steps: Vec<String>,
    /// The run of `plan`, and what fired in it.
    pub(crate) case: near_primitives::hash::CryptoHash,
    pub(crate) fired: Vec<super::run::Id>,
    /// A stopped row's refusing site and what refused it there (the
    /// function its trap is in, else its failure): the summary's group.
    pub(crate) stop: Option<(String, String)>,
}

impl<O> CurveRow<O> {
    /// For [`Resource::ReceiptGas`], what the receipt keeps of its
    /// attached gas at each point: `(axis value, attached - used -
    /// forwarded)`, under 0 past its limit.
    pub fn margins(&self) -> Vec<(u128, i128)> {
        let fw = |k: usize| self.forwarded.get(k).map_or(0, |f| f.1);
        (self.points.iter().zip(&self.limits).enumerate())
            .map(|(k, (p, l))| (p.0, margin::left(*l, p.1, fw(k))))
            .collect()
    }

    /// Why the row is still open, if it is: a stopped axis, or an axis
    /// that barely moved (C4).
    pub fn open(&self) -> Option<String> {
        match &self.reach {
            Reach::Stopped(why) => {
                Some(format!("{} stopped at {}: {why}", self.axis, self.moved_to))
            }
            Reach::Below if self.moved_to < SMALL => {
                Some(format!("{} moved only to {}", self.axis, self.moved_to))
            }
            _ => None,
        }
    }
}

/// `site ReceiptGas along /ids (length) at step 0: below the limit as far as 64; 1.2e9 a unit, at most 9e13 (at 64) of 3e14`:
/// its verdict first (the summary cuts a line at 200 characters), then its
/// largest use, the axis value it was at, and the limit there.
impl<O: fmt::Debug> fmt::Display for CurveRow<O> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let reach = match &self.reach {
            Reach::Crossed { at } if self.replay.is_none() => {
                format!("CROSSED at {at}, harmless: no check fired there (must_not_fail judges it)")
            }
            Reach::Crossed { at } => format!("CROSSED at {at}"),
            Reach::Below => format!("below the limit as far as {}", self.moved_to),
            Reach::Stopped(why) => format!("stopped at {}: {why} (open)", self.moved_to),
            Reach::AtBound(why) => format!("stopped at a bound: {why}"),
            Reach::EntryCap { evidence, .. } => format!("capped: {evidence}"),
            Reach::NoEffect => "no effect".to_owned(),
            Reach::Exhausted(why) => format!("world exhausted at {why}"),
        };
        let most = (self.points.iter().zip(&self.limits)).max_by_key(|(p, _)| p.1);
        let ((at, most), of) = most.map_or(((0, 0), self.limit), |(p, l)| (*p, *l));
        write!(
            f,
            "{} {:?} along {} at step {}: {reach}; {:.3e} a unit, at most {most} (at {at}) of {of}",
            self.site, self.resource, self.axis, self.step, self.slope
        )?;
        if let (true, Some(&(n, staked))) =
            (self.site.ends_with("(fresh callers)"), self.points.last())
        {
            let ratio = staked as f64 * STORAGE_RATIO as f64 / self.limit.max(1) as f64;
            write!(
                f,
                "; {} bytes a new caller, {ratio:.1}x what they paid",
                staked / BYTE / n.max(1)
            )?;
        }
        if let (Some(fw), Some(first)) = (self.forwarded.last(), self.forwarded.first()) {
            write!(f, "; forwarded {} to {}", first.1, fw.1)?;
        }
        if let Some(last) = self.elsewhere.last() {
            write!(f, "; {} point(s) from other cases, to {}", self.elsewhere.len(), last.0)?;
        }
        if let Some(path) = &self.replay {
            write!(f, "\n  replay: {}", path.display())?;
        }
        for (i, s) in self.steps.iter().enumerate() {
            write!(f, "\n  step {i}: {s}")?;
        }
        Ok(())
    }
}

/// Curves over witness plans, built by [`Harness::curves`] or
/// [`Harness::curve`]; [`run`](Self::run) runs them.
#[must_use]
pub struct Curves<'h, O, W> {
    harness: &'h Harness<O, W>,
    plans: Vec<Plan<O>>,
    only: Option<(usize, String)>,
    rungs: u32,
    seen: Seen,
    /// Other audits' coverage ([`with`](Self::with)): sites and checks.
    others: Vec<super::Report>,
    saves: Option<Saves>,
}

impl<O: Op<W>, W: World> Harness<O, W> {
    /// Curves along every axis of every step of `plans`, for every site
    /// and [`Resource`].
    pub fn curves(&self, plans: &[Plan<O>]) -> Curves<'_, O, W> {
        let (only, rungs, seen, others, saves) = (None, RUNGS, Seen::new(), vec![], None);
        Curves { harness: self, plans: plans.to_vec(), only, rungs, seen, others, saves }
    }

    /// Curves along one axis of step `step` of `plan`: its name
    /// (`caller gas`, `repeat Deposit`) or its JSON pointer's last part
    /// (`ids` for `/Deposit/ids`).
    pub fn curve(&self, plan: &Plan<O>, step: usize, axis: &str) -> Curves<'_, O, W> {
        let only = Some((step, axis.to_owned()));
        Curves { only, ..self.curves(std::slice::from_ref(plan)) }
    }
}

/// One receipt's use of a resource, against its limit. JSON keeps it a
/// 4-tuple, as [`Seen`] has always been saved.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(from = "(u128, u128, u128, bool)", into = "(u128, u128, u128, bool)")]
pub(crate) struct Use {
    used: u128,
    limit: u128,
    /// For [`Resource::ReceiptGas`], the gas it attached to the receipts it
    /// made.
    forwarded: u128,
    crossed: bool,
}

impl Use {
    fn new(used: u128, limit: u128, crossed: bool) -> Self {
        Use { used, limit, forwarded: 0, crossed }
    }
}

impl From<(u128, u128, u128, bool)> for Use {
    fn from((used, limit, forwarded, crossed): (u128, u128, u128, bool)) -> Self {
        Use { used, limit, forwarded, crossed }
    }
}

impl From<Use> for (u128, u128, u128, bool) {
    fn from(u: Use) -> Self {
        (u.used, u.limit, u.forwarded, u.crossed)
    }
}

/// Curve points from any case, whatever produced it (a search, a scan, a
/// curve, a hand plan): per (step's act, axis, site, resource), the use
/// at each axis value, crossed only if every case there crossed. Coverage
/// is judged on their union.
pub(crate) type Seen = BTreeMap<(String, String, String, Resource), BTreeMap<u128, Use>>;

/// `run`'s points: each step's arguments and caller gas, against what
/// that step's own transactions used. A step its entry refused is no point.
pub(crate) fn seen_of<O: Op<W>, W: World>(h: &Harness<O, W>, run: &Run<O>) -> Seen {
    let mut seen = Seen::new();
    if run.world::<W>().is_none() {
        return seen;
    }
    for (i, step) in run.plan.steps.iter().enumerate() {
        let Some(record) = run.ops.get(i).filter(|r| r.skipped.is_none()) else { continue };
        let axes = arg_axes::<O, W>(&step.act).into_iter().filter(Axis::climbs);
        let mut levels: Vec<(String, u128)> = axes.map(|a| (a.name, a.at)).collect();
        if record.unpinned > 0 {
            let gas =
                if let Some(StepGas::Exactly(g)) = step.gas { g } else { crate::chain::WALLET_GAS };
            levels.push(("caller gas".into(), gas.as_gas().into()));
        }
        let t = tally(h, &How::Gas, i, run.plan.steps.len(), 0, run, true);
        if t.refused.is_some() {
            continue;
        }
        let act = step.act.name();
        for (axis, at) in levels {
            for ((site, r), u) in &t.uses {
                let key = (act.clone(), axis.clone(), site.clone(), *r);
                merge_use(seen.entry(key).or_default(), at, *u);
            }
        }
    }
    seen
}

/// Adds a case's use at `at`: the most used, and crossed only if every
/// case there crossed.
fn merge_use(points: &mut BTreeMap<u128, Use>, at: u128, u: Use) {
    let e = points.entry(at).or_insert(u);
    let crossed = e.crossed && u.crossed;
    if u.used > e.used {
        *e = u;
    }
    e.crossed = crossed;
}

/// Adds `from`'s points to `into`.
pub(crate) fn merge_seen(into: &mut Seen, from: &Seen) {
    for (k, points) in from {
        let e = into.entry(k.clone()).or_default();
        for (&at, &u) in points {
            merge_use(e, at, u);
        }
    }
}

/// One axis of one witness step, as a curve climbs it.
struct Climb<'a, O> {
    plan: &'a Plan<O>,
    step: usize,
    axis: Axis,
    /// The step has an unpinned call.
    unpinned: bool,
}

/// One rung: the run, and what it measured.
struct Point<O> {
    value: u128,
    run: Run<O>,
    t: Tally,
}

/// What a run measured.
#[derive(Default)]
struct Tally {
    /// Per (site, resource): use, limit, forwarded, whether it crossed.
    uses: BTreeMap<(String, Resource), Use>,
    /// Per site: what C1 compares, its args and storage ops.
    marks: BTreeMap<String, String>,
    /// Each receipt's site and outcome.
    classes: BTreeSet<(String, String)>,
    /// Why the axis can't move here.
    refused: Option<Refusal>,
    /// The refused step's transactions were refused whole: each failed at
    /// its entry (or never ran), so nothing it did stayed.
    whole: bool,
}

/// Why a rung's step didn't run as the axis moved it. Two are the same
/// refusal if they are of one kind, at one site, for one reason: which
/// copy, its caller and the receipt's id are the run's, not the refusal's.
#[derive(Clone, Debug)]
enum Refusal {
    /// The case didn't finish: a harness error, or incomplete.
    Case(String),
    /// A copy skipped: the op found too little left in the world for it.
    CopySkipped(String),
    /// Copy `k` (from 1), the first that failed: its entry receipt, sent
    /// by `caller`.
    Copy { site: String, why: String, k: usize, caller: String, receipt: CryptoHash },
    /// The step's transaction ran no receipt, or was invalid.
    Tx(String),
    /// The step's entry receipt failed.
    Entry { site: String, why: String, receipt: CryptoHash },
}

impl PartialEq for Refusal {
    fn eq(&self, other: &Self) -> bool {
        std::mem::discriminant(self) == std::mem::discriminant(other)
            && self.site() == other.site()
            && self.why() == other.why()
    }
}

impl Refusal {
    /// The site that refused, else what did.
    fn site(&self) -> &str {
        match self {
            Self::Case(_) => "the case",
            Self::CopySkipped(_) => "a copy skipped",
            Self::Tx(_) => "the transaction",
            Self::Copy { site, .. } | Self::Entry { site, .. } => site,
        }
    }

    fn why(&self) -> &str {
        match self {
            Self::Case(why) | Self::CopySkipped(why) | Self::Tx(why) => why,
            Self::Copy { why, .. } | Self::Entry { why, .. } => why,
        }
    }

    /// The receipt that refused, if one did.
    fn receipt(&self) -> Option<CryptoHash> {
        match self {
            Self::Copy { receipt, .. } | Self::Entry { receipt, .. } => Some(*receipt),
            _ => None,
        }
    }

    /// The refusal with its trap named `name` ([`crate::trap::named`]).
    fn named(&self, name: &str) -> Refusal {
        let mut r = self.clone();
        if let Self::Copy { why, .. } | Self::Entry { why, .. } = &mut r {
            *why = crate::trap::named(why, name);
        }
        r
    }

    /// What refused at its site: the function its trap is in, `an
    /// unnamed trap`, or its failure's class. Never a mere message for
    /// an `unreachable`, which every refusal of a panic = "abort" build
    /// reads.
    fn cause(&self) -> String {
        let why = self.why();
        if let Some((_, frame)) = why.split_once("executed in `") {
            return format!("`{}`", frame.split('`').next().unwrap_or_default());
        }
        if why.contains(crate::trap::UNREACHABLE) {
            return "an unnamed trap".into();
        }
        // near-sdk's panic hook: `panicked: panicked at src/lib.rs:10:9:`
        // and the message on the next line; the location alone is no guard.
        let mut lines = why.lines();
        let first = lines.next().unwrap_or_default();
        match (first.contains("panicked at ") && first.ends_with(':'), lines.next()) {
            (true, Some(message)) => failure_class(message),
            _ => failure_class(why),
        }
    }
}

/// `` `site`: why ``, a copy's site marked `(a copy)`.
impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let copy = if matches!(self, Self::Copy { .. }) { " (a copy)" } else { "" };
        write!(f, "`{}{copy}`: {}", self.site(), self.why())
    }
}

/// Why an axis stopped: the reach of its rows, and the refusing site
/// when the world lacks what it needs, whose row is then open even where
/// its use stayed flat.
struct AxisStop {
    reach: Reach,
    lacks: Option<String>,
    /// The refusing site and its cause ([`Refusal::cause`]).
    group: Option<(String, String)>,
}

impl From<Reach> for AxisStop {
    fn from(reach: Reach) -> Self {
        AxisStop { reach, lacks: None, group: None }
    }
}

impl<O: Op<W>, W: World> Curves<'_, O, W> {
    /// How far each ladder climbs: to 2^(rungs - 1) times its start (12
    /// by default), sampled fourfold.
    pub fn rungs(mut self, rungs: u32) -> Self {
        self.rungs = rungs;
        self
    }

    /// Counts an audit's cases as points too (a search's, a scan's, other
    /// curves'; `&audit`, or a [`Report`](super::Report) read back): a row
    /// is judged on every point of its axis, whatever produced it; and its
    /// coverage, so a method or a check only it reached is no to-do item.
    /// For reports of campaigns already run, [`Harness::union`] does the
    /// same for their to-do lists: an `unreached:` item `done()` returns
    /// closes with a union holding a report that reached it.
    pub fn with(mut self, audit: impl Into<super::Report>) -> Self {
        let r = audit.into();
        merge_seen(&mut self.seen, &r.seen);
        self.others.push(r);
        self
    }

    /// Writes the report so far to `path` every `every` axes, as
    /// [`Scan::saves`](super::Scan::saves) does: curves that find the file
    /// (of this fixture) resume after the axes it counts, and their report
    /// includes the saved one. Margins are judged on the rows of the axes
    /// climbed since.
    pub fn saves(mut self, path: impl Into<std::path::PathBuf>, every: usize) -> Self {
        self.saves = Some(Saves::new(path.into(), every));
        self
    }

    /// Runs the curves. The number of axes and the most runs they take
    /// are printed first.
    pub fn run(self) -> Audit<O> {
        let h = self.harness;
        super::big_stack(|| {
            gas::with_fixture(h.fixture.as_ref(), || {
                let what = format!("curves of {} plan(s)", self.plans.len());
                let mut audit = Audit::new(what.clone());
                let done = Saves::resume(&self.saves, h, &mut audit);
                // From the start, so its elapsed time is the campaign's.
                let _guard = progress::start(what.clone(), "axis", 0, ", then the margins");
                let started = Instant::now();
                let phase = progress::phase("the plans as written".into(), 0, None);
                let mut runs = super::axes::direct(h, &self.plans);
                // A check failing on the fixture: its one error, no axis.
                let fixture = runs.iter().any(|r| r.0.on_fixture);
                if !fixture {
                    if self.only.is_none() {
                        let other = super::axes::other_contracts(h, &runs);
                        runs.extend(other.into_iter().map(|(p, at)| (h.run_case(&p), Some(at))));
                    }
                    runs.extend(emptying(h, &runs));
                }
                let work = self.work(&runs, done == 0, &mut audit);
                drop(phase);
                let each = self.rungs / 2 + REFINES + 2;
                let witness = started.elapsed() / self.plans.len().max(1) as u32;
                eprintln!(
                    "{what}: {} axes, at most {each} runs each, {witness:?} a witness run; a \
                     run's time grows along its axis, so the time left, from the mean axis \
                     time, prints as axes finish",
                    work.len(),
                );
                progress::total(work.len() as u32);
                let mut of = vec![];
                for (k, (w, c)) in work.iter().enumerate().skip(done) {
                    self.climb(c, &mut audit);
                    of.resize(audit.curves.len(), *w);
                    progress::step(k as u32 + 1);
                    Saves::tick(&self.saves, h, &audit, k + 1, false);
                }
                let witnesses: Vec<&Run<O>> = runs.iter().map(|r| &r.0).collect();
                self.margins(&witnesses, &of, &mut audit);
                merge_seen(&mut audit.r.seen, &self.seen);
                for r in &self.others {
                    audit.cover_with(r);
                }
                // Disjoint fields: the closure borrows `audit.seen` alone.
                audit.curves.iter_mut().for_each(|row| union::<O, W>(row, &audit.r.seen));
                Saves::tick(&self.saves, h, &audit, work.len().max(done), true);
                h.save(&mut audit);
                audit
            })
        })
    }

    /// The axes to climb, each with its witness's index in `runs`; on a
    /// fresh start (not a resume), `audit` takes the witnesses and what
    /// they leave open.
    fn work<'r>(
        &self,
        runs: &'r [(Run<O>, Option<String>)],
        fresh: bool,
        audit: &mut Audit<O>,
    ) -> Vec<(usize, Climb<'r, O>)> {
        let fixture = runs.iter().any(|r| r.0.on_fixture);
        let mut work = vec![];
        for (w, (witness, at)) in runs.iter().enumerate() {
            let plan = &witness.plan;
            if fresh {
                audit.witness(self.harness, witness, at.as_deref(), w);
                audit.fixed.extend(super::axes::fixed::<O, W>(plan));
                audit.unmoved.extend(refused::<O, W>(witness));
            }
            for step in (0..plan.steps.len()).filter(|_| !fixture) {
                let unpinned = witness.ops.get(step).is_some_and(|r| r.unpinned > 0);
                let (axes, dropped) = axes_of(self.harness, witness, step);
                for name in dropped.into_iter().filter(|_| fresh) {
                    if !audit.r.left_out_axes.contains(&name) {
                        audit.r.left_out_axes.push(name);
                    }
                }
                for axis in axes {
                    audit.unmoved.extend(same_items::<O, W>(plan, step, &axis).filter(|_| fresh));
                    if self.wanted(step, &axis) && (axis.climbs() || self.only.is_some()) {
                        work.push((w, Climb { plan, step, axis, unpinned }));
                    }
                }
            }
        }
        work
    }

    fn wanted(&self, i: usize, axis: &Axis) -> bool {
        self.only.as_ref().is_none_or(|(step, name)| {
            *step == i && (axis.name == *name || axis.name.ends_with(&format!("/{name}")))
        })
    }

    /// The rungs: from 1 Tgas to the ceiling for the caller's gas, else
    /// from the witness's value to 2^(rungs - 1) times it; fourfold, and
    /// the top.
    fn ladder(&self, &Climb { plan, step: i, ref axis, .. }: &Climb<O>) -> (Vec<u128>, u128) {
        let ceiling = u128::from(self.harness.gas_ceiling().as_gas());
        let (mut v, top) = match axis.kind {
            AxisKind::CallerGas => (u128::from(Gas::from_teragas(1).as_gas()), ceiling),
            AxisKind::Repeat | AxisKind::HoldTail => (1, COPIES),
            AxisKind::Number => {
                let act = &plan.steps[i].act;
                let max = act_json::<O, W>(act).and_then(|(j, _)| act_max::<O, W>(act, &j, axis));
                (axis.at.max(1), max.unwrap_or(u128::MAX))
            }
            AxisKind::Len | AxisKind::Bytes => (axis.at.max(1), u128::MAX),
            AxisKind::Choice => (axis.at, axis.at),
        };
        let end = v.saturating_mul(1u128.checked_shl(self.rungs - 1).unwrap_or(u128::MAX)).min(top);
        let mut out = vec![];
        while v < end {
            out.push(v);
            v = v.saturating_mul(4);
        }
        out.push(end);
        (out, top)
    }

    /// The plan at `value`, a longer list's new items fresh (else plain
    /// copies of its last); at the ceiling's gas where the step has an
    /// unpinned call, but along the caller's gas.
    fn at(&self, c: &Climb<O>, value: u128, fresh: bool) -> Option<Plan<O>> {
        let mut p = moved_with::<O, W>(c.plan, c.step, &c.axis, value, fresh)?;
        if c.unpinned && c.axis.kind != AxisKind::CallerGas {
            let ceiling = StepGas::Exactly(self.harness.gas_ceiling());
            p.steps[at_step(&c.axis, c.step, value)].gas = Some(ceiling);
        }
        Some(p)
    }

    /// `plan`'s run, measured at `value`: the witness's steps, not copies.
    fn measure(&self, c: &Climb<O>, value: u128, plan: &Plan<O>) -> Point<O> {
        let (h, run) = (self.harness, self.harness.run_case(plan));
        let t = tally(h, &c.axis.how, c.step, c.plan.steps.len(), value, &run, false);
        Point { value, t, run }
    }

    /// Climbs one axis and adds its rows to `audit`.
    fn climb(&self, c: &Climb<O>, audit: &mut Audit<O>) {
        let (i, axis) = (c.step, &c.axis);
        let points = self.sample(c);
        // The first refusal, past the bottom along the caller's gas.
        let ok = points.iter().position(|p| p.t.refused.is_none());
        let first_ok =
            if axis.kind == AxisKind::CallerGas { ok.unwrap_or(points.len()) } else { 0 };
        let refused = (points.iter().enumerate().skip(first_ok))
            .find_map(|(n, p)| Some((n, p.t.refused.as_ref()?)));
        let stop = refused.map(|(n, r)| self.judge_stop(c, &points, n, r));
        let points = &points[..refused.map_or(points.len(), |(n, _)| n + 1)];
        // Every rung is a case: what a check found there is a finding.
        for (j, p) in points.iter().enumerate() {
            if j > 0 || crossed(p) || p.run.overlapped_only {
                let at = format!("step {i}, {} = {}", axis.name, p.value);
                audit.add(&p.run, Some((&at, &axis.name)));
            } else {
                merge_seen(&mut audit.r.seen, &p.run.seen);
            }
        }
        rows::<O, W>((i, axis), points, stop, audit);
    }

    /// The axis's rungs, measured, in order of value.
    fn sample(&self, c: &Climb<O>) -> Vec<Point<O>> {
        let (h, &Climb { plan, step: i, ref axis, .. }) = (self.harness, c);
        let point = |value: u128| Some(self.measure(c, value, &self.at(c, value, true)?));
        // Coarse to fine: every third rung (64-fold) and the top, up to
        // the first refusal or crossing (along the caller's gas, refusals
        // at the bottom don't stop it); then the rung between two that
        // differ in a use, a receipt or an outcome.
        let along_gas = axis.kind == AxisKind::CallerGas;
        let stops = |p: &Point<O>| (p.t.refused.is_some() && !along_gas) || crossed(p);
        let (ladder, top) = self.ladder(c);
        let n = ladder.len();
        let mut at = BTreeMap::new();
        for k in (0..n).step_by(3).chain([n - 1]) {
            let Some(p) = (!at.contains_key(&k)).then(|| point(ladder[k])).flatten() else { break };
            let stop = stops(&p);
            at.insert(k, p);
            if stop {
                break;
            }
        }
        let mut split: Vec<_> = at.keys().zip(at.keys().skip(1)).map(|(&a, &b)| (a, b)).collect();
        while let Some((a, b)) = split.pop() {
            if b > a + 1
                && at.get(&a).zip(at.get(&b)).is_some_and(|(p, q)| differ(p, q))
                && let Some(p) = point(ladder[(a + b) / 2])
            {
                at.insert((a + b) / 2, p);
                split.extend([((a + b) / 2, b), (a, (a + b) / 2)]);
            }
        }
        let mut points: Vec<Point<O>> = at.into_values().collect();
        if let Some(n) = points.iter().position(stops) {
            points.truncate(n + 1);
        }
        // One more rung where the last two project the limit, if the axis
        // can get there (below its type's max; a size's at most 64 times
        // its last rung; a number with no size projected in its digits, as
        // it is signed and logged): a crossing past the ladder's top is
        // then measured. Nothing is reported from the projection itself.
        let far = axis.kind == AxisKind::Number && !super::scan::sizes(h, plan, i, axis, &mut None);
        if !along_gas && !points.last().is_some_and(stops) {
            let last = points.last().map_or(0, |p| p.value);
            let reach = if far { u128::MAX } else { last.saturating_mul(64) };
            let next = projection(&points, far).filter(|&x| x < top && x <= reach);
            if let Some(p) = next.and_then(point).filter(|p| p.t.refused.is_none()) {
                points.push(p);
            }
        }
        // Refine every pair of neighbouring rungs whose outcomes differ.
        let (mut budget, mut k) = (REFINES, 0);
        while k + 1 < points.len() && budget > 0 {
            let (lo, hi) = (points[k].value, points[k + 1].value);
            if points[k].t.classes == points[k + 1].t.classes || hi - lo <= (lo / 64).max(1) {
                k += 1;
                continue;
            }
            budget -= 1;
            if let Some(p) = point(lo + (hi - lo) / 2) { points.insert(k + 1, p) } else { k += 1 }
        }
        points
    }

    /// Why the axis stopped at `points[n]`, refused `r`: an entry cap
    /// with evidence (C2), a bound, or open.
    fn judge_stop(&self, c: &Climb<O>, points: &[Point<O>], n: usize, r: &Refusal) -> AxisStop {
        // Its trap named, from a rerun on an instrumented copy: the text
        // reads the named refusal, the control compares the run's own.
        let names = |run: &Run<O>| {
            let mut run = run.clone();
            self.harness.name_traps(&mut run)
        };
        let traps = r.why().contains(crate::trap::UNREACHABLE);
        let name = r.receipt().filter(|_| traps).and_then(|id| names(&points[n].run).remove(&id));
        let shown = name.map_or_else(|| r.clone(), |name| r.named(&name));
        let mut stop = self.judge(c, points, n, r, &shown);
        let copies = matches!(c.axis.kind, AxisKind::Repeat | AxisKind::HoldTail);
        if copies && matches!(stop.reach, Reach::Stopped(_)) && r.receipt().is_some() {
            stop.group = Some((r.site().to_owned(), shown.cause()));
        }
        // The step refused whole, nothing it did staying: a bound of the
        // world or the contract (a balance, an id's length), closed. A
        // copy refused stays open: what to give its signer is the hint.
        if points[n].t.whole
            && !matches!(r, Refusal::Copy { .. })
            && matches!(stop.reach, Reach::Stopped(_))
        {
            return Reach::AtBound(format!("{} fails at {shown}", points[n].value)).into();
        }
        stop
    }

    /// Who sent the witness's own call at `site`, as the run at `value`
    /// shows it: `, the witness's from <account>`, if it made one.
    fn witness_caller(&self, c: &Climb<O>, run: &Run<O>, value: u128, site: &str) -> String {
        let step = at_step(&c.axis, c.step, value);
        let receipts = run.step_txs(step).flat_map(|tx| &tx.receipts);
        let from = receipts.filter(|r| !r.is_refund()).find(|r| self::site(r) == site);
        from.map_or(String::new(), |r| format!(", the witness's from {}", r.predecessor_id))
    }

    /// Whether `run`'s fresh callers copies of `axis`'s op are raw calls:
    /// the op names no account to make fresh, as
    /// [`run::copy_json`](super::run::copy_json) reads it. Only the climbed
    /// op's copies count: a witness may hold another op's (a replay).
    fn names_no_account(&self, run: &Run<O>, axis: &Axis) -> bool {
        let of = match &axis.how {
            How::Repeat(super::axes::CopySource::Fresh(_, of))
            | How::Tail { copy: super::axes::CopySource::Fresh(_, of), .. } => of.as_ref(),
            _ => None,
        };
        let source = of.and_then(|of| run.plan.sources.iter().position(|s| s == of));
        let fresh = run.plan.steps.iter().find_map(|s| match &s.act {
            super::Act::Call(call) => (call.copies.as_deref())
                .filter(|c| c.fresh && Some(c.source) == source)
                .map(|c| (call, c)),
            super::Act::Op(_) => None,
        });
        let fixture = self.harness.fixture.as_ref();
        let made = fresh
            .and_then(|(call, c)| super::run::copy_json::<O, W>(fixture, &run.plan, call, c).ok());
        made.is_some_and(|(_, made)| made.is_empty())
    }

    /// [`Self::judge_stop`]'s verdict, but for a step refused whole:
    /// refused `r`, worded as `shown` (its trap named).
    fn judge(
        &self,
        c: &Climb<O>,
        points: &[Point<O>],
        n: usize,
        r: &Refusal,
        shown: &Refusal,
    ) -> AxisStop {
        let (&Climb { plan, step: i, ref axis, .. }, p) = (c, &points[n]);
        let lacks = |reach| AxisStop { reach, lacks: Some(r.site().to_owned()), group: None };
        let copies = |v: u128| format!("at {v} {}", if v == 1 { "copy" } else { "copies" });
        match shown {
            Refusal::CopySkipped(why) => {
                return Reach::Exhausted(format!("{}: {why}", p.value)).into();
            }
            // A fresh caller of an op naming no account (its signer picked
            // by an index, an enum) is its entry call from a new account,
            // which no op can make ready: a bound of the op, closed.
            Refusal::Copy { site, why, k, .. } if self.names_no_account(&p.run, axis) => {
                return Reach::AtBound(format!(
                    "{}, copy {k} failed at `{site}`: {why}; the op names no account, so its \
                     signer can't be varied: a fresh caller is its entry call from a new account",
                    copies(p.value)
                ))
                .into();
            }
            // A copy that can't run is a guard, or what the world lacks;
            // never a cap.
            Refusal::Copy { site, why, k, caller, .. } => {
                let witness = self.witness_caller(c, &p.run, p.value, site);
                return lacks(Reach::Stopped(format!(
                    "{}, copy {k} failed at `{site}`: {why}; the copy came from {caller}{witness}; \
                     if this is a guard (a caller check, a lock while held, a used nonce), excuse \
                     it: .excuse(\"axis:step {i}:{}\", \"<the guard>\"); else give {caller} what \
                     it lacks in the fixture, or return a fresh value from Op::copy",
                    copies(p.value),
                    axis.name
                )));
            }
            _ => {}
        }
        if matches!(r, Refusal::Tx(_)) || r.why().contains("out of gas") {
            let why = format!("{} fails at {shown}: past what one transaction carries", p.value);
            return Reach::AtBound(why).into();
        }
        // Copies (before the step or during a hold) have no numbers to set
        // to 1: no control can tell a cap.
        if matches!(axis.kind, AxisKind::Repeat | AxisKind::HoldTail) {
            let v = p.value;
            return lacks(Reach::Stopped(format!(
                "{v} fails at {shown} after {v} copies of it: the copies used up something the \
                 world holds, or the contract caps how often it runs; give the world more \
                 (funds, registrations, room), or excuse it as a cap"
            )));
        }
        let Some(pass) = points[..n].iter().rev().find(|q| q.t.refused.is_none()) else {
            return Reach::Stopped(format!("{} fails at {shown}", p.value)).into();
        };
        // A longer list refused for its fresh items, not its length: the
        // world holds fewer distinct values than it asks for.
        let plain = (axis.kind == AxisKind::Len).then(|| self.at(c, p.value, false)).flatten();
        if plain.is_some_and(|plain| self.measure(c, p.value, &plain).t.refused.is_none()) {
            return lacks(Reach::Stopped(format!(
                "{} fails at {shown} with fresh items, and passes with copies of the last: the world \
                 holds fewer distinct values than the list asks for; add them (the caller holding \
                 that many ids, tokens or accounts) or excuse",
                p.value
            )));
        }
        // The control: the refused rung with the step's numbers at 1.
        let mut control = self.at(c, p.value, true);
        let ones = arg_axes::<O, W>(&plan.steps[i].act);
        let j = at_step(axis, i, p.value);
        let set = |a: &&Axis| matches!(a.how, How::Arg { .. }) && a.kind == AxisKind::Number;
        for a in ones.iter().filter(set).filter(|a| a.how != axis.how) {
            control = control.and_then(|c| moved::<O, W>(&c, j, a, 1).or(Some(c)));
        }
        let Some(control) = control else {
            let why = format!("{} fails at {shown}; no control rebuilds", p.value);
            return Reach::Stopped(why).into();
        };
        let c = self.measure(c, p.value, &control);
        let same = c.t.refused.as_ref() == Some(r);
        let reaches = pass.t.uses.keys().any(|(s, _)| s != r.site());
        let (v, ok) = (p.value, pass.value);
        let one = c.t.refused.map_or("passes".to_owned(), |c| format!("fails at {c}"));
        let reach = match (same, reaches) {
            (true, true) => {
                let evidence = format!("{ok} passes, {v} fails with the numbers at 1 at {shown}");
                Reach::EntryCap { cap: v, evidence }
            }
            (false, _) => {
                Reach::Stopped(format!("{v} fails at {shown}; with the numbers at 1 it {one}"))
            }
            (true, false) => {
                Reach::Stopped(format!("{v} fails at {shown}, and {ok} reaches no site past it"))
            }
        };
        reach.into()
    }
}

/// A list axis whose longer lists can only repeat one item (its items'
/// type takes no fresh value): a to-do naming it, as a cost per distinct
/// item goes unseen.
fn same_items<O: Op<W>, W: World>(plan: &Plan<O>, i: usize, axis: &Axis) -> Option<Todo> {
    let grown = |fresh| moved_with::<O, W>(plan, i, axis, axis.at + 1, fresh);
    let text = |p: Option<Plan<O>>| p.map(|p| serde_json::to_string(&p).unwrap_or_default());
    (axis.kind == AxisKind::Len && axis.at > 0 && text(grown(true)) == text(grown(false))).then(
        || Todo {
            id: format!("distinct:{}", axis.name),
            why: format!(
                "`{}` grows only by copies of one item: a cost per distinct item is untried",
                axis.name
            ),
            close:
                "items a fresh value rebuilds (a string id), or a witness sending distinct items"
                    .into(),
            stop: None,
        },
    )
}

/// Links each crossing to its replay: a finding its own run showed.
pub(crate) fn link_replays<O>(audit: &mut Audit<O>) {
    let crossed = |r: &&mut CurveRow<O>| matches!(r.reach, Reach::Crossed { .. });
    for row in audit.curves.iter_mut().filter(crossed) {
        let found = audit.found.iter().find(|x| {
            x.original.fingerprint == row.case
                || row.fired.iter().any(|f| f.cause() == x.id.cause())
        });
        row.replay = found.and_then(|x| x.replay.clone());
    }
}

/// For each witness step where a stub answered amounts it was sent, a
/// witness of its own where the stub gives them all back, holding nothing
/// before: the answer that removes what the call credited, so its lists
/// grow by entries removed.
fn emptying<O: Op<W>, W: World>(
    h: &Harness<O, W>,
    runs: &[(Run<O>, Option<String>)],
) -> Vec<(Run<O>, Option<String>)> {
    let mut out = vec![];
    for (run, _) in runs {
        for i in 0..run.plan.steps.len() {
            for a in super::scan::stub_answers(h, run, i, &Some) {
                let mut p = run.plan.clone();
                let at = format!("step {i}, {}::{} gives back all it was sent", a.stub, a.method);
                p.steps[i].answers.push(a);
                out.push((h.run_case(&p), Some(at)));
            }
        }
    }
    out
}

/// A direct witness ([`other_contracts`](super::axes::other_contracts)) that its receiver
/// refuses, though its call carries lists: those lists stay as the
/// witnesses made them, an open item.
fn refused<O: Op<W>, W: World>(run: &Run<O>) -> Option<Todo> {
    let act = &run.plan.steps.first()?.act;
    let super::Act::Call(c) = act else { return None };
    let lists = arg_axes::<O, W>(act).into_iter().filter(|a| a.kind == AxisKind::Len);
    let lists: Vec<String> = lists.map(|a| format!("`{}` ({})", a.name, a.at)).collect();
    let why = run.outcomes.first()?.receipts.first()?.failure()?;
    let site = format!("{}::{}", c.receiver, c.method);
    (run.plan.steps.len() == 1 && !lists.is_empty()).then(|| Todo {
        id: format!("list:{site}"),
        why: format!(
            "`{site}` receives lists ({}), and refuses them sent directly: {why}",
            lists.join(", ")
        ),
        close: "a witness whose op grows those lists, with the world they need (a token holding \
                several ids), or one that calls the method as its callers do (Plan::call)"
            .into(),
        stop: None,
    })
}

/// Where the witness's step `i` sits in the plan `axis` moved to `value`.
fn at_step(axis: &Axis, i: usize, value: u128) -> usize {
    i + if matches!(axis.how, How::Repeat(_)) { value as usize } else { 0 }
}

/// Whether `p` and `q` differ in an outcome, a receipt reached or a use
/// (by more than 2%).
fn differ<O>(p: &Point<O>, q: &Point<O>) -> bool {
    let moved = |k| match (p.t.uses.get(k), q.t.uses.get(k)) {
        (Some(u), Some(v)) => u.used.abs_diff(v.used) > u.used.max(v.used) / 50,
        _ => true,
    };
    p.t.classes != q.t.classes
        || p.t.refused != q.t.refused
        || p.t.uses.keys().chain(q.t.uses.keys()).any(moved)
}

/// Whether a site of `p` crossed its limit.
fn crossed<O>(p: &Point<O>) -> bool {
    p.t.uses.values().any(|u| u.crossed)
}

/// What `run` used, the plan moved by `how` to `value` at step `i`; with
/// `only`, only step `i`'s own transactions.
fn tally<O: Op<W>, W: World>(
    h: &Harness<O, W>,
    how: &How,
    i: usize,
    steps: usize,
    value: u128,
    run: &Run<O>,
    only: bool,
) -> Tally {
    let n = value as usize;
    let copies = match how {
        How::Repeat(_) => i..i + n,
        How::Tail { .. } => i + 1..i + 1 + n,
        _ => 0..0,
    };
    let world = run.world::<W>().expect("the case's world");
    let sim: &crate::Sim = world.as_ref();
    let mut point = Tally::default();
    // Per-op checks not judged under overlap leave the run measured:
    // they stay open as the case's `incomplete:` item, and a tail overlaps
    // by design.
    let unfinished =
        matches!(run.verdict, super::Verdict::HarnessError | super::Verdict::Incomplete);
    if unfinished && !run.overlapped_only {
        point.refused = Some(Refusal::Case(run.reason.clone().unwrap_or_default()));
    }
    // Each transaction of step `j` failed at its entry, or never ran.
    let whole = |j: usize| {
        run.step_txs(j).all(|tx| {
            let entry = tx.receipts.first();
            entry.is_none_or(|e| e.failure().is_some())
                && tx.receipts.iter().skip(1).all(ReceiptOutcome::is_refund)
        })
    };
    // The first copy that failed or skipped: a later one may fail only
    // because that one did.
    for (k, j) in copies.clone().enumerate() {
        // A case that didn't finish stays the refusal: no copy closes it.
        if point.refused.is_some() {
            break;
        }
        if let Some(why) = run.ops.get(j).and_then(|r| r.skipped.clone()) {
            point.refused = Some(Refusal::CopySkipped(why));
            break;
        }
        let failed = run.step_txs(j).find_map(|tx| Some((tx, tx.receipts.first()?.failure()?)));
        if let Some((tx, f)) = failed {
            let entry = &tx.receipts[0];
            let (site, why, k) = (site(entry), f.to_string(), k + 1);
            let caller = entry.predecessor_id.to_string();
            point.refused = Some(Refusal::Copy { site, why, k, caller, receipt: entry.receipt_id });
            point.whole = whole(j);
        }
    }
    if let How::Repeat(super::axes::CopySource::Fresh(c, _)) = how {
        let txs: Vec<_> = copies.clone().flat_map(|j| run.step_txs(j)).collect();
        if let Some((_, _, staked, paid)) = callers_storage(sim, &txs) {
            let limit = paid.saturating_mul(STORAGE_RATIO);
            let at = format!("{}::{} (fresh callers)", c.receiver, c.method);
            point.add(at, Resource::StoragePaid, Use::new(staked, limit, staked > limit), false);
        }
    }
    let originals = (0..steps + copies.len()).filter(|j| !copies.contains(j));
    let ceiling = u128::from(h.gas_ceiling().as_gas());
    for (k, j) in originals.enumerate() {
        if only && k != i {
            continue;
        }
        for tx in run.step_txs(j) {
            let Some(entry) = tx.receipts.first() else {
                if k == i {
                    point.refused = Some(Refusal::Tx(format!("{:?}", tx.status)));
                    point.whole = whole(j);
                }
                continue;
            };
            if k == i {
                if let Some(f @ Failure::InvalidTx(_)) = tx.failure() {
                    point.refused = Some(Refusal::Tx(f.to_string()));
                    point.whole = whole(j);
                } else if let Some(f) = entry.failure()
                    && point.refused.is_none()
                {
                    let (site, why, receipt) = (site(entry), f.to_string(), entry.receipt_id);
                    point.refused = Some(Refusal::Entry { site, why, receipt });
                    point.whole = whole(j);
                }
            }
            // What the attached gas paid for: no refund, and not the
            // entry's exec fees, which the signer prepaid on top.
            let burnt = (tx.receipts.iter().filter(|r| !r.is_refund()))
                .fold(crate::Gas::ZERO, |g, r| g.saturating_add(r.gas_burnt))
                .saturating_sub(entry.prepaid_exec_fees);
            let used = Use::new(burnt.as_gas().into(), ceiling, false);
            let entry_failed = entry.failure().is_some();
            point.add(site(entry), Resource::PrepaidGas, used, false);
            for r in tx.receipts.iter().filter(|r| !r.is_refund() && !r.method_names.is_empty()) {
                // A refused entry crosses nothing: it is the axis's stop.
                let natural = !(entry_failed && std::ptr::eq(r, entry));
                point.receipt(sim, tx, r, natural);
            }
        }
    }
    point
}

impl Tally {
    /// Adds receipt `r` of `tx`: its outcome's class, what C1 compares,
    /// and its uses, crossing nothing unless `natural`.
    fn receipt(&mut self, sim: &crate::Sim, tx: &TxOutcome, r: &ReceiptOutcome, natural: bool) {
        let s = site(r);
        let failure = r.failure();
        let class = failure.as_ref().map_or("ok".to_owned(), |f| failure_class(&f.to_string()));
        self.classes.insert((s.clone(), class));
        let ops = r.storage_ops();
        let mark = format!(
            "{:?} {} {} {} {} {}",
            r.args, ops.reads, ops.writes, ops.removes, ops.written_bytes, ops.has_keys
        );
        self.marks.entry(s.clone()).or_insert(mark);
        let failed = failure.is_some();
        let mut put = |res, u: Use| {
            self.add(s.clone(), res, Use { crossed: natural && u.crossed, ..u }, failed);
        };
        let own = r.gas_burnt.saturating_sub(r.prepaid_exec_fees).as_gas();
        let out_of_gas = matches!(failure, Some(Failure::OutOfGas | Failure::GasLimitExceeded));
        let gas = Use::new(own.into(), r.attached_gas.as_gas().into(), out_of_gas);
        put(Resource::ReceiptGas, Use { forwarded: r.forwarded.as_gas().into(), ..gas });
        let (logged, limit) = sim.log_bytes(r);
        put(Resource::LogBytes, Use::new(logged.into(), limit.into(), logged > limit));
        // Only its receiver's (`prop_step_invariants` holds that).
        let bytes: i64 = (tx.effects().storage.iter())
            .filter(|c| c.receipt_id == r.receipt_id)
            .map(|c| c.bytes)
            .sum();
        let staked = sim.storage_cost(u64::try_from(bytes).unwrap_or(0));
        let paid = staked.saturating_sub(r.deposit).as_yoctonear();
        let free = sim.account(&r.receiver_id).map_or(0, |a| {
            a.amount.saturating_sub(sim.storage_cost(a.storage_usage)).as_yoctonear()
        });
        let lack =
            matches!(failure, Some(Failure::Action(ActionErrorKind::LackBalanceForState { .. })));
        put(Resource::StoragePaid, Use::new(paid, free, lack));
    }

    /// Adds one receipt's use: the most of a site's receipts. A failed
    /// receipt's use counts only where it crossed.
    fn add(&mut self, site: String, r: Resource, u: Use, failed: bool) {
        if failed && !u.crossed {
            return;
        }
        let e = self.uses.entry((site, r)).or_insert(u);
        let crossed = e.crossed || u.crossed;
        if u.used > e.used {
            *e = u;
        }
        e.crossed = crossed;
    }
}

/// How many times what they paid new callers make a contract stake for
/// them before it is a finding ([`STORAGE_FOR_CALLERS`](crate::explore::builtin::STORAGE_FOR_CALLERS)).
pub(crate) const STORAGE_RATIO: u128 = 10;

/// What `txs`, each from a caller new to the contract, made the account
/// that staked the most for them stake: its entry receipt, the bytes,
/// the yoctoNEAR staked less what the callers deposited to it, and what
/// the callers paid (gas and deposits).
pub(crate) fn callers_storage<'a>(
    sim: &crate::Sim,
    txs: &[&'a crate::TxOutcome],
) -> Option<(&'a ReceiptOutcome, i64, u128, u128)> {
    let entries = || txs.iter().filter_map(|o| o.receipts.first());
    let bytes = |e: &ReceiptOutcome| -> i64 {
        let changes = txs.iter().flat_map(|o| &o.effects().storage);
        changes.filter(|c| c.account == e.receiver_id).map(|c| c.bytes).sum()
    };
    let best = entries().max_by_key(|e| bytes(e))?;
    let mut paid = 0;
    for o in txs {
        paid += o.receipts.iter().map(|r| r.tokens_burnt.as_yoctonear()).sum::<u128>();
    }
    let into = entries().filter(|e| e.receiver_id == best.receiver_id);
    let deposited: u128 = into.map(|e| e.deposit.as_yoctonear()).sum();
    let staked = sim.storage_cost(u64::try_from(bytes(best)).unwrap_or(0)).as_yoctonear();
    Some((best, bytes(best), staked.saturating_sub(deposited), paid + deposited))
}

/// `receiver::method`.
fn site(r: &ReceiptOutcome) -> String {
    format!("{}::{}", r.receiver_id, r.method().unwrap_or_default())
}

/// The first line of a failure, with its numbers dropped: the outcome's
/// class.
fn failure_class(s: &str) -> String {
    let line = s.lines().next().unwrap_or_default();
    line.chars().filter(|c| !c.is_ascii_digit()).take(80).collect()
}

/// The nearest crossing the points project, of any site and resource
/// but the transaction's gas; in digits along a `far` number.
fn projection<O>(points: &[Point<O>], far: bool) -> Option<u128> {
    let keys: BTreeSet<&(String, Resource)> = points.iter().flat_map(|p| p.t.uses.keys()).collect();
    let ok = || points.iter().filter(|p| p.t.refused.is_none());
    let series = |k: &(String, Resource)| -> Vec<(u128, Use)> {
        ok().filter_map(|p| Some((p.value, *p.t.uses.get(k)?))).collect()
    };
    let keys = keys.into_iter().filter(|k| k.1 != Resource::PrepaidGas);
    keys.filter_map(|k| project(&series(k), far)).min()
}

fn project(series: &[(u128, Use)], digits: bool) -> Option<u128> {
    let [.., (x0, a), (x1, b)] = series else { return None };
    let at = |x: u128| if digits { (x as f64 + 1.0).log10() } else { x as f64 };
    let margin = |u: &Use| u.limit as f64 - u.used as f64;
    let fall = (margin(a) - margin(b)) / (at(*x1) - at(*x0));
    let ahead = margin(b) / fall;
    let steps = (ahead.floor() as u128).saturating_add(1);
    let x = if digits { 10f64.powf(at(*x1) + ahead) as u128 } else { x1.saturating_add(steps) };
    (fall > 0.0 && margin(b) > 0.0).then_some(x)
}

/// Counts the points other cases put past this row's own: the axis
/// moved as far as the furthest that didn't cross, and one past a stop
/// closes it. A crossing elsewhere is not the row's:
/// its other arguments or its gas (a scan's least-gas edge) may be what
/// crossed, and its own audit reports it. An entry cap, a protocol bound,
/// a crossing or no effect stands.
fn union<O: Op<W>, W: World>(row: &mut CurveRow<O>, seen: &Seen) {
    if !matches!(row.reach, Reach::Below | Reach::Stopped(_)) {
        return;
    }
    let Some(step) = row.plan.steps.get(row.step) else { return };
    let key = (step.act.name(), row.axis.name.clone(), row.site.clone(), row.resource);
    let Some(points) = seen.get(&key) else { return };
    // Nothing is past the type's max.
    let Some(past) = row.moved_to.checked_add(1) else { return };
    let further: Vec<(u128, u128)> =
        points.range(past..).filter(|p| !p.1.crossed).map(|(&v, u)| (v, u.used)).collect();
    let Some(&(furthest, _)) = further.last() else { return };
    row.reach = Reach::Below;
    row.stop = None;
    row.moved_to = furthest;
    row.elsewhere = further;
}

/// The rows of one climbed axis.
fn rows<O: Op<W>, W: World>(
    (i, axis): (usize, &Axis),
    points: &[Point<O>],
    stop: Option<AxisStop>,
    audit: &mut Audit<O>,
) {
    // A refused rung measures nothing; refused at the first, the axis
    // still has its rows, measured there, with why it stopped.
    let measured: Vec<&Point<O>> = points.iter().filter(|p| p.t.refused.is_none()).collect();
    let first = points.iter().take(1).filter(|_| measured.is_empty() && stop.is_some());
    let points: Vec<&Point<O>> = if measured.is_empty() { first.collect() } else { measured };
    let keys: BTreeSet<(String, Resource)> =
        points.iter().flat_map(|p| p.t.uses.keys().cloned()).collect();
    for key in keys {
        let series: Vec<(u128, Use)> =
            points.iter().filter_map(|p| Some((p.value, *p.t.uses.get(&key)?))).collect();
        let Some(&(moved_to, last)) = series.last() else { continue };
        let at_site: Vec<&Point<O>> =
            points.iter().copied().filter(|p| p.t.uses.contains_key(&key)).collect();
        let marks: BTreeSet<&String> =
            at_site.iter().filter_map(|p| p.t.marks.get(&key.0)).collect();
        let uses: Vec<u128> = series.iter().map(|s| s.1.used).collect();
        let (lo, hi) = (*uses.iter().min().unwrap_or(&0), *uses.iter().max().unwrap_or(&0));
        let still = marks.len() <= 1 && hi.saturating_sub(lo) <= lo / 50;
        let crossing = series.iter().position(|s| s.1.crossed);
        let reach = if let Some(c) = crossing {
            Reach::Crossed { at: series[c].0 }
        } else if let Some(stop) = stop.as_ref().filter(|s| s.lacks.as_ref() == Some(&key.0)) {
            stop.reach.clone()
        } else if hi == 0 || (still && series.len() > 1) {
            Reach::NoEffect
        } else if let Some(stop) = &stop {
            stop.reach.clone()
        } else {
            Reach::Below
        };
        let (first, end_use) = (series[0], series[series.len() - 1]);
        let run = end_use.0.saturating_sub(first.0).max(1) as f64;
        let slope = (end_use.1.used as f64 - first.1.used as f64) / run;
        let end = at_site[crossing.unwrap_or(at_site.len() - 1)];
        audit.curves.push(CurveRow {
            site: key.0.clone(),
            resource: key.1,
            step: i,
            axis: axis.clone(),
            points: series.iter().map(|s| (s.0, s.1.used)).collect(),
            forwarded: match key.1 {
                Resource::ReceiptGas => series.iter().map(|s| (s.0, s.1.forwarded)).collect(),
                _ => vec![],
            },
            limit: last.limit,
            limits: series.iter().map(|s| s.1.limit).collect(),
            slope,
            moved_to,
            elsewhere: vec![],
            plan: end.run.plan.clone(),
            steps: end.run.lines(),
            replay: None,
            case: end.run.fingerprint,
            fired: match crossing {
                Some(_) => end.run.findings.iter().map(|f| super::run::id(&end.run, f)).collect(),
                None => vec![],
            },
            stop: match &reach {
                Reach::Stopped(_) => stop.as_ref().and_then(|s| s.group.clone()),
                _ => None,
            },
            reach,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(site: &str, points: Vec<(u128, u128)>, limit: u128, reach: Reach) -> CurveRow<()> {
        let axis = Axis { name: "/ids".into(), kind: AxisKind::Len, at: 1, how: How::Gas };
        let moved_to = points.last().map_or(0, |p| p.0);
        CurveRow::<()> {
            site: site.into(),
            resource: Resource::ReceiptGas,
            step: 0,
            axis,
            limits: vec![limit; points.len()],
            points,
            forwarded: vec![],
            limit,
            slope: 0.0,
            reach,
            moved_to,
            elsewhere: vec![],
            plan: Plan::default(),
            replay: None,
            steps: vec![],
            case: Default::default(),
            fired: vec![],
            stop: None,
        }
    }

    /// A stop's group names the function its trap is in, says a trap is
    /// unnamed where the wasm has no names, and is a failure's class
    /// otherwise.
    #[test]
    fn a_refusal_s_cause_is_its_frame_else_unnamed_else_its_class() {
        let copy_k = |why: &str, k: usize| Refusal::Copy {
            site: "v::credit".into(),
            why: why.into(),
            k,
            caller: format!("c{k}"),
            receipt: CryptoHash::hash_bytes(&[k as u8]),
        };
        let copy = |why: &str| copy_k(why, 1);
        let trap = format!("WebAssembly trap: {}", crate::trap::UNREACHABLE);
        assert_eq!(copy(&trap).cause(), "an unnamed trap");
        let named = copy(&trap).named("`v::lock`, called from `credit`");
        assert_eq!(named.cause(), "`v::lock`");
        assert_eq!(copy("panicked: locked at 12").cause(), "panicked: locked at ");
        // A near-sdk panic's first line is its location: the message keys.
        let at = |m: &str| copy(&format!("panicked: panicked at src/lib.rs:10:9:\n{m}")).cause();
        assert_ne!(at("locked"), at("not registered"));
        assert_eq!(at("locked"), "locked");
        // Any other first line ending in `:` is the message itself.
        assert_eq!(copy("panicked: bad amount:\n12").cause(), "panicked: bad amount:");
        // Which copy, its caller and its receipt aren't the refusal's;
        // its kind, site and reason are.
        assert_eq!(copy(&trap), copy_k(&trap, 3));
        assert_ne!(copy(&trap), copy("panicked: locked"));
        let entry = |site: &str| Refusal::Entry {
            site: site.into(),
            why: trap.clone(),
            receipt: CryptoHash::hash_bytes(&[1]),
        };
        assert_ne!(copy(&trap), entry("v::credit"));
        assert_ne!(entry("v::credit"), entry("v::debit"));
    }

    /// A row prints its largest use and where it was, not its last: a use
    /// that falls along the axis peaked earlier.
    #[test]
    fn a_row_prints_its_largest_use() {
        let row = row("c::m", vec![(1, 90), (4, 10)], 100, Reach::Below);
        assert!(row.to_string().ends_with(", at most 90 (at 1) of 100"), "{row}");
    }

    /// The summary cuts a curve line at 200 characters: a row with long
    /// numbers keeps its verdict, which comes before them.
    #[test]
    fn a_long_row_keeps_its_verdict_under_the_summary_cut() {
        let points = vec![(1, 2_611_268_893_708), (16192, 1_001_111_268_893_708)];
        let row = row("omni-bridge.near::ft_on_transfer", points, 1_000_000_000_000_000, {
            Reach::Crossed { at: 16192 }
        });
        let line = super::super::report::short(&row.to_string(), 200);
        assert!(line.contains("CROSSED at 16192, harmless"), "{line}");
    }
}
