//! [`Audit`]: what a scan, a search, curves or a diff found, one row per cause.

use super::curves::{CurveRow, Resource};
use super::progress::Phase;
use super::run::{Id, Run, Verdict, class, failed_in, id, on_path};
use super::{Harness, Op, World};
use crate::findings::Finding;
use near_primitives::hash::CryptoHash;
use serde::Serialize;
use std::collections::HashMap;
use std::fmt;

/// What a campaign found: each cause once, with how it showed, a run
/// that shows it, and the findings folded into it. A clean audit means
/// no check failed in what was tried, not that none can.
#[must_use = "an audit fails nothing unless asserted: .assert_clean()"]
#[derive(Clone, Debug)]
pub struct Audit<O> {
    /// What it reports as it is: the counts (cases, judgments, faults),
    /// coverage, curve points, errors, seeds, bands and timings. Its
    /// findings, curve rows, to-do and excuses are filled by
    /// [`report`](Self::report) from the typed fields below.
    pub(crate) r: super::Report,
    pub(crate) found: Vec<Found<O>>,
    /// Variants that didn't complete: their family (op, axis) and case, why.
    pub(crate) incomplete: Vec<(String, String)>,
    /// Curves' rows.
    pub(crate) curves: Vec<CurveRow<O>>,
    /// The harness's excuses: the id or glob, and why.
    pub(crate) excused: Vec<(String, String)>,
    /// Replays in the store that can't be judged now (file, why).
    pub(crate) stale: Vec<(String, String)>,
    /// Replays that couldn't be written: the store's directory, and why.
    pub(crate) unsaved: Vec<(String, String)>,
    /// The witnesses' strings that aren't axes: changed, they don't deserialize.
    pub(crate) fixed: std::collections::BTreeSet<String>,
    /// Lists a call receives that its receiver refuses sent directly ([`Todo`](super::Todo)s).
    pub(crate) unmoved: Vec<super::Todo>,
    /// Variants that ran past [`CASE_TIMEOUT`](super::check::CASE_TIMEOUT).
    pub(crate) timeouts: Vec<String>,
    /// A resumed campaign's saved part ([`Scan::saves`](super::Scan::saves)).
    pub(crate) prior: Option<Box<super::Report>>,
    /// This run of the campaign, unique: a union counts it once.
    pub(crate) part: String,
    /// The audited sites a scan sent directly from `axis.near`; `None`
    /// if it sent none (no edges).
    pub(crate) sent: Option<Vec<String>>,
}

/// Adds `j`'s judgments to its check's in `judged`, keeping the first of each why.
pub(crate) fn tally(
    judged: &mut std::collections::BTreeMap<String, super::Judged>,
    j: &super::Judged,
) {
    let blank = || super::Judged { check: j.check.clone(), ..Default::default() };
    let e = judged.entry(j.check.clone()).or_insert_with(blank);
    (e.ok, e.failed, e.unjudged, e.idle) =
        (e.ok + j.ok, e.failed + j.failed, e.unjudged + j.unjudged, e.idle + j.idle);
    e.why = e.why.take().or(j.why.clone());
    e.idle_why = e.idle_why.take().or(j.idle_why.clone());
}

/// One distinct finding: the first case that showed it, how, and in how many.
#[derive(Clone, Debug)]
pub struct Found<O> {
    /// The finding as its first case showed it.
    pub finding: Finding,
    /// How it showed.
    pub label: Label,
    /// The case, shrunk if it was: replay its plan with [`Harness::run`](super::Harness::run).
    pub run: Run<O>,
    /// The case as first found.
    pub original: Run<O>,
    /// How shrinking went.
    pub shrink: Shrink,
    /// Whether it first showed while another finding shrank.
    pub incidental: bool,
    /// The cases that showed it: at least its first, a shrink's candidate
    /// for an [`incidental`](Self::incidental) one.
    pub cases: u32,
    /// Its replay file, if saved ([`Harness::store`](super::Harness::store)).
    pub replay: Option<std::path::PathBuf>,
    /// What folded into it: each forced variant's label (a fault or hold
    /// that shows a finding already shown is not another), and each
    /// finding of another row, `check at site`. A cause of its own check
    /// at its site and op shares its replay; any other its case doesn't
    /// show keeps its own, `check at site (file)`.
    pub also: Vec<String>,
    /// The op after which it broke: `breaking step 2 of 3: Skew`.
    pub broke: Option<String>,
    /// The findings of other checks folded into it. None has a row of its
    /// own in [`Audit::found`]; [`Audit::found_at`] finds this row by any
    /// of them.
    pub folded: Vec<Finding>,
    /// The replays of findings folded into it that its case doesn't show.
    pub(crate) kept: Vec<std::path::PathBuf>,
    /// The identities, in its case, of findings folded into it that its
    /// case shows: its replay holds them too.
    pub(crate) shows: Vec<Id>,
    /// The identities of its case's findings as its replay reads them:
    /// on the run before traps are named.
    pub(crate) ids: Vec<Id>,
    /// The kind of op it broke at (its breaking step's, or a one-op
    /// case's) and the first natural failure among that op's
    /// transactions; `None` if forced.
    pub(crate) broke_at: Option<(String, Option<String>)>,
    /// The edge its first case ran at, if any.
    pub(crate) at: Option<String>,
    pub(crate) id: Id,
}

/// How a finding's case was shrunk.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Shrink {
    /// Not: a scan's case.
    No,
    /// Until nothing simpler showed it, in this many runs.
    Done(u32),
    /// Cut short after this many runs: why.
    Cut(u32, String),
}

/// How a finding showed.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Label {
    /// With nothing forced, in a plan as written.
    Shown,
    /// With nothing forced, at an edge a scan or a curve set: the caller's gas
    /// or an argument's value (`/Withdraw/min_gas = 0, caller gas least + 0`).
    ShownAt(String),
    /// After a fault the test forced: these faults, at these receipts.
    Forced(String),
}

/// `shown`, `shown at ..`, `forced (..)`.
impl fmt::Display for Label {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Label::Shown => f.write_str("shown"),
            Label::ShownAt(at) => write!(f, "shown at {at}"),
            Label::Forced(what) => write!(f, "forced ({what})"),
        }
    }
}

impl<O: Clone + fmt::Debug> Audit<O> {
    pub(crate) fn new(what: String) -> Self {
        Audit {
            r: super::Report { what, ..Default::default() },
            found: vec![],
            incomplete: vec![],
            curves: vec![],
            excused: vec![],
            stale: vec![],
            unsaved: vec![],
            fixed: Default::default(),
            unmoved: vec![],
            timeouts: vec![],
            prior: None,
            part: part(),
            sent: None,
        }
    }

    /// Takes another audit's coverage (sites reached, checks judged) as
    /// this one's, counting none of its cases.
    pub(crate) fn cover_with(&mut self, r: &super::Report) {
        let hit = r.reached.iter().filter(|(_, n)| **n > 0).map(|(site, _)| site);
        self.r.covered_sites.extend(hit.chain(&r.covered_sites).cloned());
        let decided = r.judged.values().filter(|j| j.ok + j.failed > 0).map(|j| &j.check);
        self.r.covered.extend(decided.chain(&r.covered).cloned());
    }

    /// Records that the variant `name` took `took`.
    pub(crate) fn time(&mut self, name: &str, took: std::time::Duration) {
        self.r.timings.push((name.to_owned(), took.as_secs_f64()));
    }

    /// Takes in one case, which ran at `at` (an edge, and its axis) if any.
    pub(crate) fn add(&mut self, run: &Run<O>, at: Option<(&str, &str)>) {
        let (at, axis) = (at.map(|a| a.0), at.map(|a| a.1));
        self.r.cases += 1;
        super::curves::merge_seen(&mut self.r.seen, &run.seen);
        for site in &run.reached {
            *self.r.reached.entry(site.clone()).or_default() += 1;
        }
        run.judged.iter().for_each(|j| tally(&mut self.r.judged, j));
        let drawn =
            run.plan.steps.iter().map(|s| s.inject.len()).sum::<usize>() + run.untargeted.len();
        self.r.faults.0 += drawn as u32;
        self.r.faults.1 += run.landed.len() as u32;
        self.note(run, at, false);
        if run.timed_out {
            let steps = || run.lines().join("; ");
            self.timeouts.push(at.map_or_else(steps, str::to_owned));
        }
        let at = at.unwrap_or("the plan as written");
        let why = run.reason.as_deref().unwrap_or_default();
        match run.verdict {
            Verdict::HarnessError => self.r.errors.push(format!("{at}: {why}")),
            Verdict::Incomplete => {
                let op = run.ops.iter().find_map(|o| Some((&o.name, o.skipped.as_ref()?)));
                let skip = op.map(|(op, s)| format!(" ({op} skipped: {s})")).unwrap_or_default();
                // One item per check that didn't judge, if that is all, so
                // excusing one leaves the others open.
                let checks = run.judged.iter().filter(|j| run.unjudged_only && j.unjudged > 0);
                let mut families: Vec<_> = checks.map(unjudged_family).collect();
                if families.is_empty() {
                    families.push(family(op.map(|o| o.0.as_str()), axis));
                }
                let detail = format!("{at}: {why}{skip}");
                self.incomplete.extend(families.into_iter().map(|f| (f, detail.clone())));
            }
            _ => {}
        }
    }

    /// Takes in witness `index`, `at` an edge for a direct one: a witness
    /// of the user's that failed ([`failed_witness`](super::run::failed_witness))
    /// is an error, its traps named.
    pub(crate) fn witness<W: super::World>(
        &mut self,
        h: &super::Harness<O, W>,
        run: &Run<O>,
        at: Option<&str>,
        index: usize,
    ) where
        O: super::Op<W>,
    {
        self.add(run, at.map(|a| (a, a)));
        let failed = at.is_none().then(|| h.witness_error(run, index));
        self.r.errors.extend(failed.flatten());
    }

    /// Takes in `run`'s findings, not the case: one run while shrinking
    /// (`incidental`) adds what is new, as one case, and counts nothing
    /// for what is known. A case an op's panic cut short shows nothing.
    pub(crate) fn note(&mut self, run: &Run<O>, at: Option<&str>, incidental: bool) {
        if run.panicked.is_some() {
            return;
        }
        for f in &run.findings {
            let id = id(run, f);
            match self.found.iter_mut().find(|x| x.id == id) {
                Some(x) => x.cases += u32::from(!incidental),
                None => self.found.push(Found {
                    finding: f.clone(),
                    label: label(run, f, at),
                    run: run.clone(),
                    original: run.clone(),
                    shrink: Shrink::No,
                    incidental,
                    cases: 1,
                    replay: None,
                    also: vec![],
                    broke: None,
                    folded: vec![],
                    kept: vec![],
                    shows: vec![],
                    ids: vec![],
                    broke_at: None,
                    at: at.map(str::to_owned),
                    id,
                }),
            }
        }
    }
}

impl<O: Op<W>, W: World> Harness<O, W> {
    /// Settles what each finding needs, before an audit is saved:
    ///
    /// - a forced finding that shows with its plan's faults and holds
    ///   removed is shown (with all of them gone, else each one that
    ///   changes nothing goes, one at a time), and its case is the plan
    ///   without them;
    /// - rows that are then one finding are one row;
    /// - a forced finding of a shown finding's check, site, cause (what it
    ///   broke, where) and kinds of op folds into that one as evidence
    ///   ([`Found::also`]).
    ///
    /// Other findings fold into rows after, in [`fold_causes`].
    ///
    /// `phase`, if any, counts the rows it settles.
    pub(crate) fn settle_found(&self, audit: &mut Audit<O>, phase: Option<&Phase>) {
        let forced = audit.found.iter().filter(|x| x.finding.injected).count();
        let step = |done: usize, rows: usize| phase.map(|p| p.step(done as u32, rows as u32));
        let all = audit.found.len();
        for (k, x) in audit.found.iter_mut().filter(|x| x.finding.injected).enumerate() {
            step(k, forced + all);
            let key = Settled::<O>::key(&x.run, &format!("{:?}", x.id));
            let known = self.settled.borrow().shown.get(&key).cloned();
            let best = known.unwrap_or_else(|| {
                let best = self.shown_without_faults(x);
                self.settled.borrow_mut().shown.insert(key, best.clone());
                best
            });
            if let Some((f, run)) = best {
                (x.label, x.id, x.finding) = (label(&run, &f, x.at.as_deref()), id(&run, &f), f);
                x.run = run;
            }
        }
        // One finding, one row; a forced variant of a shown one, evidence.
        let mut kept: Vec<Found<O>> = vec![];
        for row in audit.found.drain(..) {
            match kept.iter_mut().find(|k| k.id == row.id) {
                Some(k) => k.cases += row.cases,
                None => kept.push(row),
            }
        }
        let folds: Vec<(usize, usize)> = (0..kept.len())
            .filter(|&i| kept[i].finding.injected)
            .filter_map(|i| {
                let a = &kept[i].id;
                let shown = |k: &usize| {
                    let x = &kept[*k].id;
                    !kept[*k].finding.injected
                        && (&x.check, &x.site, &x.cause, &x.ops)
                            == (&a.check, &a.site, &a.cause, &a.ops)
                };
                Some((i, (0..kept.len()).find(shown)?))
            })
            .collect();
        for &(i, k) in &folds {
            let label = kept[i].label.to_string();
            kept[k].also.push(label);
        }
        let gone: Vec<usize> = folds.iter().map(|f| f.0).collect();
        let rows = kept.into_iter().enumerate().filter(|(i, _)| !gone.contains(i));
        audit.found = rows.map(|(_, row)| row).collect();
        let all = audit.found.len();
        for (k, row) in audit.found.iter_mut().enumerate() {
            step(forced + k, forced + all);
            let key = Settled::<O>::key(
                &row.run,
                &format!("{} at {}", row.finding.check, row.finding.site),
            );
            let known = self.settled.borrow().breaking.get(&key).cloned();
            let breaking = known.unwrap_or_else(|| {
                let b = self.breaking(&row.run, &row.finding);
                self.settled.borrow_mut().breaking.insert(key, b.clone());
                b
            });
            let alone = row.run.ops.len() == 1 && !row.finding.injected;
            let step = breaking.as_ref().map(|b| b.step).or(alone.then_some(0));
            row.broke_at = step.map(|k| {
                // A receipt's or an op's finding fails its own way; any
                // other, the way its breaking op's transactions first did.
                let f = &row.finding;
                // A receipt's finding names its transaction too.
                let failure = if f.tx.is_some() {
                    on_path(&row.run, f)
                } else {
                    failed_in(&row.run, &row.run.ops[k].txs).and_then(|r| Some((r, r.failure()?)))
                };
                let at =
                    |(r, f): (&crate::ReceiptOutcome, _)| format!("{} {}", r.site(), class(&f));
                (row.run.ops[k].name.clone(), failure.map(at))
            });
            row.broke = breaking.map(|b| b.to_string());
            let key = Settled::<O>::key(&row.run, "");
            let known = self.settled.borrow().named.get(&key).cloned();
            let (run, names) = known.unwrap_or_else(|| {
                let mut run = row.run.clone();
                let names = self.name_traps(&mut run);
                self.settled.borrow_mut().named.insert(key, (run.clone(), names.clone()));
                (run, names)
            });
            row.ids = row.run.findings.iter().map(|g| id(&row.run, g)).collect();
            row.run = run;
            if let Some(name) = row.finding.receipt_id.and_then(|r| names.get(&r)) {
                row.finding.message = crate::trap::named(&row.finding.message, name);
            }
        }
    }

    /// A forced row's finding with its plan's faults and holds removed (all
    /// of them, else each that changes nothing, one at a time), and its
    /// run; `None` if it doesn't show without them.
    fn shown_without_faults(&self, x: &Found<O>) -> Option<(Finding, Run<O>)> {
        let (cause, budget) = (x.id.cause(), x.run.budget);
        let shows = |run: &Run<O>| -> Option<Finding> {
            let ok = run.verdict == Verdict::Findings && (!run.budget || budget);
            let found = run.findings.iter().find(|f| !f.injected && id(run, f).cause() == cause);
            found.filter(|_| ok).cloned()
        };
        let mut plan = x.run.plan.clone();
        let mut bare = plan.clone();
        bare.steps.iter_mut().for_each(|s| s.inject.clear());
        let run = self.run_case(&bare);
        let mut best = shows(&run).map(|f| (f, run));
        // Each fault that changes nothing goes, one at a time.
        let injects: Vec<(usize, usize)> = (plan.steps.iter().enumerate())
            .flat_map(|(i, s)| (0..s.inject.len()).map(move |j| (i, j)))
            .collect();
        if best.is_none() && injects.len() > 1 {
            for &(i, j) in injects.iter().rev().take(MAX_DROPS) {
                let mut fewer = plan.clone();
                fewer.steps[i].inject.remove(j);
                let run = self.run_case(&fewer);
                let same = run.findings.iter().find(|f| id(&run, f).cause() == cause).cloned();
                if let Some(f) = same {
                    plan = fewer;
                    best = Some((f, run));
                }
            }
        }
        best
    }
}

/// What settling rows learned by running, by the row's case (its plan) and
/// finding: a campaign that saves as it goes settles every row at each
/// save, and reruns only the rows whose case changed.
pub(crate) struct Settled<O> {
    shown: HashMap<CryptoHash, Option<(Finding, Run<O>)>>,
    breaking: HashMap<CryptoHash, Option<super::run::Breaking>>,
    named: HashMap<CryptoHash, (Run<O>, HashMap<CryptoHash, String>)>,
}

impl<O> Default for Settled<O> {
    fn default() -> Self {
        let (shown, breaking, named) = Default::default();
        Settled { shown, breaking, named }
    }
}

impl<O: Serialize + Clone> Settled<O> {
    /// The key of `run`'s case and `what`.
    fn key(run: &Run<O>, what: &str) -> CryptoHash {
        let plan = serde_json::to_string(&run.plan).unwrap_or_default();
        CryptoHash::hash_bytes(format!("{what}\n{plan}").as_bytes())
    }
}

/// One cause, one row, before its replay is saved. These fold into the
/// highest ranked row (the first found of a rank), each in its `also`:
///
/// - of the findings nothing forced, a check's findings at one site that
///   broke at the same kind of op (their breaking step's, or a one-op
///   case's) failing the same way (a receipt's or an op's finding, as its
///   own path did; any other, as the first natural failure among that
///   op's transactions, or none), whatever other ops ran or failed: a
///   search keeps each such case as its own identity, since shrinking one
///   into another would change it;
/// - of the findings nothing forced, other checks' findings that one
///   natural failure broke (the same receipt failing the same way on their
///   path), or, with nothing failed, the same kinds of op;
/// - of the forced findings, those one forced fault broke (the same
///   label): a check of an op ranks before one of the world.
///
/// A check's sites, keys and named causes stay apart, and gas margins,
/// where nothing broke, stay rows. Identities are unchanged. A finding of
/// the first kind shares the row's replay. Any other the row's own case
/// shows (the same cause) is one more identity of the row's replay
/// ([`Found::shows`]): it shows while any of them does. One its case
/// doesn't show keeps its own replay, `save`d and named in the row's
/// `also`. Folds are flat: a row folds once, whole.
pub(crate) fn fold_causes<O>(
    audit: &mut Audit<O>,
    save: &mut dyn FnMut(&Found<O>) -> Option<std::path::PathBuf>,
) {
    // A row's key among other checks.
    let key = |x: &Found<O>| {
        let f = &x.finding;
        let failed = on_path(&x.run, f).map(|(r, failure)| {
            (r.receiver_id.to_string(), r.method_names.clone(), class(&failure))
        });
        let alone = f.injected || f.check == super::builtin::MARGIN;
        let ops = if failed.is_none() { x.id.ops.clone() } else { vec![] };
        let other = (failed.is_some() || !ops.is_empty()).then_some((failed, ops));
        other.filter(|_| !alone).map(|k| (f.cause.clone(), k))
    };
    let mut rows = std::mem::take(&mut audit.found);
    rows.sort_by_key(|r| {
        (super::report::kind(&r.finding.check).0, r.finding.site == crate::Site::World)
    });
    let mut keys = vec![];
    for row in rows {
        let k = key(&row);
        let (f, at) = (&row.finding, &row.broke_at);
        let has = |x: &Found<O>| {
            x.finding.check == f.check || x.folded.iter().any(|g| g.check == f.check)
        };
        let same = |i: usize| {
            let (x, y) = (&audit.found[i], &audit.found[i].finding);
            (!f.injected && !y.injected && at.is_some() && x.broke_at == *at)
                && (&y.check, &y.site, &y.cause) == (&f.check, &f.site, &f.cause)
        };
        let other = |i: usize| k.is_some() && keys[i] == k && !has(&audit.found[i]);
        let forced = |i: usize| {
            let x = &audit.found[i];
            f.injected && x.finding.injected && x.label == row.label
        };
        match (0..keys.len()).find(|&i| same(i) || other(i) || forced(i)) {
            Some(i) => {
                // One cause of the row's check, at its site and op: its
                // replay stands for it.
                let alike = same(i);
                let root = &mut audit.found[i];
                // Its identity in the row's own case, if that case shows it.
                let cause = row.id.cause();
                let shown = root.ids.iter().find(|g| g.cause() == cause).cloned();
                let apart = match shown {
                    _ if alike => None,
                    Some(g) => {
                        if g != root.id && !root.shows.contains(&g) {
                            root.shows.push(g);
                        }
                        None
                    }
                    None => save(&row),
                };
                let (check, site) = (&row.finding.check, &row.finding.site);
                let replay = apart.as_ref().map(|p| format!(" ({})", p.display()));
                root.also.push(format!("{check} at {site}{}", replay.unwrap_or_default()));
                root.kept.extend(apart);
                root.folded.push(row.finding);
                root.also.extend(row.also);
            }
            None => {
                keys.push(k);
                audit.found.push(row);
            }
        }
    }
}

/// A name for one run of a campaign, unique across processes.
fn part() -> String {
    static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let at = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
    let key = format!("{} {} {n}", std::process::id(), at.as_nanos());
    CryptoHash::hash_bytes(key.as_bytes()).to_string()[..12].to_owned()
}

/// The most faults [`Harness::settle_found`] tries dropping one at a time.
const MAX_DROPS: usize = 8;

/// How `f` showed in `run`, which ran at `at` (an edge) if any.
pub(crate) fn label<O>(run: &Run<O>, f: &Finding, at: Option<&str>) -> Label {
    match (f.injected, at) {
        (true, _) => {
            let forced: Vec<String> =
                run.landed.iter().map(|l| format!("{} at {}", l.fault, l.site)).collect();
            Label::Forced(forced.join(", "))
        }
        (false, Some(at)) => Label::ShownAt(at.to_owned()),
        (false, None) => Label::Shown,
    }
}

impl<O> Audit<O> {
    /// One row per cause, each with the findings folded into it
    /// ([`Found::folded`], named in [`Found::also`];
    /// [`assert_found`](Self::assert_found) finds them). A check whose
    /// every finding folded into another's row has no row of its own: to
    /// assert a check never fired at `at`, use
    /// [`found_at`](Self::found_at)`(check, at).is_none()`; at any site,
    /// read each row's `finding` and `folded`.
    pub fn found(&self) -> &[Found<O>] {
        &self.found
    }

    /// The cases it ran.
    pub fn cases(&self) -> u32 {
        self.r.cases
    }

    /// The cases not tried to their end (a scan's edge whose op skipped
    /// among them), each with its family, `op, axis`, and why: a family
    /// is one open `incomplete:<family>` item.
    pub fn incomplete(&self) -> &[(String, String)] {
        &self.incomplete
    }

    /// The cases the harness, a plan or an op got wrong.
    pub fn errors(&self) -> &[String] {
        &self.r.errors
    }

    /// Whether a case of its own, or of an audit it was run
    /// [`with`](super::Curves::with), reached `site`.
    pub(crate) fn covers(&self, site: &str) -> bool {
        let with = self.r.covered_sites.iter().any(|s| super::check::site_matches(site, s));
        self.reached(site) > 0 || with
    }

    /// The cases that reached a receipt at `site` (`receiver::method`, as
    /// the receipt tree labels a call, or a pattern as
    /// [`Harness::must_not_fail`](super::Harness::must_not_fail) takes; for a pattern,
    /// the most of any one site it matches). A batch's calls are reached
    /// one by one: `receiver::f`, not the tree's `receiver::f,g`.
    pub fn reached(&self, site: &str) -> u32 {
        let at = |s: &&String| super::check::site_matches(site, s);
        self.r.reached.iter().filter(|(s, _)| at(s)).map(|(_, n)| *n).max().unwrap_or(0)
    }

    /// [`reached`](Self::reached) at least once, or by an audit this one
    /// was run [`with`](super::Curves::with); else a panic listing what was.
    #[track_caller]
    pub fn assert_reached(&self, site: &str) {
        let all: Vec<&String> = self.r.reached.keys().chain(&self.r.covered_sites).collect();
        assert!(
            self.covers(site),
            "no case of {} reached {site:?}; reached: {all:?}",
            self.r.cases
        );
    }

    /// Curves' rows, every one, [`Reach::NoEffect`](super::Reach::NoEffect) included.
    pub fn curves(&self) -> &[CurveRow<O>] {
        &self.curves
    }

    /// The row of `resource` at `site` along the axis named `axis` (or
    /// whose pointer ends in `/axis`), if any.
    pub fn curve(&self, site: &str, resource: Resource, axis: &str) -> Option<&CurveRow<O>> {
        self.curves.iter().find(|r| {
            r.site == site
                && r.resource == resource
                && (r.axis.name == axis || r.axis.name.ends_with(&format!("/{axis}")))
        })
    }

    /// [`curve`](Self::curve), or a panic listing the rows at `site`.
    #[track_caller]
    pub fn assert_curve(&self, site: &str, resource: Resource, axis: &str) -> &CurveRow<O>
    where
        O: fmt::Debug,
    {
        self.curve(site, resource, axis).unwrap_or_else(|| {
            let at: Vec<String> = (self.curves.iter())
                .filter(|r| r.site == site)
                .map(|r| format!("{:?} along {}", r.resource, r.axis))
                .collect();
            panic!("no {resource:?} row at {site} along {axis}; at {site}: {at:#?}")
        })
    }

    /// A search's seeds: `NEAR_SIM_SEED` set to the first repeats it.
    pub fn seeds(&self) -> &[u64] {
        &self.r.seeds
    }

    /// Nothing found, nothing wrong, and some case complete: no check
    /// failed in what was tried (not that none can). What wasn't tried
    /// is open: [`incomplete`](Self::incomplete), [`todo`](Self::todo).
    pub fn is_clean(&self) -> bool {
        self.found.is_empty()
            && self.r.errors.is_empty()
            && self.incomplete.len() < self.r.cases as usize
    }

    /// [`is_clean`](Self::is_clean), or a panic printing the audit.
    #[track_caller]
    pub fn assert_clean(&self)
    where
        O: fmt::Debug,
    {
        assert!(self.is_clean(), "not clean:\n{self}");
    }

    /// The finding of `check` at `at` (a site as it prints, with or
    /// without its ` (Op)`, or its op's name), if found.
    /// A check folded into another's row finds that row.
    pub fn found_at(&self, check: &str, at: &str) -> Option<&Found<O>> {
        let is = |f: &Finding| f.check == check && super::check::site_is(&f.site, at);
        (self.found.iter().find(|x| is(&x.finding)))
            .or_else(|| self.found.iter().find(|x| x.folded.iter().any(is)))
    }

    /// [`found_at`](Self::found_at), or a panic printing the audit.
    #[track_caller]
    pub fn assert_found(&self, check: &str, at: &str) -> &Found<O>
    where
        O: fmt::Debug,
    {
        let Some(x) = self.found_at(check, at) else {
            panic!("no finding of {check:?} at {at:?}:\n{self}")
        };
        x
    }
}

/// Its [`report`](Audit::report)'s `Display`: 60 lines at most.
impl<O: fmt::Debug> fmt::Display for Audit<O> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.report().fmt(f)
    }
}

/// The family of cases a check `j` couldn't judge: `<check> not judged`,
/// and why in brackets where it was overlap.
fn unjudged_family(j: &super::Judged) -> String {
    let overlapped = j.why.as_deref().is_some_and(|w| w.starts_with(super::run::OVERLAPPED));
    let why = if overlapped { " (overlapped)" } else { "" };
    format!("{} not judged{why}", j.check)
}

/// The family of a case not tried to its end: the op that skipped and the
/// axis its edge moved, as far as known (`InitTransfer, /InitTransfer/amount`).
pub(crate) fn family(op: Option<&str>, axis: Option<&str>) -> String {
    match (op, axis) {
        (Some(op), Some(axis)) => format!("{op}, {axis}"),
        (Some(name), None) | (None, Some(name)) => name.into(),
        (None, None) => "the plan as written".into(),
    }
}
