//! The executor: [`Harness::run`] plays one [`Plan`] on a clone of the
//! fixture, block by block, judging every checkpoint; [`Run`], what it
//! did.
//!
//! A case never stops at a finding: only a harness error or a block budget
//! ends it early. After each block the built-ins and `always` checks run,
//! `at_rest` ones if nothing is in flight, and each op whose transactions
//! just became complete gets its per-op checks. At the end the chain
//! settles, parked yields time out in timeout order, it settles again, and
//! the end checks run; an op still incomplete then is pending, not judged.

use super::check::{Harness, OpDone, RestFn};
use super::plan::{Act, Gap, Inject, Plan, Step};
use super::{Call, Fail, Op, World};
use crate::effects::Effects;
use crate::explore::builtin;
use crate::fault::Spawned;
use crate::findings::{Finding, Site, forced, forced_from, guarded};
use crate::inject::{Fault, Receipts};
use crate::outcome::{Failure, ReceiptOutcome, TxOutcome, name_trap};
use crate::sim::Sim;
use crate::trap;
use near_primitives::hash::{CryptoHash, hash};
use near_primitives::transaction::ExecutionStatus;
use near_primitives::types::{AccountId, Balance, Gas};
use near_primitives::views::FinalExecutionStatus;
use serde::{Deserialize, Serialize};
use std::any::Any;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::fmt;

/// How a case ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Verdict {
    /// Every check judged every time it ran and passed; nothing pending.
    Clean,
    /// A check failed.
    Findings,
    /// No check failed, but the case didn't establish that none would: a
    /// block budget ran out, an op stayed pending or skipped, a check
    /// couldn't judge, an injection the plan names never applied.
    Incomplete,
    /// The harness, the plan or an op is wrong.
    HarnessError,
}

/// `clean`, `findings`, `incomplete`, `harness error`: a run's `Display`
/// prints it with its reason.
impl fmt::Display for Verdict {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Verdict::Clean => "clean",
            Verdict::Findings => "findings",
            Verdict::Incomplete => "incomplete",
            Verdict::HarnessError => "harness error",
        })
    }
}

/// How often one check judged in a case.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Judged {
    /// The check's name.
    pub check: String,
    /// Times it passed.
    pub ok: u32,
    /// Times it failed.
    pub failed: u32,
    /// Times it couldn't judge.
    pub unjudged: u32,
    /// Times nothing was at stake ([`Fail::idle`](super::Fail::idle)).
    pub idle: u32,
    /// Why, the first time it couldn't judge.
    pub why: Option<String>,
    /// Why, the first time nothing was at stake.
    #[serde(default)]
    pub idle_why: Option<String>,
}

/// One plan step as it ran.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub(crate) struct OpRecord {
    pub name: String,
    pub txs: Vec<CryptoHash>,
    pub skipped: Option<String>,
    /// How many of its transactions' calls had unpinned gas.
    pub unpinned: u32,
    /// Its gas bands ([`Hook::bands`](super::Hook)).
    #[serde(default)]
    pub bands: Vec<(Gas, Gas, String)>,
    /// What ran, where the step's act doesn't say it: a copy's.
    #[serde(default)]
    pub ran: Option<String>,
    /// The height it took effect at: the first checkpoint after it
    /// submitted (for a transaction op, the next block, usually where it
    /// was included; for an op with no transaction, the checkpoint that
    /// judged the world it left, though submitted with a call in flight it
    /// can share it with that call, or follow it). `None` if skipped, or
    /// if no checkpoint followed.
    #[serde(default)]
    pub at: Option<u64>,
}

/// What a raw call copying an op runs: the op's copy itself, or the call
/// with the args the copy makes it carry, and the copy.
enum CopyRun<O> {
    Op(O),
    Call(Call, O),
}

/// Where a plan's injection landed: the fault, and its receipt's site.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub(crate) struct Landed {
    pub fault: Fault,
    pub site: String,
}

/// A case's world as it ended, whatever its type. Moved out of the case,
/// not cloned; a clone of the run shares it.
#[derive(Clone)]
pub(crate) struct Ended(std::rc::Rc<dyn Any>, u64);

impl fmt::Debug for Ended {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "world at block {}", self.1)
    }
}

/// How a per-op check not judged because its op overlapped others says so.
pub(crate) const OVERLAPPED: &str = "overlapped:";

/// One case, as it ran.
#[must_use = "a run fails nothing unless asserted: .assert_clean()"]
#[derive(Clone, Debug)]
pub struct Run<O> {
    pub(crate) plan: Plan<O>,
    pub(crate) verdict: Verdict,
    pub(crate) reason: Option<String>,
    pub(crate) findings: Vec<Finding>,
    pub(crate) judged: Vec<Judged>,
    pub(crate) ops: Vec<OpRecord>,
    /// Every step's transactions' outcomes, in submit order.
    pub(crate) outcomes: Vec<TxOutcome>,
    pub(crate) landed: Vec<Landed>,
    /// Per step, per transaction: the receipts it spawned and those
    /// congestion control held back.
    pub(crate) spawned: Vec<Vec<(Vec<Spawned>, Vec<Spawned>)>>,
    /// Sampled injections nothing fit, dropped.
    pub(crate) untargeted: Vec<String>,
    pub(crate) budget: bool,
    /// It ran past the case timeout and was cut short.
    pub(crate) timed_out: bool,
    /// Blocks it produced.
    pub(crate) blocks: u64,
    pub(crate) panicked: Option<String>,
    /// A check failed on the fixture: every case from it would repeat it,
    /// so a campaign stops there.
    pub(crate) on_fixture: bool,
    pub(crate) fingerprint: CryptoHash,
    /// Every receipt site it reached, `receiver::method`.
    pub(crate) reached: BTreeSet<String>,
    /// Each failed receipt's site, a drawn account as `<new account>`.
    pub(crate) failed: HashMap<CryptoHash, String>,
    /// Its curve points ([`Curves::with`](super::Curves::with)).
    pub(crate) seen: super::curves::Seen,
    pub(crate) world: Ended,
    /// Incomplete only because checks couldn't judge: every other part
    /// of the case ran and judged.
    pub(crate) unjudged_only: bool,
    /// Incomplete only because per-op checks read the shared world while
    /// their ops overlapped: every other part of the case judged.
    pub(crate) overlapped_only: bool,
}

impl<O> Run<O> {
    /// The outcomes of step `i`'s transactions.
    pub(crate) fn step_txs(&self, i: usize) -> impl Iterator<Item = &TxOutcome> {
        let hashes = self.ops.get(i).map(|r| r.txs.clone()).unwrap_or_default();
        self.outcomes.iter().filter(move |o| hashes.contains(&o.hash))
    }

    /// The plan as it ran: a sampled injection resolved to the receipt it
    /// landed on.
    pub fn plan(&self) -> &Plan<O> {
        &self.plan
    }

    /// How the case ended.
    pub fn verdict(&self) -> Verdict {
        self.verdict
    }

    /// Why it isn't clean, on one line; `None` only when it is: a harness
    /// error, the first finding and how many more, or what kept it from
    /// completing.
    pub fn reason(&self) -> Option<&str> {
        self.reason.as_deref()
    }

    /// Its findings.
    pub fn findings(&self) -> &[Finding] {
        &self.findings
    }

    /// How often each check judged.
    pub fn judged(&self) -> &[Judged] {
        &self.judged
    }

    /// Every step's transactions' outcomes, in submit order.
    pub fn outcomes(&self) -> &[TxOutcome] {
        &self.outcomes
    }

    /// The blocks it produced (heights it skipped while yields waited out
    /// their timeout aside).
    pub fn blocks(&self) -> u64 {
        self.blocks
    }

    /// Every receipt site it reached (`receiver::method`).
    pub fn reached(&self) -> &BTreeSet<String> {
        &self.reached
    }

    /// The world as the case left it, if it is a `W` (the harness's world
    /// type: a [`Sim`] or your own struct), to view what it ended with:
    /// `None` for any other type. Name the type:
    /// `run.world::<Sim>().expect("a Sim harness")`.
    pub fn world<W: 'static>(&self) -> Option<&W> {
        self.world.0.downcast_ref()
    }

    /// The finding of `check` at `at` (a site as [`Audit::found_at`](super::Audit::found_at)
    /// matches one, with or without its ` (Op)`), if found.
    pub fn found_at(&self, check: &str, at: &str) -> Option<&Finding> {
        self.findings.iter().find(|f| f.check == check && super::check::site_is(&f.site, at))
    }

    /// [`found_at`](Self::found_at), or a panic printing the run.
    #[track_caller]
    pub fn assert_found(&self, check: &str, at: &str) -> &Finding
    where
        O: fmt::Debug,
    {
        let Some(f) = self.found_at(check, at) else {
            panic!("no finding of {check:?} at {at:?}:\n{self}")
        };
        f
    }

    /// Every step's transactions as receipt trees, in plan order, whatever
    /// the verdict: what the case did (its `Display` draws only the
    /// findings' transactions). A skipped op says why.
    pub fn trees(&self) -> String
    where
        O: fmt::Debug,
    {
        let mut out = String::new();
        for (i, (step, op)) in self.plan.steps.iter().zip(&self.ops).enumerate() {
            out += &format!("step {i}: {}", label(&step.act));
            match &op.skipped {
                Some(why) => out += &format!(" [skipped: {why}]\n"),
                None if op.txs.is_empty() => out += " [no transaction]\n",
                None => {
                    for tx in &op.txs {
                        let o = self.outcomes.iter().find(|o| o.hash == *tx);
                        out +=
                            &o.map_or_else(|| format!("\n\n{tx}: never ran"), crate::render::trace);
                    }
                    out += "\n\n";
                }
            }
        }
        out
    }

    /// Its verdict is [`Verdict::Clean`]: every check judged and passed,
    /// nothing pending ([`Audit::is_clean`](super::Audit::is_clean) for a
    /// campaign).
    pub fn is_clean(&self) -> bool {
        self.verdict == Verdict::Clean
    }

    /// Panics, printing the run, unless it [`is_clean`](Self::is_clean).
    #[track_caller]
    pub fn assert_clean(&self)
    where
        O: fmt::Debug,
    {
        if !self.is_clean() {
            panic!("not clean:\n{self}");
        }
    }

    /// Which recorded result differs from `other`'s, if any: a run of the
    /// same plan on the same fixture runs the same way.
    pub(crate) fn differs_from(&self, other: &Run<O>) -> Option<&'static str> {
        ((self.fingerprint != other.fingerprint).then_some("outcomes"))
            .or((self.findings != other.findings).then_some("findings"))
            .or((self.judged != other.judged).then_some("judgments"))
    }
}

/// The verdict, the reason, each finding with its transaction's tree.
impl<O: fmt::Debug> fmt::Display for Run<O> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.reason {
            Some(why) => writeln!(f, "verdict: {}: {why}", self.verdict)?,
            None => writeln!(f, "verdict: {}", self.verdict)?,
        }
        for (i, s) in self.plan.steps.iter().enumerate() {
            let ran = self.ops.get(i).and_then(|o| o.ran.clone());
            let what = ran.unwrap_or_else(|| label(&s.act));
            let skipped = (self.ops.get(i).and_then(|o| o.skipped.as_ref()))
                .map_or(String::new(), |why| format!(" [skipped: {why}]"));
            let gas = s.gas.map(|g| format!(", gas {g}")).unwrap_or_default();
            let inject = s.inject.iter().map(ToString::to_string).collect::<Vec<_>>().join(", ");
            let inject = if inject.is_empty() { inject } else { format!(", inject {inject}") };
            writeln!(f, "  step {i}: {what}{skipped}, then {:?}{gas}{inject}", s.then)?;
        }
        for why in &self.untargeted {
            writeln!(f, "  no receipt fits {why}: ran without it")?;
        }
        for x in &self.findings {
            let forced = if x.injected { " (forced)" } else { "" };
            writeln!(f, "finding at {}{forced}: {x}", x.site)?;
        }
        for j in &self.judged {
            writeln!(
                f,
                "  {}: {} ok, {} failed, {} not judged, {} idle",
                j.check, j.ok, j.failed, j.unjudged, j.idle
            )?;
        }
        let traced: BTreeSet<CryptoHash> = self.findings.iter().filter_map(|x| x.tx).collect();
        for o in self.outcomes.iter().filter(|o| traced.contains(&o.hash)) {
            write!(f, "{}", crate::render::trace(o))?;
        }
        Ok(())
    }
}

impl<O: fmt::Debug> Run<O> {
    /// Each step on one line, as it ran (a copy's act as the copy ran).
    pub(crate) fn lines(&self) -> Vec<String> {
        let ran = |i: usize| self.ops.get(i).and_then(|o| o.ran.as_deref());
        self.plan.steps.iter().enumerate().map(|(i, s)| s.line_as(ran(i))).collect()
    }
}

/// A step's act on one line: the op, compacted, or the call.
fn label<O: fmt::Debug>(act: &Act<O>) -> String {
    match act {
        Act::Op(op) => super::input::compact(&format!("{op:?}")),
        Act::Call(c) => format!("call {}::{}", c.receiver, c.method),
    }
}

/// Step `i`'s op's name: a panic naming it is the harness's error.
fn op_name<O: Op<W>, W: World>(i: usize, op: &O) -> Result<String, Stop> {
    guarded(|| op.name()).map_err(|p| Stop::Harness(format!("step {i}: Op::name {p}")))
}

impl<O: Op<W>, W: World> Harness<O, W> {
    /// Runs one case: `plan` on a fresh clone of the fixture. Never panics
    /// on what the contract or a check does: a failing check is a finding,
    /// a panicking check is a harness error, and the case runs to its end.
    /// The same plan on the same fixture runs the same way every time.
    ///
    /// Every op of a written plan must run: one that skips itself makes
    /// the run `Incomplete`, naming it (a search's cases skip ops freely),
    /// and one whose `submit` panics is a harness error printing the op.
    /// An `unreachable` trap's message names the functions it was in,
    /// read from a replay on a copy of the contract that logs them. It
    /// saves no replay: [`Plan::save`] keeps a plan, and a scan of it
    /// saves its findings' replays.
    pub fn run(&self, plan: &Plan<O>) -> Run<O> {
        let mut run = self.run_written(plan);
        let failed = failed_witness(&run, None);
        self.name_traps(&mut run);
        // Naming rewrites messages only: a witness that didn't fail still
        // doesn't, and its reason stays.
        if run.reason == failed {
            run.reason = failed_witness(&run, None);
        }
        run
    }

    /// The error of a campaign's witness `index` that failed
    /// ([`failed_witness`]), its traps named.
    pub(crate) fn witness_error(&self, run: &Run<O>, index: usize) -> Option<String> {
        let why = failed_witness(run, Some(index))?;
        if !why.contains(trap::UNREACHABLE) {
            return Some(why);
        }
        let mut named = run.clone();
        self.name_traps(&mut named);
        failed_witness(&named, Some(index))
    }

    /// [`run`](Self::run) without naming traps or saving replays.
    pub(crate) fn run_written(&self, plan: &Plan<O>) -> Run<O> {
        let mut run = super::big_stack(|| self.run_case(plan));
        let skipped: Vec<String> = (run.ops.iter().enumerate())
            .filter_map(|(i, o)| Some(format!("{} (step {i}): {}", o.name, o.skipped.as_ref()?)))
            .collect();
        if !skipped.is_empty() && run.verdict == Verdict::Clean {
            run.verdict = Verdict::Incomplete;
            run.reason = Some(format!("skipped, so not judged: {}", skipped.join("; ")));
        }
        // A failed witness isn't hidden behind a finding elsewhere.
        if run.verdict != Verdict::HarnessError
            && let Some(why) = failed_witness(&run, None)
        {
            (run.verdict, run.reason) = (Verdict::HarnessError, Some(why));
        }
        // Raw calls alone: checks over ops judged nothing.
        let by_op = |c: &super::check::Rest<O, W>| matches!(c.check, RestFn::Ops(_)) && !c.calls;
        let calls = plan.steps.iter().all(|s| matches!(s.act, Act::Call(_)));
        if calls
            && run.verdict == Verdict::Clean
            && (!self.per_op.is_empty() || self.at_rest.iter().any(by_op))
        {
            let why = "only raw calls (Plan::call) ran: checks over ops judged nothing";
            (run.verdict, run.reason) = (Verdict::Incomplete, Some(why.into()));
        }
        run
    }

    /// Names, in `run`'s failures and findings, the function each
    /// `unreachable` trap is in (`crate::trap`): the case once more on a
    /// fixture whose trapping contracts log it. A receipt the copy doesn't
    /// trap the same way keeps its message. The names, by receipt.
    pub(crate) fn name_traps(&self, run: &mut Run<O>) -> HashMap<CryptoHash, String> {
        let traps = |r: &ReceiptOutcome| matches!(r.failure(), Some(Failure::Host(m)) if m.ends_with(crate::trap::UNREACHABLE));
        let receipts = run.outcomes.iter().flat_map(|o| &o.receipts);
        let accounts: BTreeSet<AccountId> =
            receipts.filter(|r| traps(r)).map(|r| r.receiver_id.clone()).collect();
        let mut copy = self.clone();
        // Its fixture differs, so an enumeration's memo is the other
        // fixture's: both its end checks' verdicts and its serial orders.
        copy.memo = None;
        let codes: HashMap<AccountId, Vec<u8>> = (accounts.into_iter())
            .filter_map(|a| Some((a.clone(), copy.fixture.as_mut().name_traps(&a)?)))
            .collect();
        let mut names = HashMap::new();
        if codes.is_empty() {
            return names;
        }
        let again = super::big_stack(|| copy.run_case(&run.plan));
        for (o, p) in run.outcomes.iter_mut().zip(&again.outcomes) {
            let mut first = None;
            for (r, s) in o.receipts.iter_mut().zip(&p.receipts) {
                let same = (r.receiver_id == s.receiver_id && r.method_names == s.method_names)
                    && traps(r)
                    && traps(s);
                let code = codes.get(&r.receiver_id).filter(|_| same);
                let Some(name) = code.and_then(|c| s.logs.iter().find_map(|l| trap::name(c, l)))
                else {
                    continue;
                };
                if let ExecutionStatus::Failure(e) = &mut r.status {
                    name_trap(e, &name);
                }
                first.get_or_insert(name.clone());
                names.insert(r.receipt_id, name);
            }
            if let (Some(name), FinalExecutionStatus::Failure(e)) = (first, &mut o.status) {
                name_trap(e, &name);
            }
        }
        for f in &mut run.findings {
            if let Some(name) = f.receipt_id.and_then(|r| names.get(&r)) {
                f.message = trap::named(&f.message, name);
            }
        }
        for (id, m) in &mut run.failed {
            if let Some(name) = names.get(id) {
                *m = trap::named(m, name);
            }
        }
        names
    }

    /// Runs one case, a sampled injection (a selector with
    /// [`nth`](Receipts::nth)) resolved first: the plan plays without it,
    /// recording what each step's transactions spawn, and the fault goes
    /// to one receipt it fits, by [`Receipts::occurrence`].
    pub(crate) fn run_case(&self, plan: &Plan<O>) -> Run<O> {
        self.run_until(plan, None)
    }

    /// [`run_case`](Self::run_case), cut at its next block past `until`,
    /// if any, as the case timeout cuts it: a shrink's candidate
    /// never runs past the shrink's deadline.
    pub(crate) fn run_until(&self, plan: &Plan<O>, until: Option<std::time::Instant>) -> Run<O> {
        super::progress::tick();
        let mut run = self.play_sampled(plan, until);
        run.seen = super::curves::seen_of(self, &run);
        run
    }

    /// Plays `plan`, each sampled injection resolved first.
    fn play_sampled(&self, plan: &Plan<O>, until: Option<std::time::Instant>) -> Run<O> {
        let mut plan = plan.clone();
        let mut drawn: Vec<(usize, Inject)> = vec![];
        for (i, step) in plan.steps.iter_mut().enumerate() {
            drawn.extend(step.inject.extract_if(.., |x| x.at.is_sample()).map(|x| (i, x)));
        }
        if drawn.is_empty() {
            return self.play(&plan, until);
        }
        let (mut untargeted, mut last) = (vec![], None);
        for (i, inject) in drawn {
            let baseline = self.play(&plan, until);
            if baseline.verdict == Verdict::HarnessError || baseline.timed_out {
                return baseline;
            }
            match target(&plan, &baseline, i, &inject) {
                // A receipt calling what its receiver doesn't export (a
                // stub's undeclared method) fails as it is: no fault to try.
                Some((_, at)) if unexported(self, &at).is_some() => {
                    let m = unexported(self, &at).unwrap_or_default();
                    untargeted.push(format!(
                        "step {i}'s {inject} (its receipt calls `{m}`, which its receiver \
                         doesn't export)"
                    ));
                    last = Some(baseline);
                }
                Some((j, at)) => {
                    plan.steps[j].inject.push(Inject { at, ..inject });
                    last = None;
                }
                None => {
                    untargeted.push(format!("step {i}'s {inject}"));
                    last = Some(baseline);
                }
            }
        }
        let mut run = last.unwrap_or_else(|| self.play(&plan, until));
        run.untargeted = untargeted;
        run
    }

    /// Runs `plan` as it is, injections and all.
    fn play(&self, plan: &Plan<O>, until: Option<std::time::Instant>) -> Run<O> {
        let mut case = Case::new(self, plan);
        case.until = until;
        if let Err(stop) = case.play() {
            case.stop.get_or_insert(stop);
        }
        case.finish()
    }
}

/// The one judgment of a written witness's failure, for
/// [`Harness::run`] and every campaign: `None` if its plan is
/// [`Plan::refused`], or none of its transactions failed but one where
/// it forced something (a fault, a hold), or a check other than
/// `Sim::findings`' three (which fire on any failure) fired at a failed
/// receipt: that failure is the finding. Otherwise a harness error naming
/// the plan (witness `index` of the campaign's, if known), its steps,
/// the receipt that failed, and the checks that fired elsewhere (their
/// findings are recorded all the same).
pub(crate) fn failed_witness<O: fmt::Debug>(run: &Run<O>, index: Option<usize>) -> Option<String> {
    let judged = |r: &&ReceiptOutcome| {
        let mine = |f: &&Finding| !crate::builtin::ALL.contains(&f.check.as_str());
        run.findings.iter().filter(mine).any(|f| f.receipt_id == Some(r.receipt_id))
    };
    let unforced = |o: &&TxOutcome| !o.receipts.iter().any(|r| forced_from(r).is_some());
    let mut written = run.outcomes.iter().filter(|_| !run.plan.refused).filter(unforced);
    let (r, why) = written.find_map(|o| {
        let failed = || o.receipts.iter().filter(|r| r.failure().is_some());
        (!failed().any(|r| judged(&r))).then_some((failed().next(), o.failure()?))
    })?;
    let at = r.map_or(String::new(), |r| format!(" at {}", r.site()));
    let which = index.map_or_else(|| "the witness".to_owned(), |i| format!("witness {i}"));
    let steps: Vec<String> = run.plan.steps.iter().map(|s| label(&s.act)).collect();
    let mut fired: Vec<&str> = vec![];
    for f in run.findings.iter().filter(|f| !crate::builtin::ALL.contains(&f.check.as_str())) {
        if !fired.contains(&f.check.as_str()) {
            fired.push(&f.check);
        }
    }
    let fired = match fired.is_empty() {
        true => String::new(),
        false => format!("; checks that fired: {}", fired.join(", ")),
    };
    Some(format!(
        "{which} ({}) failed{at}: {why}{fired}; fix the plan, or if this failure is the bug, \
         mark the plan Plan::refused()",
        steps.join("; ")
    ))
}

/// The most prefixes [`Harness::breaking`] replays one by one; a longer
/// case is bisected.
const BREAK_RUNS: usize = 32;

/// A finding's **breaking step**: the first step after which its check
/// (the same check at the same site) fails. What failed in later steps is
/// never its cause, nor what failed before it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Breaking {
    /// The step, from 0.
    pub step: usize,
    /// The case's steps.
    pub of: usize,
    /// Its op's name.
    pub op: String,
}

/// `breaking step 2 of 3: Skew`.
impl fmt::Display for Breaking {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "breaking step {} of {}: {}", self.step + 1, self.of, self.op)
    }
}

impl<O: Op<W>, W: World> Harness<O, W> {
    /// `f`'s breaking step in `run` ([`Breaking`]). A finding at a receipt
    /// or an op is its transaction's step; any other (an `always` or
    /// `at_rest` check) is found by replaying the case's prefixes, each
    /// settled after its last step, from the first; a case of more than
    /// 32 steps is bisected, which assumes the check stays broken once
    /// broken. `None` for a forced finding, a case of one step, or one
    /// that ran out of its block budget.
    pub(crate) fn breaking(&self, run: &Run<O>, f: &Finding) -> Option<Breaking> {
        let n = run.ops.len();
        if f.injected || n < 2 || run.budget || run.plan.steps.len() != n {
            return None;
        }
        let name = |k: usize| run.ops[k].name.clone();
        if let Some(tx) = f.tx {
            let k = run.ops.iter().position(|o| o.txs.contains(&tx))?;
            return Some(Breaking { step: k, of: n, op: name(k) });
        }
        let fails = |k: usize| {
            let mut trial = run.plan.clone();
            trial.steps.truncate(k + 1);
            trial.steps[k].then = Gap::Settle;
            let prefix = self.run_case(&trial);
            prefix.verdict != Verdict::HarnessError
                && !prefix.budget
                && prefix.findings.iter().any(|x| x.check == f.check && x.site == f.site)
        };
        let step = if n - 1 <= BREAK_RUNS {
            (0..n - 1).find(|&k| fails(k)).unwrap_or(n - 1)
        } else {
            let (mut lo, mut hi) = (0, n - 1);
            while lo < hi {
                let mid = (lo + hi) / 2;
                if fails(mid) { hi = mid } else { lo = mid + 1 }
            }
            lo
        };
        Some(Breaking { step, of: n, op: name(step) })
    }
}

/// Where the sample `inject`, drawn for step `step`, goes: from `step` on
/// (then from the first), the first step with no injection of its own
/// whose transaction spawned a receipt the fault fits, at the `k`th of
/// them modulo how many. A non-hold passes over a transaction that had a
/// receipt congestion held back.
fn target<O>(
    plan: &Plan<O>,
    baseline: &Run<O>,
    step: usize,
    inject: &Inject,
) -> Option<(usize, Receipts)> {
    let k = inject.at.nth_of().unwrap_or(0);
    let len = plan.steps.len();
    (0..len).map(|d| (step + d) % len).find_map(|j| {
        if !plan.steps[j].inject.is_empty() || baseline.ops.get(j)?.skipped.is_some() {
            return None;
        }
        let (spawned, buffered) = baseline.spawned.get(j)?.get(usize::from(inject.tx))?;
        if !inject.fault.is_hold() && !buffered.is_empty() {
            return None;
        }
        let fits = fitting(&inject.at, &inject.fault, spawned);
        (!fits.is_empty()).then(|| (j, address(spawned, fits[k % fits.len()], &inject.fault)))
    })
}

/// Receipt `n` of `spawned` for `fault`, as a replay names it: by its
/// receiver, method and occurrence ([`Receipts::occurrence`]); by position
/// if it makes other than one call.
pub(crate) fn address(spawned: &[Spawned], n: usize, fault: &Fault) -> Receipts {
    let [method] = &spawned[n].methods[..] else { return Receipts::index(n) };
    let at = Receipts::call(spawned[n].receiver.clone(), method);
    let k = fitting(&at, fault, spawned).iter().position(|&x| x == n).unwrap_or(0);
    Receipts::occurrence(spawned[n].receiver.clone(), method, k)
}

/// The receipts of `spawned` that `at` selects and `fault` fits, by index.
pub(crate) fn fitting(at: &Receipts, fault: &Fault, spawned: &[Spawned]) -> Vec<usize> {
    (0..spawned.len()).filter(|&n| at.matches(n, &spawned[n]) && spawned[n].fits(fault)).collect()
}

/// Copy `c.k` of the op `call` copies, from `plan`'s sources, as JSON;
/// for fresh callers, with each caller it names (an account of `fixture`
/// with no code, or the witness's signer) made fresh: the fresh ids.
pub(crate) fn copy_json<O: Op<W>, W: World>(
    fixture: &Sim,
    plan: &Plan<O>,
    call: &Call,
    c: &super::Copied,
) -> Result<(serde_json::Value, Vec<AccountId>), super::Skip> {
    let is_caller = |id: &AccountId| {
        id != &call.receiver
            && (c.signer.as_ref() == Some(id)
                || fixture.account(id).is_some_and(|a| a.code_hash == CryptoHash::default()))
    };
    let no = |why: &str| super::Skip(format!("copy {} of its op {why}", c.k));
    let source = plan.sources.get(c.source).ok_or_else(|| no("isn't in the plan"))?;
    let op = super::axes::op_copy::<O, W>(source, c.k, c.items);
    let json = op.and_then(|op| serde_json::to_value(op).ok());
    let json = json.ok_or_else(|| no("doesn't rebuild"))?;
    let mut made = vec![];
    let json =
        if c.fresh { super::axes::fresh_callers(&json, c.k, &is_caller, &mut made) } else { json };
    Ok((json, made))
}

/// Why a case ended before its plan did. Findings never stop one.
enum Stop {
    /// The harness, the plan or an op is wrong.
    Harness(String),
    /// A settle ran out of blocks with work still pending.
    Budget(String),
    /// An op's `submit` panicked: a harness error, the panic its reason.
    Panicked(String),
    /// The case ran past the case timeout.
    Timeout(String),
    /// A check failed on the fixture itself: a harness error every case
    /// from that fixture repeats.
    Fixture(String),
}

/// A step as [`Case::send`] submits it.
struct Sending<O> {
    /// Its op; `None` for a call.
    op: Option<O>,
    /// The op a copy copies.
    of: Option<O>,
    /// How a copy was made, as its record shows it.
    copy: Option<String>,
    /// Its record's name, and the step as errors name it.
    name: String,
    ours: String,
}

/// One step as the case runs it.
struct OpState<O, W> {
    record: OpRecord,
    op: Option<O>,
    /// The op a raw call copies ([`Copied`](super::Copied)).
    of: Option<O>,
    /// Its per-op checks have run, or it was skipped.
    done: bool,
    /// The world just before it submitted, kept if a check reads it.
    before: Option<W>,
    idle: bool,
    /// Submitted at rest: idle, and nothing pending, a parked yield
    /// included ([`OpDone::rest`](super::OpDone)).
    rest: bool,
    disturbed: bool,
}

impl<O, W> OpState<O, W> {
    /// It done, as its checks read it; `None` if it never submitted.
    fn done<'a>(
        &'a self,
        outcomes: &'a [TxOutcome],
        effects: &'a Effects,
        world: &'a W,
        shared: &'a std::cell::Cell<(bool, bool)>,
    ) -> Option<OpDone<'a, O, W>> {
        let (name, before, isolated) =
            (&self.record.name, self.before.as_ref()?, self.idle && !self.disturbed);
        let (op, of, rest) = (self.op.as_ref(), self.of.as_ref(), self.rest);
        Some(OpDone { op, of, name, outcomes, effects, world, before, isolated, rest, shared })
    }
}

struct Case<'h, O, W> {
    harness: &'h Harness<O, W>,
    plan: &'h Plan<O>,
    world: W,
    flagged: (usize, usize),
    ops: Vec<OpState<O, W>>,
    op_of: HashMap<CryptoHash, usize>,
    findings: Vec<Finding>,
    seen: HashMap<(String, Site, Option<String>), usize>,
    /// A world finding's last different failure: its block and message.
    late: HashMap<usize, (u64, String)>,
    judged: Vec<Judged>,
    /// Per check, the block it last passed at, and the ops submitted then.
    passed: Vec<Option<(u64, usize)>>,
    stop: Option<Stop>,
    names: Names,
    started: std::time::Instant,
    /// When a shrink's deadline cuts it ([`Harness::run_until`]).
    until: Option<std::time::Instant>,
    /// The checks are judging the fixture itself: a failure is the harness's.
    on_fixture: bool,
    /// Per-op judgments not judged because their ops overlapped.
    overlapped: u32,
}

impl<'h, O: Op<W>, W: World> Case<'h, O, W> {
    fn new(harness: &'h Harness<O, W>, plan: &'h Plan<O>) -> Self {
        let world = set_up(&harness.fixture, plan);
        let flagged = world.as_ref().chain.log().flagged();
        let judged: Vec<Judged> = (harness.check_names().into_iter())
            .skip(builtin::ALL.len())
            .map(|check| Judged { check, ..Judged::default() })
            .collect();
        Case {
            harness,
            plan,
            world,
            flagged,
            passed: vec![None; judged.len()],
            ops: vec![],
            op_of: HashMap::new(),
            findings: vec![],
            seen: HashMap::new(),
            late: HashMap::new(),
            judged,
            stop: None,
            names: Names::new(plan),
            started: std::time::Instant::now(),
            until: None,
            on_fixture: false,
            overlapped: 0,
        }
    }

    fn sim(&self) -> &Sim {
        self.world.as_ref()
    }

    fn play(&mut self) -> Result<(), Stop> {
        let problems: Vec<String> = (self.harness.problems().into_iter())
            .chain(plan_problems(self.plan))
            .chain(misnamed(self.harness, self.plan))
            .collect();
        if !problems.is_empty() {
            return Err(Stop::Harness(problems.join("; ")));
        }
        if !self.sim().is_idle() {
            return Err(Stop::Harness(
                "the fixture has transactions queued or receipts in flight: settle it before \
                 Harness::new"
                    .into(),
            ));
        }
        self.on_fixture = self.plan.setup.is_empty();
        if self.on_fixture {
            self.judge_fixture();
        } else {
            self.checkpoint();
        }
        self.on_fixture = false;
        if let Some(stop) = self.stop.take() {
            return Err(stop);
        }
        if let Some(gap) = self.plan.start {
            self.gap(gap)?;
        }
        for (i, step) in self.plan.steps.iter().enumerate() {
            self.submit(i, step)?;
            self.gap(step.then)?;
        }
        self.settle()?;
        self.drain_yields()?;
        self.rest_checks(true);
        self.end_checks();
        self.serializable();
        Ok(())
    }

    /// The checks on the fixture, at_end ones too (their passes aren't
    /// the case's): one failing there is the fixture's (or the
    /// contract's initial state's), and no case can tell anything about
    /// it. An enumeration judges them once ([`Memo`](super::check::Memo))
    /// and credits every case with the same results, as each would judge
    /// the same world: the fixture, no keys planted. The built-ins still
    /// read each case's own log.
    fn judge_fixture(&mut self) {
        self.builtins();
        // A checkpoint's world checks, then the at_end ones' failures.
        let judge_all = || {
            #[cfg(test)]
            FIXTURE_JUDGED.set(FIXTURE_JUDGED.get() + 1);
            let base = self.harness.always.len();
            let failed = |(_, r): &(usize, Result<(), Fail>)| {
                r.as_ref().is_err_and(|f| f.unjudged.is_none())
            };
            let mut results = self.judge_world();
            let at_end = self.judge_at_rest(true).into_iter().filter(failed);
            results.extend(at_end.map(|(k, r)| (base + k, r)));
            results
        };
        let memo = self.harness.memo.as_ref();
        #[cfg(test)]
        let memo = memo.filter(|_| !FIXTURE_EACH_CASE.get());
        // Planted keys make another world (no enumeration plants any).
        let results = match memo.filter(|_| self.plan.planted.is_empty()) {
            Some(m) => m.fixture.get_or_init(judge_all).clone(),
            None => judge_all(),
        };
        let height = self.sim().height();
        for (k, r) in results {
            self.record(k, r, Site::World, height, None);
        }
    }

    fn gap(&mut self, gap: Gap) -> Result<(), Stop> {
        match gap.blocks_on(self.sim()) {
            None => self.settle(),
            Some(n) => (0..n).try_for_each(|_| self.step()),
        }
    }

    /// Runs step `i`: its op, or its call, with its gas and injections.
    fn submit(&mut self, i: usize, step: &Step<O>) -> Result<(), Stop> {
        let mut hook = super::Hook::new(step.gas, step.inject.clone(), step.answers.clone());
        hook.band = self.plan.bands;
        match &step.act {
            Act::Op(op) => {
                let name = op_name(i, op)?;
                let ours = format!("{name} (step {i})");
                let sending = Sending { op: Some(op.clone()), of: None, copy: None, name, ours };
                self.send(sending, hook, |w| op.submit(w))
            }
            Act::Call(call) => {
                let ours = format!("call {}::{} (step {i})", call.receiver, call.method);
                let copy = call.copies.as_deref().map(|c| (c.k, self.copied(call, c)));
                // A fresh callers copy of an op naming its caller is that
                // op's copy, run as the op runs.
                if let Some((k, Ok(CopyRun::Op(op)))) = copy {
                    let name = op_name(i, &op)?;
                    let ran =
                        format!("copy {k} of {}, from fresh callers", label(&Act::Op(op.clone())));
                    let (of, copy) = (Some(op.clone()), Some(ran));
                    let sending = Sending { op: Some(op.clone()), of, copy, name, ours };
                    return self.send(sending, hook, |w| op.submit(w));
                }
                let near = self.funds(call.copies.as_deref()).saturating_add(call.deposit);
                let sim = self.world.as_mut();
                if sim.account(&call.signer).is_none() {
                    sim.create_account(&call.signer, near);
                }
                let name = call.method.clone();
                let (call, of, copy) = match copy {
                    None => (Ok(call.clone()), None, None),
                    Some((k, Ok(CopyRun::Call(call, of)))) => {
                        let ran = format!("copy {k} of {}: {call:?}", label(&Act::Op(of.clone())));
                        (Ok(call), Some(of), Some(super::input::compact(&ran)))
                    }
                    Some((_, Ok(CopyRun::Op(_)))) => unreachable!("an op's copy ran above"),
                    Some((_, Err(skip))) => (Err(skip), None, None),
                };
                let sending = Sending { op: None, of, copy, name, ours };
                self.send(sending, hook, |w| call.map(|call| call.send(w.as_mut())))
            }
        }
    }

    /// `call` as copy `c.k` of its op makes it: for fresh callers, the op
    /// with its callers made fresh (each created), if it names one (an
    /// account of the fixture with no code, or the witness's signer), to
    /// run as the op; else the call, with the op: the op run on a scratch
    /// copy of the world until idle, or until its first transaction has
    /// run a receipt that fits, and the call taking the args of the first
    /// receipt there with its receiver and method (its entry's, for fresh
    /// callers), else keeping its own. A copy that skips, panics or
    /// doesn't rebuild skips the step, saying why.
    fn copied(&mut self, call: &Call, c: &super::Copied) -> Result<CopyRun<O>, super::Skip> {
        let (json, made) = copy_json::<O, W>(self.harness.fixture.as_ref(), self.plan, call, c)?;
        let op: O = serde_json::from_value(json)
            .map_err(|e| super::Skip(format!("copy {} of its op doesn't rebuild: {e}", c.k)))?;
        let near = self.funds(Some(c));
        for id in &made {
            let sim = self.world.as_mut();
            if sim.account(id).is_none() {
                sim.create_account(id, near);
            }
        }
        if !made.is_empty() {
            return Ok(CopyRun::Op(op));
        }
        let mut scratch = self.world.clone();
        let from = scratch.as_ref().txs().len();
        let at = |r: &ReceiptOutcome| {
            c.fresh
                || (r.receiver_id == call.receiver
                    && r.method_names.first().is_some_and(|m| *m == call.method))
        };
        let receipts = |sim: &Sim, t: &CryptoHash| sim.ran(*t).into_iter().skip(1);
        // Receipts only add up in execution order, so once the copy's
        // first transaction has run one that fits, no later block changes
        // which the call takes: the run stops there, not at idle, where
        // the world's other queued work (earlier copies) would run too.
        let fits =
            |sim: &Sim| sim.txs().get(from).is_some_and(|t| receipts(sim, t).any(|r| at(&r)));
        let ran = guarded(|| -> Result<(), super::Skip> {
            op.submit(&mut scratch)?;
            scratch.as_mut().settle_until(fits);
            Ok(())
        });
        let why = |w: String| super::Skip(format!("copy {} of its op ({op:?}) {w}", c.k));
        ran.map_err(|p| why(p.to_string()))?.map_err(|s| why(format!("skipped: {}", s.0)))?;
        let sim = scratch.as_ref();
        let mut receipts: Vec<ReceiptOutcome> =
            sim.txs()[from..].iter().flat_map(|t| receipts(sim, t)).collect();
        if c.fresh {
            receipts.truncate(1);
        }
        let mut call = call.clone();
        if let Some(args) = receipts.into_iter().find(at).and_then(|r| r.args.into_iter().next()) {
            call.args = args;
        }
        Ok(CopyRun::Call(call, op))
    }

    /// What a fresh account of copy `c` starts with: what the witness's
    /// signer held in the fixture, and 100 NEAR at least.
    fn funds(&self, c: Option<&super::Copied>) -> Balance {
        let fixture = self.harness.fixture.as_ref();
        let held = c.and_then(|c| fixture.account(c.signer.as_ref()?)).map(|a| a.amount);
        held.unwrap_or_default().max(Balance::from_near(100))
    }

    /// Submits one step through `submit`, with `hook` set: checks it queued
    /// without producing a block, and records it.
    fn send(
        &mut self,
        Sending { op, of, copy, name, ours }: Sending<O>,
        hook: super::Hook,
        submit: impl FnOnce(&mut W) -> Result<(), super::Skip>,
    ) -> Result<(), Stop> {
        let at = |sim: &Sim| (sim.height(), sim.timestamp_ns(), sim.chain.log().blocks.len());
        let (before, txs) = (at(self.sim()), self.sim().txs().len());
        let idle = self.sim().is_idle();
        let rest = idle && self.sim().pending_receipts().is_empty();
        let reads = |c: &super::check::Rest<O, W>| {
            matches!(c.check, RestFn::Ops(_)) && (op.is_some() || c.calls)
        };
        let reads_ops = op.as_ref().is_some_and(|_| !self.harness.per_op.is_empty())
            || self.harness.at_rest.iter().any(reads)
            || self.harness.serial.is_some();
        let world_before = reads_ops.then(|| self.world.clone());
        self.world.as_mut().explore = Some(Box::new(hook));
        let world = &mut self.world;
        let ran = guarded(|| submit(world).err());
        let hook = self.world.as_mut().explore.take().map(|h| *h).unwrap_or_default();
        let shown = op.as_ref().map(|o| format!(": {o:?}")).unwrap_or_default();
        let skip = ran.map_err(|p| Stop::Panicked(format!("Op::submit {p} ({ours}{shown})")))?;
        if at(self.sim()) != before {
            return Err(Stop::Harness(format!(
                "{ours}: Op::submit produced a block or moved the clock; it only queues \
                 transactions with Tx::submit, and the plan's gaps produce blocks"
            )));
        }
        let new = self.sim().txs()[txs..].to_vec();
        if let Some(skip) = &skip
            && !new.is_empty()
        {
            return Err(Stop::Harness(format!(
                "{ours}: Op::submit returned Skip({:?}) after submitting a transaction; skip \
                 before submitting",
                skip.0
            )));
        }
        let n = self.ops.len();
        for tx in &new {
            self.op_of.insert(*tx, n);
            // A fault its plan names that can't apply is passed over, not a panic.
            self.world.as_mut().chain.lenient(tx);
        }
        let skipped = skip.map(|s| s.0);
        let done = skipped.is_some();
        if !done {
            for op in self.ops.iter_mut().filter(|o| !o.done) {
                op.disturbed = true;
            }
        }
        let (unpinned, bands) = (hook.unpinned, hook.bands);
        let record = OpRecord { name, txs: new, skipped, unpinned, bands, ran: copy, at: None };
        let before = world_before.filter(|_| !done);
        let quiet = !done && record.txs.is_empty();
        self.ops.push(OpState { record, op, of, done, before, idle, rest, disturbed: false });
        // An op that queued nothing, nothing else in flight, left the
        // world at rest: no block may follow to judge it there.
        if quiet && self.sim().is_idle() && self.sim().pending_receipts().is_empty() {
            self.checkpoint();
        } else {
            self.judge_completed();
        }
        Ok(())
    }

    /// One block, then its checkpoint; none past the harness's timeout.
    fn step(&mut self) -> Result<(), Stop> {
        let took = self.started.elapsed();
        if took > self.harness.timeout {
            return Err(Stop::Timeout(format!(
                "timed out: {took:.1?} past the case timeout ({:?}), at block {}",
                self.harness.timeout,
                self.sim().height()
            )));
        }
        if self.until.is_some_and(|t| std::time::Instant::now() > t) {
            let at = self.sim().height();
            return Err(Stop::Timeout(format!("cut at block {at}: past the shrink's deadline")));
        }
        let world = &mut self.world;
        guarded(|| world.as_mut().step())
            .map_err(|p| Stop::Harness(format!("block {} {p}", self.sim().height() + 1)))?;
        self.checkpoint();
        Ok(())
    }

    /// Blocks until idle, at most `config.max_blocks`.
    fn settle(&mut self) -> Result<(), Stop> {
        let max = self.sim().config.max_blocks;
        for _ in 0..max {
            if self.sim().is_idle() {
                return Ok(());
            }
            self.step()?;
        }
        if !self.sim().is_idle() {
            let why = format!("not idle after {max} blocks (config.max_blocks; a receipt loop?)");
            return Err(Stop::Budget(why));
        }
        Ok(())
    }

    /// Times out each parked yield, earliest first, a block at a time,
    /// skipping the heights where nothing can happen; at most
    /// `config.max_blocks` blocks in all.
    fn drain_yields(&mut self) -> Result<(), Stop> {
        let max = self.sim().config.max_blocks;
        let mut blocks = 0;
        while let Some(due) = self.sim().yields().iter().map(|y| y.expires_at).min() {
            let mut first = true;
            while first || self.sim().height() < due || !self.sim().is_idle() {
                first = false;
                if blocks >= max {
                    return Err(Stop::Budget(format!(
                        "parked yields still waiting after {max} blocks of timing them out"
                    )));
                }
                let sim = self.world.as_mut();
                if sim.is_idle() && sim.height() + 1 < due {
                    let land = sim.chain.timeout_due().map_or(due, |e| e.min(due));
                    if land > sim.height() + 1 {
                        let skipped = land - 1 - sim.height();
                        sim.chain.timestamp_ns += skipped * sim.config.block_time_ns();
                        sim.chain.height = land - 1;
                    }
                }
                self.step()?;
                blocks += 1;
            }
        }
        Ok(())
    }

    /// The built-ins over the receipts since the last checkpoint,
    /// `always`, `at_rest` if nothing is in flight, and the per-op checks
    /// of each op that just completed.
    fn checkpoint(&mut self) {
        let height = self.sim().height();
        for o in self.ops.iter_mut().filter(|o| o.record.skipped.is_none()) {
            o.record.at.get_or_insert(height);
        }
        self.builtins();
        let results = self.judge_world();
        self.record_world(0, results);
        self.judge_completed();
    }

    /// Judges the world checks of a checkpoint: `always`, and `at_rest`
    /// if nothing is in flight; each by its index.
    fn judge_world(&self) -> Vec<(usize, Result<(), Fail>)> {
        let h = self.harness;
        let always = h.always.iter().map(|c| judge(|| (c.check)(&self.world)));
        let mut results: Vec<(usize, Result<(), Fail>)> = always.enumerate().collect();
        if h.at_rest.iter().any(|c| !c.at_end)
            && self.sim().is_idle()
            && self.sim().pending_receipts().is_empty()
        {
            let base = h.always.len();
            results.extend(self.judge_at_rest(false).into_iter().map(|(k, r)| (base + k, r)));
        }
        results
    }

    /// Judges the `at_rest` checks: those judged at every rest point, or,
    /// with `end`, those judged once at the end.
    fn rest_checks(&mut self, end: bool) {
        let results = self.judge_at_rest(end);
        self.record_world(self.harness.always.len(), results);
    }

    /// Records world checks' results, the `k`th as check `base + k`.
    fn record_world(&mut self, base: usize, results: Vec<(usize, Result<(), Fail>)>) {
        let height = self.sim().height();
        for (k, r) in results {
            self.record(base + k, r, Site::World, height, None);
        }
    }

    fn judge_at_rest(&self, end: bool) -> Vec<(usize, Result<(), Fail>)> {
        let rest = &self.harness.at_rest;
        let wants_ops = rest.iter().any(|c| matches!(c.check, RestFn::Ops(_)));
        let ran: Vec<&OpState<O, W>> =
            self.ops.iter().filter(|o| wants_ops && o.before.is_some()).collect();
        let outcomes: Vec<Vec<TxOutcome>> = (ran.iter())
            .map(|o| o.record.txs.iter().map(|t| self.sim().outcome(t)).collect())
            .collect();
        let effects: Vec<Effects> = outcomes.iter().map(|o| merge(o)).collect();
        // Whole-case checks: the overlap rule reads no flag of theirs.
        let shared = Default::default();
        let dones: Vec<OpDone<'_, O, W>> = (ran.iter().zip(&outcomes).zip(&effects))
            .filter_map(|((o, outcomes), effects)| o.done(outcomes, effects, &self.world, &shared))
            .collect();
        let ops: Vec<OpDone<'_, O, W>> = dones.iter().filter(|d| d.op.is_some()).cloned().collect();
        (rest.iter().enumerate())
            .filter(|(_, c)| c.at_end == end)
            .map(|(k, c)| match &c.check {
                RestFn::World(check) => (k, judge(|| check(&self.world))),
                RestFn::Ops(check) => {
                    (k, judge(|| check(&self.world, [&ops, &dones][usize::from(c.calls)])))
                }
            })
            .collect()
    }

    /// The built-in checks' failures since the last checkpoint, each at
    /// its receipt and op; a receipt a fault starved is excused.
    fn builtins(&mut self) {
        let sim = self.world.as_ref();
        let list = sim.builtins_since(self.flagged);
        self.flagged = sim.chain.log().flagged();
        let mut found = vec![];
        for b in list {
            let Some(r) = &b.receipt else {
                found.push(Finding::at(b.check, b.message, Site::World, sim.height(), None));
                continue;
            };
            if forced(r) {
                continue;
            }
            let op = self.op_of.get(&r.tx).map(|&i| self.ops[i].record.name.clone());
            let receiver = self.names.known(&r.receiver_id, self.harness.fixture.as_ref());
            // Its message opens with the receipt's label, naming the receiver
            // whether or not the fixture has it.
            let site = Site::Receipt { receiver, method: r.method().map(str::to_owned), op };
            let mut f = Finding::at(b.check, b.message.clone(), site, r.block_height, Some(r.tx));
            f.receipt_id = Some(r.receipt_id);
            found.push(f);
        }
        for f in found {
            self.add(f);
        }
    }

    /// Runs the per-op checks of every op whose transactions are all
    /// complete now.
    fn judge_completed(&mut self) {
        let harness = self.harness;
        let base = harness.always.len() + harness.at_rest.len();
        let height = self.sim().height();
        for i in 0..self.ops.len() {
            if self.ops[i].done {
                continue;
            }
            let txs = self.ops[i].record.txs.clone();
            let outcomes: Vec<TxOutcome> = txs.iter().map(|t| self.sim().outcome(t)).collect();
            if !outcomes.iter().all(TxOutcome::is_complete) {
                continue;
            }
            self.ops[i].done = true;
            let effects = merge(&outcomes);
            let shared = Default::default();
            let Some(done @ OpDone { op: Some(op), .. }) =
                self.ops[i].done(&outcomes, &effects, &self.world, &shared)
            else {
                continue;
            };
            let name = done.name.to_owned();
            let results: Vec<Option<Result<(), Fail>>> = (harness.per_op.iter())
                .map(|c| match &c.applies {
                    Some(applies) if !guarded(|| applies(op)).unwrap_or(true) => None,
                    _ => {
                        done.shared.set((false, false));
                        let r = judge(|| (c.check)(&done));
                        // Not alone, the shared world judges nothing.
                        let read = match done.shared.get() {
                            (true, true) => Some("before() and world()"),
                            (true, false) => Some("before()"),
                            (false, true) => Some("world()"),
                            (false, false) => None,
                        };
                        Some(match (r, read) {
                            (Err(f), _) if f.panicked || f.idle => Err(f),
                            // Short enough that a summary's "not judged:"
                            // line keeps it whole beside a check name of up
                            // to 27 characters (counts under 10); a longer
                            // line keeps its head and its tail.
                            (_, Some(read)) if !done.isolated => Err(Fail::not_judged(format!(
                                "{OVERLAPPED} it read {read}, shared with the ops in flight; \
                                 read effects(), the op's own, or judge them together in \
                                 at_rest_ops (OpDone::rest)"
                            ))),
                            (r, _) => r,
                        })
                    }
                })
                .collect();
            let tx = txs.first().copied();
            for (k, r) in results.into_iter().enumerate() {
                if let Some(r) = r {
                    self.record(base + k, r, Site::Op(name.clone()), height, tx);
                }
            }
        }
    }

    /// The end checks, on the world as the case left it.
    fn end_checks(&mut self) {
        let h = self.harness;
        let base = h.always.len() + h.at_rest.len() + h.per_op.len();
        let player = |w: &mut W, plan: &Plan<O>| play_plain(w, plan);
        let key = h.memo.as_ref().filter(|_| !h.end.is_empty()).map(|m| (m.key)(&self.world));
        let results = h.end.iter().enumerate().map(|(k, c)| {
            let memo = h.memo.as_ref().zip(key);
            let known = memo.and_then(|(m, key)| m.verdicts.borrow().get(&(key, k)).cloned());
            let r = known.unwrap_or_else(|| judge(|| (c.check)(&self.world, &player)));
            if let Some((m, key)) = memo {
                m.verdicts.borrow_mut().entry((key, k)).or_insert_with(|| r.clone());
            }
            (k, r)
        });
        self.record_world(base, results.collect());
    }

    /// [`builtin::SERIALIZABLE`] over the case's overlaps: each run of ops
    /// from one submitted at rest up to the next submitted at rest (or the
    /// end), skipped ops left out.
    fn serializable(&mut self) {
        let Some(serial) = self.harness.serial.clone() else { return };
        // Its check is the last ([`Harness::check_names`]).
        let k = self.judged.len() - 1;
        // An enumeration's serial orders, each played once.
        let orders = self.harness.memo.as_ref().map(|m| &m.orders);
        let live: Vec<usize> =
            (0..self.ops.len()).filter(|&i| self.ops[i].record.skipped.is_none()).collect();
        let mut overlaps: Vec<super::books::Overlap<'_, O, W>> = vec![];
        let mut unjudged = None;
        for (n, &i) in live.iter().enumerate() {
            let o = &self.ops[i];
            let ran = super::books::Ran {
                step: &self.plan.steps[i],
                name: &o.record.name,
                outcomes: o.record.txs.iter().map(|t| self.sim().outcome(t)).collect(),
            };
            match (o.rest, overlaps.last_mut(), o.before.as_ref()) {
                (false, Some(last), _) => last.ops.push(ran),
                (true, _, Some(start)) => {
                    // The world before the next op submitted at rest.
                    let next = live[n + 1..].iter().find(|&&j| self.ops[j].rest);
                    let before = next.and_then(|&j| self.ops[j].before.as_ref());
                    let end = before.unwrap_or(&self.world);
                    let memo = orders.and_then(|m| start_key(self.plan, i).map(|k| (m, k)));
                    overlaps.push(super::books::Overlap { start, end, ops: vec![ran], memo });
                }
                _ => {
                    unjudged.get_or_insert_with(|| {
                        Fail::not_judged(format!(
                            "{} was submitted with something in flight before any op: no \
                             serial order starts there",
                            o.record.name
                        ))
                    });
                }
            }
        }
        let result = match unjudged {
            Some(f) => Err(f),
            None => judge(|| serial.judge(&overlaps)),
        };
        drop(overlaps);
        let height = self.sim().height();
        self.record(k, result, Site::World, height, None);
    }

    /// Counts check `k`'s judgment, and records a failure as a finding.
    fn record(
        &mut self,
        k: usize,
        result: Result<(), Fail>,
        site: Site,
        block: u64,
        tx: Option<CryptoHash>,
    ) {
        let j = &mut self.judged[k];
        let mut fail = match result {
            Ok(()) => {
                j.ok += 1;
                self.passed[k] = Some((block, self.ops.len()));
                return;
            }
            Err(Fail { unjudged: Some(why), idle, .. }) => {
                self.overlapped += u32::from(!idle && why.starts_with(OVERLAPPED));
                let (n, first) = if idle {
                    (&mut j.idle, &mut j.idle_why)
                } else {
                    (&mut j.unjudged, &mut j.why)
                };
                *n += 1;
                first.get_or_insert(why);
                return;
            }
            Err(fail) if fail.panicked => {
                let why = format!(
                    "the check {:?} {}: a check's panic is the harness's error, not a finding; \
                     return Err(..) for a violation, or Fail::not_judged for what it can't judge",
                    j.check, fail.message
                );
                self.stop.get_or_insert(Stop::Harness(why));
                return;
            }
            Err(fail) if self.on_fixture => {
                let why = format!(
                    "the check {:?} fails on the fixture: {}; the fixture or the contract's \
                     initial state is wrong, and no case can tell anything about this check",
                    j.check, fail.message
                );
                self.stop.get_or_insert(Stop::Fixture(why));
                return;
            }
            Err(fail) => fail,
        };
        // Failed once; each of its failures ([`Fail::all`]) a finding, the
        // first at each site and cause: a repeat is no second occurrence,
        // nor a world check's later failure.
        j.failed += 1;
        let check = j.check.clone();
        let since = if tx.is_none() { self.passed[k] } else { None };
        let more = std::mem::take(&mut fail.more);
        let mut ids = HashSet::new();
        for fail in std::iter::once(fail).chain(more) {
            let site = match (fail.key, site.clone()) {
                (Some(key), Site::Op(op)) => Site::Key { key, op: Some(op) },
                (Some(key), _) => Site::Key { key, op: None },
                (None, site) => site,
            };
            if !ids.insert((site.clone(), fail.cause.clone())) {
                continue;
            }
            let mut f = Finding::at(&check, fail.message, site, block, tx);
            (f.cause, f.since, f.ops_since) = (fail.cause, since.map(|s| s.0), since.map(|s| s.1));
            self.add(f);
        }
    }

    /// Keeps the first finding of each check, site and cause; a world
    /// check's last different failure (a kept amount grown since) too.
    fn add(&mut self, f: Finding) {
        let id = (f.check.clone(), f.site.clone(), f.cause.clone());
        match self.seen.get(&id) {
            Some(&i) => {
                self.findings[i].occurrences += 1;
                if f.tx.is_none() && f.message != self.findings[i].message {
                    self.late.insert(i, (f.block, f.message));
                }
            }
            None => {
                self.seen.insert(id, self.findings.len());
                self.findings.push(f);
            }
        }
    }

    /// A finding per failed receipt that must not fail, forced or not, and
    /// per gas margin the plan judges that a receipt keeps too little of.
    fn must_not_fail(&self, outcomes: &[TxOutcome]) -> Vec<Finding> {
        use crate::render::{failure_line, gas_summary, receipt_label, short};
        let mut found = vec![];
        for r in outcomes.iter().flat_map(|o| &o.receipts) {
            let site = |m: &String| format!("{}::{m}", r.receiver_id);
            let declared = r.method_names.iter().find(|m| self.harness.must_not_fail_at(&site(m)));
            let (Some(method), Some(failure)) = (declared, r.failure()) else { continue };
            if super::outsider::refused_outsider(self.plan, r) {
                continue;
            }
            let forced = r.fault.as_ref().map(|f| format!("; the test forced {f} on it"));
            let forced = forced.unwrap_or_default();
            let message = format!(
                "{} ({}): {} [{}]{forced}\n  declared must not fail: what earlier receipts \
                 committed stays unreconciled",
                receipt_label(r),
                short(&r.receipt_id),
                failure_line(&failure),
                gas_summary(r),
            );
            found.push(self.at_receipt(builtin::MUST_NOT_FAIL, message, r, method));
        }
        let receipts = || outcomes.iter().flat_map(|o| &o.receipts);
        // Callers new to the fixture, along a fresh callers axis.
        let new = |o: &&TxOutcome| {
            let by = o.receipts.first().map(|r| &r.predecessor_id);
            self.plan.callers
                && by.is_some_and(|a| self.harness.fixture.as_ref().account(a).is_none())
        };
        let fresh: Vec<_> = outcomes.iter().filter(new).collect();
        let sim = self.sim();
        if let Some((r, bytes, staked, paid)) = super::curves::callers_storage(sim, &fresh)
            && staked > paid.saturating_mul(super::curves::STORAGE_RATIO)
        {
            let (n, method) = (fresh.len() as u128, r.method_names.join(","));
            let a = sim.account(&r.receiver_id);
            let free = a.map_or(0, |a| {
                a.amount.saturating_sub(sim.storage_cost(a.storage_usage)).as_yoctonear()
            });
            let message = format!(
                "{} staked {staked} yoctoNEAR for {bytes} bytes of storage for {n} new callers \
                 ({} bytes each), {}x the {paid} they paid in gas and deposits; its free balance \
                 {free} covers about {} more",
                r.receiver_id,
                bytes as u128 / n,
                staked / paid.max(1),
                free / (staked / n).max(1)
            );
            found.push(self.at_receipt(builtin::STORAGE_FOR_CALLERS, message, r, &method));
        }
        for m in &self.plan.margin {
            if let Some((r, message)) = super::curves::margin::judge(m, receipts()) {
                let method = m.site.rsplit("::").next().unwrap_or_default();
                found.push(self.at_receipt(builtin::MARGIN, message, r, method));
            }
        }
        found
    }

    /// A finding of `check` at receipt `r`, calling `m`.
    fn at_receipt(&self, check: &str, why: String, r: &ReceiptOutcome, m: &str) -> Finding {
        let op = self.op_of.get(&r.tx).map(|&i| self.ops[i].record.name.clone());
        let receiver = self.names.known(&r.receiver_id, self.harness.fixture.as_ref());
        let site = Site::Receipt { receiver, method: Some(m.to_owned()), op };
        Finding {
            receipt_id: Some(r.receipt_id),
            ..Finding::at(check, why, site, r.block_height, Some(r.tx))
        }
    }

    /// Labels what was forced, and gives the verdict.
    fn finish(mut self) -> Run<O> {
        let txs: Vec<CryptoHash> = self.ops.iter().flat_map(|o| o.record.txs.clone()).collect();
        let outcomes: Vec<TxOutcome> = txs.iter().map(|t| self.sim().outcome(t)).collect();
        for f in self.must_not_fail(&outcomes) {
            self.add(f);
        }
        if let Some((r, m, why)) =
            super::outsider::self_only(self.harness, self.plan, &outcomes, self.sim())
        {
            let f = self.at_receipt(builtin::SELF_ONLY, why, r, &m);
            self.add(f);
        }
        // A finding at or after the first block where anything was forced
        // may be its consequence.
        let forced_at = outcomes.iter().flat_map(|o| &o.receipts).filter_map(forced_from).min();
        for (i, (block, why)) in std::mem::take(&mut self.late) {
            self.findings[i].message += &format!("\n  last failed at block {block}: {why}");
        }
        for f in &mut self.findings {
            f.injected = forced_at.is_some_and(|b| f.block >= b);
        }
        let sim = self.world.as_ref();
        let (landed, not_applied) = self.landed(&outcomes);
        let findings = std::mem::take(&mut self.findings);
        let receipts = || outcomes.iter().flat_map(|o| &o.receipts);
        let reached = receipts()
            .flat_map(|r| {
                r.method_names
                    .iter()
                    .map(|m| self.names.site(&r.receiver_id, Some(m.as_str()), sim))
            })
            .collect();
        let failed = (receipts().filter(|r| r.failure().is_some()))
            .map(|r| {
                (
                    r.receipt_id,
                    self.names.site(
                        &r.receiver_id,
                        r.method_names.first().map(String::as_str),
                        sim,
                    ),
                )
            })
            .collect();
        let spawned = (self.ops.iter())
            .map(|o| {
                let txs = o.record.txs.iter();
                txs.map(|t| (sim.chain.spawned(t).to_vec(), sim.chain.buffered(t).to_vec()))
                    .collect()
            })
            .collect();
        let fingerprint = hash(
            format!("{:?}", outcomes.iter().map(|o| (&o.status, &o.receipts)).collect::<Vec<_>>())
                .as_bytes(),
        );
        let ((verdict, reason), (unjudged_only, overlapped_only)) =
            self.classify(not_applied, &findings);
        let sim_height = sim.height();
        let produced = |s: &Sim| s.chain.log().blocks.len() as u64;
        let blocks = produced(sim) - produced(self.harness.fixture.as_ref());
        let panicked =
            if let Some(Stop::Panicked(why)) = &self.stop { Some(why.clone()) } else { None };
        Run {
            plan: self.plan.clone(),
            verdict,
            reason,
            findings,
            judged: self.judged,
            ops: self.ops.iter().map(|o| o.record.clone()).collect(),
            outcomes,
            landed,
            spawned,
            untargeted: vec![],
            budget: matches!(self.stop, Some(Stop::Budget(_))),
            timed_out: matches!(self.stop, Some(Stop::Timeout(_))),
            blocks,
            panicked,
            on_fixture: matches!(self.stop, Some(Stop::Fixture(_))),
            fingerprint,
            reached,
            failed,
            seen: Default::default(),
            world: Ended(std::rc::Rc::new(self.world), sim_height),
            unjudged_only,
            overlapped_only,
        }
    }

    /// Where each of the plan's injections landed, in step order; and each
    /// exact injection (no `nth`) of an op that ran that applied to none.
    /// The chain says which injection won each receipt, so two with one
    /// fault never both land on one receipt.
    fn landed(&self, outcomes: &[TxOutcome]) -> (Vec<Landed>, Vec<String>) {
        let (mut landed, mut missed, fixture) = (vec![], vec![], self.harness.fixture.as_ref());
        let chain = &self.world.as_ref().chain;
        for (i, (step, op)) in self.plan.steps.iter().zip(&self.ops).enumerate() {
            for (k, inject) in step.inject.iter().enumerate() {
                let tx = op.record.txs.get(usize::from(inject.tx));
                // Its position among the injections of its transaction.
                let at = step.inject[..k].iter().filter(|x| x.tx == inject.tx).count();
                let won = tx.and_then(|t| chain.won(t).iter().find(|(n, _)| *n == at));
                let rs = || outcomes.iter().flat_map(|o| &o.receipts);
                let r = won.and_then(|(_, id)| match inject.fault {
                    // The callback its result went to.
                    Fault::HoldResult(_) => rs().find(|r| r.receipt_id == *id).and_then(|s| {
                        let sent = |d: &CryptoHash| {
                            s.output_data_receivers.iter().any(|x| x.data_id == *d)
                        };
                        rs().find(|r| r.input_data_ids.iter().any(sent))
                    }),
                    _ => rs().find(|r| r.receipt_id == *id),
                });
                match r {
                    Some(r) => landed.push(Landed {
                        fault: inject.fault.clone(),
                        site: self.names.site(&r.receiver_id, r.method(), fixture),
                    }),
                    None if !inject.at.is_sample() && op.record.skipped.is_none() => {
                        // Why it couldn't apply where it selected, if it did.
                        let passed = tx.map_or(&[][..], |t| self.world.as_ref().chain.passed(t));
                        let why = passed.iter().find(|(f, _)| *f == inject.fault);
                        let why = why.map_or(String::new(), |(_, why)| format!(": at {why}"));
                        missed
                            .push(format!("step {i}'s inject {inject} applied to no receipt{why}"))
                    }
                    None => {}
                }
            }
        }
        (landed, missed)
    }

    /// The verdict, in precedence order: a harness error, then findings,
    /// then what kept the case from completing; and whether all that kept
    /// it from completing is checks not judged, and per-op checks not
    /// judged under overlap.
    fn classify(
        &self,
        not_applied: Vec<String>,
        found: &[Finding],
    ) -> ((Verdict, Option<String>), (bool, bool)) {
        let unjudged: u32 = self.judged.iter().map(|j| j.unjudged).sum();
        let only = |gaps: &[String]| unjudged > 0 && gaps.len() == self.judged_gaps();
        let mut gaps: Vec<String> = match &self.stop {
            Some(Stop::Harness(why) | Stop::Panicked(why) | Stop::Fixture(why)) => {
                return ((Verdict::HarnessError, Some(why.clone())), (false, false));
            }
            Some(Stop::Budget(why) | Stop::Timeout(why)) => vec![why.clone()],
            None => vec![],
        };
        let pending: Vec<String> = (self.ops.iter().enumerate())
            .filter(|(_, o)| !o.done)
            .map(|(i, o)| format!("{} (step {i})", o.record.name))
            .collect();
        if !pending.is_empty() {
            gaps.push(format!("pending, not judged: {}", pending.join(", ")));
        }
        for j in self.judged.iter().filter(|j| j.unjudged > 0) {
            let why = j.why.as_deref().unwrap_or_default();
            gaps.push(format!("{} not judged {} time(s): {why}", j.check, j.unjudged));
        }
        gaps.extend(not_applied);
        if self.stop.is_none() && self.ops.iter().all(|o| o.record.skipped.is_some()) {
            gaps.push("no op ran: the plan is empty or every op skipped".into());
        }
        let unjudged_only = found.is_empty() && only(&gaps);
        let overlapped_only = unjudged_only && unjudged == self.overlapped;
        // Findings: the first, and how many more, before any gap.
        if let Some(f) = found.first() {
            let more = found.len() - 1;
            let more = if more > 0 { format!(" and {more} more") } else { String::new() };
            gaps.insert(0, format!("{} at {}{more}", f.check, f.site));
        }
        let verdict = match (!found.is_empty(), gaps.is_empty()) {
            (true, _) => Verdict::Findings,
            (false, true) => Verdict::Clean,
            (false, false) => Verdict::Incomplete,
        };
        let only = (unjudged_only, overlapped_only);
        ((verdict, (!gaps.is_empty()).then(|| gaps.join("; "))), only)
    }

    /// The gaps the checks not judged make: one a check.
    fn judged_gaps(&self) -> usize {
        self.judged.iter().filter(|j| j.unjudged > 0).count()
    }
}

/// What fixes the world a case of `plan` keeps just before step `i`
/// submits (where the serial orders of the ops in flight from it start),
/// hashed: the plan before that step. A case is a replay of its plan from
/// the fixture, so the world there is a function of what the replay read
/// up to then; an enumeration relies on replays being deterministic and
/// checks its first case. The key covers a `W`'s own fields and what the
/// trie leaves out (the clock, epoch and validator state, stub replies,
/// parked yields) without listing them. Up to then, a case reads:
/// - the fixture: one per memo, as `name_traps`' copy drops it;
/// - the plan's setup, planted keys, start gap and sources, and the steps
///   before `i` with their gaps, gas, injections (sampled ones resolved:
///   this is the plan the case plays) and stub answers: its serde, cut at
///   `i`. Of what serde skips, `bands` is read (each step's hook);
///   `errors` stops a case before it plays, and `refused` judges only;
/// - for a raw call, step `i` itself: the case makes its signer and fresh
///   callers before it keeps the world. An op's own step runs after.
///
/// An op is its serde value ([`Op`]), as in a replay. A raw call's args,
/// if they parse as JSON, are keyed by that JSON, normalized: two calls
/// apart only in their JSON's spacing share a key though they send other
/// bytes, which `sent` compares, so the key isn't exact for them. An
/// enumeration's plans hold ops only, with no setup and no `bands`: the
/// `Act::Call` branch, and the raw calls and `bands` in the key, are for
/// plans it doesn't make.
///
/// Checks only read the world; the case timeout stops a case before its
/// serial orders; the thread-locals (the fixture's gas, progress) are an
/// enumeration's own. `None` if the plan doesn't serialize.
fn start_key<O: Op<W>, W: World>(plan: &Plan<O>, i: usize) -> Option<CryptoHash> {
    let mut before = plan.clone();
    before.steps.truncate(i);
    let own = matches!(plan.steps[i].act, Act::Call(_)).then(|| &plan.steps[i]);
    let bytes = serde_json::to_vec(&(&before, plan.bands, own)).ok()?;
    Some(CryptoHash::hash_bytes(&bytes))
}

#[cfg(test)]
thread_local! {
    /// The checks' judgments of a fixture on this thread, each a full
    /// pass over them ([`Case::judge_fixture`]).
    pub(crate) static FIXTURE_JUDGED: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    /// An enumeration judges the fixture in every case, as before it
    /// judged it once: the differential tests' reference.
    pub(crate) static FIXTURE_EACH_CASE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// The world `plan`'s case starts from: the fixture after its setup.
pub(crate) fn set_up<O: Op<W>, W: World>(fixture: &W, plan: &Plan<O>) -> W {
    let mut w = fixture.clone();
    let sim = w.as_mut();
    let new =
        |(a, k): &&(AccountId, Vec<u8>)| sim.account(a).is_none() || sim.state(a, k).is_none();
    let records: Vec<_> = (plan.planted.iter().filter(new))
        .map(|(a, k)| near_primitives::state_record::StateRecord::Data {
            account_id: a.clone(),
            data_key: k.clone().into(),
            value: vec![1].into(),
        })
        .collect();
    sim.apply_state_records(records);
    if !plan.setup.is_empty() {
        play_plain(&mut w, &Plan { steps: plan.setup.clone(), ..Plan::default() });
    }
    w
}

/// Plays `plan`'s steps on `world`, unjudged: each step submits, then its
/// gap; then the chain settles. Its transactions' outcomes, in order. A
/// step that skips or panics submits nothing.
pub(crate) fn play_plain<O: Op<W>, W: World>(world: &mut W, plan: &Plan<O>) -> Vec<TxOutcome> {
    let from = world.as_ref().txs().len();
    let gap = |world: &mut W, g: Gap| match g.blocks_on(world.as_ref()) {
        None => world.as_mut().settle(),
        Some(n) => (0..n).for_each(|_| world.as_mut().step()),
    };
    if let Some(g) = plan.start {
        gap(world, g);
    }
    for step in &plan.steps {
        world.as_mut().explore =
            Some(Box::new(super::Hook::new(step.gas, step.inject.clone(), step.answers.clone())));
        let _ = guarded(|| match &step.act {
            Act::Op(op) => drop(op.submit(world)),
            Act::Call(call) => call.send(world.as_mut()),
        });
        world.as_mut().explore = None;
        gap(world, step.then);
    }
    let sim = world.as_mut();
    sim.settle();
    let txs = sim.txs()[from..].to_vec();
    txs.iter().map(|t| sim.outcome(t)).collect()
}

/// How a case names a receipt's receiver in a site: an account the plan
/// made up (one the fixture doesn't have, named by a string of the plan,
/// whole or as its first label) or an implicit account the fixture lacks
/// is `<new account>`, so one behavior is one finding whatever name was
/// drawn. Any other account is itself.
pub(crate) struct Names {
    drawn: HashSet<String>,
}

impl Names {
    pub(crate) fn new<O: Serialize + Clone>(plan: &Plan<O>) -> Names {
        fn walk(v: &serde_json::Value, out: &mut HashSet<String>) {
            match v {
                serde_json::Value::String(s) => drop(out.insert(s.clone())),
                serde_json::Value::Array(a) => a.iter().for_each(|v| walk(v, out)),
                serde_json::Value::Object(m) => m.values().for_each(|v| walk(v, out)),
                _ => {}
            }
        }
        // The acts only: a fault's or an answer's account is the world's.
        let mut drawn = HashSet::new();
        for step in &plan.steps {
            walk(&step.act.json().unwrap_or_default(), &mut drawn);
        }
        Names { drawn }
    }

    /// `id`, or `None` if the plan made it up.
    pub(crate) fn known(&self, id: &AccountId, fixture: &Sim) -> Option<AccountId> {
        let first = id.as_str().split('.').next().unwrap_or_default();
        let drawn = self.drawn.contains(id.as_str()) || self.drawn.contains(first);
        let made_up = (drawn || implicit(id.as_str())) && fixture.account(id).is_none();
        (!made_up).then(|| id.clone())
    }

    /// `receiver::method`, or `receiver` for a receipt that calls nothing.
    pub(crate) fn site(&self, receiver: &AccountId, method: Option<&str>, fixture: &Sim) -> String {
        let new = || crate::findings::NEW_ACCOUNT.to_string();
        let receiver = self.known(receiver, fixture).map_or_else(new, |id| id.to_string());
        method.map_or(receiver.clone(), |m| format!("{receiver}::{m}"))
    }
}

/// 64 lowercase hex (NEAR), or `0x` and 40 (ETH).
fn implicit(id: &str) -> bool {
    let hex = |s: &str, n: usize| {
        s.len() == n && s.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    };
    hex(id, 64) || id.strip_prefix("0x").is_some_and(|rest| hex(rest, 40))
}

/// Runs a user check; its panic is marked, a harness error, not a finding.
fn judge(check: impl FnOnce() -> Result<(), Fail>) -> Result<(), Fail> {
    guarded(check).unwrap_or_else(|panic| Err(Fail { panicked: true, ..Fail::new(panic) }))
}

/// One op's effects over its transactions.
pub(crate) fn merge(outcomes: &[TxOutcome]) -> Effects {
    if let [one] = outcomes {
        return one.effects().clone();
    }
    let mut m = Effects::default();
    for e in outcomes.iter().map(TxOutcome::effects) {
        m.near.extend(e.near.iter().cloned());
        m.token_moves.extend(e.token_moves.iter().cloned());
        m.pending.extend(e.pending.iter().cloned());
        m.gas = m.gas.saturating_add(e.gas);
        m.queued |= e.queued;
        for (token, why) in &e.watched {
            let w = m.watched.entry(token.clone()).or_insert(None);
            *w = w.take().or_else(|| why.clone());
        }
        m.supplied.extend(e.supplied.iter().cloned());
    }
    m
}

/// What is wrong with `plan` by itself: what its builder refused, a gas
/// of 0, an injection that gives no gas.
pub(crate) fn plan_problems<O>(plan: &Plan<O>) -> Vec<String> {
    let mut out = plan.errors.clone();
    let gaps = plan.start.iter().chain(plan.steps.iter().map(|s| &s.then));
    if gaps.into_iter().any(|g| *g == Gap::Epochs(0)) {
        out.push("a gap of Epochs(0) waits for nothing: use Blocks(0) to overlap".into());
    }
    for (i, step) in plan.steps.iter().enumerate() {
        if step.gas == Some(super::StepGas::Exactly(Gas::ZERO)) {
            out.push(format!("step {i}: gas 0; nearcore refuses a call with no gas"));
        }
        for inject in &step.inject {
            match inject.fault {
                Fault::Hold(0) | Fault::HoldResult(0) => {
                    out.push(format!("step {i}: inject {inject} holds nothing"));
                }
                Fault::Gas(g) if g == Gas::ZERO => {
                    out.push(format!("step {i}: inject {inject}: nearcore refuses no gas"));
                }
                _ => {}
            }
        }
    }
    out
}

/// Each injection of `plan` naming a method no fixture contract exports
/// (or, naming a receiver with code, that receiver doesn't; a stub exports
/// the methods it declares): a spelling to check, or a receipt the scan
/// skips. An account with no code in the fixture (one an op deploys to)
/// has nothing to check against.
pub(crate) fn misnamed<O, W: World>(h: &Harness<O, W>, plan: &Plan<O>) -> Vec<String> {
    let mut out = vec![];
    for (i, step) in plan.steps.iter().enumerate() {
        for inject in &step.inject {
            if let Some(method) = unexported(h, &inject.at) {
                out.push(format!(
                    "step {i}: inject {inject}: no contract in the fixture exports `{method}`; \
                     check its spelling (a contract an op deploys mid-plan isn't known before \
                     it runs)"
                ));
            }
        }
    }
    out
}

/// The method `at` names, if no fixture contract it may address exports
/// it ([`misnamed`]).
fn unexported<'a, O, W: World>(h: &Harness<O, W>, at: &'a Receipts) -> Option<&'a str> {
    let (receiver, method) = at.method_named()?;
    let exports = h.exports.get_or_init(|| {
        let code = h.fixture.as_ref().contracts().into_iter();
        code.map(|(account, code)| (account, super::report::exports(&code))).collect()
    });
    let mine = |a: &String| receiver.is_none_or(|r| r.as_str() == a);
    let known = receiver.is_none_or(|r| exports.iter().any(|(a, _)| a == r.as_str()));
    let exported = exports.iter().any(|(a, ms)| mine(a) && ms.iter().any(|m| m == method));
    (!exports.is_empty() && known && !exported).then_some(method)
}

/// Whether `r` moves value: a deposit, tokens, a callback, or a call a
/// callback awaits. A finding's value path is drawn from these.
pub(crate) fn moves(r: &ReceiptOutcome) -> bool {
    !r.deposit.is_zero()
        || !r.token_moves.is_empty()
        || r.is_callback()
        || !r.output_data_receivers.is_empty()
}

/// A finding's identity across cases: its check, site and cause, the
/// receipt that failed on its value path and its failure class, and the
/// kinds of fault forced. A starved callback is its own finding, and so
/// is one class at two receipts. With nothing forced and nothing failed
/// on its path, the kinds of op that broke it are part of it too: a race
/// of two ops and one of them alone are two findings.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub(crate) struct Id {
    pub check: String,
    pub site: Site,
    pub cause: Option<String>,
    /// The kinds of op that broke it (its receipt's, or those last
    /// submitted before it), sorted, each once: two causes at one check
    /// and site keep two replays, and fold apart.
    pub ops: Vec<String>,
    /// The first natural failure on its value path: its site
    /// (`receiver::method`; none for a receipt finding, whose site it is)
    /// and class.
    pub receipt: Option<String>,
    pub class: Option<String>,
    pub faults: Vec<(String, String)>,
}

impl Id {
    /// The cause, the faults forced and the ops aside: what a forced
    /// variant shares with the same finding shown.
    pub(crate) fn cause(&self) -> Id {
        Id { ops: vec![], faults: vec![], ..self.clone() }
    }
}

/// The first natural failure that moves value on `f`'s path: its own
/// receipt's, else one of its op's transactions' (a per-op or built-in
/// finding), else one after the check last passed (a world check).
pub(crate) fn on_path<'r, O>(
    run: &'r Run<O>,
    f: &Finding,
) -> Option<(&'r ReceiptOutcome, Failure)> {
    let hit = match (f.receipt_id, f.tx) {
        (Some(id), _) => run.outcomes.iter().flat_map(|o| &o.receipts).find(|r| r.receipt_id == id),
        (None, Some(tx)) => {
            let txs = run.ops.iter().find(|o| o.txs.contains(&tx)).map(|o| o.txs.clone());
            failed_in(run, &txs.unwrap_or_else(|| vec![tx]))
        }
        (None, None) => failed(run)
            .filter(|r| f.since.is_none_or(|s| r.block_height > s) && r.block_height <= f.block)
            .min_by_key(|r| r.block_height),
    }?;
    Some((hit, hit.failure()?))
}

/// The natural failures that move value in `run`.
fn failed<O>(run: &Run<O>) -> impl Iterator<Item = &ReceiptOutcome> {
    let natural = |r: &&ReceiptOutcome| r.fault.is_none() && !r.is_refund() && moves(r);
    let receipts = run.outcomes.iter().flat_map(|o| &o.receipts).filter(natural);
    receipts.filter(|r| r.failure().is_some())
}

/// The first of them in the transactions `txs`.
pub(crate) fn failed_in<'r, O>(run: &'r Run<O>, txs: &[CryptoHash]) -> Option<&'r ReceiptOutcome> {
    failed(run).filter(|r| txs.contains(&r.tx)).min_by_key(|r| r.block_height)
}

/// `f`'s identity, as `run` shows it.
pub(crate) fn id<O>(run: &Run<O>, f: &Finding) -> Id {
    let landed = run.landed.iter().filter(|_| f.injected);
    let mut faults: Vec<_> = landed.map(|l| (l.site.clone(), l.fault.kind().to_owned())).collect();
    faults.sort_unstable();
    faults.dedup();
    let path = on_path(run, f);
    // A receipt finding's own receipt is its site already.
    let receipt = path.as_ref().filter(|_| f.receipt_id.is_none()).map(|(r, _)| {
        run.failed.get(&r.receipt_id).cloned().unwrap_or_else(|| r.method_names.join(","))
    });
    let class = path.map(|(_, failure)| class(&failure));
    let ops = breakers(run, f);
    let (check, site, cause) = (f.check.clone(), f.site.clone(), f.cause.clone());
    Id { check, site, cause, ops, receipt, class, faults }
}

/// The kinds of op that broke `f`, sorted, each once: its receipt's or
/// transaction's op; a per-op check's op, by its site, where it had no
/// transaction; for a check of the world, of the ops submitted since it
/// last held, those that took effect ([`OpRecord::at`]) last before it
/// failed. A race of one op and that op alone are then one.
fn breakers<O>(run: &Run<O>, f: &Finding) -> Vec<String> {
    // A receipt's finding names its transaction too.
    let mut ops: Vec<String> = match (f.tx, &f.site) {
        (Some(tx), _) => {
            run.ops.iter().filter(|o| o.txs.contains(&tx)).map(|o| o.name.clone()).collect()
        }
        (None, Site::Op(op) | Site::Key { op: Some(op), .. }) => vec![op.clone()],
        (None, _) => {
            let at: Vec<(u64, &OpRecord)> = (run.ops.iter().skip(f.ops_since.unwrap_or(0)))
                .filter_map(|o| Some((o.at?, o)))
                .filter(|(b, _)| *b <= f.block)
                .collect();
            let last = at.iter().map(|(b, _)| *b).max();
            at.iter().filter(|(b, _)| Some(*b) == last).map(|(_, o)| o.name.clone()).collect()
        }
    };
    ops.sort_unstable();
    ops.dedup();
    ops
}

/// A failure's class: its kind, with each word that names a value made
/// `#`, so two amounts are one class.
pub(crate) fn class(failure: &Failure) -> String {
    let variant = |debug: String| {
        let end = debug.find([' ', '(', '{']).unwrap_or(debug.len());
        debug[..end].to_owned()
    };
    match failure {
        Failure::OutOfGas => "out of gas".into(),
        Failure::GasLimitExceeded => "gas limit exceeded".into(),
        Failure::Panic(m) => format!("panicked: {}", words(m)),
        Failure::AccountDoesNotExist(_) => "account does not exist".into(),
        Failure::InvalidTx(e) => format!("invalid transaction: {}", variant(format!("{e:?}"))),
        Failure::Action(kind) => variant(format!("{kind:?}")),
        Failure::Host(m) => words(m),
    }
}

/// `m` with each word that names a value made `#`; at most 120 bytes.
fn words(m: &str) -> String {
    let value = |w: &str| {
        let core = w.trim_matches(|c: char| !c.is_alphanumeric());
        let account = core.contains('.')
            && core
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || "._-".contains(c));
        core.chars().any(|c| c.is_ascii_digit()) || account || core.len() >= 32
    };
    let out: Vec<&str> = m.split_whitespace().map(|w| if value(w) { "#" } else { w }).collect();
    let mut out = out.join(" ");
    if let Some((i, _)) = out.char_indices().nth(120) {
        out.truncate(i);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_derived_implicit_account_is_a_new_account() {
        let mut sim = Sim::new();
        let held = "ab".repeat(32);
        sim.create_account(&held, Balance::from_near(1));
        sim.create_account("alice.near", Balance::from_near(1));
        let plan = Plan::new().op("bob".to_owned());
        let names = Names::new(&plan);
        let id = |s: &str| s.parse::<AccountId>().unwrap();
        let new = crate::findings::NEW_ACCOUNT;
        for derived in ["8cf42e".repeat(10) + "8cf4", format!("0x{}", "dc".repeat(20))] {
            assert_eq!(names.site(&id(&derived), None, &sim), new, "{derived}");
        }
        assert_eq!(names.site(&id(&held), None, &sim), held);
        assert_eq!(names.site(&id("bob.near"), None, &sim), new);
        assert_eq!(names.site(&id("alice.near"), None, &sim), "alice.near");
        assert_eq!(names.site(&id("carol.near"), None, &sim), "carol.near");
    }

    /// A shrink's candidate past its deadline is cut at the next block
    /// timed out, saying so, never run to idle.
    #[test]
    fn a_candidate_past_its_deadline_is_cut() {
        let sim = crate::Fixture::new()
            .account("alice.near", Balance::from_near(1))
            .stub("s.near", crate::Stub::new().on("m", crate::Reply::json(())))
            .build();
        #[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
        struct Ping;
        impl super::super::Op for Ping {
            fn submit(&self, sim: &mut Sim) -> Result<(), super::super::Skip> {
                sim.call("alice.near", "s.near", "m").submit();
                Ok(())
            }
        }
        let h = super::super::Harness::new(sim);
        let plan = Plan::new().op(Ping);
        let whole = h.run_until(&plan, None);
        assert!(!whole.timed_out && whole.verdict == Verdict::Clean, "{whole}");
        let cut = h.run_until(&plan, Some(std::time::Instant::now()));
        assert!(cut.timed_out, "{cut}");
        assert!(cut.reason.as_deref().unwrap_or_default().contains("past the shrink's deadline"));
    }

    /// A case past the harness's timeout stops at its next block,
    /// incomplete, and a scan lists it as a to-do item.
    #[test]
    fn a_case_past_its_timeout_is_a_todo() {
        let sim = crate::Fixture::new()
            .account("alice.near", Balance::from_near(1))
            .stub("s.near", crate::Stub::new().on("m", crate::Reply::json(())))
            .build();
        #[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
        struct Ping;
        impl super::super::Op for Ping {
            fn submit(&self, sim: &mut Sim) -> Result<(), super::super::Skip> {
                sim.call("alice.near", "s.near", "m").submit();
                Ok(())
            }
        }
        let mut h = super::super::Harness::new(sim);
        h.timeout = std::time::Duration::from_nanos(1);
        let plan = Plan::new().op(Ping);
        let run = h.run(&plan);
        assert_eq!(run.verdict, Verdict::Incomplete, "{run}");
        assert!(run.reason.as_deref().unwrap_or_default().contains("timed out"), "{run}");
        let audit = h.scan(&[plan]).faults(super::super::Faults::none()).run();
        assert!(audit.todo().iter().any(|t| t.id.starts_with("timeout:")), "{audit}");
    }

    /// An op submitted while a yield is parked, its timeout ahead, is
    /// idle, even alone, but not at rest: the books take no reading before
    /// it, where the yield's resolution is still to come. `Park`
    /// is at rest, but not alone: its yield is unresolved when `Pay` submits.
    #[test]
    fn an_op_submitted_while_a_yield_is_parked_is_not_at_rest() {
        const WAT: &str = r#"(module
          (import "env" "promise_yield_create"
            (func $yc (param i64 i64 i64 i64 i64 i64 i64) (result i64)))
          (memory 1)
          (export "memory" (memory 0))
          (data (i32.const 0) "cb")
          (func (export "park")
            (drop (call $yc (i64.const 2) (i64.const 0) (i64.const 0) (i64.const 0)
                            (i64.const 10000000000000) (i64.const 0) (i64.const 1))))
          (func (export "cb")))"#;
        let sim = crate::Fixture::new()
            .account("alice.near", Balance::from_near(10))
            .account("bob.near", Balance::from_near(10))
            .contract("y.near", wat::parse_str(WAT).unwrap())
            .build();
        #[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
        enum Y {
            Park,
            Pay,
        }
        impl super::super::Op for Y {
            fn submit(&self, sim: &mut Sim) -> Result<(), super::super::Skip> {
                match self {
                    Y::Park => drop(sim.call("alice.near", "y.near", "park").submit()),
                    Y::Pay => drop(
                        sim.tx("alice.near", "bob.near").transfer(Balance::from_near(1)).submit(),
                    ),
                }
                Ok(())
            }
        }
        let seen = std::rc::Rc::new(std::cell::RefCell::new(vec![]));
        let into = seen.clone();
        let h = super::super::Harness::<Y>::new(sim).at_rest_ops("reads", move |_, ops| {
            *into.borrow_mut() = ops.iter().map(|d| (d.isolated, d.rest)).collect();
            Ok::<(), String>(())
        });
        let run = h.run(&Plan::new().op(Y::Park).op(Y::Pay));
        assert_eq!(*seen.borrow(), [(false, true), (true, false)], "{run}");
    }

    #[test]
    fn a_class_hides_the_values_it_names() {
        let a = class(&Failure::Panic("short by 300 for alice.near".into()));
        assert_eq!(a, class(&Failure::Panic("short by 7 for bob.near".into())));
        assert_eq!(a, "panicked: short by # for #");
        let kinds = [Fault::Hold(3), Fault::Gas(Gas::from_gas(1)), Fault::ReturnValue(vec![1])];
        assert_eq!(kinds.map(|f| f.kind()), ["Hold", "Gas", "ReturnValue"]);
    }
}
