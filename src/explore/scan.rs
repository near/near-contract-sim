//! [`Scan`]: each witness plan as written, then every fault of a
//! [`Faults`] at each receipt it fits, and every edge of every axis: the
//! caller's gas, and each number the op signs.

use super::audit::Audit;
use super::axes::{AxisKind, How, act_json, act_max, arg_axes, moved};
use super::plan::Act;
use super::plan::{Inject, Plan, StepGas, StubAnswer};

use super::run::{Run, Verdict, address, fitting, play_plain};
use super::store::Saves;
use super::{Faults, Harness, Op, World, gas, progress};
use crate::findings::guarded;
use crate::inject::{Fault, Receipts};

use near_primitives::types::Gas;
use serde_json::Value;
use std::time::Instant;

/// 1 Tgas and 1 PGas: a number between them is taken for gas, and also
/// gets the wasm's gas constants as edges.
const GAS_RANGE: std::ops::RangeInclusive<u128> = 1_000_000_000_000..=1_000_000_000_000_000;

/// A scan of witness plans, built by [`Harness::scan`]; [`run`](Self::run)
/// runs it.
#[must_use]
pub struct Scan<'h, O, W> {
    harness: &'h Harness<O, W>,
    plans: Vec<Plan<O>>,
    faults: Faults,
    saves: Option<Saves>,
}

impl<O: Op<W>, W: World> Harness<O, W> {
    /// Scans `plans`, the witnesses: each as written, then, with
    /// [`Faults::default()`] unless [`Scan::faults`] says otherwise:
    ///
    /// - each fault at each receipt of each step's transactions it fits,
    ///   one at a time (findings labelled forced);
    /// - each step at the caller's gas edges: the least that succeeds
    ///   ([`StepGas::LeastPlus`]`(0)`), and each gas constant of the
    ///   fixture's wasm, 30 and 300 Tgas, exactly and 1 gas either side;
    /// - each number the op signs at 0, 1 and its type's max (a count the
    ///   op loops over: the top a curve climbs to), as the witness ran;
    ///   one taken for gas (1 Tgas to 1 PGas, unset, or an
    ///   [`Axes::floor`](super::Axes::floor)) or a size, at the least
    ///   caller gas and the ceiling, and also at each gas constant 1 gas
    ///   either side, at the ceiling;
    /// - each call the witnesses made into an audited contract (an entry
    ///   call, another contract's call, a call to itself or a callback),
    ///   once per method, as a witness of its own sent directly from
    ///   `axis.near` after the earlier steps of the witness that made it,
    ///   with the args and deposit it had; a method only its own contract
    ///   called is judged there by
    ///   [`builtin::SELF_ONLY`](super::builtin::SELF_ONLY). The summary
    ///   names the methods that ran but weren't sent;
    /// - a raw call's deposit at 0, 1 yocto, as sent ∓ 1 and 10 × as sent
    ///   (below a fixture signer's balance), and each of the first two
    ///   strings its JSON args hold (keys in sorted order) that name a
    ///   fixture account,
    ///   swapped in turn for `axis.near`, another fixture account, a
    ///   2-char and a 64-char named id, a 64-hex implicit and an
    ///   eth-implicit one: at most 17 such edges a call.
    ///
    /// Each plan runs once. Findings at an edge are labelled with it.
    pub fn scan(&self, plans: &[Plan<O>]) -> Scan<'_, O, W> {
        Scan { harness: self, plans: plans.to_vec(), faults: Faults::default(), saves: None }
    }
}

impl<O: Op<W>, W: World> Scan<'_, O, W> {
    /// The faults to force, and whether to try the edges
    /// ([`Faults::edges`]); [`Faults::none`] runs the witnesses only.
    pub fn faults(mut self, faults: Faults) -> Self {
        self.faults = faults;
        self
    }

    /// Writes the report so far to `path` every `every` variants, and at
    /// the end; a scan that finds the file resumes after the variants it
    /// counts, and its report includes the saved one. The file is a
    /// [`Report`](super::Report) (`Report::from_json` reads it), keyed on
    /// the fixture: one saved for another starts over. Delete it to start
    /// over. It carries the rows' replays, which the store gets only when
    /// the campaign finishes ([`Harness::store`](super::Harness::store)).
    pub fn saves(mut self, path: impl Into<std::path::PathBuf>, every: usize) -> Self {
        self.saves = Some(Saves::new(path.into(), every));
        self
    }

    /// Runs the scan: the witnesses first, then their variants, each plan
    /// once. The count of variants prints first, and the time left as
    /// they finish.
    pub fn run(self) -> Audit<O> {
        let h = self.harness;
        super::big_stack(|| {
            gas::with_fixture(h.fixture.as_ref(), || {
                let what = format!("scan of {} plan(s)", self.plans.len());
                let mut audit = Audit::new(what.clone());
                let resumed = Saves::resume(&self.saves, h, &mut audit);
                let mut variants: Vec<Variant<O>> = vec![];
                let mut seen = std::collections::HashSet::new();
                // From the start, so its elapsed time is the campaign's.
                let _guard = progress::start(what.clone(), "variant", 0, ", then settling");
                let started = Instant::now();
                let phase = progress::phase("the plans as written".into(), 0, None);
                let mut runs = super::axes::direct(h, &self.plans);
                let fixture = runs.iter().any(|r| r.0.on_fixture);
                if self.faults.tries_edges() && !fixture {
                    let (swept, sent) = super::axes::sweep_plans(h, &runs);
                    runs.extend(swept.into_iter().map(|(p, at)| (h.run_case(&p), Some(at))));
                    audit.sent = Some(sent);
                }
                drop(phase);
                audit.time("the plans as written", started.elapsed());
                for (witness, _) in &runs {
                    seen.insert(serde_json::to_string(&witness.plan).unwrap_or_default());
                }
                for (w, (witness, at)) in runs.iter().enumerate() {
                    if resumed == 0 {
                        audit.witness(h, witness, at.as_deref(), w);
                    }
                    audit.fixed.extend(super::axes::fixed::<O, W>(&witness.plan));
                    for mut v in self.variants(witness, w) {
                        // A direct witness's timings name it.
                        if let Some(at) = at {
                            v.name = format!("{at}: {}", v.name);
                        }
                        // One run per plan: two edges that set the same
                        // bytes are one variant.
                        if seen.insert(serde_json::to_string(&v.plan).unwrap_or_default()) {
                            variants.push(v);
                        }
                    }
                }
                if !self.faults.tries_edges() {
                    eprintln!(
                        "{what}: no edges (these Faults have none; .and(Faults::edges()) adds them)"
                    );
                }
                let total = variants.len() as u32;
                let least = variants.iter().filter(|v| v.least).count();
                let faults = variants.iter().filter(|v| v.meant.is_some()).count();
                eprintln!(
                    "{what}: {total} variant(s): {faults} forcing a fault, {} at an edge, {least} \
                     of them at the least caller gas (a search for it each); the time left \
                     prints as they finish",
                    total as usize - faults
                );
                progress::total(total);
                for (k, v) in variants.into_iter().enumerate().skip(resumed) {
                    let started = Instant::now();
                    let run = h.run_case(&v.plan);
                    let took = started.elapsed();
                    for (i, rec) in run.ops.iter().enumerate() {
                        for (least, all, site) in &rec.bands {
                            let act = format!("{:?}", run.plan.steps[i].act);
                            let (least, all) = (StepGas::Exactly(*least), StepGas::Exactly(*all));
                            audit.r.bands.push(format!(
                                "{act} (step {i}): the transaction succeeds from {least}, \
                                 every receipt only from {all}; between, {site} fails"
                            ));
                        }
                    }
                    audit.time(&v.name, took);
                    audit.add(&run, v.at.as_deref().map(|at| (at, v.axis.as_str())));
                    // An edge whose op skipped wasn't tried: open, never a
                    // pass. A later op skipping is what the edge did.
                    let skipped =
                        run.ops.get(v.step).and_then(|o| Some((&o.name, o.skipped.as_ref()?)));
                    if let (Some(at), Some((op, why))) = (&v.at, skipped)
                        && run.verdict != Verdict::Incomplete
                    {
                        let w = v.witness;
                        let family = super::audit::family(Some(op), Some(&v.axis));
                        audit.incomplete.push((family, format!(
                            "witness {w}, {at}: edge not tried: op skipped: {op} (step {}): {why}",
                            v.step
                        )));
                    }
                    if let Some(why) = v.meant.and_then(|e| e.differs(&run)) {
                        audit.r.errors.push(why);
                    }
                    progress::step(k as u32 + 1);
                    Saves::tick(&self.saves, h, &audit, k + 1, false);
                }
                Saves::tick(&self.saves, h, &audit, (total as usize).max(resumed), true);
                h.save(&mut audit);
                audit
            })
        })
    }

    /// Every variant of `witness`'s plan, each with the edge it sets, and
    /// for a fault the receipt it is meant for; of a call the sweep added
    /// ([`Plan::swept`]), of that last step only: its witness varies the
    /// rest.
    fn variants(&self, witness: &Run<O>, w: usize) -> Vec<Variant<O>> {
        let plan = &witness.plan;
        let h = self.harness;
        let mut out = vec![];
        let ceiling = StepGas::Exactly(h.gas_ceiling());
        let least = StepGas::LeastPlus(Gas::ZERO);
        let from = if plan.swept.is_some() { plan.steps.len().saturating_sub(1) } else { 0 };
        for (i, step) in plan.steps.iter().enumerate().skip(from) {
            let Some(record) = witness.ops.get(i).filter(|r| r.skipped.is_none()) else {
                continue;
            };
            let with = |f: &dyn Fn(&mut super::Step<O>)| {
                let mut p = plan.clone();
                f(&mut p.steps[i]);
                p
            };
            // Each fault at each receipt it fits.
            for (inject, meant) in fault_injects(&self.faults, witness, i) {
                let name = format!("step {i}, {inject} ({})", meant.site);
                let plan = with(&|s| s.inject.push(inject.clone()));
                // A receipt for a method its receiver lacks: nothing to hold.
                if !super::run::misnamed(h, &plan).is_empty() {
                    continue;
                }
                let (least, witness, step, axis) = (false, w, i, String::new());
                let meant = Some(meant);
                out.push(Variant { plan, at: None, name, meant, least, witness, step, axis });
            }
            if !self.faults.tries_edges() {
                continue;
            }
            let edge = |plan, at: String, axis: &str, least| Variant {
                plan,
                name: at.clone(),
                at: Some(at),
                meant: None,
                least,
                witness: w,
                step: i,
                axis: axis.into(),
            };
            // A stub that kept what it was sent once answers twice that.
            for p in over_answers(h, witness, i) {
                let StubAnswer { stub, method, json } = p.steps[i].answers.last().expect("pushed");
                let at = format!(
                    "step {i}, {stub}::{method} answers {json}, twice what was sent, after a setup copy it kept"
                );
                let axis = format!("{stub}::{method} answers");
                out.push(edge(p, at, &axis, false));
            }
            // And, holding nothing before, gives back all it was sent.
            for a in stub_answers(h, witness, i, &Some) {
                let at = format!("step {i}, {}::{} gives back all it was sent", a.stub, a.method);
                let axis = format!("{}::{} gives back", a.stub, a.method);
                out.push(edge(with(&|s| s.answers.push(a.clone())), at, &axis, false));
            }
            // The caller's gas, on a step with a call whose gas it sets.
            let unpinned = record.unpinned > 0;
            if unpinned {
                // A gas 1 either side of a constant prints as such.
                let mut edges = vec![(least, least.to_string())];
                for g in gas::thresholds() {
                    let (t, near) = (StepGas::Exactly(g), |d| g.as_gas().wrapping_add_signed(d));
                    let near = |d| StepGas::Exactly(Gas::from_gas(near(d)));
                    edges.extend([(near(-1), format!("{t} - 1 gas")), (t, t.to_string())]);
                    edges.push((near(1), format!("{t} + 1 gas")));
                }
                for (g, shown) in edges {
                    let at = format!("step {i}, caller gas {shown}");
                    // The gas band, measured here only.
                    let plan = Plan { bands: g == least, ..with(&|s| s.gas = Some(g)) };
                    out.push(edge(plan, at, "caller gas", g == least));
                }
            }
            // Each number the step's act signs, at its edges: one that
            // touches gas or a size also at the least caller gas and the
            // ceiling, any other as the witness ran.
            let mut world = None;
            for (moved, at, cross) in number_edges(h, plan, i, &mut world) {
                // `<axis> = <value>`.
                let axis = at.split(" = ").next().unwrap_or(&at).to_owned();
                let gases = match (cross, unpinned) {
                    (Cross::Both, true) => vec![Some(least), Some(ceiling)],
                    (Cross::Ceiling, true) => vec![Some(ceiling)],
                    _ => vec![step.gas],
                };
                for g in gases {
                    let caller = match g {
                        Some(g) if unpinned && cross != Cross::AsRan => format!(", caller gas {g}"),
                        _ => String::new(),
                    };
                    let mut p = moved.clone();
                    p.steps[i].gas = g;
                    out.push(edge(p, format!("step {i}, {at}{caller}"), &axis, g == Some(least)));
                }
            }
            // A raw call's deposit, and the account ids its args name.
            if let Act::Call(c) = &step.act {
                let call = |f: &dyn Fn(&mut super::Call)| {
                    with(&|s| {
                        if let Act::Call(c) = &mut s.act {
                            f(c);
                        }
                    })
                };
                // A fixture signer that can't pay is refused before the
                // contract runs; one the fixture lacks (`axis.near`) is
                // made with what the call attaches. The cap reads the
                // fixture's balance and leaves nothing for gas: a deposit
                // near it is refused, a variant lost, never a wrong verdict.
                let sim = h.fixture.as_ref();
                let afford = sim.account(&c.signer).map_or(u128::MAX, |a| a.amount.as_yoctonear());
                let deposits = super::outsider::deposits(c.deposit.as_yoctonear());
                for d in deposits.into_iter().filter(|&d| d < afford) {
                    let p = call(&|c| c.deposit = crate::Balance::from_yoctonear(d));
                    out.push(edge(p, format!("step {i}, deposit = {d}"), "deposit", false));
                }
                for (axis, args, at) in super::outsider::id_edges(h.fixture.as_ref(), &c.args) {
                    let p = call(&|c| c.args.clone_from(&args));
                    out.push(edge(p, format!("step {i}, {at}"), &axis, false));
                }
            }
        }
        out
    }
}

/// Where a stub answered a call of step `i` with amounts (a decimal
/// string, or a list of them) and the call's args carry what was sent (a
/// decimal string, or a list of them as long): the stub keeping all it was
/// sent in a setup copy of steps 0..=i, then answering twice what it was
/// sent: a counterparty claiming back more than it got, from a balance it
/// holds (one with none can only take back its own).
fn over_answers<O: Op<W>, W: World>(h: &Harness<O, W>, run: &Run<O>, i: usize) -> Vec<Plan<O>> {
    let keeps = stub_answers(h, run, i, &|_| Some(0));
    let twice = stub_answers(h, run, i, &|x| x.checked_mul(2));
    let mut out = vec![];
    for twice in twice {
        let Some(keep) = keeps.iter().find(|k| k.stub == twice.stub).cloned() else { continue };
        let mut plan = run.plan.clone();
        plan.setup = plan.steps[..=i].to_vec();
        plan.setup[i].answers.push(keep);
        plan.steps[i].answers.push(twice);
        out.push(plan);
    }
    out
}

/// What each stub that answered a call of step `i` with amounts, where the
/// call's args carry what was sent (one, or a list as long), answers with
/// each amount sent mapped by `f`. With `f` the identity, the stub gives
/// back everything it was sent, and one that answered `false` (kept a
/// token) answers `true`: the answer that empties what it was credited.
pub(crate) fn stub_answers<O: Op<W>, W: World>(
    h: &Harness<O, W>,
    run: &Run<O>,
    i: usize,
    f: &dyn Fn(u128) -> Option<u128>,
) -> Vec<StubAnswer> {
    let receipts = run.step_txs(i).flat_map(|o| &o.receipts);
    let num = |v: &Value| v.as_str()?.parse::<u128>().ok();
    // An amount, or a list of `n` of them, each mapped by `f`.
    let amounts = |v: &Value, n: Option<usize>, f: &dyn Fn(u128) -> Option<u128>| {
        let one = |v: &Value| Some(Value::String(f(num(v)?)?.to_string()));
        match (v, n) {
            (Value::String(_), None) => one(v),
            (Value::Array(xs), Some(n)) if xs.len() == n => {
                xs.iter().map(one).collect::<Option<Vec<_>>>().map(Value::Array)
            }
            _ => None,
        }
    };
    let json = |b: &[u8]| serde_json::from_slice::<Value>(b).ok();
    let mut out: Vec<StubAnswer> = vec![];
    for r in receipts {
        let ([method], Some(args), Some(answered)) =
            (&r.method_names[..], r.args.first().and_then(|a| json(a)), r.value().and_then(json))
        else {
            continue;
        };
        let stub = &r.receiver_id;
        if out.iter().any(|a| a.stub == *stub) || !h.fixture.as_ref().is_stub(stub) {
            continue;
        }
        let n = answered.as_array().map(Vec::len);
        let mut sent = args.as_object().into_iter().flat_map(|m| m.values());
        let back = answered == Value::Bool(false) && f(1) == Some(1);
        let answer = if back {
            Some(Value::Bool(true))
        } else {
            amounts(&answered, n, &Some)
                .and(sent.find(|v| amounts(v, n, &Some).is_some()))
                .and_then(|sent| amounts(sent, n, f))
        };
        if let Some(json) = answer {
            out.push(StubAnswer { stub: stub.clone(), method: method.clone(), json });
        }
    }
    out
}

/// One variant of a witness: its plan, the edge it sets (a fault's is
/// `None`: its label is what it forced), its name in the timings, the
/// receipt a fault is meant for, and whether it searches for the least
/// caller gas.
struct Variant<O> {
    plan: Plan<O>,
    at: Option<String>,
    name: String,
    meant: Option<Meant>,
    least: bool,
    /// Its witness's index, the step it varies, and the axis an edge moves.
    witness: usize,
    step: usize,
    axis: String,
}

/// Each fault of `faults` at each receipt of step `i`'s transactions it
/// fits in `witness`, one at a time, and with [`Faults::reorder`] each
/// pair of them swapped: the injection, and the receipt it is meant for.
pub(crate) fn fault_injects<O>(
    faults: &Faults,
    witness: &Run<O>,
    i: usize,
) -> Vec<(Inject, Meant)> {
    let mut out = vec![];
    let Some(txs) = witness.spawned.get(i) else { return out };
    let outcomes: Vec<&crate::TxOutcome> = witness.step_txs(i).collect();
    for (tx, (spawned, buffered)) in txs.iter().enumerate() {
        let mut tried = vec![];
        let mut push = |n: usize, fault: Fault| {
            let (at, site) = (address(spawned, n, &fault), spawned[n].site());
            let meant = Meant { step: i, tx, n, site };
            out.push((Inject { tx: tx as u8, at, fault }, meant));
        };
        for arm in faults.arms() {
            for fault in &arm.faults {
                if !fault.is_hold() && !buffered.is_empty() {
                    continue;
                }
                for n in fitting(&arm.at, fault, spawned) {
                    if tried.contains(&(n, fault)) {
                        continue;
                    }
                    tried.push((n, fault));
                    push(n, fault.clone());
                }
            }
        }
        let Some(outcome) = outcomes.get(tx).filter(|_| faults.reorders()) else { continue };
        for (held, until) in super::faults::reorders(spawned, outcome) {
            let target = address(spawned, until, &Fault::HoldUntil(Receipts::all()));
            push(held, Fault::HoldUntil(target));
        }
    }
    out
}

/// The receipt a fault variant is meant for: the witness's receipt `n` of
/// transaction `tx` of step `step`, and its site.
pub(crate) struct Meant {
    step: usize,
    tx: usize,
    n: usize,
    site: String,
}

impl Meant {
    /// Why `run` held another receipt there, if it did: an op that submits
    /// other transactions each time would make the variant's fault land
    /// elsewhere, and pass as a clean run.
    pub(crate) fn differs<O>(&self, run: &Run<O>) -> Option<String> {
        let spawned = run.spawned.get(self.step)?.get(self.tx)?;
        let got = spawned.0.get(self.n).map_or("nothing".to_owned(), |s| s.site());
        let Meant { step, n, site, .. } = self;
        (got != *site).then(|| format!("step {step}: receipt {n} was {site} in the plain case and {got} here: an op must submit the same transactions each time"))
    }
}

/// At which caller gas an edge runs, on a step with an unpinned call.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Cross {
    /// As the witness ran: the edge touches neither gas nor a size.
    AsRan,
    /// At the ceiling: a gas constant set on a gas argument, which the
    /// contract's own budget then doesn't bind.
    Ceiling,
    /// At the least caller gas and at the ceiling: 0, 1 or the top of a
    /// number taken for gas or a size.
    Both,
}

/// Each edge of each number step `i` signs, and each other value of each
/// choice: the plan moved there, `pointer = value`, and the caller's gas
/// to try it at ([`Cross`]).
///
/// A number taken for gas (between 1 Tgas and 1 PGas, a declared floor,
/// or one the witness leaves unset: no value says it isn't gas) also gets
/// each gas constant 1 gas either side. A number the op turns into a size
/// ([`sizes`]: a count, a length) is tried at the top a curve climbs to
/// instead of its type's max, which no loop or allocation finishes; that
/// top runs as the witness ran, the curve owns what lies past it.
fn number_edges<O: Op<W>, W: World>(
    h: &Harness<O, W>,
    plan: &Plan<O>,
    i: usize,
    world: &mut Option<W>,
) -> Vec<(Plan<O>, String, Cross)> {
    let act = &plan.steps[i].act;
    let Some((json, axes)) = act_json::<O, W>(act) else { return vec![] };
    let mut out = vec![];
    for axis in arg_axes::<O, W>(act) {
        if let How::Choice { values, .. } = &axis.how {
            for k in (0..values.len() as u128).filter(|&k| k != axis.at) {
                if let Some(p) = moved::<O, W>(plan, i, &axis, k) {
                    let v = &values[k as usize];
                    let v = v.as_str().map_or_else(|| v.to_string(), str::to_owned);
                    out.push((p, format!("{} = {v}", axis.name), Cross::AsRan));
                }
            }
            continue;
        }
        if axis.kind != AxisKind::Number {
            continue;
        }
        let unset = matches!(axis.how, How::Unset { .. });
        let floor = axes.floor_of(&axis.name);
        let gas_like = GAS_RANGE.contains(&axis.at) || floor.is_some() || unset;
        let size = sizes(h, plan, i, &axis, world);
        let top = if size {
            Some(axis.at.max(1) << (super::curves::RUNGS - 1))
        } else {
            act_max::<O, W>(act, &json, &axis)
        };
        let ends: Vec<u128> = [Some(0), Some(1), top].into_iter().flatten().collect();
        let mut edges = ends.clone();
        if gas_like && !size {
            let from = floor.map_or(0, |f| f.as_gas());
            let mut gases: Vec<u64> =
                gas::thresholds().iter().map(|g| g.as_gas()).filter(|&g| g >= from).collect();
            gases.extend(floor.map(|f| f.as_gas()));
            for g in gases {
                edges.extend([g - 1, g, g + 1].map(u128::from));
            }
        }
        edges.sort_unstable();
        edges.dedup();
        for n in edges.into_iter().filter(|&n| unset || n != axis.at) {
            if let Some(p) = moved::<O, W>(plan, i, &axis, n) {
                let cross = match (gas_like || size, ends.contains(&n)) {
                    (false, _) => Cross::AsRan,
                    (true, false) => Cross::Ceiling,
                    (true, true) if size && Some(n) == top => Cross::AsRan,
                    (true, true) => Cross::Both,
                };
                out.push((p, format!("{} = {n}", axis.name), cross));
            }
        }
    }
    out
}

/// Whether the op turns the number at `axis` of step `i` into a size: with
/// 10 and with 99 there (the same digits, so an amount signs the same
/// bytes), it submits a different number of transactions or calls, or
/// signs a different number of bytes. Probed by submitting on the world
/// as it stands before step `i` (played once, in `world`), with no block
/// produced. An op that skips or panics at 10 is taken for no size; one
/// that runs at 10 and skips at 99 is one.
pub(crate) fn sizes<O: Op<W>, W: World>(
    h: &Harness<O, W>,
    plan: &Plan<O>,
    i: usize,
    axis: &super::Axis,
    world: &mut Option<W>,
) -> bool {
    if !matches!(plan.steps[i].act, Act::Op(_)) {
        return false;
    }
    let base = world.get_or_insert_with(|| {
        let mut w = super::run::set_up(&h.fixture, plan);
        let mut prefix = plan.clone();
        prefix.steps.truncate(i);
        if i > 0 {
            play_plain(&mut w, &prefix);
        }
        w
    });
    let probe = |v: u128| -> Option<(usize, u32, usize)> {
        let p = moved::<O, W>(plan, i, axis, v)?;
        let Act::Op(op) = &p.steps[i].act else { return None };
        let mut w = base.clone();
        let from = w.as_ref().txs().len();
        w.as_mut().explore = Some(Box::new(super::Hook::new(None, vec![], vec![])));
        let ran = guarded(|| op.submit(&mut w).is_ok());
        let hook = w.as_mut().explore.take()?;
        let bytes = hook.signed.iter().map(|s| s.to_string().len()).sum();
        (ran == Ok(true)).then(|| (w.as_ref().txs().len() - from, hook.calls, bytes))
    };
    let (ten, more) = (probe(10), probe(99));
    ten.is_some() && ten != more
}
