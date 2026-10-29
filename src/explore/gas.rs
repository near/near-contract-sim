//! Gas for a plan step's unpinned calls ([`Step::gas`](super::Step::gas)):
//! strategies of `Option<StepGas>` for [`Plans::gas`](super::Plans::gas).
//!
//! `None` leaves a call's own gas (300 Tgas for [`Tx::call`](crate::Tx::call)).
//! A drawn value is an index from the fixture's ceiling
//! ([`Sim::gas_ceiling`](crate::Sim::gas_ceiling), 1 PGas under the protocol's
//! limits), so shrinking tries `None`, then moves gas up toward the ceiling
//! within the arm it was drawn from: a finding in a gas window shrinks to the
//! window's top edge, one gas doesn't matter to shrinks to `None`.

use super::plan::StepGas;
use crate::Sim;
use crate::chain::WALLET_GAS;
use near_primitives::types::Gas;
use proptest::num::u32::BinarySearch;
use proptest::prelude::*;
use proptest::strategy::{NewTree, ValueTree};
use proptest::test_runner::TestRunner;

/// 0.1 Tgas, the resolution of drawn values.
const STEP: u64 = 100_000_000_000;
/// 1 Tgas.
const TGAS: u64 = 1_000_000_000_000;
/// The normal arm's floor: 100 Tgas.
const NORMAL_FLOOR: u64 = 100 * TGAS;
/// The low arm's floor: 1 Tgas.
const LOW_FLOOR: u64 = TGAS;
/// Steps of the log index: adjacent values are at most 0.1 Tgas apart
/// below about 26 Tgas.
const LOG_STEPS: u32 = 4096;

/// The default of [`plans`](super::plans): about 3 draws in 4 uniform over
/// 100 Tgas to the ceiling, the rest log-uniform over 1 Tgas to it (under
/// 1 PGas, about one call in fifteen below 10 Tgas). One draw in eight is a
/// threshold (30 Tgas, 300 Tgas, a whole-Tgas constant of the fixture's
/// contracts) or 1 gas either side, one in sixteen tight: the least gas with
/// which each transaction succeeds, plus 0, 1 gas or 0.1 Tgas
/// ([`StepGas::LeastPlus`]), bisected when the op submits, as a relayer
/// attaches (about 50 runs of its transaction on a copy of the world).
/// Every draw is a value; `None` is reached by shrinking.
pub fn mixed() -> impl Strategy<Value = Option<StepGas>> {
    Explored { low: 0.25, thresholds: THRESHOLD_SHARE, tight: TIGHT_SHARE, unset: true }
}

/// Log-uniform over 1 Tgas to the ceiling, bar thresholds and tight draws: for
/// hunting gas windows. Finds bugs needing several successful calls less
/// often than [`mixed`], as many calls stop at their first receipt.
pub fn wide() -> impl Strategy<Value = Option<StepGas>> {
    Explored { low: 1.0, thresholds: THRESHOLD_SHARE, tight: TIGHT_SHARE, unset: true }
}

/// [`input::gas`](super::input::gas): [`mixed`]'s values, a threshold in
/// half the draws, never `None` and never tight.
pub(crate) fn field() -> impl Strategy<Value = Gas> + Clone {
    Explored { low: 0.25, thresholds: 0.5, tight: 0.0, unset: false }.prop_map(|g| match g {
        Some(StepGas::Exactly(g)) => g,
        other => unreachable!("a gas field is a value, not {other:?}"),
    })
}

/// The share of [`mixed`] and [`wide`] draws that are a threshold.
const THRESHOLD_SHARE: f64 = 0.125;
/// The share of [`mixed`] and [`wide`] draws that are tight.
const TIGHT_SHARE: f64 = 0.0625;
/// A tight draw's offsets from the least gas that succeeds.
const TIGHT_PLUS: [u64; 3] = [0, 1, STEP];

/// The gas a contract likely compares against, which [`mixed`] and [`wide`]
/// draw exactly and 1 gas either side: 30 Tgas, 300 Tgas (what wallets
/// attach) and, during a [`search`](super::Harness::search), every
/// whole-Tgas constant in the fixture's contracts (`Gas::from_tgas`: a
/// `prepaid_gas` floor, a callback's static gas). The 0.1 Tgas grid misses
/// an off-by-one floor; these hit it. The search report lists them.
pub(crate) fn thresholds() -> Vec<Gas> {
    let mut all = std::collections::BTreeSet::from([30 * TGAS, WALLET_GAS.as_gas()]);
    FIXTURE.with(|f| all.extend(f.borrow().iter().copied()));
    all.into_iter().map(Gas::from_gas).collect()
}

/// The top of the drawn gas: the ceiling of the fixture of the search
/// running on this thread, or outside one the default config's.
fn top() -> u64 {
    CEILING.with(|c| c.get()).unwrap_or_else(|| crate::Config::default().gas_ceiling().as_gas())
}

thread_local! {
    /// The gas constants of the fixture of the search running on this
    /// thread: plan generation reads them while the search generates.
    static FIXTURE: std::cell::RefCell<std::collections::BTreeSet<u64>> =
        const { std::cell::RefCell::new(std::collections::BTreeSet::new()) };
    /// Its gas ceiling.
    static CEILING: std::cell::Cell<Option<u64>> = const { std::cell::Cell::new(None) };
}

/// Runs `f` with the gas constants of `fixture`'s contracts as
/// [`thresholds`], and its ceiling as the top of drawn gas, as a search
/// does while it generates and shrinks.
pub(crate) fn with_fixture<T>(fixture: &Sim, f: impl FnOnce() -> T) -> T {
    let ceiling = fixture.gas_ceiling().as_gas();
    let codes = fixture.contracts();
    let found: std::collections::BTreeSet<u64> =
        codes.iter().flat_map(|(_, c)| constants(c, ceiling)).collect();
    let before = (FIXTURE.with(|t| t.replace(found)), CEILING.with(|c| c.replace(Some(ceiling))));
    let out = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));
    FIXTURE.with(|t| *t.borrow_mut() = before.0);
    CEILING.with(|c| c.set(before.1));
    out.unwrap_or_else(|p| std::panic::resume_unwind(p))
}

/// Always `None`: each call keeps its own gas.
pub fn off() -> impl Strategy<Value = Option<StepGas>> {
    Just(None)
}

/// Draws a threshold with probability `thresholds`, a tight one with
/// probability `tight`, else the low (log) arm with probability `low`,
/// else the normal arm. With `unset`, shrinking tries `None` first.
#[derive(Clone, Copy, Debug)]
struct Explored {
    low: f64,
    thresholds: f64,
    tight: f64,
    unset: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Arm {
    /// Uniform over 100 Tgas to this top at 0.1 Tgas; index 0 is the top.
    Normal(u64),
    /// Log-uniform over 1 Tgas to this top; index 0 is the top.
    Low(u64),
    /// Exactly this gas: a threshold, or 1 gas either side of one.
    Exact(u64),
    /// The least gas that succeeds, plus this much.
    Tight(u64),
}

impl Arm {
    /// The largest index.
    fn last(self) -> u32 {
        match self {
            Arm::Normal(top) => (top.saturating_sub(NORMAL_FLOOR) / STEP) as u32,
            Arm::Low(_) => LOG_STEPS,
            Arm::Exact(_) | Arm::Tight(_) => 0,
        }
    }

    fn value(self, i: u32) -> StepGas {
        let gas = match self {
            Arm::Tight(plus) => return StepGas::LeastPlus(Gas::from_gas(plus)),
            Arm::Exact(gas) => gas,
            Arm::Normal(top) => top - u64::from(i) * STEP,
            Arm::Low(top) => {
                let f = f64::from(i) / f64::from(LOG_STEPS);
                let v = top as f64 * (LOW_FLOOR as f64 / top as f64).powf(f);
                ((v / STEP as f64).round() as u64 * STEP).clamp(LOW_FLOOR, top)
            }
        };
        StepGas::Exactly(Gas::from_gas(gas))
    }
}

/// Where a drawn value stands while it shrinks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    /// `None`; nothing simpler.
    Unset,
    /// `None` as a candidate; the accepted value is the index's.
    TryingUnset,
    /// The index's value.
    Pinned,
}

/// A value of [`mixed`] or [`wide`] as it shrinks: `None` first, then its
/// index toward 0 (the ceiling) within its arm.
#[derive(Clone, Debug)]
struct GasTree {
    arm: Arm,
    phase: Phase,
    /// `None` not tried yet.
    fresh: bool,
    idx: BinarySearch,
}

impl Strategy for Explored {
    type Tree = GasTree;
    type Value = Option<StepGas>;

    fn new_tree(&self, runner: &mut TestRunner) -> NewTree<Self> {
        let edge = self.thresholds + self.tight;
        let edge = edge >= 1.0 || prop::bool::weighted(edge).new_tree(runner)?.current();
        let low = self.low >= 1.0 || prop::bool::weighted(self.low).new_tree(runner)?.current();
        let arm = if edge {
            // Slots: each threshold option `per` times, then the tight draw's
            // offsets, so the two take their shares of the edge draws.
            let edges = thresholds();
            let options = edges.len() * 3;
            let (per, tight) = match (self.thresholds > 0.0, self.tight > 0.0) {
                (true, false) => (1, 0),
                (false, _) => (0, TIGHT_PLUS.len()),
                // The tight share is half the threshold share in
                // `mixed` and `wide`.
                (true, true) => ((self.thresholds / self.tight).round() as usize, options),
            };
            let k = (0..options * per + tight).new_tree(runner)?.current();
            match k.checked_sub(options * per) {
                Some(j) => Arm::Tight(TIGHT_PLUS[j % TIGHT_PLUS.len()]),
                None => {
                    let k = k / per;
                    let gas = edges[k / 3].as_gas();
                    Arm::Exact([gas - 1, gas, gas + 1][k % 3])
                }
            }
        } else if low {
            Arm::Low(top())
        } else {
            Arm::Normal(top())
        };
        let i = (0..=arm.last()).new_tree(runner)?.current();
        let fresh = self.unset;
        Ok(GasTree { arm, phase: Phase::Pinned, fresh, idx: BinarySearch::new(i) })
    }
}

impl ValueTree for GasTree {
    type Value = Option<StepGas>;

    fn current(&self) -> Option<StepGas> {
        match self.phase {
            Phase::Unset | Phase::TryingUnset => None,
            Phase::Pinned => Some(self.arm.value(self.idx.current())),
        }
    }

    fn simplify(&mut self) -> bool {
        match self.phase {
            Phase::Unset => false,
            // Called after `None` was accepted: nothing is simpler.
            Phase::TryingUnset => {
                self.phase = Phase::Unset;
                false
            }
            Phase::Pinned if self.fresh => {
                self.fresh = false;
                self.phase = Phase::TryingUnset;
                true
            }
            Phase::Pinned => self.idx.simplify(),
        }
    }

    fn complicate(&mut self) -> bool {
        match self.phase {
            Phase::Unset => false,
            // `None` was rejected: on to the index's first candidate,
            // rather than re-testing the value already accepted.
            Phase::TryingUnset => {
                self.phase = Phase::Pinned;
                self.idx.simplify()
            }
            Phase::Pinned => self.idx.complicate(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `n` draws: each value's gas, and `TIGHT` for a tight one.
    fn draws(s: impl Strategy<Value = Option<StepGas>>, n: usize) -> Vec<u64> {
        let mut runner = TestRunner::deterministic();
        (0..n).map(|_| value(s.new_tree(&mut runner).unwrap().current())).collect()
    }

    const TIGHT: u64 = u64::MAX;

    fn value(g: Option<StepGas>) -> u64 {
        match g {
            Some(StepGas::Exactly(g)) => g.as_gas(),
            Some(StepGas::LeastPlus(_)) => TIGHT,
            None => panic!("a draw is never unset"),
        }
    }

    fn exactly(g: Option<StepGas>) -> Option<Gas> {
        match g {
            Some(StepGas::Exactly(g)) => Some(g),
            _ => None,
        }
    }

    /// 30 Tgas or 300 Tgas, exactly or 1 gas either side: the thresholds
    /// outside a fixture.
    fn at_threshold(g: &u64) -> bool {
        [30 * TGAS, WALLET_GAS.as_gas()].iter().any(|t| (t - 1..=t + 1).contains(g))
    }

    /// 300 Tgas and 1 gas either side.
    fn at_300(g: &u64) -> bool {
        (WALLET_GAS.as_gas() - 1..=WALLET_GAS.as_gas() + 1).contains(g)
    }

    /// Runs `f` as a search of a fixture under `limits` does, with that
    /// fixture's ceiling.
    fn under<T>(limits: crate::GasLimits, f: impl FnOnce(u64) -> T) -> T {
        let mut sim = Sim::new();
        sim.config.gas_limits = limits;
        let ceiling = sim.gas_ceiling().as_gas();
        with_fixture(&sim, || {
            assert_eq!(top(), ceiling, "the top is the fixture's ceiling");
            f(ceiling)
        })
    }

    const LIMITS: [crate::GasLimits; 2] = [crate::GasLimits::Protocol, crate::GasLimits::Legacy300];

    /// `n` of 4000 expected, give or take `by`.
    fn near(n: usize, expected: f64, by: f64) -> bool {
        (n as f64 - expected).abs() <= by
    }

    /// The bands follow from the ceiling: 13/16 of the draws are drawn
    /// values, 1/8 thresholds (half of them at 300 Tgas), 1/16 tight.
    #[test]
    fn mixed_is_mostly_normal_with_a_low_tail_and_some_thresholds() {
        for limits in LIMITS {
            under(limits, |top| {
                let all = draws(mixed(), 4000);
                let t = top as f64 / TGAS as f64;
                let tight = all.iter().filter(|&&g| g == TIGHT).count();
                assert!((180..330).contains(&tight), "{limits:?}: {tight} of 4000 tight");
                let gas: Vec<u64> = all.into_iter().filter(|&g| g != TIGHT).collect();
                let off_grid = |g: &&u64| !g.is_multiple_of(STEP);
                assert!(gas.iter().all(|g| (LOW_FLOOR..=top + 1).contains(g)), "{limits:?}");
                assert!(
                    gas.iter().any(|&g| g > top - 20 * TGAS),
                    "{limits:?}: draws reach the top"
                );
                assert!(
                    gas.iter().filter(off_grid).all(at_threshold),
                    "only thresholds are off the grid"
                );
                // 1/8 thresholds, two thirds of them off by one gas.
                let edges = gas.iter().filter(off_grid).count();
                assert!((250..420).contains(&edges), "{edges} of 4000 at a threshold ± 1 gas");
                // Half the thresholds are 300 Tgas or 1 gas either side.
                let wallet = gas.iter().filter(|g| at_300(g)).count();
                assert!(
                    (180..330).contains(&wallet),
                    "{limits:?}: {wallet} of 4000 at 300 Tgas ± 1"
                );
                // 13/16 of: 3/4 normal, plus the low arm's share above 100
                // Tgas; and the 300 Tgas thresholds.
                let normal = gas.iter().filter(|&&g| g >= NORMAL_FLOOR).count();
                let share = 13.0 / 16.0 * (0.75 + 0.25 * (t / 100.0).ln() / t.ln()) + 1.0 / 16.0;
                assert!(
                    near(normal, 4000.0 * share, 130.0),
                    "{limits:?}: {normal} at 100 Tgas or more"
                );
                // 13/16 of ln(10)/ln(top) of the low quarter.
                let below_10 = gas.iter().filter(|&&g| g < 10 * TGAS).count();
                let share = 13.0 / 16.0 * 0.25 * 10f64.ln() / t.ln();
                assert!(
                    near(below_10, 4000.0 * share, 90.0),
                    "{limits:?}: {below_10} below 10 Tgas"
                );
            });
        }
    }

    /// A gas field: half its draws at a threshold (30 or 300 Tgas, or 1
    /// gas either side), the rest as `mixed`; it shrinks toward the
    /// ceiling, never to `None`.
    #[test]
    fn a_gas_field_is_at_a_threshold_in_half_the_draws() {
        for limits in LIMITS {
            under(limits, |top| {
                let mut runner = TestRunner::deterministic();
                let gas: Vec<u64> = (0..4000)
                    .map(|_| field().new_tree(&mut runner).unwrap().current().as_gas())
                    .collect();
                let at = gas.iter().filter(|g| at_threshold(g)).count();
                assert!((1850..2150).contains(&at), "{at} of 4000 at a threshold ± 1 gas");
                assert!(gas.contains(&(30 * TGAS)) && gas.contains(&(30 * TGAS - 1)));
                let wallet = WALLET_GAS.as_gas();
                assert!(gas.contains(&wallet) && gas.contains(&(wallet + 1)));
                assert!(gas.iter().all(|g| (LOW_FLOOR - 1..=top + 1).contains(g)));
                let mut tree = field().new_tree(&mut runner).unwrap();
                while tree.simplify() {}
                assert!(tree.current().as_gas() >= 30 * TGAS - 1);
            });
        }
    }

    #[test]
    fn wide_is_log_uniform() {
        for limits in LIMITS {
            under(limits, |top| {
                let gas = draws(wide(), 4000);
                let t = top as f64 / TGAS as f64;
                let below_10 = gas.iter().filter(|&&g| g < 10 * TGAS).count();
                // 13/16 of ln(10)/ln(top) of all draws.
                let share = 13.0 / 16.0 * 10f64.ln() / t.ln();
                assert!(
                    near(below_10, 4000.0 * share, 150.0),
                    "{limits:?}: {below_10} below 10 Tgas"
                );
                let value = |arm: Arm, i| exactly(Some(arm.value(i))).unwrap().as_gas();
                assert_eq!(value(Arm::Low(top), 0), top);
                assert_eq!(value(Arm::Normal(top), 0), top);
                assert_eq!(value(Arm::Low(top), LOG_STEPS), LOW_FLOOR);
                assert_eq!(value(Arm::Normal(top), Arm::Normal(top).last()), NORMAL_FLOOR);
            });
        }
    }

    /// A tight draw is the least gas that succeeds, plus 0, 1 gas or 0.1
    /// Tgas, a third each, and shrinks to `None` only.
    #[test]
    fn tight_is_the_least_gas_plus_a_little() {
        let mut runner = TestRunner::deterministic();
        let mut plus = [0; 3];
        for _ in 0..600 {
            let tight = Explored { low: 0.0, thresholds: 0.0, tight: 1.0, unset: true };
            let mut tree = tight.new_tree(&mut runner).unwrap();
            let Some(StepGas::LeastPlus(g)) = tree.current() else {
                panic!("{:?}", tree.current())
            };
            plus[TIGHT_PLUS.iter().position(|&p| p == g.as_gas()).unwrap()] += 1;
            assert!(tree.simplify() && tree.current().is_none());
            assert!(tree.complicate() || tree.current().is_some());
        }
        assert!(plus.iter().all(|&n| (150..250).contains(&n)), "{plus:?}");
    }
}

/// Every `i64.const` in `code`'s functions that is a whole number of Tgas
/// from 1 to `top`: the gas a contract compares `prepaid_gas` against or
/// attaches to a call, as near-sdk's `Gas::from_tgas` compiles. Empty for
/// code that doesn't parse.
pub(crate) fn constants(code: &[u8], top: u64) -> std::collections::BTreeSet<u64> {
    use wasmparser::{Operator, Parser, Payload};
    let mut found = std::collections::BTreeSet::new();
    for payload in Parser::new(0).parse_all(code) {
        let Ok(Payload::CodeSectionEntry(body)) = payload else { continue };
        let Ok(mut ops) = body.get_operators_reader() else { continue };
        while !ops.eof() {
            let Ok(op) = ops.read() else { break };
            // `prepaid >= 30 Tgas` compiles to `prepaid > 30 Tgas - 1`.
            if let Operator::I64Const { value } = op
                && let Ok(v) = u64::try_from(value)
                && let Some(v) = [v, v + 1].into_iter().find(|v| v % TGAS == 0)
                && (TGAS..=top).contains(&v)
            {
                found.insert(v);
            }
        }
    }
    found
}

#[cfg(test)]
mod thresholds_tests {
    use super::*;

    /// A search's fixture adds its contracts' gas constants up to its
    /// ceiling, a `>= C` floor as compiled (`> C - 1`) included, and
    /// they're drawn exactly; outside a search only 30 and 300 Tgas are.
    #[test]
    fn a_fixtures_gas_constants_are_drawn_exactly() {
        let wat = r#"(module (func (export "f") (result i64)
            i64.const 5000000000000 drop i64.const 41999999999999 drop
            i64.const 123 drop i64.const 1100000000000000 drop i64.const 400000000000000))"#;
        let code = wat::parse_str(wat).unwrap();
        let tgas = |v: &[u64]| v.iter().map(|t| Gas::from_teragas(*t)).collect::<Vec<_>>();
        assert_eq!(thresholds(), tgas(&[30, 300]));
        for (limits, want) in [
            (crate::GasLimits::Protocol, tgas(&[5, 30, 42, 300, 400])),
            (crate::GasLimits::Legacy300, tgas(&[5, 30, 42, 300])),
        ] {
            let mut sim = crate::Fixture::new().contract("c.near", code.clone()).build();
            sim.config.gas_limits = limits;
            let (inside, draws) = with_fixture(&sim, || {
                let mut runner = TestRunner::deterministic();
                let draws: Vec<u64> = (0..2000)
                    .map(|_| mixed().new_tree(&mut runner).unwrap().current())
                    .filter_map(|g| match g {
                        Some(StepGas::Exactly(g)) => Some(g.as_gas()),
                        _ => None,
                    })
                    .collect();
                (thresholds(), draws)
            });
            assert_eq!(inside, want, "{limits:?}");
            assert_eq!(thresholds(), tgas(&[30, 300]), "restored after the search");
            for t in [5, 30, 42] {
                let t = t * TGAS;
                for g in [t - 1, t + 1] {
                    assert!(draws.contains(&g), "{g} drawn");
                }
            }
        }
    }
}
