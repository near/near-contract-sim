//! Progress on stderr for a long campaign (a search, a scan, curves): a
//! count and the time left, at most every [`EVERY`], and one line at the
//! end; `NEAR_SIM_PROGRESS=0` silences it. A phase outside the count (the
//! witnesses, shrinking, settling the findings) prints its own line as it
//! starts and its own time left, and its time is left out of the count's
//! mean. It changes nothing a campaign does or reports.

use std::cell::RefCell;
use std::time::{Duration, Instant};

/// The least time between two lines.
pub(crate) const EVERY: Duration = Duration::from_secs(10);

/// What a campaign or a phase counts: `case` 40 of 512.
struct Count {
    unit: String,
    done: u32,
    total: u32,
    started: Instant,
    /// The deadline it ends by, if it has one (a shrink budget).
    by: Option<Instant>,
}

impl Count {
    fn new(unit: String, total: u32, by: Option<Instant>) -> Count {
        Count { unit, done: 0, total, started: Instant::now(), by }
    }
}

/// One campaign's progress: `search of 256 case(s) per seed`, its count,
/// and the phase it is in, if any.
struct Stage {
    what: String,
    count: Count,
    phase: Option<Count>,
    /// The time phases took: not the count's.
    aside: Duration,
    /// What follows the count, said with its time left.
    then: String,
    last: Option<Instant>,
    /// Campaigns running inside this one: they leave its count alone.
    inner: u32,
}

thread_local! {
    static STAGE: RefCell<Option<Stage>> = const { RefCell::new(None) };
}

/// Starts reporting a campaign of `total` `unit`s on this thread, unless
/// silenced, until the guard drops; a campaign already reported here (a
/// scan inside another campaign) keeps its own, and the inner one's
/// counts and phases print nothing. `then` follows the time left: what
/// runs after the count.
pub(crate) fn start(what: String, unit: &str, total: u32, then: &str) -> Guard {
    if std::env::var("NEAR_SIM_PROGRESS").is_ok_and(|v| v.trim() == "0") {
        return Guard { owns: false };
    }
    STAGE.with(|s| {
        let mut s = s.borrow_mut();
        match s.as_mut() {
            Some(outer) => outer.inner += 1,
            None => {
                let count = Count::new(unit.to_owned(), total, None);
                let (phase, aside, last, then) = (None, Duration::ZERO, None, then.to_owned());
                *s = Some(Stage { what, count, phase, aside, then, last, inner: 0 });
            }
        }
        Guard { owns: s.as_ref().is_some_and(|s| s.inner == 0) }
    })
}

/// Ends the campaign it started, with its last line, when dropped.
pub(crate) struct Guard {
    owns: bool,
}

impl Drop for Guard {
    fn drop(&mut self) {
        if !self.owns {
            STAGE.with(|s| s.borrow_mut().as_mut().map(|s| s.inner = s.inner.saturating_sub(1)));
            return;
        }
        if let Some(s) = STAGE.with(|s| s.borrow_mut().take()) {
            let elapsed = clock(s.count.started.elapsed());
            eprintln!("{}: done, {} {}(s) in {elapsed}", s.what, s.count.done, s.count.unit);
        }
    }
}

/// `done` units of the campaign are done.
pub(crate) fn step(done: u32) {
    update(|s| s.count.done = if s.inner == 0 { done } else { s.count.done });
}

/// The campaign has `total` units in all, now that it knows.
pub(crate) fn total(total: u32) {
    update(|s| s.count.total = if s.inner == 0 { total } else { s.count.total });
}

/// Starts a phase of `total` `unit`s (none: uncounted) outside the
/// campaign's count, ending `by` a deadline if it has one, until the
/// guard drops. Its first line prints now.
pub(crate) fn phase(unit: String, total: u32, by: Option<Instant>) -> Phase {
    STAGE.with(|s| {
        let mut s = s.borrow_mut();
        let Some(stage) = s.as_mut().filter(|s| s.inner == 0 && s.phase.is_none()) else {
            return Phase { owns: false };
        };
        stage.phase = Some(Count::new(unit, total, by));
        print(stage, Instant::now());
        Phase { owns: true }
    })
}

/// Ends the phase it started when dropped; the count's lines resume.
pub(crate) struct Phase {
    owns: bool,
}

impl Phase {
    /// `done` of the phase's units are done, of `total`.
    pub(crate) fn step(&self, done: u32, total: u32) {
        if self.owns {
            update(|s| s.phase.as_mut().map_or((), |p| (p.done, p.total) = (done, total)));
        }
    }
}

impl Drop for Phase {
    fn drop(&mut self) {
        if self.owns {
            STAGE.with(|s| {
                let mut s = s.borrow_mut();
                let Some(stage) = s.as_mut() else { return };
                if let Some(p) = stage.phase.take() {
                    stage.aside += p.started.elapsed();
                }
            });
        }
    }
}

/// Prints a line if one is due: a long unit or phase item still says
/// where it is.
pub(crate) fn tick() {
    update(|_| ());
}

fn update(f: impl FnOnce(&mut Stage)) {
    STAGE.with(|s| {
        let mut s = s.borrow_mut();
        let Some(stage) = s.as_mut() else { return };
        f(stage);
        let now = Instant::now();
        if now.saturating_duration_since(stage.last.unwrap_or(stage.count.started)) >= EVERY {
            print(stage, now);
        }
    });
}

fn print(stage: &mut Stage, now: Instant) {
    stage.last = Some(now);
    eprintln!("{}", line(stage, now));
}

/// `scan of 3 plan(s) variant 40/192 · 4m10s elapsed, ~15m50s left (from
/// the mean variant time)`; in a phase, `search ...: shrinking seed 1's
/// findings 3/12 · 5m10s elapsed, at most 9m50s left (its budget)`.
fn line(s: &Stage, now: Instant) -> String {
    let elapsed = clock(now.saturating_duration_since(s.count.started));
    let Some(p) = &s.phase else {
        let c = &s.count;
        let mut out = format!("{} {} {}/{} · {elapsed} elapsed", s.what, c.unit, c.done, c.total);
        if c.done > 0 {
            let spent = now.saturating_duration_since(c.started).saturating_sub(s.aside);
            let left = spent / c.done * c.total.saturating_sub(c.done);
            out += &format!(", ~{} left (from the mean {} time){}", clock(left), c.unit, &s.then);
        }
        return out;
    };
    let mut out = format!("{}: {}", s.what, p.unit);
    if p.total > 0 {
        out += &format!(" {}/{}", p.done, p.total);
    }
    out += &format!(" · {elapsed} elapsed");
    let spent = now.saturating_duration_since(p.started);
    match p.by {
        Some(by) => {
            out +=
                &format!(", at most {} left (its budget)", clock(by.saturating_duration_since(now)))
        }
        None if p.done > 0 && p.total > 0 => {
            let left = spent / p.done * p.total.saturating_sub(p.done);
            out += &format!(", ~{} left (from the mean time so far)", clock(left));
        }
        None => {}
    }
    out
}

/// `4m10s`, `35s`, `1h02m`.
pub(crate) fn clock(d: Duration) -> String {
    let s = d.as_secs();
    match (s / 3600, s / 60 % 60, s % 60) {
        (0, 0, s) => format!("{s}s"),
        (0, m, s) => format!("{m}m{s:02}s"),
        (h, m, _) => format!("{h}h{m:02}m"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stage(what: &str, unit: &str, done: u32, total: u32, started: Instant) -> Stage {
        let count = Count { unit: unit.into(), done, total, started, by: None };
        let (phase, aside, last) = (None, Duration::ZERO, None);
        Stage { what: what.into(), count, phase, aside, then: String::new(), last, inner: 0 }
    }

    /// The header: the count, the time so far and the time left at the
    /// mean unit time; none left before the first unit is done.
    #[test]
    fn the_line_counts_and_estimates_from_the_mean() {
        let s = Duration::from_secs;
        let t0 = Instant::now();
        let at = |n| stage("scan of 3 plan(s)", "variant", n, 192, t0);
        assert_eq!(
            line(&at(40), t0 + s(250)),
            "scan of 3 plan(s) variant 40/192 · 4m10s elapsed, ~15m50s left (from the mean \
             variant time)"
        );
        assert_eq!(line(&at(0), t0 + s(5)), "scan of 3 plan(s) variant 0/192 · 5s elapsed");
        assert_eq!((clock(s(35)), clock(s(3720))), ("35s".into(), "1h02m".into()));
    }

    /// A phase outside the count, such as shrinking, says what it is, how
    /// far it is, and its time left: what its deadline leaves, or its own
    /// mean. Its time stays out of the count's mean, and the count says
    /// what follows it.
    #[test]
    fn a_phase_says_where_it_is_and_stays_out_of_the_mean() {
        let s = Duration::from_secs;
        let t0 = Instant::now();
        let what = "search of 256 case(s) per seed";
        let mut st = stage(what, "case", 256, 512, t0);
        st.phase = Some(Count {
            unit: "shrinking seed 0's findings".into(),
            done: 3,
            total: 12,
            started: t0 + s(60),
            by: Some(t0 + s(960)),
        });
        assert_eq!(
            line(&st, t0 + s(370)),
            "search of 256 case(s) per seed: shrinking seed 0's findings 3/12 · 6m10s elapsed, \
             at most 9m50s left (its budget)"
        );
        let settling = Count::new("settling findings".into(), 40, None);
        st.phase = Some(Count { done: 10, started: t0, ..settling });
        let settled = line(&st, t0 + s(20));
        assert!(settled.ends_with("10/40 · 20s elapsed, ~1m00s left (from the mean time so far)"));
        // Back to the cases: 60 s of them, 10 min aside, and what follows.
        (st.phase, st.aside, st.then) = (None, s(600), ", then shrinking".into());
        assert_eq!(
            line(&st, t0 + s(660)),
            "search of 256 case(s) per seed case 256/512 · 11m00s elapsed, ~1m00s left (from \
             the mean case time), then shrinking"
        );
    }

    /// A campaign inside another (a scan inside curves) leaves the outer
    /// one's stage when it ends, and its count alone while it runs: an
    /// inner scan never sets the outer's total.
    #[test]
    fn a_nested_campaign_keeps_the_outer_stage() {
        let count = || STAGE.with(|s| s.borrow().as_ref().map(|s| (s.count.done, s.count.total)));
        let outer = start("curves".into(), "axis", 9, "");
        if !outer.owns {
            return; // NEAR_SIM_PROGRESS=0
        }
        let inner = start("scan".into(), "variant", 0, "");
        total(500);
        step(40);
        assert!(!phase("settling".into(), 0, None).owns);
        assert_eq!(count(), Some((0, 9)));
        drop(inner);
        step(3);
        assert_eq!(count(), Some((3, 9)));
        drop(outer);
        assert_eq!(count(), None);
    }
}
