//! [`Store`]: one replay file per row a campaign reports, and every
//! replay as a regression ([`Harness::regressions`]).
//!
//! A row's replay is its case as reported: shrunk, its faults dropped if
//! the finding shows without them. It holds the identity of each finding
//! folded into the row whose cause its case shows, and reads
//! [`Status::Shows`] while any of them shows. Another unforced cause of
//! the row's check, at its site and op, shares it (one bug, one row); any other
//! folded finding its case doesn't show keeps a replay of its own. Only a finished campaign writes replays, and
//! nothing deletes them: a campaign's `saves` file carries its rows'
//! replays until it finishes. [`Harness::run`] saves nothing.
//!
//! A file is `<check>@<site>-<hash>.json` under the store's directory,
//! the hash of all it holds (the finding's identities, the fixture's
//! fingerprint, the plan and the harness's checks), so a file is never
//! replaced by another, and campaigns sharing a store can't race:
//! `NEAR_SIM_FOUND_DIR`, or `near-contract-sim/found` in cargo's target
//! directory (`CARGO_TARGET_DIR` if set, else `target`), then the
//! test's name, unless [`Harness::store`] names one. It holds the plan as
//! it ran (a sampled fault resolved to its receipt), the finding's
//! identity, and a fingerprint of the fixture (state root and code).
//!
//! A replay belongs to the harness that saved it, as the checks that
//! harness had: one with every one of them (more is fine) judges it; any
//! other skips it, as another harness's sharing the directory, and
//! [`Harness::regressions`] says how many it skipped on stderr.
//!
//! A replay on a changed fixture still runs, so a fixed bug reads
//! [`Status::Gone`] and a path that is no longer there
//! [`Status::NotReached`]; [`Regression::fixture_changed`] flags it. A replay is
//! [`Status::Stale`] only when it can't be judged (its plan no longer reads
//! as this harness's op, its check is gone, or a harness error), never a panic.

use super::audit::{Audit, Found};
use super::check::site_is;
use super::run::{Id, Run, Verdict};
use super::{Harness, Op, Plan, World};
use crate::sim::Sim;
use near_primitives::hash::CryptoHash;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// A directory of replay files.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Store {
    dir: PathBuf,
}

/// One replay, reproduced on a harness.
#[derive(Debug)]
pub struct Regression<O> {
    /// Its file.
    pub path: PathBuf,
    /// The check and site of the finding it was saved for.
    pub check: String,
    /// The site, as it printed.
    pub site: String,
    /// Whether the fixture differs from the one it was saved on.
    pub fixture_changed: bool,
    /// Saved before replays kept their finding's identity: matched on
    /// check and site alone, so another cause there reads as this one
    /// ([`Harness::regressions`] says so on stderr). Regenerate it.
    pub by_site: bool,
    /// What reproducing it gave.
    pub status: Status<O>,
}

impl<O> Regression<O> {
    /// Whether the finding no longer shows on the fixture it was saved on:
    /// the contract is the same, so an op or a check changed, not the
    /// contract. A stale replay to delete or regenerate, not a fix;
    /// [`Harness::regressions`] warns of each on stderr.
    pub fn harness_changed(&self) -> bool {
        !self.fixture_changed && matches!(self.status, Status::Gone(_) | Status::NotReached(_))
    }
}

/// What a replay did when reproduced.
#[derive(Debug)]
pub enum Status<O> {
    /// The finding shows again: the bug is still there.
    Shows(Run<O>),
    /// The plan ran, its breaking receipts' sites ran too, and the
    /// finding didn't show: the check holds there.
    Gone(Run<O>),
    /// The finding didn't show, but a site of its breaking receipts never
    /// ran (or its method isn't there now), an op skipped, or a fault it
    /// forced fit no receipt: the path is absent, not shown fixed.
    NotReached(Run<O>),
    /// It can't be judged: why.
    Stale(String),
}

/// A replay file.
#[derive(Serialize, Deserialize)]
struct Saved<P> {
    /// Its format's version: 0 in a file written before versions.
    #[serde(default)]
    format: u32,
    check: String,
    site: String,
    label: String,
    fixture: String,
    plan: P,
    /// The sites of its breaking receipts: its own, else those of the
    /// last block before it.
    #[serde(default)]
    sites: BTreeSet<String>,
    /// The checks of the harness that saved it: none in an older file.
    #[serde(default)]
    checks: Vec<String>,
    /// The finding's identity (its cause, breaking ops, failed receipt and
    /// class, faults): none in an older file.
    #[serde(default)]
    id: Option<Id>,
    /// The identities of the findings folded into its row that its case
    /// shows.
    #[serde(default)]
    also: Vec<Id>,
}

/// Whether a replay saved by a harness with `saved` checks is `h`'s: `h`
/// has each of them. The checks `h` lacks otherwise.
fn foreign<O: Op<W>, W: World>(h: &Harness<O, W>, saved: &[String]) -> Vec<String> {
    let mine = h.check_names();
    saved.iter().filter(|c| !mine.contains(c)).cloned().collect()
}

impl<O: Op<W>, W: World> Harness<O, W> {
    /// Its store: [`store`](Harness::store)'s directory, else the running
    /// test's (the module docs).
    pub(crate) fn replays(&self) -> Store {
        self.store.clone().map_or_else(Store::here, Store::at)
    }

    /// Every replay in its [`store`](Harness::store) that is this
    /// harness's, reproduced on it: each a regression test. A replay is
    /// the harness's that saved it, by its checks: one lacking any of them
    /// skips it. One stderr line counts this harness's replays, other
    /// harnesses' and unreadable files. A fixed bug reads
    /// [`Status::Gone`], a path no longer there [`Status::NotReached`], a
    /// plan that no longer reads [`Status::Stale`];
    /// [`Regression::fixture_changed`] flags a changed fixture.
    ///
    /// An empty or missing [`store`](Harness::store) gives none: a clean
    /// campaign writes nothing. Without one, the store is the running
    /// test's, which each campaign of the test makes, so a missing one
    /// panics, naming it: a test that ran none reads another's replays
    /// with `.store(dir)`.
    pub fn regressions(&self) -> Vec<Regression<O>> {
        let store = self.replays();
        assert!(
            self.store.is_some() || store.dir.is_dir(),
            "no replays at {}: no campaign of this test saved there; without Harness::store, \
             the store is the running test's, so a test doesn't read another's replays; name \
             their directory with .store(dir)",
            store.dir.display()
        );
        store.regressions(self)
    }

    /// Saves `audit`'s findings to its store, and a resumed campaign's
    /// saved rows' (its saves file carries them), and lists the store's
    /// replays that can't be judged now. One it can't write is a
    /// `replays:` item.
    pub(crate) fn save(&self, audit: &mut Audit<O>) {
        let what = || super::progress::phase("settling the findings".into(), 0, None);
        let phase = (!audit.found.is_empty()).then(what);
        let store = self.replays();
        let mut failed: Option<String> = None;
        let mut put = |path: &Path, text: &str| match store.put(path, text) {
            Ok(()) => true,
            Err(e) => {
                eprintln!("replay not saved: {}: {e}", path.display());
                failed.get_or_insert(e);
                false
            }
        };
        self.save_found(audit, phase.as_ref(), &mut |x| {
            let (path, text) = store.replay(self, x);
            put(&path, &text).then_some(path)
        });
        // In this store, wherever the saved part's was.
        let here = |path: &str| store.dir.join(Path::new(path).file_name().unwrap_or_default());
        for row in audit.prior.iter_mut().flat_map(|p| p.findings.iter_mut()) {
            for (path, text) in std::mem::take(&mut row.saved) {
                let (wrote, now) = (put(&here(&path), &text), here(&path).display().to_string());
                if row.replay.as_ref() == Some(&path) {
                    row.replay = wrote.then(|| now.clone());
                }
                // A folded finding's own replay, named in `also` only if
                // written, as a fold's whose save failed.
                let cited = if wrote { format!(" ({now})") } else { String::new() };
                row.also.iter_mut().for_each(|a| *a = a.replace(&format!(" ({path})"), &cited));
            }
        }
        // The running test's store is made, so its `regressions()` reads it.
        if self.store.is_none() {
            let _ = std::fs::create_dir_all(&store.dir);
        }
        let dir = store.dir.display().to_string();
        audit.unsaved.extend(failed.map(|why| (dir, why)));
        audit.stale = store.stale(self);
    }

    /// Settles `audit`'s findings (counted by `phase`) and gives each row
    /// the replay `save` makes of it, and each folded finding its case
    /// doesn't show its own: what a campaign's saves file needs as it
    /// goes, and what its end writes.
    pub(crate) fn save_found(
        &self,
        audit: &mut Audit<O>,
        phase: Option<&super::progress::Phase>,
        save: &mut dyn FnMut(&Found<O>) -> Option<PathBuf>,
    ) {
        self.settle_found(audit, phase);
        super::audit::fold_causes(audit, save);
        for x in &mut audit.found {
            x.replay = save(x);
        }
        self.cover(audit);
        super::curves::link_replays(audit);
    }
}

/// Where [`Store::here`] keeps replays: `found` as it is; else
/// `near-contract-sim/found` in `target` (a relative one from the manifest
/// directory, as cargo resolves it from the package's root); else in the
/// manifest's (or the temp directory's) `target`.
fn found_root(
    found: Option<OsString>,
    target: Option<OsString>,
    manifest: Option<OsString>,
) -> PathBuf {
    if let Some(found) = found {
        return found.into();
    }
    let manifest = manifest.map_or_else(std::env::temp_dir, PathBuf::from);
    // `join` of an absolute path is that path.
    let target = target.map_or_else(|| manifest.join("target"), |t| manifest.join(t));
    target.join("near-contract-sim/found")
}

impl Store {
    /// The store at `dir`.
    pub(crate) fn at(dir: impl Into<PathBuf>) -> Store {
        Store { dir: dir.into() }
    }

    /// The running test's store, as the module docs say; the test's name is
    /// libtest's thread's.
    pub(crate) fn here() -> Store {
        let var = std::env::var_os;
        let root = found_root(
            var("NEAR_SIM_FOUND_DIR"),
            var("CARGO_TARGET_DIR"),
            var("CARGO_MANIFEST_DIR"),
        );
        let test =
            std::thread::current().name().filter(|n| *n != "main").unwrap_or("unnamed").to_owned();
        Store::at(root.join(file_name(&test)))
    }

    /// Row `x`'s replay: its file and what it holds.
    pub(crate) fn replay<O: Op<W>, W: World>(
        &self,
        h: &Harness<O, W>,
        x: &Found<O>,
    ) -> (PathBuf, String) {
        let Found { finding: f, label, id, run, shows, .. } = x;
        let plan = &run.plan;
        let saved = Saved {
            format: super::FORMAT,
            check: f.check.clone(),
            site: f.site.to_string(),
            label: label.to_string(),
            fixture: fingerprint(h.fixture.as_ref()),
            plan,
            sites: sites(run, Some(f)),
            checks: h.check_names(),
            id: Some(id.clone()),
            also: {
                // In one order: the same replay is the same file.
                let mut also = shows.clone();
                also.sort_by_cached_key(|id| format!("{id:?}"));
                also
            },
        };
        let name = file_name(&format!("{}@{}", f.check, f.site));
        let text = serde_json::to_string_pretty(&saved).expect("a replay serializes");
        // Keyed by all it holds: a file a report cites keeps reproducing
        // what it printed, whatever campaign or harness writes after it; a
        // replay saved again writes the same file, the same bytes.
        let path = self.dir.join(format!("{name}-{}.json", short_hash(text.as_bytes())));
        (path, text)
    }

    /// Writes `text` at `path` in this store, whole: a reader sees the
    /// file as it was or as it is, never part of it.
    pub(crate) fn put(&self, path: &Path, text: &str) -> Result<(), String> {
        static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let tmp = path.with_extension(format!("{}-{n}.tmp", std::process::id()));
        std::fs::create_dir_all(&self.dir)
            .and_then(|()| std::fs::write(&tmp, text))
            .and_then(|()| std::fs::rename(&tmp, path))
            .map_err(|e| {
                let _ = std::fs::remove_file(&tmp);
                e.to_string()
            })
    }

    /// Every replay in the store that is `harness`'s, reproduced on it.
    pub(crate) fn regressions<O: Op<W>, W: World>(
        &self,
        harness: &Harness<O, W>,
    ) -> Vec<Regression<O>> {
        let (mut skipped, mut unreadable, mut lacks) = (0, 0, BTreeSet::new());
        let mine = |path: &PathBuf| {
            let Some(head) = read_head(path) else {
                unreadable += 1;
                return false;
            };
            let missing = foreign(harness, &head.checks);
            skipped += usize::from(!missing.is_empty());
            lacks.extend(missing.iter().cloned());
            missing.is_empty()
        };
        let files: Vec<PathBuf> = self.files().into_iter().filter(mine).collect();
        let lacks: Vec<String> = lacks.into_iter().map(|c| format!("{c:?}")).collect();
        let lacks =
            if lacks.is_empty() { String::new() } else { format!(" lacking {}", lacks.join(", ")) };
        let (n, dir) = (files.len(), self.dir.display());
        eprintln!(
            "{n} replays of this harness in {dir} ({skipped} other harnesses'{lacks}, \
             {unreadable} unreadable)"
        );
        let regressions = files.into_iter().filter_map(|path| regression(&path, harness));
        regressions
            .inspect(|r| {
                if r.by_site {
                    eprintln!(
                        "{}: saved before identities, matched on its check and site alone; \
                         regenerate it",
                        r.path.display()
                    );
                }
                if r.harness_changed() {
                    eprintln!(
                        "{}: {} no longer shows on the fixture it was saved on: its op or check \
                         changed, not the contract; delete or regenerate the replay",
                        r.path.display(),
                        r.check
                    );
                }
            })
            .collect()
    }

    /// Why the replay at `path` can't be judged on `harness` now (its plan
    /// no longer reads as the op type's, or its check is gone), unrun;
    /// `None` if it can, or isn't a replay.
    pub(crate) fn unjudgeable<O: Op<W>, W: World>(
        harness: &Harness<O, W>,
        path: &Path,
    ) -> Option<String> {
        read::<O, W>(path, harness)?.1.err()
    }
}

/// The replay at `path`, reproduced on `harness`; `None` if not a replay.
/// It is gone only where its saved path ran again: every site of its
/// breaking receipts ran (it saved some), no op skipped, and every fault
/// it forced landed.
fn regression<O: Op<W>, W: World>(path: &Path, harness: &Harness<O, W>) -> Option<Regression<O>> {
    let fixture = fingerprint(harness.fixture.as_ref());
    let (head, plan) = read::<O, W>(path, harness)?;
    let status = match plan {
        Err(why) => Status::Stale(why),
        Ok(plan) => {
            let run = harness.run_case(&plan);
            // Its identities, where it was saved: one of two causes at its
            // check and site fixed reads fixed; a finding folded into its
            // row that its case showed, not fixed, still shows.
            let matches = |f: &crate::findings::Finding| match &head.id {
                Some(id) => {
                    let found = super::run::id(&run, f);
                    found == *id || head.also.contains(&found)
                }
                None => f.check == head.check && site_is(&f.site, &head.site),
            };
            // Its path ran again: its sites, every op, and every fault it
            // forced, on a receipt (one that fits none forced nothing).
            let injections = plan.steps.iter().map(|s| s.inject.len()).sum::<usize>();
            let ran = head.sites.is_subset(&sites(&run, None))
                && run.ops.iter().all(|o| o.skipped.is_none())
                && run.landed.len() == injections;
            if run.verdict == Verdict::HarnessError {
                let why = run.reason.clone().unwrap_or_default();
                Status::Stale(format!("a harness error: {why}"))
            } else if run.findings.iter().any(matches) {
                Status::Shows(run)
            } else if ran {
                Status::Gone(run)
            } else {
                Status::NotReached(run)
            }
        }
    };
    let (check, site, fixture_changed) = (head.check, head.site, head.fixture != fixture);
    let by_site = head.id.is_none();
    Some(Regression { path: path.to_owned(), check, site, fixture_changed, by_site, status })
}

/// A replay file's contents, its plan unread; `None` if not a replay.
fn read_head(path: &Path) -> Option<Saved<serde_json::Value>> {
    serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()
}

/// A replay's head and plan, or why it can't be judged; `None` if not one.
#[allow(clippy::type_complexity)]
fn read<O: Op<W>, W: World>(
    path: &Path,
    harness: &Harness<O, W>,
) -> Option<(Saved<serde_json::Value>, Result<Plan<O>, String>)> {
    let head = read_head(path)?;
    let plan = Plan::<O>::from_json(&head.plan.to_string());
    let plan = match plan.map_err(|why| format!("its plan no longer reads: {why}")) {
        _ if head.format > super::FORMAT => Err(format!(
            "written in format {}, newer than this build's {}",
            head.format,
            super::FORMAT
        )),
        // Its identity may read otherwise now: judged, it could read Gone.
        _ if head.format < super::FORMAT => Err(format!(
            "written in format {}, an older identity scheme than this build's {}; rerun the \
             campaign",
            head.format,
            super::FORMAT
        )),
        _ if !harness.check_names().contains(&head.check) => {
            Err(format!("the harness has no check {:?} now", head.check))
        }
        plan => plan,
    };
    Some((head, plan))
}

impl Store {
    /// The replay files, sorted.
    fn files(&self) -> Vec<PathBuf> {
        let entries = std::fs::read_dir(&self.dir).into_iter().flatten().flatten();
        let mut files: Vec<PathBuf> = (entries.map(|e| e.path()))
            .filter(|p| p.extension().is_some_and(|e| e == "json"))
            .collect();
        files.sort();
        files
    }

    /// The replays of `harness`'s that can't be judged on it (path, why),
    /// unrun.
    pub(crate) fn stale<O: Op<W>, W: World>(
        &self,
        harness: &Harness<O, W>,
    ) -> Vec<(String, String)> {
        let stale = |p: PathBuf| {
            let (head, plan) = read(&p, harness)?;
            let why = plan.err().filter(|_| foreign(harness, &head.checks).is_empty())?;
            Some((p.display().to_string(), why))
        };
        self.files().into_iter().filter_map(stale).collect()
    }
}

/// The sites (`receiver::method`) of `f`'s breaking receipts in `run`: its
/// own receipt's, else those of the last block at or before it that ran a
/// method; with no `f`, every site `run` ran.
fn sites<O>(run: &Run<O>, f: Option<&crate::findings::Finding>) -> BTreeSet<String> {
    // A call whose method or code isn't there (another build) reached
    // nothing.
    let absent = |r: &crate::ReceiptOutcome| {
        use near_primitives::errors::{ActionErrorKind as A, FunctionCallError as F};
        matches!(
            r.failure(),
            Some(crate::Failure::Action(A::FunctionCallError(
                F::MethodResolveError(_) | F::CompilationError(_)
            )))
        )
    };
    let calls = || {
        (run.outcomes.iter().flat_map(|o| &o.receipts))
            .filter(|r| !r.method_names.is_empty() && !absent(r))
    };
    let last = calls().map(|r| r.block_height).filter(|&b| f.is_none_or(|f| b <= f.block)).max();
    let at = |r: &&crate::ReceiptOutcome| {
        f.is_none_or(|f| f.receipt_id.map_or(Some(r.block_height) == last, |id| r.receipt_id == id))
    };
    calls().filter(at).map(crate::ReceiptOutcome::site).collect()
}

/// Where a campaign saves its report as it goes (`saves(path, every)` on
/// a scan, a search or curves): a [`Report`](super::Report) with the units
/// done and the fixture's fingerprint, which `Report::from_json` reads.
pub(crate) struct Saves {
    pub(crate) path: PathBuf,
    pub(crate) every: usize,
    /// What else of the campaign a file must match: an enumeration's ops,
    /// first ops and faults ([`scope`](Self::scope)); empty for any other.
    scope: String,
    failed: std::cell::Cell<bool>,
}

impl Saves {
    pub(crate) fn new(path: PathBuf, every: usize) -> Saves {
        Saves { path, every: every.max(1), scope: String::new(), failed: Default::default() }
    }

    /// A file resumes only a campaign with this `part` too, `name`d.
    pub(crate) fn scope(&mut self, name: &str, part: &impl serde::Serialize) {
        let part = serde_json::to_vec(part).unwrap_or_default();
        self.scope = format!("{name} {}", short_hash(&part));
    }

    /// The report saved for this fixture, these checks, this campaign's
    /// [`scope`](Self::scope) and this build of the test, if any. One of
    /// another fixture, check set, scope or build, or unreadable, is left
    /// to be overwritten: the campaign starts over, and says so in one
    /// line.
    fn load<O: Op<W>, W: World>(&self, h: &Harness<O, W>) -> Option<super::Report> {
        let text = std::fs::read_to_string(&self.path).ok()?;
        let fixture = fingerprint(h.fixture.as_ref());
        let (checked, now) = (campaign(h, ""), campaign(h, &self.scope));
        let why = match super::Report::from_json(&text) {
            Ok(r) => match mismatch((&r.fixture, &r.build), [&fixture, &checked, &now], &build()) {
                None => return Some(r),
                Some(why) => why.to_owned(),
            },
            Err(e) => format!("not a report ({e})"),
        };
        eprintln!("{}: {why}: starting over", self.path.display());
        None
    }

    /// `audit` resumed from the report this build saved for `h`'s fixture
    /// and checks, if any: the saved part stands, and the units it had
    /// done, returned, don't run again.
    pub(crate) fn resume<O: Op<W>, W: World>(
        saves: &Option<Saves>,
        h: &Harness<O, W>,
        audit: &mut Audit<O>,
    ) -> usize {
        let Some(saved) = saves.as_ref().and_then(|s| s.load(h)) else { return 0 };
        let done = saved.done;
        audit.prior = Some(Box::new(saved));
        done
    }

    /// Writes `audit` after `done` units, at the `end` or every `every`.
    pub(crate) fn tick<O: Op<W>, W: World>(
        saves: &Option<Saves>,
        h: &Harness<O, W>,
        audit: &Audit<O>,
        done: usize,
        end: bool,
    ) {
        if let Some(s) = saves.as_ref().filter(|s| end || done.is_multiple_of(s.every)) {
            s.write(h, audit, done);
        }
    }

    /// Writes `audit` with `done` units done, creating its directory; a
    /// failure is reported once. Each row carries its replays, which the
    /// campaign's end writes to the store.
    fn write<O: Op<W>, W: World>(&self, h: &Harness<O, W>, audit: &Audit<O>, done: usize) {
        let mut snap = audit.clone();
        let (store, mut replays) = (h.replays(), std::collections::HashMap::new());
        h.save_found(&mut snap, None, &mut |x| {
            let (path, text) = store.replay(h, x);
            replays.insert(path.clone(), text);
            Some(path)
        });
        let mut report = snap.report();
        // Each row of its own carries its replays (a resumed one, its own).
        for x in &snap.found {
            let at = |p: &PathBuf| p.display().to_string();
            let mine = |r: &&mut super::report::Row| r.replay == x.replay.as_ref().map(at);
            let Some(row) = report.findings.iter_mut().find(mine) else { continue };
            for p in x.replay.iter().chain(&x.kept) {
                let Some(text) = replays.get(p) else { continue };
                if !row.saved.iter().any(|s| s.0 == at(p)) {
                    row.saved.push((at(p), text.clone()));
                }
            }
        }
        (report.done, report.fixture, report.build) = (done, campaign(h, &self.scope), build());
        let dir = self.path.parent().filter(|d| !d.as_os_str().is_empty());
        let wrote = dir.map_or(Ok(()), std::fs::create_dir_all).and_then(|()| {
            std::fs::write(&self.path, serde_json::to_string(&report).unwrap_or_default())
        });
        match wrote {
            Err(e) if !self.failed.replace(true) => {
                eprintln!("not saving to {}: {e}", self.path.display())
            }
            _ => {}
        }
    }
}

/// The fixture's fingerprint: its state root and every contract's code.
fn fingerprint(sim: &Sim) -> String {
    let mut parts = sim.state_root().to_string();
    // Sorted: the key map's order is not stable across runs.
    let mut ids: Vec<_> = sim.keys.keys().collect();
    ids.sort();
    for id in ids {
        if let Some(a) = sim.account(id).filter(|a| a.code_hash != CryptoHash::default()) {
            parts += &format!("{id}={}", a.code_hash);
        }
    }
    short_hash(parts.as_bytes())
}

/// What a saved campaign is resumed on: the fixture's fingerprint, the
/// harness's checks and the campaign's `scope` if any; [`build`] must
/// match too.
fn campaign<O: Op<W>, W: World>(h: &Harness<O, W>, scope: &str) -> String {
    let checks = format!("{:?}", h.check_names());
    let at = format!("{} {}", fingerprint(h.fixture.as_ref()), short_hash(checks.as_bytes()));
    if scope.is_empty() { at } else { format!("{at} {scope}") }
}

/// Why a file saved for campaign `saved.0` by build `saved.1` doesn't
/// resume this one on this `build`, if it doesn't: `now` is this
/// fixture's fingerprint, that with the checks, and that with the scope
/// ([`campaign`]).
fn mismatch(saved: (&str, &str), now: [&str; 3], build: &str) -> Option<&'static str> {
    let ((at, by), [fixture, checked, scoped]) = (saved, now);
    match () {
        () if at == scoped && by == build => None,
        () if at == scoped => Some("saved by another build of the test"),
        () if at.starts_with(checked) => {
            Some("saved for another enumeration (its ops, first ops or faults)")
        }
        () if at.starts_with(fixture) => Some("saved by a harness with other checks"),
        () => Some("saved for another fixture"),
    }
}

/// The build of the running test: its executable's size and time.
fn build() -> String {
    static BUILD: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    BUILD
        .get_or_init(|| {
            let exe = std::env::current_exe().and_then(std::fs::metadata);
            let at = exe.as_ref().ok().and_then(|m| m.modified().ok());
            short_hash(format!("{:?} {at:?}", exe.map(|m| m.len()).ok()).as_bytes())
        })
        .clone()
}

/// The first 4 bytes of the sha256 of `bytes`, as hex.
fn short_hash(bytes: &[u8]) -> String {
    CryptoHash::hash_bytes(bytes).as_ref()[..4].iter().map(|b| format!("{b:02x}")).collect()
}

/// `withdraw delivers@Withdraw` -> `withdraw-delivers@Withdraw`: letters,
/// digits and `._@` kept, runs of anything else one `-`.
fn file_name(s: &str) -> String {
    let mut out = String::new();
    for c in s.chars() {
        if c.is_ascii_alphanumeric() || "._@".contains(c) {
            out.push(c);
        } else if !out.ends_with('-') {
            out.push('-');
        }
    }
    out.trim_matches('-').to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `NEAR_SIM_FOUND_DIR` wins; else `CARGO_TARGET_DIR` (absolute, or
    /// relative to the manifest); else the manifest's `target`.
    #[test]
    fn replays_go_in_the_found_dir_else_the_cargo_target_dir() {
        let os = |s: &str| Some(OsString::from(s));
        let root = |f, t, m| found_root(f, t, m);
        assert_eq!(root(os("/f"), os("/t"), os("/m")), PathBuf::from("/f"));
        assert_eq!(root(None, os("/t"), os("/m")), PathBuf::from("/t/near-contract-sim/found"));
        assert_eq!(root(None, os("t2"), os("/m")), PathBuf::from("/m/t2/near-contract-sim/found"));
        assert_eq!(root(None, None, os("/m")), PathBuf::from("/m/target/near-contract-sim/found"));
    }

    /// A saved campaign resumes only on its fixture, checks, scope (an
    /// enumeration's ops, first ops and faults; none for another verb)
    /// and build, and each other one says which differs.
    #[test]
    fn a_saved_campaign_resumes_only_where_all_of_it_matches() {
        let why = |at, by, scoped| mismatch((at, by), ["f", "f c", scoped], "b");
        assert_eq!(why("f c enumeration x", "b", "f c enumeration x"), None);
        assert_eq!(why("f c", "b", "f c"), None);
        let build = Some("saved by another build of the test");
        assert_eq!(why("f c enumeration x", "a", "f c enumeration x"), build);
        let other = Some("saved for another enumeration (its ops, first ops or faults)");
        assert_eq!(why("f c enumeration y", "b", "f c enumeration x"), other);
        assert_eq!(why("f c", "b", "f c enumeration x"), other);
        assert_eq!(why("f c enumeration x", "b", "f c"), other);
        let checks = Some("saved by a harness with other checks");
        assert_eq!(why("f d", "b", "f c"), checks);
        assert_eq!(why("f d enumeration x", "b", "f c enumeration x"), checks);
        assert_eq!(why("g c", "b", "f c"), Some("saved for another fixture"));
    }
}
