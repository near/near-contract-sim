//! [`Harness::diff`]: two harnesses (two builds of a contract, a fix and
//! the bug) run the same plans, and only what differs is reported: the
//! books' readings on the two fixtures, once; then per plan, each contract
//! method's outcome classes, the findings, and how far each reading moved
//! from the plan's start (before step 0) to its end.
//!
//! Each side runs the same executor as [`Harness::run`]. A sampled fault
//! resolves on each side, so it can land on different receipts; a finding's
//! replay (its plan as it ran) pins it.

use super::run::{Run, class, set_up};
use super::{Harness, Op, Plan, World};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

/// What differs between two harnesses on the same plans.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Diff {
    /// The books' readings that differ on the two fixtures, before any
    /// plan: `books key: a -> b`.
    pub fixture: Vec<String>,
    /// Per plan with a difference, in the order given.
    pub plans: Vec<PlanDiff>,
}

/// One plan's differences.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlanDiff {
    /// Its index in the plans given.
    pub plan: usize,
    /// Its steps.
    pub steps: Vec<String>,
    /// Each difference: `what key: a -> b`, `none` where a side has none.
    pub lines: Vec<String>,
}

impl Diff {
    /// Whether nothing differs.
    pub fn is_empty(&self) -> bool {
        self.fixture.is_empty() && self.plans.is_empty()
    }

    /// The differences, the fixture's then each plan's, one per line.
    pub fn lines(&self) -> impl Iterator<Item = &str> {
        let plans = self.plans.iter().flat_map(|p| &p.lines);
        self.fixture.iter().chain(plans).map(String::as_str)
    }
}

impl fmt::Display for Diff {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.is_empty() {
            return writeln!(f, "no difference on these plans");
        }
        if !self.fixture.is_empty() {
            writeln!(f, "the fixtures, before any plan:")?;
        }
        for line in &self.fixture {
            writeln!(f, "  {line}")?;
        }
        for p in &self.plans {
            writeln!(f, "plan {}: {}", p.plan, p.steps.join("; "))?;
            for line in &p.lines {
                writeln!(f, "  {line}")?;
            }
        }
        Ok(())
    }
}

impl<O: Op<W>, W: World> Harness<O, W> {
    /// Runs each of `plans` on this harness and on `other`, and reports
    /// only what differs: a books reading that differs on the fixtures
    /// prints once, and per plan only a difference in how far it moved. A
    /// build diffed against itself reports nothing.
    pub fn diff(&self, other: &Harness<O, W>, plans: &[Plan<O>]) -> Diff {
        let shown = |m: BTreeMap<String, u128>| m.into_iter().map(|(k, v)| (k, v.to_string()));
        let (a, b) = (self.read(&self.fixture), other.read(&other.fixture));
        let mut diff = Diff::default();
        compare("books", shown(a).collect(), shown(b).collect(), &mut diff.fixture);
        for (i, plan) in plans.iter().enumerate() {
            let (a, b) = (self.run_written(plan), other.run_written(plan));
            let mut lines = vec![];
            compare("outcome", outcomes(&a), outcomes(&b), &mut lines);
            compare("finding", findings(&a), findings(&b), &mut lines);
            compare("books", self.books_moved(plan, &a), other.books_moved(plan, &b), &mut lines);
            if !lines.is_empty() {
                let steps = plan.steps.iter().map(|s| s.line_as(None)).collect();
                diff.plans.push(PlanDiff { plan: i, steps, lines });
            }
        }
        diff
    }

    /// The books' readings in `w`: each liability's total and each asset
    /// held.
    fn read(&self, w: &W) -> BTreeMap<String, u128> {
        self.readings.iter().map(|(name, read)| (name.clone(), read(w))).collect()
    }

    /// How far each reading moved in `run` of `plan`: from the plan's
    /// start (the fixture after its setup) to the run's end, signed.
    fn books_moved(&self, plan: &Plan<O>, run: &Run<O>) -> BTreeMap<String, String> {
        let Some(end) = run.world::<W>() else { return BTreeMap::new() };
        let start = self.read(&set_up(&self.fixture, plan));
        let signed =
            |a: u128, b: u128| if b >= a { format!("+{}", b - a) } else { format!("-{}", a - b) };
        let moved = self.read(end).into_iter().map(|(k, v)| {
            let by = signed(start.get(&k).copied().unwrap_or_default(), v);
            (format!("{k} since step 0"), by)
        });
        moved.collect()
    }
}

/// Each method's outcome classes in `run`, `receiver::method`: `ok`, or
/// the failure's class (values made `#`).
fn outcomes<O>(run: &Run<O>) -> BTreeMap<String, String> {
    let mut by_site: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let receipts = run.outcomes.iter().flat_map(|o| &o.receipts);
    for r in receipts.filter(|r| !r.method_names.is_empty()) {
        let site = r.site();
        let outcome = r.failure().map_or_else(|| "ok".into(), |f| class(&f));
        by_site.entry(site).or_default().insert(outcome);
    }
    let joined = |c: BTreeSet<String>| c.into_iter().collect::<Vec<_>>().join(" | ");
    by_site.into_iter().map(|(s, c)| (s, joined(c))).collect()
}

/// `run`'s findings, `check at site`, each with how it showed.
fn findings<O>(run: &Run<O>) -> BTreeMap<String, String> {
    let label = |f: &crate::findings::Finding| if f.injected { "forced" } else { "shown" };
    let key = |f: &crate::findings::Finding| format!("{} at {}", f.check, f.site);
    run.findings.iter().map(|f| (key(f), label(f).to_owned())).collect()
}

/// A line per key whose value differs between `a` and `b`.
fn compare(
    what: &str,
    a: BTreeMap<String, String>,
    b: BTreeMap<String, String>,
    out: &mut Vec<String>,
) {
    let keys: BTreeSet<&String> = a.keys().chain(b.keys()).collect();
    for k in keys {
        let (x, y) = (a.get(k), b.get(k));
        if x != y {
            let side = |v: Option<&String>| v.map_or("none", String::as_str).to_owned();
            out.push(format!("{what} {k}: {} -> {}", side(x), side(y)));
        }
    }
}
