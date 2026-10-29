//! [`Faults`]: which faults exploration tries, at which receipts, and in
//! what share of cases.

use super::plan::Inject;
use crate::TxOutcome;
use crate::fault::Spawned;
use crate::inject::{Fault, Receipts};
use near_primitives::hash::CryptoHash;
use proptest::prelude::*;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Which faults to try, at which receipts, in what share of cases. A search
/// samples one fault per case in [`per_case`](Self::per_case) of cases,
/// placed on a receipt it fits as the case runs (none fitting, it runs
/// without); a scan tries each fault at each receipt it fits, one at a time.
/// [`Faults::default()`] is what to audit with.
///
/// ```
/// # use near_contract_sim::explore::Faults;
/// # use near_contract_sim::{Fault, Receipts};
/// let faults = Faults::tolerable()
///     .and(Faults::new(Receipts::callback("on_withdraw"), [Fault::ReturnValue(vec![])]))
///     .per_case(0.5);
/// # let _ = faults;
/// ```
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Faults {
    arms: Vec<Arm>,
    per_case: f64,
    /// A scan also tries its edges ([`Faults::edges`]).
    #[serde(default)]
    edges: bool,
    /// A scan and an enumeration also reorder ([`Faults::reorder`]).
    #[serde(default)]
    reorder: bool,
}

/// One selector and the faults to try at what it selects.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct Arm {
    pub at: Receipts,
    pub faults: Vec<Fault>,
}

/// How often a sampled case gets a fault, unless `per_case` says.
const PER_CASE: f64 = 0.25;
/// The `k` of a sampled fault's `nth(k)` is drawn from 0 to this.
const MAX_NTH: usize = 7;

/// The default of [`plans`](super::plans), and what to scan with:
/// [`Faults::tolerable`], [`Fault::OutOfGas`] at every receipt that calls a
/// method ([`Receipts::all`]), in a quarter of cases, and in a scan
/// [`Faults::edges`]. A search samples the four kinds evenly, so a starved
/// receipt is a sixteenth of cases. Its findings are forced ("not shown")
/// but still findings: a callback's fixed budget is what input size, log
/// volume and storage shape exhaust, and the report gives its headroom.
impl Default for Faults {
    fn default() -> Self {
        let mut faults = Faults::tolerable();
        // Last: a sampled fault shrinks toward the first arm.
        faults.arms.push(Arm { at: Receipts::all(), faults: vec![Fault::OutOfGas] });
        faults.edges = true;
        faults
    }
}

impl Faults {
    /// No faults and no edges: a scan runs the witnesses only. `new` and
    /// `tolerable` have no edges either; `.and(Faults::edges())` adds them.
    pub fn none() -> Self {
        Faults { arms: vec![], per_case: PER_CASE, edges: false, reorder: false }
    }

    /// No fault on a receipt: a [`scan`](super::Harness::scan)'s edges, which
    /// it lists: direct resubmissions into audited contracts, a stub's answer
    /// twice what it was sent, each number's edges, and the caller's gas.
    /// Findings there are shown, at the caller's gas. An op that pins every
    /// call's gas ([`Tx::gas`](crate::Tx::gas)) keeps it, as the scan says.
    /// An [`enumerate`](super::Harness::enumerate) runs each new step placed
    /// at rest at the per-step ones (caller gas, numbers and choices, a
    /// stub giving back all it was sent), each a case not extended.
    /// A search ignores this: its plans draw the caller's gas
    /// ([`Plans::gas`](super::Plans::gas)).
    pub fn edges() -> Self {
        Faults { arms: vec![], per_case: PER_CASE, edges: true, reorder: false }
    }

    /// What a network can do to a contract's calls that a correct contract
    /// survives, in a quarter of cases: `PromiseFailed` at a callback whose
    /// calls all go to other accounts ([`Receipts::cross_contract_callbacks`];
    /// a contract's call to itself failing is not tolerable), and a receipt
    /// or a result held 1 to 3 blocks ([`Fault::Hold`], [`Fault::HoldResult`]).
    /// For a regression test that a fixed build survives what it must.
    pub fn tolerable() -> Self {
        let holds = |hold: fn(u64) -> Fault| (1..=3).map(hold).collect();
        Faults {
            arms: vec![
                Arm {
                    at: Receipts::cross_contract_callbacks(),
                    faults: vec![Fault::PromiseFailed],
                },
                Arm { at: Receipts::all(), faults: holds(Fault::Hold) },
                Arm { at: Receipts::all(), faults: holds(Fault::HoldResult) },
            ],
            per_case: PER_CASE,
            edges: false,
            reorder: false,
        }
    }

    /// Each of `faults` at the receipts `at` selects, in a quarter of
    /// cases. Panics on no faults, on a hold of 0 blocks, and on a
    /// [`Receipts::in_tx`] anywhere: each case runs its own transactions,
    /// so a hash names one case's transaction and, in another case's
    /// step, an earlier step's or none. Hold across transactions in a
    /// plan step, by the hash of the run it was written for.
    #[track_caller]
    pub fn new(at: Receipts, faults: impl IntoIterator<Item = Fault>) -> Self {
        let faults: Vec<Fault> = faults.into_iter().collect();
        assert!(!faults.is_empty(), "Faults::new({at}, []): no faults to try");
        for f in &faults {
            if let Fault::Hold(0) | Fault::HoldResult(0) = f {
                panic!("Faults::new({at}, {f:?}): holds nothing; hold for at least 1 block");
            }
            let until = if let Fault::HoldUntil(u) = f { u.tx_of() } else { None };
            assert!(
                at.tx_of().or(until).is_none(),
                "Faults::new({at}, {f}): Receipts::in_tx names one transaction, and each case \
                 runs its own; hold across transactions in a plan step"
            );
        }
        Faults { arms: vec![Arm { at, faults }], per_case: PER_CASE, edges: false, reorder: false }
    }

    /// Each pair of receipts of one transaction where neither descends
    /// from the other (by a call or a promise result), run in the other
    /// order: the one that ran first held with [`Fault::HoldUntil`] until
    /// the other has run, one pair at a time, from the receipts each
    /// step's transactions spawned as written. A scan and an enumeration
    /// try it; a search ignores it, as it ignores [`edges`](Self::edges).
    /// Its findings are forced: the sim shows the order is possible on one
    /// shard, not that a network delays a receipt that long.
    pub fn reorder() -> Self {
        Faults { reorder: true, ..Faults::none() }
    }

    /// These and `other`'s, at this value's share of cases.
    pub fn and(mut self, other: Faults) -> Self {
        self.arms.extend(other.arms);
        self.edges |= other.edges;
        self.reorder |= other.reorder;
        self
    }

    /// The share of sampled cases that get a fault, from 0 to 1.
    #[track_caller]
    pub fn per_case(mut self, p: f64) -> Self {
        assert!((0.0..=1.0).contains(&p), "Faults::per_case({p}): not a share from 0 to 1");
        self.per_case = p;
        self
    }

    /// The selectors and their faults, for a scan to enumerate.
    pub(crate) fn arms(&self) -> &[Arm] {
        &self.arms
    }

    /// Whether a scan tries its edges ([`edges`](Self::edges)).
    pub(crate) fn tries_edges(&self) -> bool {
        self.edges
    }

    /// Whether it reorders ([`reorder`](Self::reorder)).
    pub(crate) fn reorders(&self) -> bool {
        self.reorder
    }

    /// One fault to inject, in `per_case` of cases: an arm, one of its faults,
    /// and its selector with `nth(k)`, `k` from 0 to 7, on the step's first
    /// transaction; the run resolves it to a receipt it fits ([`Plan`]'s
    /// [`Inject`]). Shrinks to no fault (unless `per_case` is 1), then to the
    /// first arm, its first fault and `k` = 0.
    ///
    /// [`Plan`]: super::Plan
    pub(crate) fn strategy(&self) -> BoxedStrategy<Option<Inject>> {
        if self.arms.is_empty() || self.per_case == 0.0 {
            return Just(None).boxed();
        }
        let arms = self.arms.clone();
        let pick = (0..arms.len(), any::<prop::sample::Index>(), 0..=MAX_NTH).prop_map(
            move |(a, fault, k)| {
                let arm = &arms[a];
                let fault = arm.faults[fault.index(arm.faults.len())].clone();
                Inject { tx: 0, at: arm.at.clone().nth(k), fault }
            },
        );
        if self.per_case >= 1.0 {
            return pick.prop_map(Some).boxed();
        }
        prop::option::weighted(self.per_case, pick).boxed()
    }
}

/// The pairs of `spawned` (a transaction's receipts, by index) that
/// [`Faults::reorder`] swaps, as `(held, until)`: neither descends from the
/// other in `outcome`, the transaction's receipts (by a receipt it
/// created, or a callback its result goes to), and `held` ran first.
/// Receipts that never ran are left out.
pub(crate) fn reorders(spawned: &[Spawned], outcome: &TxOutcome) -> Vec<(usize, usize)> {
    let at: HashMap<CryptoHash, usize> =
        outcome.receipts.iter().enumerate().map(|(k, r)| (r.receipt_id, k)).collect();
    // What each receipt leads to: those it created, and the callbacks
    // its result goes to.
    let next = |k: usize| -> Vec<usize> {
        let r = &outcome.receipts[k];
        let data: Vec<&CryptoHash> = r.output_data_receivers.iter().map(|d| &d.data_id).collect();
        let fed = (outcome.receipts.iter().enumerate())
            .filter(|(_, c)| c.input_data_ids.iter().any(|d| data.contains(&d)))
            .map(|(j, _)| j);
        r.receipt_ids.iter().filter_map(|id| at.get(id).copied()).chain(fed).collect()
    };
    let descends = |from: usize, to: usize| {
        let (mut stack, mut seen) = (vec![from], vec![from]);
        while let Some(k) = stack.pop() {
            for j in next(k) {
                if j == to {
                    return true;
                }
                if !seen.contains(&j) {
                    seen.push(j);
                    stack.push(j);
                }
            }
        }
        false
    };
    let ran: Vec<(usize, usize)> =
        spawned.iter().enumerate().filter_map(|(n, s)| Some((n, *at.get(&s.id)?))).collect();
    let mut out = vec![];
    for (i, &(n, a)) in ran.iter().enumerate() {
        for &(m, b) in &ran[i + 1..] {
            if descends(a, b) || descends(b, a) {
                continue;
            }
            let ra = &outcome.receipts[a];
            let rb = &outcome.receipts[b];
            let first = (ra.block_height, a) <= (rb.block_height, b);
            out.push(if first { (n, m) } else { (m, n) });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::strategy::ValueTree;
    use proptest::test_runner::TestRunner;

    /// `edges` and `reorder` add no fault; `and` keeps each side's.
    #[test]
    fn edges_and_reorder_add_no_fault_and_and_keeps_both() {
        let (edges, reorder) = (Faults::edges(), Faults::reorder());
        assert!(edges.arms().is_empty() && edges.tries_edges() && !edges.reorders());
        assert!(reorder.arms().is_empty() && !reorder.tries_edges() && reorder.reorders());
        let both = Faults::tolerable().and(Faults::edges()).and(Faults::reorder());
        assert_eq!(both.arms().len(), Faults::tolerable().arms().len());
        assert!(both.tries_edges() && both.reorders());
        let none = Faults::none().and(Faults::none());
        assert!(!none.tries_edges() && !none.reorders());
    }

    /// The tolerable faults never fail a callback's call to
    /// its own contract, and never starve anything.
    #[test]
    fn tolerable_fails_only_cross_contract_calls() {
        let mut runner = TestRunner::deterministic();
        let strategy = Faults::tolerable().strategy();
        let (mut some, mut failed) = (0, 0);
        for _ in 0..400 {
            let Some(i) = strategy.new_tree(&mut runner).unwrap().current() else { continue };
            some += 1;
            match i.fault {
                Fault::PromiseFailed => {
                    failed += 1;
                    let at = i.at.to_string();
                    assert!(at.starts_with("Receipts::cross_contract_callbacks().nth("), "{at}");
                }
                Fault::Hold(n) | Fault::HoldResult(n) => assert!((1..=3).contains(&n)),
                other => panic!("not tolerable: {other:?}"),
            }
        }
        assert!((60..=140).contains(&some), "a quarter of 400: {some}");
        assert!(failed > 0);
    }

    /// The default adds starved receipts to the tolerable faults, in a
    /// quarter of the faulted cases, and never fails a self-call.
    #[test]
    fn the_default_starves_a_sixteenth_of_cases() {
        let mut runner = TestRunner::deterministic();
        let strategy = Faults::default().strategy();
        let (mut some, mut starved) = (0, 0);
        for _ in 0..1600 {
            let Some(i) = strategy.new_tree(&mut runner).unwrap().current() else { continue };
            some += 1;
            match i.fault {
                Fault::OutOfGas => {
                    starved += 1;
                    assert!(i.at.to_string().starts_with("Receipts::all().nth("), "{}", i.at);
                }
                Fault::PromiseFailed => {
                    let at = i.at.to_string();
                    assert!(at.starts_with("Receipts::cross_contract_callbacks().nth("), "{at}");
                }
                Fault::Hold(_) | Fault::HoldResult(_) => {}
                other => panic!("not a default fault: {other:?}"),
            }
        }
        assert!((320..=480).contains(&some), "a quarter of 1600: {some}");
        assert!((60..=140).contains(&starved), "a sixteenth of 1600: {starved}");
    }

    #[test]
    fn a_sampled_fault_shrinks_away() {
        let mut runner = TestRunner::deterministic();
        let strategy = Faults::tolerable().per_case(0.99).strategy();
        let mut tree = strategy.new_tree(&mut runner).unwrap();
        assert!(tree.current().is_some());
        while tree.simplify() {}
        assert_eq!(tree.current(), None);
        let always = Faults::tolerable().per_case(1.0).strategy();
        let mut tree = always.new_tree(&mut runner).unwrap();
        while tree.simplify() {}
        let first = tree.current().expect("always a fault");
        assert_eq!(first.at.to_string(), "Receipts::cross_contract_callbacks().nth(0)");
        assert_eq!(Faults::none().strategy().new_tree(&mut runner).unwrap().current(), None);
    }

    #[test]
    #[should_panic(expected = "holds nothing")]
    fn a_zero_hold_is_refused() {
        let _ = Faults::new(Receipts::all(), [Fault::Hold(0)]);
    }

    /// A transaction's hash names it in one case only: refused at the
    /// receipts and in a `HoldUntil`.
    #[test]
    fn a_fault_naming_a_transaction_is_refused() {
        let tx = CryptoHash::hash_bytes(b"t1");
        let refused = |at: Receipts, f: Fault| {
            let r = std::panic::catch_unwind(|| Faults::new(at, [f]));
            let msg = r.err().and_then(|e| e.downcast::<String>().ok()).expect("a panic");
            assert!(msg.contains("Receipts::in_tx names one transaction"), "{msg}");
        };
        refused(Receipts::all().in_tx(tx), Fault::Hold(1));
        refused(Receipts::all(), Fault::HoldUntil(Receipts::method("on_deposit").in_tx(tx)));
        let _ = Faults::new(Receipts::all(), [Fault::HoldUntil(Receipts::method("on_deposit"))]);
    }
}
