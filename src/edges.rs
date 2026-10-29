//! Exact edges of a key over a `u64` knob: caller gas
//! ([`Tx::gas_edges`](crate::Tx::gas_edges)), or a size the caller's
//! closure builds an input of.

use std::collections::{BTreeMap, BTreeSet};
use std::ops::RangeInclusive;

/// A maximal run of the knob where every sample read one key: what
/// [`edges`] and [`Tx::gas_edges`](crate::Tx::gas_edges) return, in
/// order. The regions tile the range: each starts 1 above the last's
/// end, and each end is exact to 1 (its neighbor was read and differs).
/// The interior was sampled, not proven: an island inside that no read
/// landed on is part of it.
#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Region<K> {
    /// The knob values it covers, from the first sample to the last.
    pub range: RangeInclusive<u64>,
    /// What the probe returned on it.
    pub key: K,
    /// Why the earliest read of its samples was read.
    pub why: Why,
}

/// Why a [`Region`] was found: why the earliest read of its samples was
/// read. The grid and its bisection are general; the other two are
/// heuristics.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Why {
    /// A grid read, or the bisection between two grid reads that differ.
    Grid,
    /// First read probing within `radius` of an edge, or bisecting from
    /// such a read.
    Probe,
    /// First read at a solved point of
    /// [`Tx::gas_edges`](crate::Tx::gas_edges) (a child at 0 gas or a
    /// gas constant), or bisecting from one.
    Solved,
}

/// Every point in `range` where `probe` changes, each exact to 1, read
/// every `step` and bisected, then probed one by one within `radius` of
/// each edge. A region is a run of agreeing samples: its interior was
/// sampled, not proven.
///
/// It reads the start of `range`, every `step` on, and its end, then
/// bisects every two neighboring reads that differ until they are 1
/// apart, to a fixpoint: between two reads that differ it finds an edge,
/// exact to 1, and every region a midpoint lands on, each refined in
/// turn. Then it reads every point within `radius` of each edge (an
/// island beside an edge), bisects again, and repeats until a round reads
/// nothing new. It costs `ceil((end - start) / step) + 1` reads, about
/// `log2(step)` more an edge, and up to `2 * radius` more an edge for the
/// probe; `radius` 0 skips the probe. A panic in `probe` propagates.
///
/// What can still be missed: an island that no read lands on, farther
/// than `radius` from every edge, whether its key equals its
/// neighbors' or not; one between two reads that differ is found only
/// if a midpoint of their bisection lands on it.
/// [`Tx::gas_edges`](crate::Tx::gas_edges) adds solved points for gas.
///
/// A finding's window over one numeric argument ("fires for a deposit
/// in 28..=72"): `edges(range, step, radius, |v| { let run =
/// h.run(&plan_at(v)); (run.verdict(), run.found_at(check, at).is_some())
/// })`, where `plan_at(v)` rebuilds the finding's plan with the argument
/// at `v` ([`Run::found_at`](crate::explore::Run::found_at)); the verdict
/// keeps a value where the plan errs or stays incomplete apart from one
/// where the check passed. Each read is a run, and the radius probe is
/// the heuristic above: a window off every read is missed.
///
/// Panics if `step` is 0 or `range` is empty; if the key changes more
/// than 16 times between two neighboring grid reads, naming where (a
/// heuristic guard: a key that changes nearly everywhere holds something
/// that moves with the knob, and would bisect without end; a key with
/// that many real edges a step needs a smaller step). It catches a leak
/// that changes at least 17 times a step; a slower one reads as edges.
/// And if 16 rounds of probing still read something new (islands closer
/// together than `radius`: a smaller one ends it).
///
/// A limit at 1,234, found to 1 from reads 100 apart; a size limit is the
/// same with a closure that runs a call on an input of `n` bytes
/// (`tests/gas_edges.rs`):
///
/// ```
/// use near_contract_sim::explore::{Why, edges};
///
/// let found = edges(0..=10_000, 100, 64, |n| n <= 1_234);
/// let found: Vec<_> = found.iter().map(|r| (r.range.clone(), r.key, r.why)).collect();
/// assert_eq!(found, [(0..=1_234, true, Why::Grid), (1_235..=10_000, false, Why::Grid)]);
/// ```
#[track_caller]
pub fn edges<K: Eq + Clone>(
    range: RangeInclusive<u64>,
    step: u64,
    radius: u64,
    probe: impl Fn(u64) -> K,
) -> Vec<Region<K>> {
    scan(range, step, radius, |at| (probe(at), ()), |_| BTreeSet::new())
}

/// More pairs of neighbors 1 apart that differ than this between two
/// neighboring grid reads, and the key is taken to leak something that
/// moves with the knob.
const LEAK: usize = 16;
/// Rounds of probing (radius, then solved points, then bisection) before
/// it panics rather than stop.
const ROUNDS: usize = 16;

/// One read: the key, what the probe returned besides it, and why it was
/// read.
pub(crate) struct Sample<K, X> {
    pub(crate) key: K,
    pub(crate) extra: X,
    pub(crate) why: Why,
    /// How many reads came before it.
    pub(crate) n: usize,
}

/// The reads of a scan so far, by knob value.
pub(crate) type Samples<K, X> = BTreeMap<u64, Sample<K, X>>;

/// [`edges`], with what `probe` returns besides the key kept beside it,
/// and `solve` naming more points to read from the samples so far (read
/// as [`Why::Solved`]).
#[track_caller]
pub(crate) fn scan<K: Eq + Clone, X>(
    range: RangeInclusive<u64>,
    step: u64,
    radius: u64,
    probe: impl Fn(u64) -> (K, X),
    solve: impl Fn(&Samples<K, X>) -> BTreeSet<u64>,
) -> Vec<Region<K>> {
    let (lo, hi) = (*range.start(), *range.end());
    assert!(step > 0, "edges: a step of 0 never moves");
    assert!(lo <= hi, "edges: {range:?} is empty");
    let mut s = Scan { lo, step, samples: BTreeMap::new(), probe, probed: BTreeSet::new() };
    let mut at = lo;
    loop {
        s.read(at, Why::Grid);
        if at == hi {
            break;
        }
        at = at.saturating_add(step).min(hi);
    }
    s.refine(Why::Grid);
    for _ in 0..ROUNDS {
        let before = s.samples.len();
        let near: Vec<u64> = s.edges().into_iter().filter(|e| s.probed.insert(*e)).collect();
        for e in near {
            for at in e.saturating_sub(radius).max(lo)..=e.saturating_add(radius).min(hi) {
                s.read(at, Why::Probe);
            }
        }
        s.guard();
        s.refine(Why::Probe);
        for at in solve(&s.samples) {
            if (lo..=hi).contains(&at) {
                s.read(at, Why::Solved);
            }
        }
        s.guard();
        s.refine(Why::Solved);
        if s.samples.len() == before {
            return s.regions();
        }
    }
    panic!(
        "edges: {ROUNDS} rounds of probing still read new points over {range:?}: islands closer \
         together than the radius {radius}, or solved points that keep finding more, each start \
         a new round. A coarser key or a smaller radius ends it"
    )
}

struct Scan<K, X, P> {
    lo: u64,
    step: u64,
    samples: Samples<K, X>,
    probe: P,
    /// The edges whose neighborhood was read.
    probed: BTreeSet<u64>,
}

impl<K: Eq + Clone, X, P: Fn(u64) -> (K, X)> Scan<K, X, P> {
    /// Reads `at` unless it was, as `why`.
    fn read(&mut self, at: u64, why: Why) {
        if !self.samples.contains_key(&at) {
            let (key, extra) = (self.probe)(at);
            let n = self.samples.len();
            self.samples.insert(at, Sample { key, extra, why, n });
        }
    }

    fn key(&self, at: u64) -> &K {
        &self.samples[&at].key
    }

    /// Bisects every two neighboring reads that differ and are more than
    /// 1 apart, until none are, reading as `why`.
    #[track_caller]
    fn refine(&mut self, why: Why) {
        loop {
            let pairs: Vec<(u64, u64)> = (self.samples.iter().zip(self.samples.iter().skip(1)))
                .filter(|((a, x), (b, y))| x.key != y.key && *b - *a > 1)
                .map(|((a, _), (b, _))| (*a, *b))
                .collect();
            if pairs.is_empty() {
                return;
            }
            // Each pair is more than 1 apart, so a pass reads its midpoints:
            // a pass that reads nothing would loop forever.
            let before = self.samples.len();
            for (a, b) in pairs {
                let (mut lo, mut hi) = (a, b);
                let low = self.key(a).clone();
                // The keys this bisection met: more than `LEAK + 1` are more
                // than `LEAK` edges within one step, as the guard counts.
                let mut met = vec![low.clone(), self.key(b).clone()];
                while hi - lo > 1 {
                    let mid = lo + (hi - lo) / 2;
                    self.read(mid, why);
                    let key = self.key(mid);
                    if !met.contains(key) {
                        met.push(key.clone());
                        if met.len() > LEAK + 1 {
                            self.leak(lo, mid);
                        }
                    }
                    if *key == low { lo = mid } else { hi = mid }
                }
                self.guard();
            }
            debug_assert!(self.samples.len() > before, "edges: a pass of bisection read nothing");
        }
    }

    /// The first value of each new key: every edge, once refined.
    fn edges(&self) -> Vec<u64> {
        (self.samples.iter().zip(self.samples.iter().skip(1)))
            .filter(|((_, x), (_, y))| x.key != y.key)
            .map(|(_, (b, _))| *b)
            .collect()
    }

    /// Panics if more than [`LEAK`] pairs of neighbors 1 apart differ
    /// between two neighboring grid reads.
    #[track_caller]
    fn guard(&self) {
        let mut per_step = BTreeMap::<u64, usize>::new();
        let exact = (self.samples.iter().zip(self.samples.iter().skip(1)))
            .filter(|((a, x), (b, y))| *a + 1 == **b && x.key != y.key);
        for ((&at, _), _) in exact {
            let n = per_step.entry((at - self.lo) / self.step).or_default();
            *n += 1;
            if *n > LEAK {
                self.leak(at, at + 1);
            }
        }
    }

    /// Panics: the key differs between `a` and `b` and more than [`LEAK`]
    /// times within the step they are in.
    #[track_caller]
    fn leak(&self, a: u64, b: u64) -> ! {
        let from = self.lo + (a - self.lo) / self.step * self.step;
        panic!(
            "edges: the key changes more than {LEAK} times within one step from {from}, as \
             between {a} and {b}: something in it moves with the knob (a gas or a length), so \
             it would bisect without end. Leave that out of the key, or if the edges are real, \
             take a smaller step"
        )
    }

    /// The runs of agreeing samples, each with why its first was read.
    fn regions(self) -> Vec<Region<K>> {
        let samples: Vec<(u64, Sample<K, X>)> = self.samples.into_iter().collect();
        debug_assert!(samples.first().is_some_and(|s| s.0 == self.lo));
        (samples.chunk_by(|(_, a), (_, b)| a.key == b.key))
            .map(|run| {
                let (_, first) = run.iter().min_by_key(|(_, s)| s.n).expect("a run has a sample");
                let range = run[0].0..=run[run.len() - 1].0;
                Region { range, key: first.key.clone(), why: first.why }
            })
            .collect()
    }
}

/// The least value in `(lo, hi]` where `ok` holds, given it fails at `lo`
/// and holds at `hi`.
pub(crate) fn bisect(mut lo: u64, mut hi: u64, ok: impl Fn(u64) -> bool) -> u64 {
    while hi - lo > 1 {
        let mid = lo + (hi - lo) / 2;
        if ok(mid) { hi = mid } else { lo = mid }
    }
    hi
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    /// The `true` ranges of `edges` over 1..=100 at `step` and `radius`,
    /// with the key holding on `held`. More than 300 reads panics, so a
    /// loop that never ends fails at once.
    fn held_of(step: u64, radius: u64, held: &[RangeInclusive<u64>]) -> Vec<RangeInclusive<u64>> {
        let reads = Cell::new(0);
        let found = edges(1..=100, step, radius, |g| {
            reads.set(reads.get() + 1);
            assert!(reads.get() <= 300, "step {step}: over 300 reads");
            held.iter().any(|w| w.contains(&g))
        });
        tiles(&found, 1, 100);
        found.into_iter().filter(|r| r.key).map(|r| r.range).collect()
    }

    /// The regions tile `lo..=hi`, and neighbors differ.
    fn tiles<K: Eq>(found: &[Region<K>], lo: u64, hi: u64) {
        assert_eq!(*found[0].range.start(), lo);
        assert_eq!(*found.last().unwrap().range.end(), hi);
        for w in found.windows(2) {
            assert_eq!(*w[0].range.end() + 1, *w[1].range.start());
            assert!(w[0].key != w[1].key);
        }
    }

    #[test]
    fn edges_bisect_each_edge_to_one() {
        // A window touching an end is found at any step; one inside, at a
        // step narrower than it.
        for step in [1, 3, 10, 99, u64::MAX] {
            for held in [vec![], vec![1..=100], vec![1..=25], vec![95..=100]] {
                assert_eq!(held_of(step, 0, &held), held, "step {step}");
            }
            let found = if step <= 10 { vec![30..=55] } else { vec![] };
            assert_eq!(held_of(step, 0, &[30..=55]), found, "step {step}");
        }
        // Edges on a reading (1, 11, 21, ...), and on either side of one.
        for held in [11..=21, 12..=21, 11..=20, 10..=22, 2..=99, 100..=100, 1..=1] {
            assert_eq!(held_of(10, 0, std::slice::from_ref(&held)), [held], "step 10");
        }
        let two = [10..=20, 50..=70];
        assert_eq!(held_of(5, 0, &two), two);
        let one = edges(7..=7, 10, 64, |g| g == 7);
        assert_eq!(one, [Region { range: 7..=7, key: true, why: Why::Grid }]);
        assert_eq!(edges(0..=0, 1, 0, |_| 0u8)[0].range, 0..=0);
    }

    /// With no edge, it reads `ceil(span / step) + 1` points and no more.
    #[test]
    fn edges_read_ceil_of_the_span_over_the_step_plus_one_with_no_edge() {
        for (step, reads) in [(10, 11), (11, 10), (99, 2), (u64::MAX, 2)] {
            let n = Cell::new(0);
            let found = edges(1..=100, step, 64, |_| {
                n.set(n.get() + 1);
                assert!(n.get() <= reads, "step {step}: more than {reads} reads");
            });
            assert_eq!(found, [Region { range: 1..=100, key: (), why: Why::Grid }]);
            assert_eq!(n.get(), reads, "step {step}");
        }
    }

    /// Three keys: between two reads that differ, an island a midpoint
    /// lands on is found (`Grid`), and one none lands on is missed; one
    /// between two reads that agree is found only within `radius` of an
    /// edge (`Probe`), and missed beyond it.
    #[test]
    fn an_island_is_found_where_a_read_lands_on_it() {
        // 0 below 50, 1 from 50; an island of `at` on 2.
        let key = |at: RangeInclusive<u64>| {
            move |g: u64| match g {
                g if at.contains(&g) => 2,
                50.. => 1,
                _ => 0,
            }
        };
        // Reads 1 and 100 differ; bisection reads 50, 25, 37, ...
        let found = edges(1..=100, 99, 0, key(37..=40));
        assert_eq!(
            found.iter().map(|r| (r.range.clone(), r.key)).collect::<Vec<_>>(),
            [(1..=36, 0), (37..=40, 2), (41..=49, 0), (50..=100, 1)]
        );
        assert!(found.iter().all(|r| r.why == Why::Grid), "{found:?}");
        let off = edges(1..=100, 99, 0, key(28..=31));
        assert_eq!(off.len(), 2, "pinned miss, off the bisection's path: {off:?}");
        // At a step of 10 the island lies between reads 31 and 41, both 0,
        // and 10 below the edge at 50: inside a radius of 12, not of 8.
        let near = edges(1..=100, 10, 12, key(37..=40));
        let at = near.iter().find(|r| r.key == 2).expect("found within the radius");
        assert_eq!((at.range.clone(), at.why), (37..=40, Why::Probe), "{near:?}");
        tiles(&near, 1, 100);
        let far = edges(1..=100, 10, 8, key(37..=40));
        assert_eq!(far.len(), 2, "pinned miss: {far:?}");
    }

    /// The guard counts edges a step: 99 real edges, one every 100, pass;
    /// 17 within one step trip it, naming the step.
    #[test]
    fn the_guard_counts_edges_within_a_step() {
        let found = edges(0..=9_999, 10, 0, |g| g / 100);
        assert_eq!(found.len(), 100);
        tiles(&found, 0, 9_999);
        let crowded = std::panic::catch_unwind(|| edges(0..=1_000, 1_000, 0, |g| g.min(17)));
        let msg = *crowded.unwrap_err().downcast::<String>().unwrap();
        assert!(msg.contains("within one step from 0") && msg.contains("smaller step"), "{msg}");
        // 16 within one step is fine.
        assert_eq!(edges(0..=1_000, 1_000, 0, |g| g.min(16)).len(), 17);
        // Steps count from the start: 17 edges at 1,000..1,016 lie in the
        // step from 1,000, and trip it however the reads fall.
        let late = |g: u64| g.clamp(1_000, 1_017);
        let msg = std::panic::catch_unwind(|| edges(500..=2_500, 500, 0, late)).unwrap_err();
        let msg = *msg.downcast::<String>().unwrap();
        assert!(msg.contains("within one step from 1000, as between 1016 and 1017:"), "{msg}");
        assert_eq!(edges(500..=2_500, 500, 0, |g| g.clamp(1_000, 1_016)).len(), 17);
        // 16 either side of the step boundary at 1,100 are two steps of 16.
        let found = edges(100..=2_100, 1_000, 0, |g| g.clamp(1_084, 1_116));
        assert_eq!(found.len(), 33);
        tiles(&found, 100, 2_100);
    }

    /// One bisection that lands on a new key at every midpoint: 16 edges
    /// in its step pass, 17 trip it there, before the guard runs.
    #[test]
    fn a_bisection_meeting_more_than_16_edges_trips_the_guard() {
        // The bit length: an edge at each power of 2, all on the path down
        // from the top.
        let bits = |g: u64| 64 - g.leading_zeros();
        let found = edges(0..=1 << 15, 1 << 15, 0, bits);
        assert_eq!(found.len(), 17);
        tiles(&found, 0, 1 << 15);
        let msg = std::panic::catch_unwind(|| edges(0..=1 << 16, 1 << 16, 0, bits)).unwrap_err();
        let msg = *msg.downcast::<String>().unwrap();
        assert!(msg.contains("from 0, as between 0 and 1:"), "{msg}");
    }

    /// Islands closer together than the radius keep starting rounds: the
    /// cap panics rather than stop.
    #[test]
    #[should_panic(expected = "a smaller radius ends it")]
    fn rounds_that_keep_reading_panic() {
        // A 1-wide island every 50, the last at the end: each round's probe
        // of 64 reaches only the next one down.
        edges(0..=10_025, 100, 64, |g| g % 50 == 25);
    }

    /// A key that changes at every point trips the guard rather than
    /// bisect without end.
    #[test]
    #[should_panic(expected = "moves with the knob")]
    fn a_key_that_changes_everywhere_trips_the_guard() {
        edges(0..=1_000_000, 100_000, 0, |g| g);
    }

    /// Solved points are read, labeled `Solved`, and refined from.
    #[test]
    fn solved_points_are_read_and_refined() {
        let key = |g: u64| (501..=504).contains(&g);
        let solve = |_: &Samples<bool, ()>| BTreeSet::from([502]);
        let found = scan(0..=1000, 100, 64, |g| (key(g), ()), solve);
        let at = found.iter().find(|r| r.key).unwrap();
        assert_eq!((at.range.clone(), at.why), (501..=504, Why::Solved), "{found:?}");
        // Its neighbors, first read by the grid, stay `Grid`.
        assert!(found.iter().filter(|r| !r.key).all(|r| r.why == Why::Grid), "{found:?}");
        assert!(edges(0..=1000, 100, 64, key).iter().all(|r| !r.key));
    }

    #[test]
    fn bisect_finds_the_least_that_holds() {
        assert_eq!(bisect(0, 100, |g| g >= 37), 37);
        assert_eq!(bisect(36, 37, |g| g >= 37), 37);
    }
}
