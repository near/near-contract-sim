//! [`Enumerate`]: every sequence of an alphabet's ops up to a depth, each
//! next op at every block offset inside the previous op's measured
//! flight, states at rest deduplicated, as plans a harness runs.

use super::audit::Audit;
use super::plan::{Gap, Plan, SHORT_EPOCH};
use super::report::Todo;
use super::run::{Run, Verdict};
use super::scan::{Meant, fault_injects, step_edges, take_in};
use super::store::Saves;
use super::{Faults, Harness, Op, World, gas, progress};
use crate::{Fault, Sim};
use near_primitives::hash::CryptoHash;
use std::collections::{BTreeMap, BTreeSet};
use std::rc::Rc;

/// What an enumeration without edges doesn't vary, which its report
/// states.
const NOT_VARIED: &str = "not varied: caller gas (as each op sets it), callers (as each op \
    signs; no direct call from axis.near), stub answers (the fixture's), argument values (as the \
    alphabet holds them)";

/// What one with edges ([`Faults::edges`]) doesn't vary, which its report
/// states.
const NOT_VARIED_EDGES: &str = "not varied: callers (as each op signs; no direct call from \
    axis.near), a stub answering more than it was sent, and on a step placed inside a flight \
    caller gas, stub answers and argument values; a step placed at rest also ran at a scan's \
    per-step edges (caller gas, a stub giving back all it was sent, the op's axes), each edge a \
    case not extended";

/// The verification boundary an enumeration under `faults` states, on a
/// fixture whose epochs it places boundaries in or not (the dedup key's
/// is the merged states' note): the hold lengths it tried, each kind's
/// where they differ, and none where it tried none.
fn boundary(faults: &Faults, epochs: bool) -> String {
    let all = || faults.arms().iter().flat_map(|a| &a.faults);
    let of =
        |kind: fn(&Fault) -> Option<u64>| -> BTreeSet<u64> { all().filter_map(kind).collect() };
    let receipt = of(|f| if let Fault::Hold(n) = f { Some(*n) } else { None });
    let result = of(|f| if let Fault::HoldResult(n) = f { Some(*n) } else { None });
    let until = faults.reorders() || all().any(|f| matches!(f, Fault::HoldUntil(_)));
    let held = if receipt == result {
        format!("holds{}", untried(&receipt))
    } else {
        format!("receipt holds{}, result holds{}", untried(&receipt), untried(&result))
    };
    // A reordering holds until another receipt has run, for no count.
    let counted = !receipt.is_empty() || !result.is_empty();
    let held = match (counted, until) {
        (false, true) => "holds for a count of blocks".to_owned(),
        (true, true) => format!("{held} (reorderings aside)"),
        _ => held,
    };
    let not_tried = if faults.arms().is_empty() && !faults.reorders() {
        format!("Offsets outside the flight and {held} were not tried")
    } else {
        format!("Offsets outside the flight, {held} and two faults on one step were not tried")
    };
    let epoch = if epochs {
        "an epoch boundary goes one to each run of ops in flight together, with no fault of its own"
            .to_owned()
    } else {
        format!("no epoch boundary was tried (epochs over {SHORT_EPOCH} blocks)")
    };
    let forced = if counted || until {
        " Holds and reorderings are forced: the sim shows they are possible on one shard, not \
         that a network produces them."
    } else {
        ""
    };
    format!("{not_tried}; {epoch}.{forced} A bounded enumeration finds bugs and proves nothing.")
}

/// Which holds of a kind tried at `lengths` (block counts) were not:
/// those over the longest where every length from 1 was tried, else
/// those of other lengths; all where none was.
fn untried(lengths: &BTreeSet<u64>) -> String {
    match lengths.last() {
        Some(&n) if lengths.iter().copied().eq(1..=n) => format!(" over {n} block(s)"),
        Some(_) => {
            let l: Vec<String> = lengths.iter().map(u64::to_string).collect();
            format!(" other than {} block(s)", l.join("/"))
        }
        None => String::new(),
    }
}

/// What the default dedup key leaves out.
const LEFT_OUT: &str = "the block height and timestamp within an epoch, accounts without \
    code, and contracts not audited (a contract's balance, gas refunds included, is in it)";

/// Cases an enumeration runs unless [`Enumerate::cases`] says.
const CASES: u32 = 2_000;

type KeyFn<W> = Rc<dyn Fn(&W) -> u64>;

/// A state to extend: the plan that reached it, how many blocks of the
/// flight of the ops in flight with its last step remain after that
/// step's submission, whether it is extended after it settles too, and
/// the dedup key of the world before those ops (the run's base) and of
/// its own.
struct Node<O> {
    plan: Plan<O>,
    remaining: u64,
    settle: bool,
    base: u64,
    rest: u64,
}

/// A case to run: its plan, what kind of case, and its run's base.
type Case<O> = (Plan<O>, Kind, u64);

/// A case run: its plan, run, span and base, and whether it is forced.
type Done<O> = (Plan<O>, Run<O>, u64, u64, bool);

/// What a case is: a plan as enumerated, or a variant of one (its last
/// step gets no variants of its own).
enum Kind {
    Plain,
    /// A fault at the receipt it is meant for.
    Fault(Meant),
    /// An epoch boundary inside the flight of its last ops.
    Edge,
    /// A per-step edge of its last step, placed at rest (where, and the
    /// axis it moves): a leaf, never extended.
    Leaf(String, String),
}

/// An enumeration, built by [`Harness::enumerate`]; [`run`](Self::run)
/// runs it.
#[must_use]
pub struct Enumerate<'h, O, W> {
    harness: &'h Harness<O, W>,
    alphabet: Vec<O>,
    first: Option<Vec<O>>,
    depth: usize,
    faults: Faults,
    key: Option<KeyFn<W>>,
    cases: u32,
    saves: Option<Saves>,
}

impl<O: Op<W>, W: World> Harness<O, W> {
    /// Every sequence of `alphabet`'s ops up to [`depth`](Enumerate::depth)
    /// (3), the first from [`first`](Enumerate::first)'s ops if it is
    /// called, each next op placed at every block offset inside the previous
    /// op's measured flight and after it settles, under
    /// [`faults`](Enumerate::faults): each case a [`Plan`], so its
    /// findings, replays and to-do items are a scan's. An op that doesn't
    /// apply in a state returns `Err(Skip)`, as everywhere.
    ///
    /// - **The flight** of a step is the blocks from its submission to its
    ///   last receipt, measured from its own run (a hold's variant
    ///   measures its own); the next op goes in at each offset before the
    ///   last block of any op in flight with it ([`Gap::Blocks`]), or
    ///   after they settle. The report prints each op's flights.
    /// - **Epoch boundaries**, on a fixture of epochs of 100 blocks or
    ///   fewer: each new step not under a fault also runs with an epoch
    ///   boundary at each block of the flight of the ops in flight with
    ///   it, placed by a [`Gap::EpochEdge`] before the first of them (one
    ///   boundary to each such run of ops). Offsets an epoch long or more
    ///   or past 127 blocks are not tried (`unexplored:epoch boundaries`);
    ///   a case whose ops went in while the ops before were still in
    ///   flight ran as another interleaving
    ///   (`unexplored:epoch boundaries placed late`). Serial orders
    ///   ([`SERIALIZABLE`](super::builtin::SERIALIZABLE)) run settled, so
    ///   their receipts meet the boundary elsewhere.
    /// - **Faults**: each fault at each receipt of the new step it fits,
    ///   as a scan tries them, [`Faults::reorder`] included. None by
    ///   default.
    /// - **Edges**, with [`Faults::edges`] (in [`Faults::default`]): each
    ///   new step placed at rest also runs at the scan's per-step edges
    ///   there: the caller's gas (the least that succeeds, and each gas
    ///   constant 1 gas either side), each number and choice of its
    ///   [`Op::axes`] (narrowed by [`Axes::only`](super::Axes::only)), and
    ///   each stub giving back all it was sent. Each is a leaf: a case of
    ///   its own, never extended, so they add V cases per state at rest
    ///   and op. A step placed inside a flight runs as the alphabet holds
    ///   it, and no call is sent directly from `axis.near`.
    /// - **Dedup**: after each depth, in a fixed order, each case's world
    ///   at rest is keyed: by default by every audited contract's state,
    ///   balance and locked balance, the epoch height and the chain
    ///   config ([`key`](Enumerate::key) overrides it). A state already
    ///   seen is extended only inside the flight of its last ops, and
    ///   only if those ops, from a state before them with their key, are
    ///   new (its findings stay); the end checks' verdicts (the books'
    ///   liveness) are reused for it. The default
    ///   leaves out the block height and timestamp within an epoch,
    ///   accounts without code, and contracts not audited: two states
    ///   apart only there are merged, so a contract that reads the height
    ///   needs a key with it.
    ///
    /// Enumerated N cases to depth D over these ops, faults and offsets
    /// inside each op's measured flight. Not varied: caller gas (as each
    /// op sets it), callers (no direct call from `axis.near`), stub
    /// answers (the fixture's) and argument values (as the alphabet holds
    /// them). States equal under the dedup key were explored once; a bug
    /// reachable only through what the key leaves out is missed. Offsets
    /// outside the flight were not tried, nor holds of other kinds or
    /// lengths than the faults' (none under [`Faults::none`]), nor two
    /// faults on one step, nor epoch boundaries on epochs over 100
    /// blocks. Holds and reorderings are forced: the sim shows they are
    /// possible on one shard, not that a network produces them. A bounded
    /// enumeration finds bugs and proves nothing.
    pub fn enumerate(&self, alphabet: &[O]) -> Enumerate<'_, O, W> {
        Enumerate {
            harness: self,
            alphabet: alphabet.to_vec(),
            first: None,
            depth: 3,
            faults: Faults::none(),
            key: None,
            cases: CASES,
            saves: None,
        }
    }
}

impl<O: Op<W>, W: World> Enumerate<'_, O, W> {
    /// Sequences of up to `ops` ops; 3 by default.
    pub fn depth(mut self, ops: usize) -> Self {
        self.depth = ops;
        self
    }

    /// The first step from `ops`, later steps from the alphabet; without
    /// it every step is the alphabet's. Nothing else changes: an op after
    /// itself, the offsets, faults, epoch boundaries and dedup by state
    /// are as without it. An empty `ops`, or an empty alphabet (even at
    /// depth 1), tries nothing. The report's headline says "first step
    /// from K op(s)" and a note names them, so a restricted run never
    /// reads as the whole alphabet's; a [`saves`](Self::saves) file
    /// resumes only an enumeration with the same first ops, in the same
    /// order.
    ///
    /// - **Targeted**: `first(&[x])` to depth 2 runs `x`, then each op of
    ///   the alphabet at each block of its flight and after it settles,
    ///   without the pairs that start elsewhere.
    /// - **Split over processes**: give each process a part of the
    ///   alphabet as its first ops, with its own `saves` file, and merge
    ///   the reports read back ([`Report::from_json`](super::Report::from_json))
    ///   with [`Harness::union`]. The union is a superset of one run's
    ///   cases: each process dedups on its own, so a state two of them
    ///   reach is extended in both. Findings merge by id, their cases
    ///   summed; each process folds causes into rows on its own, so causes
    ///   one run folds into a row can come back as rows of their own. The
    ///   union's headline keeps each part's text once (parts of equal
    ///   counts read alike): its case count and the parts' first-ops
    ///   notes are the record. There is no `shard` knob: the split
    ///   is the first ops.
    pub fn first(mut self, ops: &[O]) -> Self {
        self.first = Some(ops.to_vec());
        self
    }

    /// The faults to force on each new step, as [`Scan::faults`](super::Scan::faults)
    /// takes them, and whether to run each new step placed at rest at its
    /// per-step edges ([`Faults::edges`]); [`Faults::none`] by default.
    /// [`Faults::default`]'s faults without its edges are
    /// `Faults::tolerable().and(Faults::new(Receipts::all(), [Fault::OutOfGas]))`.
    pub fn faults(mut self, faults: Faults) -> Self {
        self.faults = faults;
        self
    }

    /// The dedup key of a world at rest, instead of the default: states
    /// with one key are explored once, and the end checks' verdicts (the
    /// books' liveness) judged once for them: a key coarser than what
    /// those checks read reuses one state's verdict for another.
    pub fn key(mut self, key: impl Fn(&W) -> u64 + 'static) -> Self {
        self.key = Some(Rc::new(key));
        self
    }

    /// The most cases it runs, 2,000 by default; the serial orders the
    /// serial-order check runs inside a case don't count. Stopped short,
    /// it leaves a `truncated:` item, and each depth it never began an
    /// `unexplored:` one.
    pub fn cases(mut self, n: u32) -> Self {
        self.cases = n;
        self
    }

    /// Writes the report so far to `path` every `every` cases, as
    /// [`Scan::saves`](super::Scan::saves) does: an enumeration that finds
    /// the file replays the cases it counts, to rebuild its states, and
    /// reports only the cases after. It resumes only the same ops, first
    /// ops and faults (else it starts over, saying so), with any depth or
    /// [`cases`](Self::cases): cases run depth by depth in one order, so a
    /// deeper run reports only the cases past the file's. The
    /// [`key`](Self::key) closure can't be fingerprinted: a file saved
    /// under another key resumes as if it were this one's.
    pub fn saves(mut self, path: impl Into<std::path::PathBuf>, every: usize) -> Self {
        self.saves = Some(Saves::new(path.into(), every));
        self
    }

    /// Runs it, depth by depth.
    pub fn run(self) -> Audit<O> {
        super::big_stack(|| gas::with_fixture(self.harness.fixture.as_ref(), || self.run_here()))
    }

    fn run_here(mut self) -> Audit<O> {
        let (n, d) = (self.alphabet.len(), self.depth);
        // A first step from other ops says so wherever the run is named.
        let from = (self.first.as_ref())
            .map_or(String::new(), |f| format!(", first step from {} op(s)", f.len()));
        // A saves file replays by count, so it resumes only these ops,
        // first ops and faults; depth and the budget only cut the order.
        if let Some(s) = &mut self.saves {
            s.scope("enumeration", &(&self.alphabet, &self.first, &self.faults));
        }
        let firsts = self.first.as_ref().unwrap_or(&self.alphabet);
        let what = format!("enumeration of {n} op(s) to depth {d}{from}");
        let mut audit = Audit::new(what.clone());
        if n == 0 || firsts.is_empty() || d == 0 || self.cases == 0 {
            audit.r.errors.push(format!("{what}, {} case(s): it tries nothing", self.cases));
            return audit;
        }
        let key: KeyFn<W> = match &self.key {
            Some(k) => k.clone(),
            None => {
                let audits = self.harness.audits.clone();
                Rc::new(move |w: &W| default_key(w.as_ref(), audits.as_deref()))
            }
        };
        // The end checks' verdicts, by key, shared by the cases.
        let mut h = self.harness.clone();
        h.memo = Some(Rc::new(super::check::Memo::new(key.clone())));
        let h = &h;
        let resumed = Saves::resume(&self.saves, h, &mut audit);
        let _guard =
            progress::start(what.clone(), "case", self.cases.min(1), " for the depth under way");
        let epochs = (1..=SHORT_EPOCH).contains(&h.fixture.as_ref().config.epoch_length);
        let fixture_key = key(&h.fixture);
        let mut seen: BTreeSet<u64> = BTreeSet::from([fixture_key]);
        // The runs of ops in flight together tried, by their base.
        let mut runs_seen: BTreeSet<u64> = BTreeSet::new();
        let root = Node {
            plan: Plan::new(),
            remaining: 0,
            settle: true,
            base: fixture_key,
            rest: fixture_key,
        };
        let mut frontier: Vec<Node<O>> = vec![root];
        let (mut ran, mut merged) = (0u32, 0usize);
        let mut flights: BTreeMap<String, BTreeSet<u64>> = BTreeMap::new();
        let (mut unplaced, mut late) = (0usize, 0usize);
        let mut cut = None;
        for depth in 1..=d {
            if frontier.is_empty() {
                break;
            }
            let mut queue: Vec<Case<O>> = vec![];
            let ops = if depth == 1 { firsts } else { &self.alphabet };
            for node in &frontier {
                for op in ops {
                    for gap in gaps(node) {
                        // A new run of ops in flight starts from this state.
                        let base =
                            if matches!(gap, Gap::Blocks(_)) { node.base } else { node.rest };
                        queue.push((extend(&node.plan, gap, op.clone()), Kind::Plain, base));
                    }
                }
            }
            let mut done: Vec<Done<O>> = vec![];
            let mut k = 0;
            while k < queue.len() {
                if ran >= self.cases {
                    cut = Some((depth, queue.len() - k, frontier.len()));
                    break;
                }
                let (plan, kind, base) =
                    std::mem::replace(&mut queue[k], (Plan::new(), Kind::Edge, 0));
                k += 1;
                let run = h.run_case(&plan);
                ran += 1;
                // The time left is the known cases' (this depth's queue),
                // not the cap's: later depths aren't known yet.
                let known = u32::try_from(queue[k..].len()).unwrap_or(u32::MAX);
                progress::total(self.cases.min(ran.saturating_add(known)));
                progress::step(ran);
                // Run again on the harness without the memo, so an end
                // check that isn't deterministic shows.
                let again = (ran == 1 && run.verdict != Verdict::HarnessError)
                    .then(|| run.differs_from(&self.harness.run_case(&plan)))
                    .flatten();
                if let Some(what) = again {
                    audit.r.errors.push(format!(
                        "the first case ran twice with different results ({what}): the world's \
                         Clone shares state with the fixture, or an op or a check is \
                         nondeterministic"
                    ));
                    return audit;
                }
                if run.on_fixture {
                    audit.r.cases += 1;
                    audit.r.errors.push(run.reason.clone().unwrap_or_default());
                    return audit;
                }
                // Replaying what the saves hold adds and writes nothing:
                // they already count past it.
                let new = ran as usize > resumed;
                let last = plan.steps.len() - 1;
                // A leaf is taken in, and goes no further.
                if let Kind::Leaf(at, axis) = &kind {
                    if new {
                        take_in(&mut audit, &run, Some((at, axis)), last, "the case");
                        Saves::tick(&self.saves, h, &audit, ran as usize, false);
                    }
                    continue;
                }
                if new {
                    audit.add(&run, None);
                    if let Kind::Fault(m) = &kind
                        && let Some(why) = m.differs(&run)
                    {
                        audit.r.errors.push(why);
                    }
                }
                let f = step_flight(&run, last);
                if let Some(op) = run.ops.get(last).filter(|o| o.skipped.is_none()) {
                    flights.entry(op.name.clone()).or_default().insert(f);
                }
                let span = group_span(&run, &plan);
                if matches!(kind, Kind::Edge) && !placed(&run, group_start(&plan)) {
                    late += 1;
                }
                // Its variants: a fault at each receipt it fits, or an
                // epoch boundary inside its flight.
                if matches!(kind, Kind::Plain)
                    && run.ops.get(last).is_some_and(|o| o.skipped.is_none())
                {
                    for (inject, meant) in fault_injects(&self.faults, &run, last) {
                        let mut p = plan.clone();
                        p.steps[last].inject.push(inject);
                        if super::run::misnamed(h, &p).is_empty() {
                            queue.push((p, Kind::Fault(meant), base));
                        }
                    }
                    if epochs {
                        let length = h.fixture.as_ref().config.epoch_length;
                        let (plans, dropped) = epoch_variants(&plan, span, length);
                        unplaced += dropped;
                        queue.extend(plans.into_iter().map(|p| (p, Kind::Edge, base)));
                    }
                    // A step placed at rest, at each of its edges.
                    if self.faults.tries_edges() && group_start(&plan) == last {
                        for e in step_edges(h, &run, last) {
                            queue.push((e.plan, Kind::Leaf(e.at, e.axis), base));
                        }
                    }
                }
                if new {
                    Saves::tick(&self.saves, h, &audit, ran as usize, false);
                }
                done.push((plan, run, span, base, matches!(kind, Kind::Fault(_))));
            }
            // The forced last: a state a plain case or an edge reaches too
            // is extended from that one.
            done.sort_by_key(|d| d.4);
            let mut next = vec![];
            for (plan, run, span, base, _) in done {
                let whole = !run.budget && !run.timed_out && run.verdict != Verdict::HarnessError;
                let Some(world) = run.world::<W>().filter(|_| whole) else { continue };
                let lead = group_lead(&plan, plan.steps.len() - 1);
                let remaining = span.saturating_sub(lead);
                let rest = key(world);
                let fresh = seen.insert(rest);
                // The ops in flight together, from the state before them:
                // what an op placed inside their flight meets.
                let flight_new = remaining > 0 && runs_seen.insert(run_key(base, &plan));
                if fresh || flight_new {
                    next.push(Node { plan, remaining, settle: fresh, base, rest });
                } else {
                    merged += 1;
                }
            }
            if cut.is_some() {
                frontier = next;
                break;
            }
            frontier = next;
        }
        if let Some((depth, left, states)) = cut {
            audit.unmoved.push(Todo::new(
                format!("truncated:depth {depth}"),
                format!(
                    "the {} case budget ran out at depth {depth}, {left} case(s) of it not run \
                     (from {states} state(s))",
                    self.cases
                ),
                "Enumerate::cases with more, or a smaller alphabet or depth",
            ));
            let start = depth + 1;
            for depth in start..=d {
                let k = if depth == start { frontier.len() } else { 0 };
                audit.unmoved.push(Todo::new(
                    format!("unexplored:depth {depth}"),
                    format!("depth {depth} never began; {k} state(s) at depth {} left", depth - 1),
                    "Enumerate::cases with more",
                ));
            }
        }
        if unplaced > 0 {
            audit.unmoved.push(Todo::new(
                "unexplored:epoch boundaries".to_owned(),
                format!(
                    "{unplaced} epoch boundary offset(s) not tried: an epoch long or more, or past \
                     127 blocks, into a flight"
                ),
                "a plan with Gap::EpochEdge by hand",
            ));
        }
        if late > 0 {
            audit.unmoved.push(Todo::new(
                "unexplored:epoch boundaries placed late".to_owned(),
                format!(
                    "{late} case(s) with an epoch boundary submitted their ops while the ops \
                     before were still in flight: they ran, as another interleaving, and the \
                     boundary meant there was not tried"
                ),
                "a plan with Gap::EpochEdge by hand after the ops before settle",
            ));
        }
        let flights: Vec<String> = flights
            .iter()
            .map(|(op, f)| {
                let f: Vec<String> = f.iter().map(u64::to_string).collect();
                format!("{op} {}", f.join("/"))
            })
            .collect();
        let key_note = if self.key.is_some() {
            "by Enumerate::key".to_owned()
        } else {
            format!("by the default key, which leaves out {LEFT_OUT}")
        };
        let key_note = format!(
            "{key_note}: states equal under it were explored once, and a bug reachable only \
             through what it leaves out is missed"
        );
        audit.r.what = format!("enumeration of {ran} case(s) to depth {d}{from}");
        let first = self.first.iter().map(|f| {
            let ops: Vec<String> =
                f.iter().map(|o| super::input::compact(&format!("{o:?}"))).collect();
            let ops = ops.join(", ");
            format!("first step from {} op(s): {ops}; later steps from the alphabet's {n}", f.len())
        });
        audit.r.notes = first
            .chain([
                format!("flights (blocks), per op: {}", flights.join(", ")),
                format!("{merged} state(s) merged {key_note}"),
                if self.faults.tries_edges() { NOT_VARIED_EDGES } else { NOT_VARIED }.to_owned(),
                boundary(&self.faults, epochs),
            ])
            .collect();
        Saves::tick(&self.saves, h, &audit, (ran as usize).max(resumed), true);
        h.save(&mut audit);
        audit
    }
}

/// The gaps an op appended after `node`'s plan goes in at: each block
/// offset inside the flight of the ops in flight with its last step, and
/// after they settle unless its state was seen. The first step has one.
fn gaps<O>(node: &Node<O>) -> Vec<Gap> {
    if node.plan.steps.is_empty() {
        return vec![Gap::Settle];
    }
    let inside = (0..node.remaining.min(u64::from(u16::MAX))).map(|b| Gap::Blocks(b as u16));
    inside.chain(node.settle.then_some(Gap::Settle)).collect()
}

/// `plan` with `op` appended after `gap`.
fn extend<O: Clone>(plan: &Plan<O>, gap: Gap, op: O) -> Plan<O> {
    let mut p = plan.clone();
    if let Some(last) = p.steps.last_mut() {
        last.then = gap;
    }
    p.op(op)
}

/// Step `i`'s flight in `run`: the blocks from its submission to its last
/// receipt; 0 for a step with no transaction.
pub(crate) fn step_flight<O>(run: &Run<O>, i: usize) -> u64 {
    let outcomes: Vec<_> = run.step_txs(i).collect();
    let sent = outcomes.iter().filter_map(|o| o.transaction.as_ref().map(|t| t.block_height)).min();
    let last = outcomes.iter().flat_map(|o| &o.receipts).map(|r| r.block_height).max();
    match (sent, last) {
        (Some(sent), Some(last)) => last + 1 - sent,
        _ => 0,
    }
}

/// The first step of the ops in flight with the last one: back over the
/// block gaps the enumeration placed inside a flight.
fn group_start<O>(plan: &Plan<O>) -> usize {
    group_start_at(plan, plan.steps.len() - 1)
}

/// The first step of the ops in flight with step `i`.
fn group_start_at<O>(plan: &Plan<O>, i: usize) -> usize {
    let at_rest = plan.steps[..i].iter().rposition(|s| !matches!(s.then, Gap::Blocks(_)));
    at_rest.map_or(0, |r| r + 1)
}

/// Blocks from the submission of the first op in flight with step `i` to
/// step `i`'s.
fn group_lead<O>(plan: &Plan<O>, i: usize) -> u64 {
    let g = group_start_at(plan, i);
    (plan.steps[g..i].iter())
        .map(|s| if let Gap::Blocks(b) = s.then { u64::from(b) } else { 0 })
        .sum()
}

/// The flight of the ops in flight with `plan`'s last step, from the
/// first one's submission: the latest of each one's offset plus flight.
fn group_span<O: Clone>(run: &Run<O>, plan: &Plan<O>) -> u64 {
    let g = group_start(plan);
    (g..plan.steps.len()).map(|i| group_lead(plan, i) + step_flight(run, i)).max().unwrap_or(0)
}

/// Whether step `g` went in after every step before it had run its last
/// receipt (unknown for a step with no transaction: taken as placed).
fn placed<O>(run: &Run<O>, g: usize) -> bool {
    let sent = run.step_txs(g).filter_map(|o| o.transaction.as_ref().map(|t| t.block_height)).min();
    let before = (0..g).flat_map(|i| run.step_txs(i)).flat_map(|o| &o.receipts);
    match (sent, before.map(|r| r.block_height).max()) {
        (Some(sent), Some(last)) => last < sent,
        _ => true,
    }
}

/// The dedup key of a run of ops in flight together: the key of the
/// world before them, and the steps, the gap before them included.
fn run_key<O: Clone + serde::Serialize>(base: u64, plan: &Plan<O>) -> u64 {
    let g = group_start(plan);
    let before = if g == 0 { plan.start } else { Some(plan.steps[g - 1].then) };
    let steps = serde_json::to_vec(&(before, &plan.steps[g..])).unwrap_or_default();
    let h = CryptoHash::hash_bytes(&[&base.to_le_bytes()[..], &steps].concat());
    u64::from_le_bytes(h.as_ref()[..8].try_into().expect("8 bytes"))
}

/// `plan` with an epoch boundary at each block of `span`, the flight of
/// the ops in flight with its last step: a [`Gap::EpochEdge`] before the
/// first of them, unless that gap is set already; and how many offsets
/// were left out, an epoch long or more or past 127 blocks.
fn epoch_variants<O: Clone>(plan: &Plan<O>, span: u64, length: u64) -> (Vec<Plan<O>>, usize) {
    let g = group_start(plan);
    let before = if g == 0 { plan.start } else { Some(plan.steps[g - 1].then) };
    if before.is_some_and(|gap| gap != Gap::Settle) {
        return (vec![], 0);
    }
    let fits = |k: u64| k < length && i8::try_from(k).is_ok();
    let plans = (0..=span)
        .filter(|&k| fits(k))
        .map(|k| {
            let mut p = plan.clone();
            let edge = Gap::EpochEdge(-(k as i8));
            match g {
                0 => p.start = Some(edge),
                _ => p.steps[g - 1].then = edge,
            }
            p
        })
        .collect();
    (plans, (0..=span).filter(|&k| !fits(k)).count())
}

/// The default dedup key of `sim` at rest: each audited contract's state,
/// balance and locked balance, the epoch height and the config.
pub(crate) fn default_key(sim: &Sim, audits: Option<&[String]>) -> u64 {
    let mut bytes: Vec<u8> = vec![];
    for (account, _) in sim.contracts() {
        if audits.is_some_and(|a| !a.contains(&account)) {
            continue;
        }
        let Some(a) = sim.account(account.as_str()) else { continue };
        bytes.extend(account.as_bytes());
        bytes.extend(a.amount.as_yoctonear().to_le_bytes());
        bytes.extend(a.locked.as_yoctonear().to_le_bytes());
        for (k, v) in sim.view_state(account.as_str(), b"") {
            bytes.extend((k.len() as u64).to_le_bytes());
            bytes.extend(k);
            bytes.extend((v.len() as u64).to_le_bytes());
            bytes.extend(v);
        }
    }
    bytes.extend(sim.epoch_height().to_le_bytes());
    bytes.extend(format!("{:?}", sim.config).as_bytes());
    let h = CryptoHash::hash_bytes(&bytes);
    u64::from_le_bytes(h.as_ref()[..8].try_into().expect("8 bytes"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Balance;

    /// a, 1 block, b; settled; c, 2 blocks, d.
    fn plan() -> Plan<u8> {
        Plan::new().op(1).blocks(1).op(2).op(3).blocks(2).op(4)
    }

    /// A group of ops in flight together starts after the last step that
    /// settled; its lead is the blocks from its first submission.
    #[test]
    fn a_group_starts_after_the_last_settled_step() {
        let p = plan();
        let starts: Vec<usize> = (0..4).map(|i| group_start_at(&p, i)).collect();
        assert_eq!(starts, [0, 0, 2, 2]);
        assert_eq!(group_start(&p), 2);
        assert_eq!((group_lead(&p, 1), group_lead(&p, 2), group_lead(&p, 3)), (1, 0, 2));
    }

    /// A run's key reads its base, the gap before its group and the
    /// group's steps, not the steps before.
    #[test]
    fn a_run_key_reads_the_gap_before_its_group() {
        let settled = Plan::new().op(1u8).op(2);
        let edge = Plan::new().op(1u8).then(Gap::EpochEdge(0)).op(2);
        assert_ne!(run_key(7, &settled), run_key(7, &edge));
        assert_eq!(run_key(7, &settled), run_key(7, &Plan::new().op(9u8).op(2)));
        assert_ne!(run_key(7, &settled), run_key(8, &settled));
    }

    /// One boundary at each block of the span, before the group: in the
    /// gap before it, or before the plan; offsets an epoch long left out;
    /// none where that gap is set already.
    #[test]
    fn epoch_variants_go_before_the_group() {
        let (plans, dropped) = epoch_variants(&Plan::new().op(1u8).op(2), 3, 3);
        let gaps: Vec<Gap> = plans.iter().map(|v| v.steps[0].then).collect();
        assert_eq!(gaps, [Gap::EpochEdge(0), Gap::EpochEdge(-1), Gap::EpochEdge(-2)]);
        assert!(plans.iter().all(|v| v.start.is_none() && v.steps[1].then == Gap::Settle));
        assert_eq!(dropped, 1);
        let (plans, dropped) = epoch_variants(&Plan::new().op(1u8), 0, 10);
        assert_eq!((plans[0].start, dropped), (Some(Gap::EpochEdge(0)), 0));
        let set = Plan::new().op(1u8).then(Gap::EpochEdge(0)).op(2);
        let (plans, dropped) = epoch_variants(&set, 3, 10);
        assert!(plans.is_empty() && dropped == 0);
    }

    /// The boundary names the hold lengths its faults try, each kind's
    /// where they differ, and no hold under faults that try none; a
    /// reordering holds for no count of blocks. Only holds and
    /// reorderings are called forced. Epoch boundaries are named only on
    /// a fixture it places them in.
    #[test]
    fn the_boundary_names_the_holds_tried() {
        use crate::Receipts;
        let says = |f: &Faults, what: &str, forced: bool| {
            let b = boundary(f, true);
            assert!(b.contains(what) && b.contains("are forced") == forced, "{b}");
            // The summary cuts a note past 400 characters.
            assert!(b.len() <= 400, "{} characters: {b}", b.len());
        };
        let mixed = ", holds over 3 block(s) and two faults on one step were not tried;";
        says(&Faults::tolerable(), mixed, true);
        let sparse = Faults::new(Receipts::all(), [Fault::HoldResult(5), Fault::HoldResult(2)]);
        says(&sparse, ", receipt holds, result holds other than 2/5 block(s) and", true);
        says(&Faults::none(), "Offsets outside the flight and holds were not tried;", false);
        let starve = Faults::new(Receipts::all(), [Fault::OutOfGas]);
        says(&starve, ", holds and two faults on one step were not tried;", false);
        let count = ", holds for a count of blocks and two faults on one step";
        says(&Faults::reorder(), count, true);
        says(&Faults::new(Receipts::all(), [Fault::HoldUntil(Receipts::all())]), count, true);
        let both = "holds over 3 block(s) (reorderings aside) and two faults";
        says(&Faults::default().and(Faults::reorder()), both, true);
        let receipts = Faults::new(Receipts::all(), [1, 2, 3].map(Fault::Hold));
        let one = ", receipt holds over 3 block(s), result holds (reorderings aside) and";
        says(&receipts.and(Faults::reorder()), one, true);
        let epochs = "an epoch boundary goes one to each run of ops in flight together";
        assert!(boundary(&Faults::none(), true).contains(epochs));
        let long = boundary(&Faults::none(), false);
        assert!(long.contains("; no epoch boundary was tried (epochs over 100 blocks)."), "{long}");
    }

    /// The default key with audits reads only the audited contracts.
    #[test]
    fn the_default_key_reads_only_the_audited_contracts() {
        let mut sim = Sim::new();
        sim.create_account("alice.near", Balance::from_near(100));
        let wasm = wat::parse_str(include_str!("../../tests/res/flight_lock.wat")).unwrap();
        for c in ["a.near", "b.near"] {
            sim.create_account(c, Balance::from_near(10));
            sim.deploy(c, wasm.clone());
        }
        let a = ["a.near".to_owned()];
        let before = sim.clone();
        sim.call("alice.near", "b.near", "work").args(vec![0]).transact().assert_failure();
        assert_eq!(default_key(&before, Some(&a)), default_key(&sim, Some(&a)));
        assert_ne!(default_key(&before, None), default_key(&sim, None));
        sim.call("alice.near", "a.near", "work").args(vec![0]).transact().assert_failure();
        assert_ne!(default_key(&before, Some(&a)), default_key(&sim, Some(&a)));
    }
}
