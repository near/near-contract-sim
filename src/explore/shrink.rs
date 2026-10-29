//! Shrinking a plan by what it is, not by the seed that drew it: for the
//! cases no seed drew (a scan's edges, a curve's crossing, a finding first
//! shown while another shrank). Blocks of steps go first, halving down to
//! one, then each step's injections, then its gas, then the start gap.

use super::audit::Shrink;
use super::{Gap, Plan, Run};
use std::time::Instant;

/// The most runs one reduction takes.
pub(crate) const RUNS: u32 = 200;

/// Shrinks `best`'s plan through `run`, keeping each candidate `shows`
/// accepts, until none is, `RUNS` ran, or `until` passed.
pub(crate) fn reduce<O: Clone>(
    mut best: Run<O>,
    run: &mut dyn FnMut(&Plan<O>) -> Run<O>,
    shows: &dyn Fn(&Run<O>) -> bool,
    until: Instant,
) -> (Run<O>, Shrink) {
    let mut ran = 0;
    let status = 'shrink: loop {
        let plan = best.plan.clone();
        for candidate in smaller(&plan) {
            if ran >= RUNS {
                break 'shrink Shrink::Cut(ran, "its run limit".into());
            }
            if Instant::now() > until {
                break 'shrink Shrink::Cut(ran, "its time limit".into());
            }
            ran += 1;
            let r = run(&candidate);
            if shows(&r) {
                best = r;
                continue 'shrink;
            }
        }
        break Shrink::Done(ran);
    };
    (best, status)
}

/// The plans one simplification smaller than `plan`, simplest first, each
/// made only when asked for.
fn smaller<O: Clone>(plan: &Plan<O>) -> impl Iterator<Item = Plan<O>> + '_ {
    // A dropped block's gaps join the step's before it, so the steps that
    // remain are no closer than they were: no overlap no strategy drew.
    // Gaps no one gap outlasts drop no block.
    let steps = blocks(plan.steps.len()).into_iter().filter_map(move |block| {
        let mut p = plan.clone();
        let dropped: Vec<Gap> = p.steps.drain(block.clone()).map(|s| s.then).collect();
        if let Some(before) = block.start.checked_sub(1) {
            let step = &mut p.steps[before];
            step.then = dropped.into_iter().try_fold(step.then, join)?;
        }
        Some(p)
    });
    let each = move |keep: fn(&super::Step<O>) -> bool, clear: fn(&mut super::Step<O>)| {
        (0..plan.steps.len()).filter(move |&i| keep(&plan.steps[i])).map(move |i| {
            let mut p = plan.clone();
            clear(&mut p.steps[i]);
            p
        })
    };
    let injects = each(|s| !s.inject.is_empty(), |s| s.inject.clear());
    let gas = each(|s| s.gas.is_some(), |s| s.gas = None);
    let start = plan.start.iter().map(|_| Plan { start: None, ..plan.clone() });
    steps.chain(injects).chain(gas).chain(start)
}

/// Gap `a` then gap `b` as one gap that waits as long as both, if there
/// is one: blocks add up, as do epoch starts; `Blocks(0)` adds nothing;
/// a settle after a settle is one. An epoch gap waits only for the next
/// boundary, maybe a block away, and a settle for no block count, so no
/// other pair has one.
fn join(a: Gap, b: Gap) -> Option<Gap> {
    match (a, b) {
        (Gap::Blocks(x), Gap::Blocks(y)) => x.checked_add(y).map(Gap::Blocks),
        (Gap::Epochs(x), Gap::Epochs(y)) => x.checked_add(y).map(Gap::Epochs),
        (g, Gap::Blocks(0)) | (Gap::Blocks(0), g) => Some(g),
        (Gap::Settle, Gap::Settle) => Some(Gap::Settle),
        _ => None,
    }
}

/// Blocks of `0..n` to drop: sizes `n / 2`, `n / 4`, ... down to 1, each
/// size's blocks from the end.
fn blocks(n: usize) -> Vec<std::ops::Range<usize>> {
    let mut out = vec![];
    let mut size = (n / 2).max(1);
    if n == 0 {
        return out;
    }
    loop {
        let mut end = n;
        while end > 0 {
            let start = end.saturating_sub(size);
            out.push(start..end);
            end = start;
        }
        if size == 1 {
            break;
        }
        size /= 2;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{blocks, smaller};
    use crate::explore::{Gap, Plan};

    /// Dropping a block never brings the steps that remain closer:
    /// dropping a repeat's last copy (step 2) leaves its first (step 1)
    /// with the copy's `Settle`, not its own `Blocks(0)` into an op no
    /// strategy overlapped it with; dropping its first copy leaves step 0
    /// settling, not overlapping the copy that remains.
    #[test]
    fn a_dropped_block_keeps_the_gap_before_it() {
        let plan = Plan::new().op('a').op('b').blocks(0).op('b').op('c');
        let gaps = |p: &Plan<char>| p.steps.iter().map(|s| s.then).collect::<Vec<_>>();
        assert_eq!(gaps(&plan), [Gap::Settle, Gap::Blocks(0), Gap::Settle, Gap::Settle]);
        let ops = |p: &Plan<char>| p.ops().into_iter().flatten().copied().collect::<String>();
        let dropped: Vec<Plan<char>> = smaller(&plan).filter(|p| ops(p) == "abc").collect();
        assert!(!dropped.is_empty());
        for p in &dropped {
            assert_eq!(gaps(p), [Gap::Settle, Gap::Settle, Gap::Settle], "{:?}", p.steps);
        }
        // Dropping steps 1 and 2 keeps step 0's own gap: the block's last.
        let ac: Vec<Plan<char>> = smaller(&plan).filter(|p| ops(p) == "ac").collect();
        assert!(ac.iter().all(|p| gaps(p) == [Gap::Settle, Gap::Settle]));
    }

    /// A merged gap that can't wait as long as both it replaces is no
    /// candidate: an epoch gap waits only for the next boundary, maybe one
    /// block, so it never outlasts a settle before it, nor keeps blocks
    /// after it.
    #[test]
    fn a_gap_no_single_gap_outlasts_drops_no_step() {
        let ops = |p: &Plan<char>| p.ops().into_iter().flatten().copied().collect::<String>();
        let gaps = |p: &Plan<char>| p.steps.iter().map(|s| s.then).collect::<Vec<_>>();
        for plan in [
            Plan::new().op('a').op('b').then(Gap::EpochEdge(0)).op('c'),
            Plan::new().op('a').then(Gap::EpochEdge(1)).op('b').blocks(3).op('c'),
            Plan::new().op('a').blocks(2).op('b').then(Gap::EpochEdge(0)).op('c'),
            Plan::new().op('a').blocks(2).op('b').op('c'),
        ] {
            let ac: Vec<_> = smaller(&plan).filter(|p| ops(p) == "ac").collect();
            assert!(
                ac.is_empty(),
                "{:?} -> {:?}",
                gaps(&plan),
                ac.iter().map(gaps).collect::<Vec<_>>()
            );
        }
        let both = |plan: Plan<char>| smaller(&plan).find(|p| ops(p) == "ac").map(|p| gaps(&p)[0]);
        let plan = Plan::new().op('a').blocks(2).op('b').blocks(3).op('c');
        assert_eq!(both(plan), Some(Gap::Blocks(5)));
        let plan = Plan::new().op('a').epochs(1).op('b').epochs(2).op('c');
        assert_eq!(both(plan), Some(Gap::Epochs(3)));
        let plan = Plan::new().op('a').blocks(0).op('b').then(Gap::EpochEdge(-1)).op('c');
        assert_eq!(both(plan), Some(Gap::EpochEdge(-1)));
    }

    #[test]
    fn blocks_halve_from_the_end_down_to_one() {
        assert!(blocks(0).is_empty());
        assert_eq!(blocks(1), vec![0..1]);
        assert_eq!(blocks(4), vec![2..4, 0..2, 3..4, 2..3, 1..2, 0..1]);
        assert_eq!(blocks(5)[..3], [3..5, 1..3, 0..1]);
    }
}
