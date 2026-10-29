//! [`Search`]: generated cases, every distinct finding, each shrunk on its
//! own. Proptest generates the plans and proposes simpler ones; this loop
//! decides what counts, so one finding never hides another.
//!
//! Unlike proptest's stock runner it keeps going after a finding, keeps
//! each finding ([identity](super::run::Id)) and its replay, and shrinks
//! each only through cases still showing it, so one finding can neither
//! hide nor replace another. One that turns up while another shrinks is
//! kept too. Findings of one cause share a row (`audit::fold_causes`).

use super::audit::{Audit, Label, Shrink, label};
use super::run::{Id, Run, Verdict, id};
use super::store::Saves;
use super::{Harness, Op, Plan, World, gas, plan, progress, shrink};
use crate::findings::guarded;
use proptest::strategy::{BoxedStrategy, Strategy, ValueTree};
use proptest::test_runner::{Config, RngAlgorithm, RngSeed, TestRng, TestRunner};
use std::collections::HashMap;
use std::time::{Duration, Instant};

/// What a plan strategy's `new_tree` gives.
type Tree<O> = Box<dyn ValueTree<Value = Plan<O>>>;

/// Per op and list field, the lengths the cases sent.
type Lengths = std::collections::BTreeMap<(String, String), std::collections::BTreeSet<u128>>;

/// Cases per seed unless told: proptest's default.
const CASES: u32 = 256;
/// The least wall time each shown finding's shrink gets.
const SHRINK_TIME: Duration = Duration::from_secs(60);
/// The most wall time a seed's shrinking takes in all.
const SHRINKS: Duration = Duration::from_secs(15 * 60);
/// Plans a shrink meets again, judged already, before it stops.
const REPEATS: u32 = 256;
/// A shown row's shrink runs per value in its plan: a u128's binary
/// search, so its time share, not a run count, cuts a long shrink short.
/// A forced row's runs 2 per value: its triage matters less, and forced
/// rows are most of a search's shrinking.
const RUNS_PER_VALUE: [u32; 2] = [128, 2];

/// A search, built by [`Harness::search`]; [`run`](Self::run) runs it.
///
/// The environment overrides the code: `NEAR_SIM_SEED` (decimal, or hex
/// with `0x`) sets the first seed and `NEAR_SIM_CASES` the cases per seed,
/// so a search reruns from the seed its audit printed, and a nightly run
/// can search longer.
#[must_use]
pub struct Search<'h, O, W> {
    harness: &'h Harness<O, W>,
    plans: BoxedStrategy<Plan<O>>,
    cases: u32,
    seed: Option<u64>,
    seeds: u32,
    saves: Option<Saves>,
}

impl<O: Op<W>, W: World> Harness<O, W> {
    /// A search over the plans `plans` generates, usually
    /// [`plans`](super::plans)`(ops)`: 256 cases per seed, one seed unless
    /// [`seeds`](Search::seeds) says more. Each finding's first case is kept
    /// as [`Found::original`](super::Found::original).
    pub fn search(&self, plans: impl Strategy<Value = Plan<O>> + 'static) -> Search<'_, O, W> {
        let plans = plans.boxed();
        Search { harness: self, plans, cases: CASES, seed: None, seeds: 1, saves: None }
    }
}

impl<O: Op<W>, W: World> Search<'_, O, W> {
    /// Cases per seed; 256 by default. A search of none is an error: it
    /// tries nothing.
    pub fn cases(mut self, n: u32) -> Self {
        self.cases = n;
        self
    }

    /// The first seed; a fresh one by default, recorded in the audit.
    pub fn seed(mut self, seed: u64) -> Self {
        self.seed = Some(seed);
        self
    }

    /// Searches `n` seeds, each its cases: one seed can miss what two
    /// find, so 2 is the recommended campaign.
    pub fn seeds(mut self, n: u32) -> Self {
        self.seeds = n.max(1);
        self
    }

    /// Writes the report so far to `path` every `every` cases and after
    /// each seed's shrinking, as [`Scan::saves`](super::Scan::saves) does: a
    /// search that finds the file (of this fixture) resumes after the
    /// cases it counts, with the same seeds, and its report includes the
    /// saved one.
    pub fn saves(mut self, path: impl Into<std::path::PathBuf>, every: usize) -> Self {
        self.saves = Some(Saves::new(path.into(), every));
        self
    }

    /// Runs the search: each seed's cases, then each new finding shrunk.
    /// A seed's shrinking takes as long as its cases did, or a minute per
    /// shown finding if that is longer, at most 15 minutes: shown findings
    /// first, each its share of what is left and at least a minute, then
    /// forced ones share the rest; a finding not fully shrunk says so. Its
    /// replays are saved ([`Harness::store`](super::Harness::store)).
    pub fn run(self) -> Audit<O> {
        // The strategy is dropped on the big stack too.
        super::big_stack(move || self.run_here())
    }

    fn run_here(self) -> Audit<O> {
        let h = self.harness;
        let mut audit = Audit::new(String::new());
        let (cases, seed) = match overrides(self.cases, self.seed) {
            Ok((0, _)) => Err("a search of 0 cases tries nothing".to_owned()),
            other => other,
        }
        .unwrap_or_else(|why| {
            audit.r.errors.push(why);
            (0, None)
        });
        audit.r.what = format!("search of {cases} case(s) per seed");
        if cases == 0 {
            return audit;
        }
        let first =
            seed.unwrap_or_else(|| draw(&proptest::num::u64::ANY, &mut TestRunner::default()));
        let resumed = Saves::resume(&self.saves, h, &mut audit);
        let fixture = h.fixture.as_ref();
        gas::with_fixture(fixture, || {
            plan::with_epoch_length(fixture.config.epoch_length, || {
                let then = &*format!(", each seed's findings shrinking after it ({})", {
                    progress::clock(SHRINKS)
                });
                let _progress =
                    progress::start(audit.r.what.clone(), "case", cases * self.seeds, then);
                let mut lengths = Lengths::new();
                for k in 0..self.seeds {
                    // Seeds far apart in the stream, the first as given.
                    let seed = first.wrapping_add(u64::from(k).wrapping_mul(0x9E37_79B9_7F4A_7C15));
                    audit.r.seeds.push(seed);
                    let (at, started) = (k * cases, Instant::now());
                    let Some(new) =
                        self.explore(&mut audit, seed, (cases, at, resumed), &mut lengths)
                    else {
                        break;
                    };
                    let done = (at + cases).max(resumed as u32) as usize;
                    self.shrink_all(&mut audit, new, (k, started.elapsed()));
                    Saves::tick(&self.saves, h, &audit, done, true);
                }
                // A list every case sent at one length: a to-do naming it.
                for ((act, list), n) in lengths.iter().filter(|(_, n)| n.len() == 1) {
                    audit.unmoved.push(super::Todo {
                        id: format!("length:{act}:{list}"),
                        why: format!(
                            "every case's {act} sent `{list}` with {} item(s): a bug that needs \
                             another length went untried",
                            n.first().copied().unwrap_or_default()
                        ),
                        close: "a strategy that draws other lengths, or curves along it".into(),
                        stop: None,
                    });
                }
                h.save(&mut audit);
            })
        });
        audit
    }

    /// Generates and runs `cases` cases from `seed`, the first numbered
    /// `done`, those before `resumed` drawn but not run: the new findings'
    /// rows with the case seed that drew each, or `None` if the strategy
    /// or the world is wrong. The first case runs twice, and must run the
    /// same both times.
    fn explore(
        &self,
        audit: &mut Audit<O>,
        seed: u64,
        (cases, done, resumed): (u32, u32, usize),
        lengths: &mut Lengths,
    ) -> Option<Vec<(usize, [u8; 32])>> {
        let h = self.harness;
        let mut master = TestRunner::new(Config { rng_seed: RngSeed::Fixed(seed), ..config() });
        let mut next = draw_seed(&mut master);
        let mut new = vec![];
        for case in 0..cases {
            progress::step(done + case);
            let case_seed = next;
            let (_, plan, mut runner) = match self.generate(&case_seed) {
                Ok(x) => x,
                Err(why) => return error(audit, format!("case {case}: {why}")),
            };
            next = draw_seed(&mut runner);
            let n = (done + case) as usize;
            if n < resumed {
                continue;
            }
            for act in plan.steps.iter().map(|s| &s.act).filter(|a| a.op().is_some()) {
                let lists = super::axes::arg_axes::<O, W>(act).into_iter();
                for a in lists.filter(|a| a.kind == super::AxisKind::Len) {
                    let key = (act.name(), a.name);
                    lengths.entry(key).or_default().insert(a.at);
                }
            }
            let run = h.run_case(&plan);
            let twice = (case == 0 && run.verdict != Verdict::HarnessError)
                .then(|| run.differs_from(&h.run_case(&plan)))
                .flatten();
            if let Some(what) = twice {
                return error(
                    audit,
                    format!(
                        "case 0 ran twice with different results ({what}): the world's Clone \
                         shares state with the fixture, or an op or a check is nondeterministic"
                    ),
                );
            }
            // A harness error (an op's panic on one input) is that case's,
            // as in a scan: recorded, and the search goes on.
            // A check failing on the fixture: every later case repeats it.
            if run.on_fixture {
                audit.r.cases += 1;
                return error(audit, format!("case {n}: {}", run.reason.unwrap_or_default()));
            }
            if run.verdict == Verdict::HarnessError {
                audit.r.cases += 1;
                // Drawn, if not placed: the run may have erred first.
                audit.r.faults.0 += plan.steps.iter().map(|s| s.inject.len() as u32).sum::<u32>();
                audit.r.errors.push(format!("case {n}: {}", run.reason.unwrap_or_default()));
                Saves::tick(&self.saves, h, audit, n + 1, false);
                continue;
            }
            let before = audit.found.len();
            audit.add(&run, None);
            new.extend((before..audit.found.len()).map(|i| (i, case_seed)));
            Saves::tick(&self.saves, h, audit, n + 1, false);
        }
        // The last case is done too: the progress count ends where the
        // summary's does.
        progress::step(done + cases);
        Some(new)
    }

    /// Shrinks the rows `new` names: shown ones first, each its share of
    /// what is left and at least [`SHRINK_TIME`], then forced ones and
    /// whatever turned up while they shrank (by the plan reducer) share the
    /// rest. The budget is seed `k`'s case time `took`, or [`SHRINK_TIME`]
    /// per shown row if that is longer, at most [`SHRINKS`]. A candidate
    /// still running at its row's deadline is cut there. Replays are
    /// written when the search finishes; a saves file holds the rows
    /// before.
    fn shrink_all(
        &self,
        audit: &mut Audit<O>,
        mut new: Vec<(usize, [u8; 32])>,
        (k, took): (u32, Duration),
    ) {
        if new.is_empty() {
            return;
        }
        new.sort_by_key(|(i, _)| !audit.found[*i].id.faults.is_empty());
        let shown = new.iter().filter(|(i, _)| audit.found[*i].id.faults.is_empty()).count();
        let budget = shrink_budget(took, shown);
        let (from, end) = (audit.found.len(), Instant::now() + budget);
        let out = |i: usize, audit: &mut Audit<O>| {
            let why = format!("the search's shrink budget ({budget:.0?}) ran out first");
            audit.found[i].shrink = Shrink::Cut(0, why);
        };
        let deadline = |rows: usize, least: Duration| {
            let now = Instant::now();
            now + share(end.saturating_duration_since(now), rows, least)
        };
        let n = new.len();
        let what = format!("shrinking the findings of seed {} of {}", k + 1, self.seeds);
        let phase = progress::phase(what, n as u32, Some(end));
        for (k, (i, case_seed)) in new.into_iter().enumerate() {
            phase.step(k as u32, (n + audit.found.len() - from) as u32);
            if Instant::now() >= end {
                out(i, audit);
                continue;
            }
            let until = if k < shown {
                deadline(shown - k, SHRINK_TIME)
            } else {
                deadline(n - k, Duration::ZERO)
            };
            self.shrink(audit, i, &case_seed, until);
        }
        for i in from..audit.found.len() {
            phase.step((n + i - from) as u32, (n + audit.found.len() - from) as u32);
            if Instant::now() >= end {
                out(i, audit);
                continue;
            }
            let until = deadline(audit.found.len() - i, Duration::ZERO);
            let target = audit.found[i].id.clone();
            let original = audit.found[i].run.clone();
            let mut run = |p: &Plan<O>| self.candidate(audit, p, until);
            let shows = |r: &Run<O>| shows(r, &target, &original);
            let (best, status) = shrink::reduce(original.clone(), &mut run, &shows, until);
            settle(audit, i, best, status);
        }
    }

    /// Shrinks row `i` through its seed's tree: proptest's loop (simplify
    /// after an accepted candidate, complicate after a rejected one) with
    /// this acceptance: the same finding, no block budget running out
    /// unless the original's did, and a forced fault at the same receipt
    /// or none, so a forced finding can become a shown one, never the
    /// reverse, and never another receipt's.
    fn shrink(&self, audit: &mut Audit<O>, i: usize, case_seed: &[u8; 32], until: Instant) {
        let original = audit.found[i].run.clone();
        let target = audit.found[i].id.clone();
        let Ok((mut tree, generated, _)) = self.generate(case_seed) else {
            let why = "its seed no longer generates a plan".into();
            return settle(audit, i, original, Shrink::Cut(0, why));
        };
        let forced = matches!(audit.found[i].label, Label::Forced(_));
        let most = run_limit(values(&serde_json::to_value(&generated).unwrap_or_default()), forced);
        let key = |plan: &Plan<O>| serde_json::to_string(plan).unwrap_or_default();
        let mut judged = HashMap::from([(key(&generated), true)]);
        let (started, mut best, mut runs, mut repeats) = (Instant::now(), original.clone(), 0, 0);
        let status = 'shrink: {
            if !guarded(|| tree.simplify()).unwrap_or(false) {
                break 'shrink Shrink::Done(0);
            }
            loop {
                let Ok(plan) = guarded(|| tree.current()) else { break 'shrink Shrink::Done(runs) };
                let accept = match judged.get(&key(&plan)) {
                    Some(&accept) => {
                        repeats += 1;
                        if repeats > REPEATS {
                            break 'shrink Shrink::Cut(
                                runs,
                                "the tree kept proposing judged plans".into(),
                            );
                        }
                        accept
                    }
                    None if runs >= most => {
                        break 'shrink Shrink::Cut(runs, "its run limit".into());
                    }
                    None if Instant::now() > until => {
                        let share = until.saturating_duration_since(started);
                        break 'shrink Shrink::Cut(
                            runs,
                            format!("its share of the shrink budget ({share:.1?})"),
                        );
                    }
                    None => {
                        runs += 1;
                        let run = self.candidate(audit, &plan, until);
                        let accept = shows(&run, &target, &original);
                        judged.insert(key(&plan), accept);
                        if accept {
                            best = run;
                        }
                        accept
                    }
                };
                let more = guarded(|| if accept { tree.simplify() } else { tree.complicate() });
                if !more.unwrap_or(false) {
                    break 'shrink Shrink::Done(runs);
                }
            }
        };
        // Then steps that don't change the finding go, by the plan reducer.
        let shows = |r: &Run<O>| shows(r, &target, &original);
        let mut run = |p: &Plan<O>| self.candidate(audit, p, until);
        let (best, more) = shrink::reduce(best, &mut run, &shows, until);
        let status = match (status, more) {
            (Shrink::Done(a), Shrink::Done(b)) => Shrink::Done(a + b),
            (Shrink::Done(a), Shrink::Cut(b, why)) => Shrink::Cut(a + b, why),
            (cut, _) => cut,
        };
        settle(audit, i, best, status);
    }

    /// One shrink candidate, run, cut at its row's deadline; what it shows
    /// that is new is kept, unless it was cut.
    fn candidate(&self, audit: &mut Audit<O>, plan: &Plan<O>, until: Instant) -> Run<O> {
        let run = self.harness.run_until(plan, Some(until));
        if run.verdict != Verdict::HarnessError && !run.timed_out {
            audit.note(&run, None, true);
        }
        run
    }

    /// The tree a case seed generates, its plan and its runner; an error
    /// if the strategy panicked or gave up.
    fn generate(&self, seed: &[u8; 32]) -> Result<(Tree<O>, Plan<O>, TestRunner), String> {
        let mut runner =
            TestRunner::new_with_rng(config(), TestRng::from_seed(RngAlgorithm::ChaCha, seed));
        let tree = guarded(|| self.plans.new_tree(&mut runner))
            .map_err(|p| format!("the plan strategy {p}"))?
            .map_err(|why| format!("the plan strategy generated no plan: {why}"))?;
        let plan = guarded(|| tree.current()).map_err(|p| format!("the plan strategy {p}"))?;
        Ok((tree, plan, runner))
    }
}

/// Whether `run` shows `target`, or, for a forced `target`, its cause
/// with nothing forced, within the original's block budget.
fn shows<O>(run: &Run<O>, target: &Id, original: &Run<O>) -> bool {
    let forced = !target.faults.is_empty();
    run.verdict == Verdict::Findings
        && (!run.budget || original.budget)
        && (!run.timed_out || original.timed_out)
        && run.findings.iter().any(|f| {
            let x = id(run, f);
            x == *target || (forced && x.faults.is_empty() && x.cause() == target.cause())
        })
}

/// Row `i` takes `best` as its case: the finding as `best` shows it.
fn settle<O: Clone>(audit: &mut Audit<O>, i: usize, best: Run<O>, status: Shrink) {
    let row = &mut audit.found[i];
    let natural = Id { faults: vec![], ..row.id.clone() };
    let f = best.findings.iter().find(|f| id(&best, f) == natural && !f.injected);
    if let Some(f) = f.or_else(|| best.findings.iter().find(|f| id(&best, f) == row.id)) {
        (row.finding, row.label, row.id) = (f.clone(), label(&best, f, None), id(&best, f));
    }
    row.run = best;
    row.shrink = status;
}

fn error<T, O>(audit: &mut Audit<O>, why: String) -> Option<T> {
    audit.r.errors.push(why);
    None
}

fn config() -> Config {
    Config { failure_persistence: None, ..Config::default() }
}

/// `NEAR_SIM_CASES` and `NEAR_SIM_SEED` over the code's settings.
fn overrides(cases: u32, seed: Option<u64>) -> Result<(u32, Option<u64>), String> {
    let var = |name: &str| std::env::var(name).ok();
    let cases = match var("NEAR_SIM_CASES").as_deref().map(str::trim) {
        Some(v) => v.parse().map_err(|_| format!("NEAR_SIM_CASES={v:?} is not a case count"))?,
        None => cases,
    };
    let seed = match var("NEAR_SIM_SEED").as_deref().map(str::trim) {
        Some(v) => Some(
            match v.strip_prefix("0x").or_else(|| v.strip_prefix("0X")) {
                Some(hex) => u64::from_str_radix(hex, 16),
                None => v.parse(),
            }
            .map_err(|_| format!("NEAR_SIM_SEED={v:?} is not a u64 seed"))?,
        ),
        None => seed,
    };
    Ok((cases, seed))
}

/// A seed's shrink budget: its cases' time `took`, or [`SHRINK_TIME`] per
/// shown row if that is longer, at most [`SHRINKS`].
fn shrink_budget(took: Duration, shown: usize) -> Duration {
    took.max(SHRINK_TIME * shown as u32).min(SHRINKS)
}

/// One of `rows` rows' share of `rest`, at least `least` (within `rest`).
fn share(rest: Duration, rows: usize, least: Duration) -> Duration {
    (rest / rows.max(1) as u32).max(least).min(rest)
}

/// The most runs a row's shrink takes, `values` in its plan: at least 64.
fn run_limit(values: u32, forced: bool) -> u32 {
    values.saturating_mul(RUNS_PER_VALUE[usize::from(forced)]).max(64)
}

/// The values in a plan's JSON: a shrink's budget is
/// [`RUNS_PER_VALUE`] runs per value.
fn values(v: &serde_json::Value) -> u32 {
    match v {
        serde_json::Value::Array(a) => a.iter().map(values).sum(),
        serde_json::Value::Object(m) => m.values().map(values).sum(),
        _ => 1,
    }
}

/// The next case seed: 32 bytes from the master generator, via proptest's
/// strategies (its RNG trait's name changed in 1.12).
fn draw_seed(runner: &mut TestRunner) -> [u8; 32] {
    std::array::from_fn(|_| draw(&proptest::num::u32::ANY, runner) as u8)
}

fn draw<S: Strategy>(strategy: &S, runner: &mut TestRunner) -> S::Value {
    strategy.new_tree(runner).expect("an integer strategy always generates").current()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A shown row shrinks fully (128 runs a value); a forced one stops
    /// at 2 a value; either gets at least 64.
    #[test]
    fn forced_rows_shrink_on_the_cheap_limit() {
        assert_eq!((run_limit(10, false), run_limit(10, true)), (1280, 64));
        assert_eq!(run_limit(40, true), 80);
    }

    #[test]
    fn shown_rows_get_a_minute_each_within_fifteen() {
        let (s, m) = (Duration::from_secs(1), Duration::from_secs(60));
        // The seed's case time, or a minute per shown row, at most 15.
        assert_eq!(shrink_budget(10 * s, 3), 3 * m);
        assert_eq!(shrink_budget(5 * m, 1), 5 * m);
        assert_eq!(shrink_budget(10 * s, 0), 10 * s);
        assert_eq!(shrink_budget(40 * m, 2), 15 * m);
        assert_eq!(shrink_budget(10 * s, 27), 15 * m);
        // A shown row: its share or a minute; a forced one its share.
        assert_eq!(share(10 * m, 2, m), 5 * m);
        assert_eq!(share(10 * m, 20, m), m);
        assert_eq!(share(10 * m, 20, Duration::ZERO), 30 * s);
        assert_eq!(share(30 * s, 1, m), 30 * s);
    }
}
