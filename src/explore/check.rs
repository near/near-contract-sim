//! [`Harness`]: a fixture plus checks; [`OpDone`], what a per-op check
//! reads.

use super::{Fail, Op, Plan, World};
use crate::effects::Effects;
use crate::explore::builtin;
use crate::findings::Site;
use crate::outcome::{ReceiptOutcome, TxOutcome};
use crate::sim::Sim;
use std::path::PathBuf;
use std::rc::Rc;

/// A fixture and the checks every case is judged by. [`run`](Self::run)
/// executes one [`Plan`] on a fresh clone of the fixture.
///
/// Where each kind of check runs is in the skill's harness.md.
///
/// The built-in checks ([`builtin`]) run too, as in [`Sim::findings`]: a
/// receipt a fault starved is excused, what it caused downstream is not.
/// A check returns `Result<(), E>` for any `E: Into<`[`Fail`]`>`. Check
/// names must be unique, and differ from the built-ins'. Each case judges
/// the checks on the fixture before its first step, [`at_end`](Self::at_end)
/// ones too: one failing there is a harness error naming it (the fixture
/// or the contract's initial state is wrong, and no case can tell
/// anything about that check). After a [`Plan::setup`] it is a finding.
#[must_use]
#[derive(Clone)]
pub struct Harness<O, W = Sim> {
    pub(crate) fixture: W,
    pub(crate) always: Vec<Check<W>>,
    pub(crate) at_rest: Vec<Rest<O, W>>,
    pub(crate) per_op: Vec<PerOp<O, W>>,
    /// Checks at the end of each case, which may play plans on a copy of
    /// the world (the books' liveness).
    pub(crate) end: Vec<End<O, W>>,
    pub(crate) must_not_fail: Vec<String>,
    /// Sites any caller may call ([`anyone_may_call`](Self::anyone_may_call)).
    pub(crate) anyone: Vec<String>,
    /// To-do items excused, and why ([`excuse`](Self::excuse)).
    pub(crate) excused: Vec<(String, String)>,
    /// Where replays go ([`store`](Self::store)).
    pub(crate) store: Option<PathBuf>,
    /// The contracts audited ([`audits`](Self::audits)); `None`, all.
    pub(crate) audits: Option<Vec<String>>,
    /// The books' readings ([`diff`](Self::diff)).
    pub(crate) readings: Vec<(String, super::books::Reading<W>)>,
    /// While an enumeration runs, what its cases share ([`Memo`]): the
    /// end checks' verdicts, the serial orders, the fixture's judgments.
    pub(crate) memo: Option<Rc<Memo<W>>>,
    /// What [`builtin::SERIALIZABLE`] reads, once books are declared.
    pub(crate) serial: Option<Rc<super::books::Serial<W>>>,
    /// Each fixture contract's exports, read once ([`misnamed`](super::run::misnamed)).
    pub(crate) exports: std::cell::OnceCell<Vec<(String, Vec<String>)>>,
    /// The longest a case runs: [`CASE_TIMEOUT`], shorter in unit tests.
    pub(crate) timeout: std::time::Duration,
    /// What settling each finding's row learned by running ([`Settled`](super::audit::Settled)),
    /// shared by the harness's clones.
    pub(crate) settled: Rc<std::cell::RefCell<super::audit::Settled<O>>>,
}

/// An enumeration's memo of the end checks' verdicts (the books'
/// liveness), by the dedup key of the world at rest they judged: a state
/// the key calls equal is judged once. And of
/// [`SERIALIZABLE`](builtin::SERIALIZABLE)'s serial orders, by the plan
/// that reached their start, never by that key ([`books::Orders`](super::books::Orders)).
/// And of the checks' judgments of the fixture, which every case
/// without setup starts from: judged by the first, credited to each.
pub(crate) struct Memo<W> {
    pub key: Rc<dyn Fn(&W) -> u64>,
    pub verdicts: std::cell::RefCell<std::collections::HashMap<(u64, usize), Result<(), Fail>>>,
    pub orders: super::books::Orders,
    /// The results a case records on the fixture, each by its check's
    /// index, in judging order: an `at_end` check's failure only.
    pub fixture: std::cell::OnceCell<Vec<(usize, Result<(), Fail>)>>,
}

impl<W> Memo<W> {
    pub(crate) fn new(key: Rc<dyn Fn(&W) -> u64>) -> Self {
        let (verdicts, orders, fixture) = Default::default();
        Memo { key, verdicts, orders, fixture }
    }
}

/// The longest one case runs: a case past it stops at its next block,
/// incomplete, and an audit lists it once as a to-do item
/// (`timeout:<variant>`); the campaign goes on.
pub(crate) const CASE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

type CheckFn<W> = Rc<dyn Fn(&W) -> Result<(), Fail>>;

fn world<W, E: Into<Fail>>(check: impl Fn(&W) -> Result<(), E> + 'static) -> CheckFn<W> {
    Rc::new(move |w: &W| check(w).map_err(Into::into))
}

#[derive(Clone)]
pub(crate) struct Check<W> {
    pub name: String,
    pub check: CheckFn<W>,
}

type OpsFn<O, W> = Rc<dyn for<'a> Fn(&W, &[OpDone<'a, O, W>]) -> Result<(), Fail>>;

/// An `at_rest` check: of the world, or of the world and the case's ops.
#[derive(Clone)]
pub(crate) enum RestFn<O, W> {
    World(CheckFn<W>),
    Ops(OpsFn<O, W>),
}

#[derive(Clone)]
pub(crate) struct Rest<O, W> {
    pub name: String,
    pub check: RestFn<O, W>,
    /// Judged once, at the case's end, not at every rest point.
    pub at_end: bool,
    /// Its ops include the plan's raw calls ([`Plan::call`]): the books'.
    pub calls: bool,
}

/// Plays a plan's steps on a world, unjudged, and returns its outcomes.
pub(crate) type Player<O, W> = dyn Fn(&mut W, &Plan<O>) -> Vec<TxOutcome>;

type EndFn<O, W> = Rc<dyn Fn(&W, &Player<O, W>) -> Result<(), Fail>>;

#[derive(Clone)]
pub(crate) struct End<O, W> {
    pub name: String,
    pub check: EndFn<O, W>,
}

/// Which ops a per-op check judges.
type Applies<O> = Rc<dyn Fn(&O) -> bool>;

type PerOpFn<O, W> = Rc<dyn for<'a> Fn(&OpDone<'a, O, W>) -> Result<(), Fail>>;

#[derive(Clone)]
pub(crate) struct PerOp<O, W> {
    pub name: String,
    /// The ops it judges; `None` for every op.
    pub applies: Option<Applies<O>>,
    pub check: PerOpFn<O, W>,
}

impl<O: Op<W>, W: World> Harness<O, W> {
    /// A harness on `fixture`, cloned for every case. Settle it first: a
    /// case must start with nothing queued or in flight.
    pub fn new(fixture: W) -> Self {
        Harness {
            fixture,
            always: vec![],
            at_rest: vec![],
            per_op: vec![],
            end: vec![],
            must_not_fail: vec![],
            anyone: vec![],
            excused: vec![],
            store: None,
            audits: None,
            readings: vec![],
            serial: None,
            memo: None,
            exports: Default::default(),
            timeout: CASE_TIMEOUT,
            settled: Default::default(),
        }
    }

    /// The fixture every case starts from.
    pub fn fixture(&self) -> &W {
        &self.fixture
    }

    /// The most one transaction may attach on its fixture
    /// ([`Sim::gas_ceiling`]).
    pub(crate) fn gas_ceiling(&self) -> near_primitives::types::Gas {
        self.fixture.as_ref().gas_ceiling()
    }

    /// Saves replay files in `dir` instead of
    /// `target/near-contract-sim/found/<test>/` (`CARGO_TARGET_DIR` for
    /// `target` when set, or `NEAR_SIM_FOUND_DIR`).
    pub fn store(mut self, dir: impl Into<PathBuf>) -> Self {
        self.store = Some(dir.into());
        self
    }

    /// A check that must hold after every block, the fixture included,
    /// and after an op that queued nothing, nothing in flight.
    pub fn always<E: Into<Fail>>(
        mut self,
        name: &str,
        check: impl Fn(&W) -> Result<(), E> + 'static,
    ) -> Self {
        self.always.push(Check { name: name.to_owned(), check: world(check) });
        self
    }

    /// A check that must hold whenever nothing is in flight: no receipt
    /// pending, parked yields and the callbacks waiting on them included;
    /// the fixture too, and at the end of the case.
    pub fn at_rest<E: Into<Fail>>(
        self,
        name: &str,
        check: impl Fn(&W) -> Result<(), E> + 'static,
    ) -> Self {
        self.rest(name, RestFn::World(world(check)), false)
    }

    /// An [`at_rest`](Self::at_rest) check judged once per case, at its
    /// end (everything settled, parked yields timed out), instead of at
    /// every rest point: for a check too costly to run after every block.
    /// It can't name the step that broke it as finely.
    pub fn at_end<E: Into<Fail>>(
        self,
        name: &str,
        check: impl Fn(&W) -> Result<(), E> + 'static,
    ) -> Self {
        self.rest(name, RestFn::World(world(check)), true)
    }

    pub(crate) fn rest(mut self, name: &str, check: RestFn<O, W>, at_end: bool) -> Self {
        self.at_rest.push(Rest { name: name.to_owned(), check, at_end, calls: false });
        self
    }

    /// An [`at_rest`](Self::at_rest) check that also reads every op of the
    /// case that submitted so far, in plan order, each complete, with the
    /// world now; a raw call ([`Plan::call`]) is no op. The ops in flight
    /// together: [`OpDone::rest`].
    pub fn at_rest_ops<E: Into<Fail>>(
        self,
        name: &str,
        check: impl Fn(&W, &[OpDone<O, W>]) -> Result<(), E> + 'static,
    ) -> Self {
        let check: OpsFn<O, W> =
            Rc::new(move |w: &W, ops: &[OpDone<'_, O, W>]| check(w, ops).map_err(Into::into));
        self.rest(name, RestFn::Ops(check), false)
    }

    /// A check of what one op did, once its transactions are complete,
    /// even while other ops are in flight: [`OpDone::effects`] are the
    /// op's own receipts' moves. An op still incomplete at the end of the
    /// case is *pending, not judged*, and the run is incomplete. A check
    /// for some ops only is [`per_op_for`](Self::per_op_for), not an early
    /// `Ok(())` here, which counts every other op as a pass.
    pub fn per_op<E: Into<Fail>>(
        self,
        name: &str,
        check: impl Fn(&OpDone<O, W>) -> Result<(), E> + 'static,
    ) -> Self {
        self.op_check(None, name, check)
    }

    /// A [`per_op`](Self::per_op) check of only the ops `applies` accepts:
    /// `per_op_for(|op| matches!(op, VaultOp::Withdraw { .. }), name, f)`.
    pub fn per_op_for<E: Into<Fail>>(
        self,
        applies: impl Fn(&O) -> bool + 'static,
        name: &str,
        check: impl Fn(&OpDone<O, W>) -> Result<(), E> + 'static,
    ) -> Self {
        self.op_check(Some(Rc::new(applies)), name, check)
    }

    fn op_check<E: Into<Fail>>(
        mut self,
        applies: Option<Applies<O>>,
        name: &str,
        check: impl Fn(&OpDone<O, W>) -> Result<(), E> + 'static,
    ) -> Self {
        let check: PerOpFn<O, W> =
            Rc::new(move |d: &OpDone<'_, O, W>| check(d).map_err(Into::into));
        self.per_op.push(PerOp { name: name.to_owned(), applies, check });
        self
    }

    /// Declares that a receipt calling `site` must never fail, natural or
    /// forced: any failure there is a finding of [`builtin::MUST_NOT_FAIL`].
    /// For callbacks that commit or reverse value after an earlier receipt
    /// changed the books. `site` is a method or `receiver::method`; `*`
    /// matches any run of characters.
    pub fn must_not_fail(mut self, site: &str) -> Self {
        self.must_not_fail.push(site.to_owned());
        self
    }

    /// Declares that any account may call `site`, a method meant to be
    /// public though the witnesses only saw its own contract call it:
    /// [`builtin::SELF_ONLY`] doesn't judge it. `site` is a method or
    /// `receiver::method`; `*` matches any run of characters.
    /// `anyone_may_call("*")` is the opt-out: it turns the check off.
    pub fn anyone_may_call(mut self, site: &str) -> Self {
        self.anyone.push(site.to_owned());
        self
    }

    /// Closes the to-do item `todo_id` ([`Todo::id`](super::Todo)) with a
    /// reason, which every audit prints beside it; `*` in it matches any
    /// run of characters, so one excuse closes many
    /// (`unreached:dao.near::acl_*`). The only excuse: an empty `why` is a
    /// harness error.
    pub fn excuse(mut self, todo_id: &str, why: &str) -> Self {
        self.excused.push((todo_id.to_owned(), why.to_owned()));
        self
    }

    /// Whether `site` (`receiver::method`) is one any caller may call.
    pub(crate) fn anyone_may_call_at(&self, site: &str) -> bool {
        self.anyone.iter().any(|p| site_matches(p, site))
    }

    /// Whether `site` (`receiver::method`) is one that must not fail.
    pub(crate) fn must_not_fail_at(&self, site: &str) -> bool {
        self.must_not_fail.iter().any(|p| site_matches(p, site))
    }

    /// Every check's name, the built-ins' first.
    pub(crate) fn check_names(&self) -> Vec<String> {
        let mine = (self.always.iter().map(|c| &c.name))
            .chain(self.at_rest.iter().map(|c| &c.name))
            .chain(self.per_op.iter().map(|c| &c.name))
            .chain(self.end.iter().map(|c| &c.name));
        let serial = self.serial.as_ref().map(|_| builtin::SERIALIZABLE.to_owned());
        builtin::ALL.iter().map(|s| s.to_string()).chain(mine.cloned()).chain(serial).collect()
    }

    /// What is wrong with the harness's settings, each: a check name used
    /// twice, an empty site or reason.
    pub(crate) fn problems(&self) -> Vec<String> {
        let mut out = vec![];
        let names = self.check_names();
        for (i, name) in names.iter().enumerate() {
            if names[..i].contains(name) {
                out.push(format!(
                    "two checks are named {name:?}: check names are unique, and differ from the \
                     built-ins'"
                ));
            }
        }
        if let Some(p) = self.must_not_fail.iter().find(|p| p.trim().is_empty()) {
            out.push(format!("must_not_fail({p:?}) names no site"));
        }
        if let Some(p) = self.anyone.iter().find(|p| p.trim().is_empty()) {
            out.push(format!("anyone_may_call({p:?}) names no site"));
        }
        for (id, why) in &self.excused {
            if id.trim().is_empty() || why.trim().is_empty() {
                out.push(format!("excuse({id:?}, {why:?}) needs an item and why"));
            }
        }
        out
    }
}

/// An op whose transactions are complete, as a per-op check sees it (and
/// an [`at_rest_ops`](Harness::at_rest_ops) check, each op of the case;
/// the books also see each raw call, which has no op).
#[derive(Clone)]
pub struct OpDone<'a, O, W = Sim> {
    pub(crate) op: Option<&'a O>,
    /// The op a raw call copies, if it does: who authorized it.
    pub(crate) of: Option<&'a O>,
    pub(crate) name: &'a str,
    pub(crate) outcomes: &'a [TxOutcome],
    pub(crate) effects: &'a Effects,
    pub(crate) world: &'a W,
    pub(crate) before: &'a W,
    pub(crate) isolated: bool,
    /// Submitted at rest: nothing in flight or parked, so
    /// [`before`](Self::before) is a settled world, whatever ran after.
    pub(crate) rest: bool,
    /// Whether the check read [`before`](Self::before), and
    /// [`world`](Self::world): one flag its clones share.
    pub(crate) shared: &'a std::cell::Cell<(bool, bool)>,
}

impl<O, W> OpDone<'_, O, W> {
    /// The op.
    pub fn op(&self) -> &O {
        self.op.expect("a raw call (Plan::call) has no op")
    }

    /// Its name, [`Op::name`].
    pub fn name(&self) -> &str {
        self.name
    }

    /// The outcome of each transaction it submitted, in submit order.
    pub fn outcomes(&self) -> &[TxOutcome] {
        self.outcomes
    }

    /// The outcome of its one transaction; `None` if it submitted none.
    /// Panics, naming the op, if it submitted several: read
    /// [`outcomes`](Self::outcomes) then.
    #[track_caller]
    pub fn outcome(&self) -> Option<&TxOutcome> {
        match self.outcomes {
            [] => None,
            [one] => Some(one),
            several => panic!(
                "OpDone::outcome(): {} submitted {} transactions, not one: read outcomes()",
                self.name,
                several.len()
            ),
        }
    }

    /// The world just before the op submitted: what it read to pick its
    /// arguments. Earlier ops still in flight are in flight here too. A
    /// per-op check of an op not [`isolated`](Self::isolated) that reads
    /// it is not judged, whatever it returns.
    pub fn before(&self) -> &W {
        self.shared.set((true, self.shared.get().1));
        self.before
    }

    /// Whether it ran alone: the chain was idle when it submitted, and no
    /// other op submitted before its transactions were complete. Not
    /// alone, [`before`](Self::before) and [`world`](Self::world) are the
    /// shared world, other ops' changes in them, so a per-op check that
    /// reads either is counted *not judged* ("overlapped"), never a pass
    /// or a finding; its own deltas, [`effects`](Self::effects) (NEAR,
    /// and any per-holder view a [`Token`](crate::Token) watches), are
    /// judged.
    pub fn isolated(&self) -> bool {
        self.isolated
    }

    /// Submitted at rest: nothing in flight or parked, so
    /// [`before`](Self::before) is a settled world. In an
    /// [`at_rest_ops`](Harness::at_rest_ops) check, the ops from the last
    /// one submitted at rest went in flight together (the window
    /// [`SERIALIZABLE`](builtin::SERIALIZABLE) judges): judge them from its
    /// `before()` to the world now. A raw call ([`Plan::call`]) is not in
    /// `ops`, though `SERIALIZABLE` sees it: a window one opens is judged
    /// together with the one before it, from the last op at rest before
    /// it, if any, so the invariant must hold over their union (a sum
    /// does; a per-window bound may not). With ops but none at
    /// rest, return [`Fail::not_judged`]; only with no op yet is the check
    /// idle. That idle can't tell a raw call from nothing, so a raw call
    /// before the first op is in no window: what it can break, judge in an
    /// [`at_rest`](Harness::at_rest) check or the books, which see raw
    /// calls. On `tests/res/vault_pair.wat`, each window's deposits mint a
    /// share a token, however many overlap:
    ///
    /// ```
    /// use near_contract_sim::explore::{Call, Fail, Harness, Op, OpDone, Plan, Skip};
    /// use near_contract_sim::{Balance, Fault, Receipts, Sim};
    /// use serde::{Deserialize, Serialize};
    ///
    /// /// alice moves this many tokens into the vault.
    /// #[derive(Clone, Debug, Serialize, Deserialize)]
    /// struct Deposit(u64);
    ///
    /// /// `transfer_call`'s args: the amount, then the receiver.
    /// fn to_vault(n: u64) -> Vec<u8> {
    ///     [&n.to_le_bytes()[..], b"vault.near"].concat()
    /// }
    ///
    /// impl Op for Deposit {
    ///     fn submit(&self, sim: &mut Sim) -> Result<(), Skip> {
    ///         let call = sim.call("alice.near", "token.near", "transfer_call");
    ///         call.args(to_vault(self.0)).submit();
    ///         Ok(())
    ///     }
    /// }
    ///
    /// /// The vault's tokens and alice's shares.
    /// fn books(sim: &Sim) -> [i128; 2] {
    ///     let held = |at: &str, who: &str| sim.state(at, who.as_bytes()).map_or(0, |v| {
    ///         i128::from(u64::from_le_bytes(v.try_into().unwrap()))
    ///     });
    ///     [held("token.near", "vault.near"), held("vault.near", "alice.near")]
    /// }
    ///
    /// /// The ops in flight together since the last submitted at rest.
    /// fn window(now: &Sim, ops: &[OpDone<Deposit>]) -> Result<(), Fail> {
    ///     let Some(first) = ops.iter().rposition(|d| d.rest()) else {
    ///         return Err(match ops {
    ///             [] => Fail::idle("no op yet"),
    ///             _ => Fail::not_judged("a raw call opened the window: no op at rest"),
    ///         });
    ///     };
    ///     let ([t0, s0], [t1, s1]) = (books(ops[first].before()), books(now));
    ///     match (t1 - t0, s1 - s0) {
    ///         (t, s) if t == s => Ok(()),
    ///         (t, s) => Err(format!("the vault got {t} tokens and minted {s} shares").into()),
    ///     }
    /// }
    ///
    /// let wasm = wat::parse_file("tests/res/vault_pair.wat").unwrap();
    /// let mut sim = Sim::new();
    /// sim.create_account("alice.near", Balance::from_near(100));
    /// sim.deploy("token.near", wasm.clone());
    /// sim.deploy("vault.near", wasm);
    /// sim.call("alice.near", "token.near", "mint").args(100u64.to_le_bytes()).transact();
    /// let h = Harness::new(sim).at_rest_ops("a share a token", window);
    /// let plan = Plan::new().op(Deposit(10)).op(Deposit(20)).blocks(0).op(Deposit(30));
    /// h.run(&plan).assert_clean();
    /// // The last deposit's resolver out of gas: the token takes its 30
    /// // back, the credit stays, in the window of two.
    /// let broken = plan.inject(Receipts::callback("resolve_deposit"), Fault::OutOfGas);
    /// let run = h.run(&broken);
    /// let f = run.assert_found("a share a token", "world");
    /// assert!(f.message.contains("got 20 tokens and minted 50 shares"), "{run}");
    /// // A raw call isn't in `ops`: a window one opens, with no op at rest
    /// // before it, is not judged, never idle.
    /// let call = Call::new("alice.near", "token.near", "transfer_call").args(to_vault(20));
    /// let opened = Plan::new().call(call).blocks(0).op(Deposit(30));
    /// let run = h.run(&opened.inject(Receipts::callback("resolve_deposit"), Fault::OutOfGas));
    /// assert!(!run.is_clean() && run.findings().is_empty(), "{run}");
    /// assert!(run.reason().unwrap().contains("a share a token not judged"), "{run}");
    /// ```
    pub fn rest(&self) -> bool {
        self.rest
    }

    /// What its own transactions' receipts did, merged over them: the NEAR
    /// and token moves, and the gas its signers paid.
    pub fn effects(&self) -> &Effects {
        self.effects
    }

    /// The world at the block the op completed in (in an `at_rest_ops`
    /// check, the world now). Shared, as [`before`](Self::before) is.
    pub fn world(&self) -> &W {
        self.shared.set((self.shared.get().0, true));
        self.world
    }

    /// The receipt each of its transactions first sent, in submit order:
    /// what it resolved to (receiver, methods, arguments, deposit, gas).
    pub fn entries(&self) -> Vec<&ReceiptOutcome> {
        (self.outcomes.iter())
            .flat_map(|o| {
                let first = o.transaction.as_ref().map_or(&[][..], |t| &t.receipt_ids[..]);
                o.receipts.iter().filter(move |r| first.contains(&r.receipt_id) && !r.is_refund())
            })
            .collect()
    }

    /// Every receipt its transactions ran, refunds left out, by block.
    pub fn calls(&self) -> Vec<&ReceiptOutcome> {
        let mut all: Vec<&ReceiptOutcome> = (self.outcomes.iter())
            .flat_map(|o| o.receipts.iter().filter(|r| !r.is_refund()))
            .collect();
        all.sort_by_key(|r| r.block_height);
        all
    }

    /// The first of its [`calls`](Self::calls) that failed and moves
    /// value: a deposit or tokens, a callback, or a call a callback awaits.
    pub fn failed_on_value_path(&self) -> Option<&ReceiptOutcome> {
        (self.calls().into_iter()).find(|r| r.failure().is_some() && super::run::moves(r))
    }
}

/// Whether `site` (`receiver::method`) matches `pattern`: a method, or
/// `receiver::method`, where `*` matches any run of characters.
pub(crate) fn site_matches(pattern: &str, site: &str) -> bool {
    let method = site.rsplit_once("::").map_or(site, |(_, m)| m);
    let target = if pattern.contains("::") { site } else { method };
    glob(pattern.as_bytes(), target.as_bytes())
}

pub(crate) fn glob(pattern: &[u8], text: &[u8]) -> bool {
    match pattern.split_first() {
        None => text.is_empty(),
        Some((b'*', rest)) => (0..=text.len()).any(|i| glob(rest, &text[i..])),
        Some((c, rest)) => text.split_first().is_some_and(|(t, tail)| t == c && glob(rest, tail)),
    }
}

/// Whether `site` answers to `at`: its whole print, its print without
/// ` (Op)`, or its op's name alone.
pub(crate) fn site_is(site: &Site, at: &str) -> bool {
    if site.to_string() == at {
        return true;
    }
    match site {
        Site::Receipt { receiver, method, op } => {
            let method = method.clone();
            let bare = Site::Receipt { receiver: receiver.clone(), method, op: None };
            op.as_deref() == Some(at) || bare.to_string() == at
        }
        Site::Key { key, op } => key == at || op.as_deref() == Some(at),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn site_patterns_match_methods_and_sites() {
        let site = "custody.near::resolve_deposit";
        assert!(site_matches("resolve_deposit", site));
        assert!(site_matches("custody.near::resolve_deposit", site));
        assert!(site_matches("resolve_*", site));
        assert!(site_matches("*::resolve_deposit", site));
        assert!(!site_matches("resolve", site));
        assert!(!site_matches("token.near::resolve_deposit", site));
    }

    #[test]
    fn a_receipt_and_a_key_answer_to_the_same_forms() {
        let receipt = Site::Receipt {
            receiver: Some("v.near".parse().unwrap()),
            method: Some("on_withdraw".into()),
            op: Some("Withdraw".into()),
        };
        let key = Site::Key { key: "bob.near".into(), op: Some("Withdraw".into()) };
        for (site, bare) in [(&receipt, "v.near::on_withdraw"), (&key, "bob.near")] {
            assert!(site_is(site, &site.to_string()), "{site}");
            assert!(site_is(site, bare), "{site}");
            assert!(site_is(site, "Withdraw"), "{site}");
            assert!(!site_is(site, "Deposit"), "{site}");
        }
    }
}
