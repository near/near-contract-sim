//! The core hook: what a plan step sets on the transactions its op
//! submits, held in `Sim::explore` while `Op::submit` runs and read by
//! `Tx::queue`. Its entry points are the seam between the chain and
//! exploration: [`prepare`](Hook::prepare) and [`least_plus`](Hook::least_plus).

use super::plan::{Inject, StepGas, StubAnswer};
use super::{Op, Skip};
use crate::Sim;
use crate::inject::{Fault, Receipts};
use near_primitives::action::Action;
use near_primitives::types::Gas;
use std::panic::{AssertUnwindSafe, catch_unwind, resume_unwind};

/// One step's settings for the transactions its op queues.
#[derive(Clone, Debug, Default)]
pub(crate) struct Hook {
    gas: Option<StepGas>,
    inject: Vec<Inject>,
    /// What stubs answer its transactions' calls.
    pub(crate) answers: Vec<StubAnswer>,
    /// Transactions queued so far.
    txs: usize,
    /// Calls its transactions carried, and how many were unpinned.
    pub(crate) calls: u32,
    pub(crate) unpinned: u32,
    /// Per transaction at just enough gas where an inner receipt fails:
    /// that gas, the least with which every receipt succeeds, and the
    /// receipt that failed.
    pub(crate) bands: Vec<(Gas, Gas, String)>,
    /// Whether to measure them.
    pub(crate) band: bool,
    /// The args each call signed, as JSON (`null` if they aren't).
    pub(crate) signed: Vec<serde_json::Value>,
    /// Its injections pass over a fault that can't apply, as a plan step's
    /// do, instead of panicking as [`Tx::inject`](crate::Tx::inject)'s do.
    pub(crate) lenient: bool,
}

impl Hook {
    pub(crate) fn new(gas: Option<StepGas>, inject: Vec<Inject>, answers: Vec<StubAnswer>) -> Hook {
        Hook { gas, inject, answers, lenient: true, ..Hook::default() }
    }

    /// For a transaction about to be signed: gives its unpinned calls the
    /// step's gas, clamped so all its calls stay within `max` and each
    /// keeps at least 1 gas, records what they sign, and returns the
    /// injections that are this transaction's, in the step's order. A
    /// [`StepGas::LeastPlus`] is left to the transaction, which bisects.
    pub(crate) fn prepare(
        &mut self,
        actions: &mut [Action],
        unpinned: &[usize],
        max: Gas,
    ) -> Vec<(Receipts, Fault)> {
        for a in actions.iter() {
            if let Action::FunctionCall(fc) = a {
                self.calls += 1;
                self.signed.push(serde_json::from_slice(&fc.args).unwrap_or_default());
            }
        }
        self.unpinned += unpinned.len() as u32;
        if let (Some(StepGas::Exactly(gas)), false) = (self.gas, unpinned.is_empty()) {
            let pinned = crate::tx::pinned_gas(actions, unpinned).as_gas();
            let room = max.as_gas().saturating_sub(pinned) / unpinned.len() as u64;
            let each = Gas::from_gas(gas.as_gas().min(room).max(1));
            for &i in unpinned {
                if let Some(Action::FunctionCall(fc)) = actions.get_mut(i) {
                    fc.gas = each;
                }
            }
        }
        let n = self.txs;
        self.txs += 1;
        (self.inject.iter())
            .filter(|i| usize::from(i.tx) == n)
            .map(|i| (i.at.clone(), i.fault.clone()))
            .collect()
    }

    /// The step's [`StepGas::LeastPlus`], if it has one.
    pub(crate) fn least_plus(&self) -> Option<Gas> {
        if let Some(StepGas::LeastPlus(plus)) = self.gas { Some(plus) } else { None }
    }
}

/// Submits `op` on `world` with `injections` forced on its first
/// transaction, as if it were built with [`Tx::inject`](crate::Tx::inject)
/// for each, ahead of its own (which panics where a fault doesn't fit):
/// how code outside a [`Harness`](super::Harness), such as an enumerator
/// that clones a world at each branch, reuses an op. No block is
/// produced. Panics on a hold of 0 blocks, inside a harness's op, if `op`
/// submits no transaction to inject into, and where `op` panics.
#[track_caller]
pub fn submit_injected<O: Op<W>, W: AsMut<Sim>>(
    world: &mut W,
    op: &O,
    injections: impl IntoIterator<Item = (Receipts, Fault)>,
) -> Result<(), Skip> {
    let inject = (injections.into_iter())
        .map(|(at, fault)| {
            if let Fault::Hold(0) | Fault::HoldResult(0) = fault {
                panic!("submit_injected(.., {fault:?}) holds nothing: hold for at least 1 block");
            }
            Inject { tx: 0, at, fault }
        })
        .collect();
    let hook = Hook { lenient: false, ..Hook::new(None, inject, vec![]) };
    let sim = world.as_mut();
    assert!(sim.explore.is_none(), "submit_injected inside a harness's op: inject in its plan");
    sim.explore = Some(Box::new(hook));
    let submitted = catch_unwind(AssertUnwindSafe(|| op.submit(world)));
    let hook = world.as_mut().explore.take().expect("the hook");
    let submitted = submitted.unwrap_or_else(|p| resume_unwind(p));
    assert!(
        hook.inject.is_empty() || hook.txs > 0 || submitted.is_err(),
        "submit_injected: {op:?} submitted no transaction to inject into"
    );
    submitted
}

#[cfg(test)]
mod tests {
    use super::*;
    use near_primitives::types::Balance;

    /// A step with two injections forces both, each at its own receipt of
    /// the step's transaction: `answer` held 2 blocks, and its callback
    /// `cb` given a failed result.
    #[test]
    fn a_step_with_two_injections_fires_both() {
        let wat = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/res/callback_probe.wat");
        let mut sim = Sim::new();
        sim.create_account("alice.near", Balance::from_near(10));
        sim.deploy("probe.near", wat::parse_file(wat).unwrap());
        let inject = |at: Receipts, fault| Inject { tx: 0, at, fault };
        let injections = vec![
            inject(Receipts::method("answer"), Fault::Hold(2)),
            inject(Receipts::callback("cb"), Fault::PromiseFailed),
        ];
        sim.explore = Some(Box::new(Hook::new(None, injections, vec![])));
        let tx = sim.call("alice.near", "probe.near", "start").args(b"hi".to_vec()).submit();
        sim.explore = None;
        sim.settle();
        let out = sim.outcome(tx);
        let (answer, cb) = (out.expect_receipt("answer"), out.expect_receipt("cb"));
        assert_eq!(answer.held, Some(2), "{out:#}");
        assert_eq!(cb.fault, Some(Fault::PromiseFailed), "{out:#}");
    }
}
