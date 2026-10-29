//! What an [`Audit`] reports: findings ranked by consequence, the curve
//! rows that matter, which exported methods ran, and the to-do list
//! ([`Audit::todo`]) that [`Audit::done`] refuses on. [`Audit::report`]
//! gives it as a [`Report`]: its `Display` prints it in 60 lines at most,
//! and [`Report::json`] writes what [`Report::from_json`] reads.
//!
//! Each to-do item has a stable id (explore.md lists them).
//! [`Harness::excuse`] closes items by id or glob (`unreached:token.near::*`),
//! and the summary's header names every excuse and what it closed.

use super::audit::Audit;
use super::check::glob;
use super::curves::{CurveRow, Reach};
use super::{Harness, Op, World};
use serde::{Deserialize, Serialize};
use std::fmt::{self, Write};

/// One open item: its stable id, why it is open, and what closes it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Todo {
    /// Stable across runs of the same harness and plans.
    pub id: String,
    /// Why it is open.
    pub why: String,
    /// The call or code that closes it, or [`Harness::excuse`].
    pub close: String,
    /// A stopped curve row's refusing site and what refused it there: the
    /// summary prints the items of one such pair as one line.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) stop: Option<(String, String)>,
}

impl Todo {
    pub(crate) fn new(id: String, why: String, close: impl Into<String>) -> Self {
        Todo { id, why, close: close.into(), stop: None }
    }
}

/// Which exported methods of the audited contracts ran: in a case, in
/// the fixture's setup, or as a view (a check's, a setup's).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Paths {
    /// The contracts audited ([`Harness::audits`], else every contract of
    /// the fixture).
    pub audited: Vec<String>,
    /// The fixture's contracts left out by [`Harness::audits`].
    pub left_out: Vec<String>,
    /// Their exported methods.
    pub total: u32,
    /// Those that never ran: open, as `unreached:` items.
    pub unreached: Vec<String>,
    /// Those that ran, in a case or the fixture, that a scan with edges
    /// never sent directly from `axis.near` (`None` without edges): the
    /// sweep sends what the witnesses' receipts called, one function call
    /// each, so a method that ran only in the fixture (an `init`) or
    /// beside another call in one receipt is here. Views and the methods
    /// that never ran (`unreached`) are not.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub never_sent: Option<Vec<String>>,
}

/// An audit as data. Its `Display` is the audit's summary;
/// [`json`](Report::json) writes what [`from_json`](Report::from_json)
/// reads.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Report {
    /// Its JSON format's version: the current one for a report this build
    /// made ([`from_json`](Report::from_json) refuses another; a file
    /// written before versions reads as 0).
    #[serde(default)]
    pub format: u32,
    /// The campaign.
    pub what: String,
    /// Cases run.
    pub cases: u32,
    /// Findings, ranked.
    pub findings: Vec<Row>,
    /// Curve rows, but those of no effect.
    pub curves: Vec<Curve>,
    /// Value-path coverage.
    pub paths: Paths,
    /// Open items.
    pub todo: Vec<Todo>,
    /// Excuses: the id or glob, why, and the items it closed.
    pub excused: Vec<(String, String, Vec<String>)>,
    /// The witnesses' strings that aren't axes: changed, they don't
    /// deserialize, and no other value was found for them.
    pub fixed: Vec<String>,
    /// Each variant's run time in seconds, slowest first: wall-clock
    /// data, which the summary leaves out (two runs of the same cases
    /// print the same summary).
    #[serde(default)]
    pub timings: Vec<(String, f64)>,
    /// Gas bands a scan or an enumeration with edges found, each once: a
    /// step whose transaction succeeds at just enough gas while one of its
    /// receipts fails, up to what gas. The summary prints the first 8.
    #[serde(default)]
    pub bands: Vec<String>,
    /// Its curve points, the sites it reached and its checks' judgments:
    /// what [`Curves::with`](super::Curves::with) counts.
    #[serde(default, with = "seen_json")]
    pub(crate) seen: super::curves::Seen,
    #[serde(default)]
    pub(crate) reached: std::collections::BTreeMap<String, u32>,
    #[serde(default)]
    pub(crate) judged: std::collections::BTreeMap<String, super::Judged>,
    /// Checks another report judged for it: covered, not counted.
    #[serde(default)]
    pub(crate) covered: std::collections::BTreeSet<String>,
    /// Sites another report reached for it: covered, not counted.
    #[serde(default)]
    pub(crate) covered_sites: std::collections::BTreeSet<String>,
    /// In a file `saves` wrote: the campaign's units done (variants,
    /// cases, axes); 0 otherwise.
    #[serde(default)]
    pub done: usize,
    /// In a file `saves` wrote: the fingerprint of what it resumes only
    /// on: the fixture (its state root and code), the harness's checks,
    /// and an enumeration's ops, [`first`](super::Enumerate::first) ops
    /// and faults.
    #[serde(default)]
    pub fixture: String,
    /// A search's seeds: `NEAR_SIM_SEED` set to the first repeats it.
    #[serde(default)]
    pub seeds: Vec<u64>,
    /// Whether curves ran, so the summary names them.
    #[serde(default)]
    pub(crate) ran_curves: bool,
    /// Each harness error, as [`Audit::errors`] gives it.
    #[serde(default)]
    pub(crate) errors: Vec<String>,
    /// In a file `saves` wrote: the build of the test that wrote it.
    #[serde(default)]
    pub(crate) build: String,
    /// The campaign runs it holds: its own, and a resumed one's saved part's.
    #[serde(default)]
    pub(crate) parts: Vec<String>,
    /// Injections drawn, and those that landed on a receipt.
    #[serde(default)]
    pub(crate) faults: (u32, u32),
    /// Cases not tried to their end, each with its family and why: open,
    /// one `incomplete:<family>` item a family ([`Audit::incomplete`]).
    #[serde(default)]
    pub incomplete: Vec<(String, String)>,
    /// The repeats and tails an op's [`Axes::only`](super::Axes::only)
    /// left out: not climbed.
    #[serde(default)]
    pub(crate) left_out_axes: Vec<String>,
    /// What an enumeration measured and left out: each op's flights, the
    /// states its dedup key merged, what it didn't vary, and its
    /// verification boundary; with [`first`](super::Enumerate::first), a
    /// note naming the first ops before them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
}

/// [`Report::seen`] as a list: JSON keys are strings.
mod seen_json {
    use super::super::curves::Seen;
    use serde::{Deserialize, Deserializer, Serializer};

    pub(super) fn serialize<S: Serializer>(seen: &Seen, s: S) -> Result<S::Ok, S::Error> {
        s.collect_seq(seen.iter().map(|(k, v)| (k, v.iter().collect::<Vec<_>>())))
    }

    pub(super) fn deserialize<'d, D: Deserializer<'d>>(d: D) -> Result<Seen, D::Error> {
        let v: Vec<(_, Vec<(u128, _)>)> = Vec::deserialize(d)?;
        Ok(v.into_iter().map(|(k, p)| (k, p.into_iter().collect())).collect())
    }
}

impl<O: fmt::Debug> From<&Audit<O>> for Report {
    fn from(audit: &Audit<O>) -> Report {
        audit.report()
    }
}

impl From<&Report> for Report {
    fn from(report: &Report) -> Report {
        report.clone()
    }
}

/// One curve row of a [`Report`]: a [`CurveRow`] without its plan and
/// points.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Curve {
    /// `receiver::method`.
    pub site: String,
    /// What it uses.
    pub resource: super::Resource,
    /// The witness step the axis moves.
    pub step: usize,
    /// The axis's name.
    pub axis: String,
    /// Its use per unit of the axis, first point to last.
    pub slope: f64,
    /// Its use at the last point.
    pub used: u128,
    /// The limit at the last point.
    pub limit: u128,
    /// The furthest the axis moved with the site reached.
    pub moved_to: u128,
    /// How it stands against its limit.
    pub reach: Reach,
    /// The replay of a crossing a check fired at.
    pub replay: Option<String>,
    /// The row as it prints, on one line.
    pub line: String,
}

impl<O: fmt::Debug> From<&CurveRow<O>> for Curve {
    fn from(row: &CurveRow<O>) -> Curve {
        Curve {
            site: row.site.clone(),
            resource: row.resource,
            step: row.step,
            axis: row.axis.name.clone(),
            slope: row.slope,
            used: row.points.last().map_or(0, |p| p.1),
            limit: row.limit,
            moved_to: row.moved_to,
            reach: row.reach.clone(),
            replay: row.replay.as_ref().map(|p| p.display().to_string()),
            line: row.to_string().lines().next().unwrap_or_default().to_owned(),
        }
    }
}

/// One finding of a [`Report`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Row {
    /// What it breaks: `books` or `conservation`, `stranded` or `not
    /// serializable`, `claim stuck`, `check`, `gas margin`, `storage paid`
    /// or `failed receipt`; the order they rank in.
    pub kind: String,
    /// The check.
    pub check: String,
    /// Where.
    pub site: String,
    /// How it showed.
    pub label: String,
    /// Its message's first line.
    pub message: String,
    /// Cases that showed it.
    pub cases: u32,
    /// The case's steps, as they ran.
    pub steps: Vec<String>,
    /// Its replay file.
    pub replay: Option<String>,
    /// What folded into it ([`Found::also`](super::Found::also)).
    #[serde(default)]
    pub also: Vec<String>,
    /// The op after which it broke.
    #[serde(default)]
    pub broke: Option<String>,
    /// Its identity, which [`Harness::union`] merges rows by.
    #[serde(default)]
    pub id: String,
    /// Why its case wasn't shrunk to the end, if it wasn't: a search's
    /// shrink cut short, or its budget spent first.
    #[serde(default)]
    pub shrink: String,
    /// In a file `saves` wrote: its replays (each file and what it holds),
    /// which the campaign writes to the store when it finishes.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) saved: Vec<(String, String)>,
}

impl Report {
    /// Reads a report [`json`](Self::json) wrote.
    /// One of an older format is refused too: its rows' identities may
    /// read otherwise now, so a union would hold one finding twice.
    pub fn from_json(text: &str) -> Result<Report, String> {
        let r: Report = serde_json::from_str(text).map_err(|e| e.to_string())?;
        older_or_newer(r.format).map_or(Ok(r), Err)
    }

    /// The report as pretty JSON.
    pub fn json(&self) -> String {
        serde_json::to_string_pretty(self).expect("a report serializes")
    }

    /// `Ok` if nothing is open, else the open items.
    pub fn done(&self) -> Result<(), Vec<Todo>> {
        if self.todo.is_empty() { Ok(()) } else { Err(self.todo.clone()) }
    }
}

impl<O: Op<W>, W: World> Harness<O, W> {
    /// Several audits' reports (saved ones, read back) as one: their
    /// findings, curve rows and cases together. A report another one holds
    /// (a resumed campaign's saved part) counts once. A method is
    /// unreached, and a check unjudged, only if no report reached or
    /// judged it, excused or not; a replay no longer stale here is no
    /// item. Their excuses and this harness's close items of all of them
    /// ([`Report::done`]).
    ///
    /// Fixture variants (funding states of one setup): one `Harness` per
    /// variant, each with its own [`store`](Harness::store), then `union`
    /// their reports. A store two variants share runs each one's replays
    /// on the other's fixture (`fixture_changed`), where a bug that needs
    /// its own variant's funding reads [`Gone`](super::Status::Gone) or
    /// [`NotReached`](super::Status::NotReached). Varying the funding with
    /// an op instead lets an enumeration merge states its default key
    /// can't tell apart (accounts without code).
    ///
    /// # Panics
    ///
    /// On a report of another format, which
    /// [`from_json`](Report::from_json) refuses.
    pub fn union(&self, reports: &[Report]) -> Report {
        if let Some(why) = reports.iter().find_map(|r| older_or_newer(r.format)) {
            panic!("Harness::union: a report {why}");
        }
        let still = |id: &str| {
            let Some(path) = id.strip_prefix("stale:") else { return true };
            let path = std::path::Path::new(path);
            path.exists() && super::Store::unjudgeable(self, path).is_some()
        };
        let mut out = union(&self.excused, reports);
        // A saves file's replays are its campaign's to write.
        out.findings.iter_mut().for_each(|row| row.saved.clear());
        let gone: Vec<String> =
            out.todo.iter().map(|t| t.id.clone()).filter(|id| !still(id)).collect();
        out.todo.retain(|t| !gone.contains(&t.id));
        for e in &mut out.excused {
            e.2.retain(|id| !gone.contains(id));
        }
        out
    }
}

/// Why a report of `format` can't be read here, if it can't.
fn older_or_newer(format: u32) -> Option<String> {
    let now = super::FORMAT;
    (format > now)
        .then(|| format!("written in format {format}, newer than this build's {now}"))
        .or_else(|| {
            (format < now).then(|| {
                format!(
                    "written in format {format}, an older identity scheme than this build's {now}; \
                 rerun the campaign"
                )
            })
        })
}

/// [`Harness::union`] with these excuses, every report's stale items kept.
/// One finding (its identity) in several reports is one row, its first,
/// counting their cases.
pub(crate) fn union(excused: &[(String, String)], reports: &[Report]) -> Report {
    // A report whose runs another holds is in that one already.
    let holds = |a: &Report, b: &Report| b.parts.iter().all(|p| a.parts.contains(p));
    let reports: Vec<&Report> = (reports.iter().enumerate())
        .filter(|(i, r)| {
            r.parts.is_empty()
                || !reports.iter().enumerate().any(|(j, s)| {
                    j != *i && holds(s, r) && (s.parts.len() > r.parts.len() || j < *i)
                })
        })
        .map(|(_, r)| r)
        .collect();
    let mut findings: Vec<Row> = vec![];
    for row in reports.iter().flat_map(|r| &r.findings) {
        match findings.iter_mut().find(|x| !row.id.is_empty() && x.id == row.id) {
            Some(x) => x.cases += row.cases,
            None => findings.push(row.clone()),
        }
    }
    let mut what: Vec<&str> = vec![];
    reports.iter().flat_map(|r| r.what.split(" + ")).for_each(|w| push_new(&mut what, w));
    let mut out = Report {
        format: super::FORMAT,
        what: what.join(" + "),
        cases: reports.iter().map(|r| r.cases).sum(),
        findings,
        curves: reports.iter().flat_map(|r| r.curves.clone()).collect(),
        paths: reports.first().map(|r| r.paths.clone()).unwrap_or_default(),
        fixed: reports.iter().flat_map(|r| r.fixed.clone()).collect(),
        timings: reports.iter().flat_map(|r| r.timings.clone()).collect(),
        bands: reports.iter().flat_map(|r| r.bands.clone()).collect(),
        ran_curves: reports.iter().any(|r| r.ran_curves),
        errors: reports.iter().flat_map(|r| r.errors.clone()).collect(),
        incomplete: reports.iter().flat_map(|r| r.incomplete.clone()).collect(),
        faults: reports.iter().fold((0, 0), |a, r| (a.0 + r.faults.0, a.1 + r.faults.1)),
        ..Report::default()
    };
    for r in &reports {
        r.seeds.iter().for_each(|&seed| push_new(&mut out.seeds, seed));
        r.parts.iter().for_each(|part| push_new(&mut out.parts, part.clone()));
        r.left_out_axes.iter().for_each(|a| push_new(&mut out.left_out_axes, a.clone()));
        r.notes.iter().for_each(|n| push_new(&mut out.notes, n.clone()));
    }
    for r in &reports {
        super::curves::merge_seen(&mut out.seen, &r.seen);
        for (site, n) in &r.reached {
            *out.reached.entry(site.clone()).or_default() += n;
        }
        r.judged.values().for_each(|j| super::audit::tally(&mut out.judged, j));
        out.covered.extend(r.covered.iter().cloned());
        out.covered_sites.extend(r.covered_sites.iter().cloned());
    }
    // Coverage open in every report that ran a case (a resume may run none).
    let ran: Vec<&&Report> = reports.iter().filter(|r| r.cases > 0).collect();
    let unreached = |s: &String| ran.iter().all(|r| r.paths.unreached.contains(s));
    out.paths.unreached.retain(unreached);
    // Ran but never sent: what a sweeping report lists, but what another
    // ran in a case and didn't list, so sent.
    let swept: Vec<&&Report> = reports.iter().filter(|r| r.paths.never_sent.is_some()).collect();
    let sent = |s: &String| {
        swept.iter().any(|r| {
            r.reached.contains_key(s) && !r.paths.never_sent.iter().flatten().any(|n| n == s)
        })
    };
    out.paths.never_sent = (!swept.is_empty()).then(|| {
        let mut all: Vec<String> = swept
            .iter()
            .flat_map(|r| r.paths.never_sent.iter().flatten())
            .filter(|s| !sent(s))
            .cloned()
            .collect();
        all.sort();
        all.dedup();
        all
    });
    // Every item: the reports' own, open or excused, but coverage and
    // errors, which are the union's.
    let coverage = |id: &str| {
        id == "errors"
            || ["unreached:", "unjudged:", "incomplete:"].iter().any(|p| id.starts_with(p))
    };
    let mut items: Vec<Todo> = errors_item(&out.errors).into_iter().collect();
    for r in &reports {
        let closed = r.excused.iter().flat_map(|e| e.2.iter().map(move |id| (id, &e.1)));
        let excused = closed.map(|(id, why)| Todo::new(id.clone(), format!("excused: {why}"), ""));
        for t in r.todo.iter().cloned().chain(excused).filter(|t| !coverage(&t.id)) {
            if !items.iter().any(|x| x.id == t.id) {
                items.push(t);
            }
        }
    }
    let cases = out.cases;
    let unjudged = out.judged.values().filter(|j| open(j, &out.covered));
    items.extend(unjudged.map(|j| unjudged_item(j, cases)));
    items.extend(out.paths.unreached.iter().map(|s| unreached_item(s)));
    items.extend(incomplete_items(&out.incomplete));
    // This harness's excuses, then each report's.
    let mut patterns: Vec<(String, String)> = excused.to_vec();
    for (p, why, _) in reports.iter().flat_map(|r| &r.excused) {
        if !patterns.iter().any(|x| x.0 == *p) {
            patterns.push((p.clone(), why.clone()));
        }
    }
    (out.todo, out.excused) = split(&patterns, items);
    out.excused.retain(|e| !e.2.is_empty());
    out
}

/// An excuse's id or glob, why, and the items it closed.
type Excused = (String, String, Vec<String>);

/// `items` split by `excuses` (an id or a glob, and why): those none
/// closes, and each excuse with the ids it closes; an item goes to the
/// first excuse that matches it.
fn split(excuses: &[(String, String)], items: Vec<Todo>) -> (Vec<Todo>, Vec<Excused>) {
    let mut excused: Vec<_> =
        excuses.iter().map(|(p, why)| (p.clone(), why.clone(), vec![])).collect();
    let mut open = vec![];
    for t in items {
        let id = t.id.as_bytes();
        match excused.iter_mut().find(|(p, ..)| p.as_bytes() == id || glob(p.as_bytes(), id)) {
            Some(e) => e.2.push(t.id),
            None => open.push(t),
        }
    }
    (open, excused)
}

/// The to-do item of these harness errors, if any.
fn errors_item(errors: &[String]) -> Option<Todo> {
    let first = errors.first()?;
    let why = format!("{} harness error(s): {first}", errors.len());
    Some(Todo::new("errors".into(), why, "fix the harness or the plan"))
}

/// Pushes `x` unless `v` holds it.
fn push_new<T: PartialEq>(v: &mut Vec<T>, x: T) {
    if !v.contains(&x) {
        v.push(x);
    }
}

/// Whether check `j` is an `unjudged:` item: no case judged it (a check
/// is in `judged` once a case ran), and no report `covered` it. Not
/// [`SERIALIZABLE`](super::builtin::SERIALIZABLE) with nothing ever in
/// flight together, as settled cases leave nothing to order (a check
/// that couldn't judge is an `incomplete:` one).
fn open(j: &super::Judged, covered: &std::collections::BTreeSet<String>) -> bool {
    let idle_serial = j.check == super::builtin::SERIALIZABLE && j.unjudged == 0;
    j.ok + j.failed == 0 && !covered.contains(&j.check) && !idle_serial
}

fn unjudged_item(j: &super::Judged, cases: u32) -> Todo {
    let why = match (&j.why, &j.idle_why) {
        (Some(w), _) => format!("never judged in {cases} case(s): {w}"),
        (None, Some(w)) => format!("nothing at stake in {cases} case(s): {w}"),
        (None, None) => format!("never ran in {cases} case(s)"),
    };
    Todo::new(format!("unjudged:{}", j.check), why, "a witness plan where it applies")
}

/// The to-do items of cases not tried to their end: one a family (op,
/// axis), with its count and its first case and why. A family is often
/// one op's precondition across an axis's edges: one item, one excuse.
fn incomplete_items(entries: &[(String, String)]) -> Vec<Todo> {
    let mut families: Vec<(&str, usize, &str)> = vec![];
    for (family, case) in entries {
        match families.iter_mut().find(|f| f.0 == family) {
            Some(f) => f.1 += 1,
            None => families.push((family, 1, case)),
        }
    }
    // A check that couldn't judge, else an op that skipped.
    let close = |family: &str| match family {
        f if f.ends_with(" not judged (overlapped)") => {
            "read effects() in the check (the op's own, judged under overlap), or judge the ops \
             in flight together in at_rest_ops, from the last whose OpDone::rest() is true"
        }
        f if f.ends_with(" not judged") => "a plan where the check can judge",
        _ => "an op that doesn't skip there, or a plan where it applies",
    };
    let item = |(family, n, case): (&str, usize, &str)| {
        let why = format!(
            "{n} case(s) not tried to their end, so nothing after them judged; first: {case}"
        );
        Todo::new(format!("incomplete:{family}"), why, close(family))
    };
    families.into_iter().map(item).collect()
}

/// The to-do item of an exported method that never ran.
fn unreached_item(site: &str) -> Todo {
    let why = "no case, setup or view ran this method of an audited contract";
    let close = "a witness plan that calls it, Harness::union with a report that reached it, or \
                 Harness::audits";
    Todo::new(format!("unreached:{site}"), why.into(), close)
}

/// The `unreached:` items as one line, their methods cut to `n` chars:
/// `unreached: 3 exported methods never ran: a::x, a::y, a::z`.
fn unreached_line(items: &[&Todo], n: usize) -> Option<String> {
    let names: Vec<&str> = items.iter().filter_map(|t| t.id.strip_prefix("unreached:")).collect();
    let k = names.len();
    (k > 0).then(|| {
        let s = if k == 1 { "" } else { "s" };
        format!("unreached: {k} exported method{s} never ran: {}", short(&names.join(", "), n))
    })
}

/// What a finding of `check` breaks, by rank: the books, value stranded,
/// a claim stuck, another check or a self-only method open to others, a
/// gas margin, storage the contract paid, or a failed receipt.
pub(crate) fn kind(check: &str) -> (u8, &'static str) {
    use crate::explore::builtin;
    if check.contains(" is backed by ") || check.ends_with(" backs what it owes") {
        (0, "books")
    } else if check.ends_with(", owed to no one") || check.contains(" falls only for ") {
        (1, "stranded")
    } else if check.ends_with(" is redeemable") {
        (2, "claim stuck")
    } else if check == builtin::MARGIN || check == builtin::STORAGE_FOR_CALLERS {
        (4, if check == builtin::MARGIN { "gas margin" } else { "storage paid" })
    } else if check == builtin::CONSERVATION {
        (0, "conservation")
    } else if check == builtin::SERIALIZABLE {
        (1, "not serializable")
    } else if check == builtin::SELF_ONLY {
        (3, "self-only")
    } else if builtin::ALL.contains(&check) {
        (4, "failed receipt")
    } else {
        (3, "check")
    }
}

impl<O: Op<W>, W: World> Harness<O, W> {
    /// Audits only `accounts`' contracts: their methods are the value
    /// paths whose coverage every audit reports. By default every contract
    /// of the fixture is audited; the ones this leaves out are named in
    /// each summary's header.
    pub fn audits<'a>(mut self, accounts: impl IntoIterator<Item = &'a str>) -> Self {
        self.audits = Some(accounts.into_iter().map(str::to_owned).collect());
        self
    }

    /// Fills `audit`'s coverage and excuses.
    pub(crate) fn cover(&self, audit: &mut Audit<O>) {
        let sim = self.fixture.as_ref();
        let audited = |a: &String| self.audits.as_ref().is_none_or(|x| x.contains(a));
        let fixture_reached: std::collections::BTreeSet<String> =
            (sim.txs().iter()).flat_map(|tx| sim.outcome(tx).receipts).map(|r| r.site()).collect();
        let viewed = |site: &String| sim.viewed.lock().is_ok_and(|v| v.contains(site));
        let mut paths = Paths::default();
        for (account, code) in &sim.contracts() {
            if !audited(account) {
                paths.left_out.push(account.clone());
                continue;
            }
            paths.audited.push(account.clone());
            for method in exports(code) {
                let site = format!("{account}::{method}");
                paths.total += 1;
                let ran = audit.reached(&site) > 0 || fixture_reached.contains(&site);
                let viewed_or_ran = ran || audit.covers(&site) || viewed(&site);
                if !viewed_or_ran {
                    paths.unreached.push(site.clone());
                }
                if ran && audit.sent.as_ref().is_some_and(|sent| !sent.contains(&site)) {
                    paths.never_sent.get_or_insert_with(Vec::new).push(site);
                }
            }
        }
        if audit.sent.is_some() {
            paths.never_sent.get_or_insert_with(Vec::new);
        }
        audit.r.paths = paths;
        audit.excused = self.excused.clone();
    }
}

/// A contract's exported functions.
pub(crate) fn exports(code: &[u8]) -> Vec<String> {
    use wasmparser::{ExternalKind, Parser, Payload};
    let mut out = vec![];
    for payload in Parser::new(0).parse_all(code).flatten() {
        if let Payload::ExportSection(e) = payload {
            let funcs = e.into_iter().flatten().filter(|e| e.kind == ExternalKind::Func);
            out.extend(funcs.map(|e| e.name.to_owned()));
        }
    }
    out
}

impl<O: fmt::Debug> Audit<O> {
    /// Every open item, excused ones left out, in a stable order.
    pub fn todo(&self) -> Vec<Todo> {
        split(&self.excused, self.items()).0
    }

    /// `Ok` with the summary if nothing is open, else the open items: a
    /// clean audit with open items is not done. Coverage another campaign
    /// reached (an `unreached:` method, an `unjudged:` check) closes with
    /// [`Harness::union`] of both reports.
    pub fn done(&self) -> Result<String, Vec<Todo>> {
        let open = self.todo();
        if open.is_empty() { Ok(self.to_string()) } else { Err(open) }
    }

    /// Every item, excused or not.
    fn items(&self) -> Vec<Todo> {
        let mut out: Vec<Todo> = vec![];
        let mut push = |t: Todo| {
            if !out.iter().any(|x| x.id == t.id) {
                out.push(t);
            }
        };
        if let Some(t) = errors_item(&self.r.errors) {
            push(t);
        }
        // What the world or the op lacks comes before the rows it leaves open.
        for t in &self.unmoved {
            push(t.clone());
        }
        for row in &self.curves {
            let Some(why) = row.open() else { continue };
            let (axis, step) = (&row.axis.name, row.step);
            let id = format!("axis:step {step}:{axis}");
            let close = format!(
                "Harness::curve(witness, {step}, {axis:?}) from a witness that moves it further"
            );
            push(Todo { stop: row.stop.clone(), ..Todo::new(id, why, close) });
        }
        for j in self.r.judged.values().filter(|j| open(j, &self.r.covered)) {
            push(unjudged_item(j, self.r.cases));
        }
        for at in &self.timeouts {
            let why = "ran past the one-minute case timeout, so nothing past it was judged".into();
            push(Todo::new(format!("timeout:{at}"), why, "an op that bounds the size"));
        }
        incomplete_items(&self.incomplete).into_iter().for_each(&mut push);
        for (file, why) in &self.stale {
            let close = "delete the replay, or restore its check or op";
            push(Todo::new(format!("stale:{file}"), why.clone(), close));
        }
        for (dir, why) in &self.unsaved {
            let why = format!("replays not saved, so their rows cite none: {why}");
            push(Todo::new(
                format!("replays:{dir}"),
                why,
                "a store it can write, Harness::store(dir)",
            ));
        }
        self.r.paths.unreached.iter().for_each(|site| push(unreached_item(site)));
        out
    }

    /// The audit as data.
    pub fn report(&self) -> Report {
        let mut found: Vec<_> = self.found.iter().collect();
        found.sort_by_key(|x| (kind(&x.finding.check).0, x.finding.injected));
        let findings = found.iter().map(|x| Row {
            kind: kind(&x.finding.check).1.into(),
            check: x.finding.check.clone(),
            site: x.finding.site.to_string(),
            label: x.label.to_string(),
            message: x.finding.message.lines().next().unwrap_or_default().into(),
            cases: x.cases,
            steps: x.run.lines(),
            replay: x.replay.as_ref().map(|p| p.display().to_string()),
            also: x.also.clone(),
            broke: x.broke.clone(),
            id: format!("{:?}", x.id),
            shrink: match &x.shrink {
                super::Shrink::Cut(n, why) => format!("shrink cut after {n} run(s): {why}"),
                _ => String::new(),
            },
            saved: vec![],
        });
        let (todo, excused) = split(&self.excused, self.items());
        let mut own = Report {
            format: super::FORMAT,
            findings: findings.collect(),
            curves: self
                .curves
                .iter()
                .filter(|r| r.reach != Reach::NoEffect)
                .map(Curve::from)
                .collect(),
            todo,
            excused,
            fixed: self.fixed.iter().cloned().collect(),
            incomplete: self.incomplete.clone(),
            ran_curves: !self.curves.is_empty(),
            parts: vec![self.part.clone()],
            ..self.r.clone()
        };
        own.timings.sort_by(|a, b| b.1.total_cmp(&a.1));
        match &self.prior {
            Some(p) => union(&self.excused, &[(**p).clone(), own]),
            None => own,
        }
    }

    /// The write-up with its open items first, under "Coverage not
    /// achieved" and `why` (a time box, a first pass): the honest
    /// override of [`done`](Self::done). An empty `why` panics.
    #[track_caller]
    pub fn with_open_gaps(&self, why: &str) -> String {
        assert!(!why.trim().is_empty(), "with_open_gaps needs why the items stay open");
        let open = self.todo();
        let mut s = String::new();
        if !open.is_empty() {
            let _ = writeln!(s, "Coverage not achieved ({why}): {} item(s) open", open.len());
            let (unreached, rest): (Vec<&Todo>, Vec<&Todo>) =
                open.iter().partition(|t| t.id.starts_with("unreached:"));
            if let Some(line) = unreached_line(&unreached, usize::MAX) {
                let _ = writeln!(s, "  {line}; closes with {}", unreached[0].close);
            }
            for t in rest {
                let _ = writeln!(s, "  {}: {}; closes with {}", t.id, t.why, t.close);
            }
        }
        s + &self.to_string()
    }
}

/// The report in 60 lines at most: what was found and what is open, the
/// coverage, the findings ranked (the first three with their steps), the
/// curve rows that crossed or stopped, and the to-do list. Every
/// row is in the [`Report`].
impl fmt::Display for Report {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (r, mut s) = (self, String::new());
        let p = &r.paths;
        let forced = r.findings.iter().filter(|f| f.label.starts_with("forced")).count();
        // Each check some judgment skipped: never a pass, never hidden.
        let skipped: Vec<String> = (r.judged.values().filter(|j| j.unjudged > 0))
            .map(|j| {
                let of = j.ok + j.failed + j.unjudged + j.idle;
                let why = j.why.as_deref().unwrap_or_default();
                format!("{} {} of {of} time(s) ({why})", j.check, j.unjudged)
            })
            .collect();
        let _ = match (r.findings.len(), r.todo.len()) {
            (0, 0) if !skipped.is_empty() => writeln!(
                s,
                "No violation in what was tried and no item open, but checks went unjudged \
                 (not judged, below)."
            ),
            (0, 0) => writeln!(s, "No violation in what was tried, and nothing open."),
            (0, t) => writeln!(s, "No violation in what was tried, with {t} item(s) open below."),
            (n, t) => writeln!(
                s,
                "{n} finding(s), {} shown and {forced} forced; {t} item(s) open.",
                n - forced
            ),
        };
        let lists: Vec<&String> = r.excused.iter().flat_map(|e| &e.2).collect();
        let lists: Vec<&&String> = lists.iter().filter(|id| id.starts_with("list:")).collect();
        if !lists.is_empty() {
            let ids = lists.iter().map(|x| x.as_str()).collect::<Vec<_>>().join(", ");
            let _ = writeln!(s, "Excused, lists no curve grew: {}", short(&ids, 110));
        }
        // Curves are named only by an audit that ran some.
        let ran_curves = r.ran_curves;
        let _ = writeln!(s, "{}", if ran_curves { GUARANTEE } else { "Only what was tried." });
        let (what, cases, n, curves, todo) =
            (&r.what, r.cases, r.findings.len(), r.curves.len(), r.todo.len());
        let rows = if ran_curves { format!(", {curves} curve row(s)") } else { String::new() };
        let unshrunk = r.findings.iter().filter(|f| !f.shrink.is_empty()).count();
        let unshrunk =
            if unshrunk > 0 { format!(" ({unshrunk} not fully shrunk)") } else { String::new() };
        let _ =
            writeln!(s, "{what}: {cases} case(s), {n} finding(s){unshrunk}{rows}, {todo} to do");
        let (reached, total, audited) =
            (p.total as usize - p.unreached.len(), p.total, p.audited.join(", "));
        let _ = writeln!(s, "coverage: {reached} of {total} exported methods ran ({audited})");
        if let Some(never) = &p.never_sent {
            // All of them in `Paths::never_sent`.
            let never =
                if never.is_empty() { "none".to_owned() } else { short(&never.join(", "), 300) };
            let _ = writeln!(s, "ran but never sent from axis.near: {never}");
        }
        if !p.left_out.is_empty() {
            let _ = writeln!(s, "left out of the audit: {}", p.left_out.join(", "));
        }
        for note in &r.notes {
            let _ = writeln!(s, "{}", short(note, 400));
        }
        if !skipped.is_empty() {
            let _ = writeln!(s, "not judged: {}", short(&skipped.join("; "), 200));
        }
        let (drawn, landed) = r.faults;
        if drawn > 0 {
            let _ = writeln!(s, "faults: {drawn} drawn, {landed} landed");
        }
        if drawn > 0 && landed == 0 {
            let _ = writeln!(
                s,
                "  none landed: only receipts a transaction spawns (continuations, callbacks) \
                 can be faulted; its first receipt fails by the caller's gas"
            );
        }
        if let Some(seed) = r.seeds.first() {
            let n = r.seeds.len();
            let _ = writeln!(s, "seed {seed:#x} (NEAR_SIM_SEED repeats it), {n} seed(s)");
        }
        // What is cut says how many, and where all of them are.
        let more = |s: &mut String, n: usize, shown: usize, what: &str| {
            if n > shown {
                let _ =
                    writeln!(s, "  ... {} more {what}; print it with {{:#?}} for all", n - shown);
            }
        };
        for (id, why, closed) in r.excused.iter().take(5) {
            let _ = writeln!(s, "excused {id} ({} item(s)): {}", closed.len(), short(why, 200));
        }
        more(&mut s, r.excused.len(), 5, "excuse(s) (Report::excused)");
        for band in r.bands.iter().take(8) {
            let _ = writeln!(s, "gas band: {}", short(band, 200));
        }
        more(&mut s, r.bands.len(), 8, "gas band(s) (Report::bands)");
        if !r.fixed.is_empty() {
            let names = short(&r.fixed.join(", "), 110);
            let _ = writeln!(s, "not axes (changed, they don't deserialize): {names}");
        }
        if !r.left_out_axes.is_empty() {
            let names = short(&r.left_out_axes.join(", "), 150);
            let _ = writeln!(s, "left out by Axes::only (not climbed): {names}");
        }
        for (k, f) in r.findings.iter().enumerate().take(8) {
            let also = if f.also.is_empty() {
                String::new()
            } else {
                format!(" (+{} folded in)", f.also.len())
            };
            let cut = if f.shrink.is_empty() { String::new() } else { format!("; {}", f.shrink) };
            let (n, kind, check, site, label) = (k + 1, &f.kind, &f.check, &f.site, &f.label);
            let _ = writeln!(s, "{n}. [{kind}] {check} at {site}: {label}{also}{cut}");
            let _ = writeln!(s, "   {}", f.message);
            if k < 3 {
                for (i, step) in f.steps.iter().enumerate().take(4) {
                    let _ = writeln!(s, "   step {i}: {}", short(step, 160));
                }
                more(&mut s, f.steps.len(), 4, &format!("step(s) in Report::findings[{k}].steps"));
                if let Some(path) = &f.replay {
                    let _ = writeln!(s, "   replay: {path}");
                }
            }
        }
        more(&mut s, r.findings.len(), 8, "finding(s) (Report::findings)");
        let rows: Vec<&Curve> = r.curves.iter().filter(|c| c.reach != Reach::Below).collect();
        for row in rows.iter().take(8) {
            let _ = writeln!(s, "curve: {}", short(&row.line, 200));
        }
        more(&mut s, rows.len(), 8, "curve row(s) (Report::curves)");
        // Unreached methods are one line: often dozens, closed by family;
        // so are incomplete cases.
        let (unreached, rest): (Vec<&Todo>, Vec<&Todo>) =
            r.todo.iter().partition(|t| t.id.starts_with("unreached:"));
        if let Some(line) = unreached_line(&unreached, 150) {
            let _ = writeln!(s, "{line}");
        }
        let (incomplete, rest): (Vec<&Todo>, Vec<&Todo>) =
            rest.into_iter().partition(|t| t.id.starts_with("incomplete:"));
        if !incomplete.is_empty() {
            let families: Vec<String> = (incomplete.iter())
                .filter_map(|t| t.id.strip_prefix("incomplete:"))
                .map(|f| format!("{f} ({})", r.incomplete.iter().filter(|e| e.0 == f).count()))
                .collect();
            let k = families.len();
            let _ = writeln!(
                s,
                "incomplete: {k} op and axis famil{} not tried to their end: {}",
                if k == 1 { "y" } else { "ies" },
                short(&families.join("; "), 200)
            );
        }
        // Stopped axes are one line per site and what refused them there.
        let (stops, rest): (Vec<&Todo>, Vec<&Todo>) =
            rest.into_iter().partition(|t| t.stop.is_some());
        let mut groups: Vec<(&(String, String), Vec<&str>)> = vec![];
        for t in &stops {
            let Some(key) = &t.stop else { continue };
            match groups.iter_mut().find(|g| g.0 == key) {
                Some(g) => g.1.push(&t.id),
                None => groups.push((key, vec![&t.id])),
            }
        }
        let open: Vec<&str> = r.todo.iter().map(|t| t.id.as_str()).collect();
        for ((site, cause), ids) in groups.iter().take(10) {
            let _ = writeln!(s, "{}", short(&stop_line(site, cause, ids, &open), 300));
        }
        more(&mut s, groups.len(), 10, "stopped site(s) (Report::todo)");
        for t in rest.iter().take(10) {
            let _ = writeln!(s, "TODO {}: {}", t.id, short(&t.why, 200));
        }
        more(&mut s, rest.len(), 10, "item(s) to do (Report::todo)");
        f.write_str(&s)
    }
}

/// One summary line for the `axis:` items `ids` stopped at `site` by
/// `cause`, with the excuse that closes exactly them among the `open`
/// items, if a guard refused them.
fn stop_line(site: &str, cause: &str, ids: &[&str], open: &[&str]) -> String {
    let n = ids.len();
    let axes: Vec<&str> = ids.iter().map(|id| id.strip_prefix("axis:").unwrap_or(id)).collect();
    let excuse = match excuse_for(ids, open) {
        Some(glob) => format!(".excuse(\"{glob}\", \"<the guard>\")"),
        None => "an excuse for each id".to_owned(),
    };
    format!(
        "stopped: {n} ax{} at `{site}` by {cause}: a guard? {excuse}; else fix the fixture or \
         Op::copy: {}",
        if n == 1 { "is" } else { "es" },
        axes.join("; ")
    )
}

/// A glob closing exactly `ids` among the `open` ids (which hold them),
/// if one is found: of ids as long as each other (one id alone, too),
/// each run of places they differ starred; else their common head and
/// tail around one star. Equal lengths never need the second: it hits
/// whatever the first does.
fn excuse_for(ids: &[&str], open: &[&str]) -> Option<String> {
    let chars: Vec<Vec<char>> = ids.iter().map(|id| id.chars().collect()).collect();
    let first = chars.first()?;
    let mut g = String::new();
    if chars.iter().all(|c| c.len() == first.len()) {
        for (k, &c) in first.iter().enumerate() {
            match chars.iter().all(|x| x[k] == c) {
                true => g.push(c),
                false if !g.ends_with('*') => g.push('*'),
                false => {}
            }
        }
    } else {
        let same = |a: &[char], b: &[char]| a.iter().zip(b).take_while(|(x, y)| x == y).count();
        let head = chars.iter().map(|c| same(c, first)).min().unwrap_or(0);
        let rev: Vec<Vec<char>> = chars.iter().map(|c| c.iter().rev().copied().collect()).collect();
        let tail = rev.iter().map(|c| same(c, &rev[0])).min().unwrap_or(0);
        // Head and tail may overlap in the shortest id: it then misses
        // the glob, which the check below refuses.
        g.extend(&first[..head]);
        g.push('*');
        g.extend(&first[first.len() - tail..]);
    }
    let hits = |id: &str| glob(g.as_bytes(), id.as_bytes());
    open.iter().all(|id| hits(id) == ids.contains(id)).then_some(g)
}

/// Every summary's second line.
const GUARANTEE: &str = "Only what was tried: a crossed curve is a finding where a check \
    fired in its run (else harmless), one below its limit is so only as far as it moved.";

/// `s` in at most `n` chars and ` ... `: its head and its tail, so a line
/// keeps both what it is (a verdict, an op) and how it ends (the failing
/// site, the fault, the reason).
pub(super) fn short(s: &str, n: usize) -> String {
    let len = s.chars().count();
    if len <= n {
        return s.to_owned();
    }
    let tail = n / 4;
    let head: String = s.chars().take(n - tail).collect();
    let tail: String = s.chars().skip(len - tail).collect();
    format!("{head} ... {tail}")
}

#[cfg(test)]
mod tests {
    /// A line cut short keeps its head and its tail: a gas
    /// band's failing receipt, a step's forced fault, an error's reason.
    #[test]
    fn a_cut_line_keeps_its_head_and_its_tail() {
        let band = format!(
            "Op(Deposit {{ {} }}): between, intents.near::ft_on_transfer fails",
            "x".repeat(300)
        );
        let cut = super::short(&band, 200);
        assert!(cut.starts_with("Op(Deposit { xxx"), "{cut}");
        assert!(cut.ends_with("between, intents.near::ft_on_transfer fails"), "{cut}");
        assert_eq!(cut.chars().count(), 200 + " ... ".len());
        assert_eq!(super::short("short", 20), "short");
    }
}
