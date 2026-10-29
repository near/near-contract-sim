//! The serial-order memo ([`Orders`](super::books::Orders)): an
//! enumeration reports the same with it as without, keys it exactly
//! where the dedup key isn't, and plays each serial order once; a scan,
//! a search and `Harness::run` play every one. And the fixture's
//! judgments ([`Memo`](super::check::Memo)): an enumeration judges them
//! once and reports the same as judging them in each case. In-crate,
//! since the switches that turn both off for the reference run are a
//! test's alone.
//!
//! Generic demos only: `tests/res`' pool_sync, flight_lock, yield_pay
//! and claim_queue, ported from `tests/explore_enumerate.rs` (each of its
//! SERIALIZABLE tests as an enumeration of its ops), and the token vault
//! of `tests/explore_vault`, cut to run in seconds.
//!
//! Verification boundary: how SERIALIZABLE gets its serial outcomes,
//! and each case its checks' results on the fixture, inside an
//! enumeration; the schedule and every verdict are unchanged, which the
//! memo-off reference run shows over actual runs.

use super::books::{PLAYED, UNMEMOIZED};
use super::enumerate::default_key;
use super::run::{FIXTURE_EACH_CASE, FIXTURE_JUDGED};
use super::{Amount, Audit, Books, Claim, Fail, Faults, Gap, Harness, Op, OpDone, Plan, Skip};
use crate::{Balance, Fixture, Sim, Token, TxOutcome};
use proptest::strategy::Just;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::BTreeMap;

/// `enumerate` on a fresh `harness()`, with the memo and without it (as
/// before the memo, each case judging the fixture): one report, but for
/// the wall-clock timings and the run's own id (`parts`). The serial
/// orders played, and the fixture's judgments, with and without.
fn same_report<O: Op>(
    harness: impl Fn() -> Harness<O>,
    enumerate: impl Fn(&Harness<O>) -> Audit<O>,
) -> (usize, usize, (usize, usize)) {
    let dir = std::env::temp_dir().join(format!(
        "ncs-memo-{}-{}",
        std::process::id(),
        std::thread::current().name().unwrap_or("t").replace("::", "-")
    ));
    let run = |off: bool| {
        struct Reset;
        impl Drop for Reset {
            fn drop(&mut self) {
                UNMEMOIZED.set(false);
                FIXTURE_EACH_CASE.set(false);
            }
        }
        let _reset = Reset;
        // The same store each time, emptied: the replays' paths match.
        let _ = std::fs::remove_dir_all(&dir);
        UNMEMOIZED.set(off);
        FIXTURE_EACH_CASE.set(off);
        let (played, judged) = (PLAYED.get(), FIXTURE_JUDGED.get());
        let mut report = enumerate(&harness().store(&dir)).report();
        report.timings.clear();
        report.parts.clear();
        (report, PLAYED.get() - played, FIXTURE_JUDGED.get() - judged)
    };
    let (on, played, judged) = run(false);
    let (off, unmemoized, each) = run(true);
    let _ = std::fs::remove_dir_all(&dir);
    assert!(on == off, "with the memo:\n{on}\nwithout:\n{off}\n{on:#?}\n{off:#?}");
    // The fixture: judged once for the cases, and once in the first
    // case's run-again guard, which has no memo; before, in each case.
    // A run that stops at its first case judged it once, both ways.
    assert_eq!(judged, each.min(2), "the fixture judged {judged} times, {each} before");
    (played, unmemoized, (judged, each))
}

// ---- pool_sync: two deposits in flight double count ----

const POOL: &str = "pool.near";

fn pool_sync() -> Sim {
    let mut sim = Sim::new();
    sim.create_account("alice.near", Balance::from_near(100));
    sim.create_account("bob.near", Balance::from_near(100));
    sim.create_account(POOL, Balance::from_near(10));
    sim.deploy(POOL, wat::parse_str(include_str!("../../tests/res/pool_sync.wat")).unwrap());
    sim.call("alice.near", POOL, "init").transact().assert_success();
    sim
}

#[derive(Clone, Debug, Serialize, Deserialize)]
enum PoolOp {
    Deposit(String),
}

impl Op for PoolOp {
    fn submit(&self, sim: &mut Sim) -> Result<(), Skip> {
        let PoolOp::Deposit(who) = self;
        sim.call(who, POOL, "deposit").deposit(Balance::from_near(1)).submit();
        Ok(())
    }
}

fn booked(sim: &Sim) -> u128 {
    let bytes = sim.view_raw(POOL, "booked", vec![]).result.unwrap();
    u128::from_le_bytes(bytes.try_into().unwrap())
}

/// Each depositor's credit, and what the pool booked beyond them.
fn pool_books(fixture: &Sim) -> Books<PoolOp> {
    let at_start = booked(fixture);
    Books::new().liability("credits", move |sim: &Sim| {
        let mut credits: BTreeMap<String, u128> = BTreeMap::new();
        for (k, v) in sim.view_state(POOL, b"c") {
            let who = String::from_utf8(k[1..].to_vec()).unwrap();
            credits.insert(who, u128::from_le_bytes(v.try_into().unwrap()));
        }
        let sum: u128 = credits.values().sum();
        credits
            .insert("booked beyond the credits".into(), booked(sim).saturating_sub(at_start + sum));
        credits
    })
}

fn pool() -> Harness<PoolOp> {
    let fixture = pool_sync();
    Harness::new(fixture.clone()).books(pool_books(&fixture))
}

fn deposits() -> [PoolOp; 2] {
    [PoolOp::Deposit("alice.near".into()), PoolOp::Deposit("bob.near".into())]
}

/// `an_overlapped_deposit_has_no_serial_match`: the race a finding, three
/// in flight at depth 3, and a second books adding its readings.
#[test]
fn a_pool_race_reports_the_same_with_and_without_the_memo() {
    let alphabet = deposits();
    let (played, unmemoized, _) = same_report(pool, |h| h.enumerate(&alphabet).depth(3).run());
    assert!(played * 4 < unmemoized, "{played} of {unmemoized} played");
    let more = || Books::new().liability("nothing", |_: &Sim| BTreeMap::new());
    same_report(|| pool().books(more()), |h| h.enumerate(&alphabet).depth(2).run());
}

/// Hit evidence: at depth 2 over a pair, every overlap starts at the
/// fixture, and each pair goes in at each block of the first's flight,
/// so the same serial orders repeat. Played once each: [a, a], [a, b],
/// [b, a], [b, b], the pairs (a, b) and (b, a) sharing theirs.
#[test]
fn a_depth_2_pair_plays_each_serial_order_once() {
    let alphabet = deposits();
    let (played, unmemoized, _) = same_report(pool, |h| h.enumerate(&alphabet).depth(2).run());
    // Without it, each of the 4 pairs at each of 4 offsets, 2 orders each.
    assert_eq!((played, unmemoized), (4, 4 * 4 * 2));
}

// ---- flight_lock: what lands inside a start's flight ----

const LOCK: &str = "lock.near";

fn lock_with(epoch_length: u64) -> Sim {
    let mut sim = Sim::new();
    sim.config.epoch_length = epoch_length;
    sim.create_account("alice.near", Balance::from_near(100));
    sim.create_account("bob.near", Balance::from_near(100));
    sim.create_account(LOCK, Balance::from_near(10));
    sim.deploy(LOCK, wat::parse_str(include_str!("../../tests/res/flight_lock.wat")).unwrap());
    sim
}

/// `tests/explore_enumerate.rs`' `LockOp`, `WorkOp` and `Probe` as one.
#[derive(Clone, Debug, Serialize, Deserialize)]
enum LockOp {
    Start,
    Sync,
    Work { scratch: bool },
    PayBack,
    Echo,
    SyncWhileBusy,
    SyncByWhoever,
    StartAgain,
    TrySync { plain: bool },
    Trap(u64),
}

impl Op for LockOp {
    fn submit(&self, sim: &mut Sim) -> Result<(), Skip> {
        let bob = |sim: &mut Sim, method: &str| {
            sim.call("bob.near", LOCK, method).submit();
        };
        match self {
            LockOp::Start => {
                sim.call("alice.near", LOCK, "start").submit();
            }
            LockOp::StartAgain => {
                sim.call(LOCK, LOCK, "start").submit();
            }
            LockOp::Sync => bob(sim, "sync"),
            LockOp::TrySync { .. } => bob(sim, "try_sync"),
            LockOp::Work { scratch } => {
                sim.call("bob.near", LOCK, "work").args(vec![u8::from(*scratch)]).submit();
            }
            LockOp::PayBack => {
                let call = sim.call("bob.near", LOCK, "work").args(vec![2]);
                call.deposit(Balance::from_near(1)).submit();
            }
            LockOp::Echo => {
                let busy = !sim.is_idle();
                sim.call("bob.near", LOCK, "work").args(vec![u8::from(busy)]).submit();
            }
            LockOp::SyncWhileBusy => {
                if sim.state(LOCK, b"*busy").is_none() {
                    return Err("nothing in flight".into());
                }
                bob(sim, "sync");
            }
            LockOp::SyncByWhoever => {
                let caller = if sim.is_idle() { "bob.near" } else { "alice.near" };
                sim.call(caller, LOCK, "sync").submit();
            }
            LockOp::Trap(at) => {
                sim.call("bob.near", LOCK, "trap").args(at.to_le_bytes()).submit();
            }
        }
        Ok(())
    }

    fn refused(&self, outcomes: &[TxOutcome]) -> bool {
        matches!(self, LockOp::TrySync { plain: false })
            && outcomes.iter().any(|o| o.value() == Some(&b"false"[..]))
    }
}

/// The work done, and the lock's NEAR.
fn lock_on(sim: Sim) -> Harness<LockOp> {
    let books = Books::new()
        .liability("work done", |sim: &Sim| {
            let v = sim.view_raw(LOCK, "worked", vec![]).result.unwrap();
            BTreeMap::from([(LOCK.to_owned(), u64::from_le_bytes(v.try_into().unwrap()).into())])
        })
        .near("lock NEAR", LOCK);
    Harness::new(sim).books(books)
}

/// Each lock test of `tests/explore_enumerate.rs`' SERIALIZABLE section
/// as an enumeration of its ops: refused only under interleaving, the
/// drop rule, orders apart only in gas, an order that skips, ops sending
/// other arguments or from another caller serially, and an op's own
/// refusal; on mainnet-long epochs.
#[test]
fn the_lock_s_serial_verdicts_report_the_same_with_and_without_the_memo() {
    use LockOp::*;
    let lock = || lock_on(lock_with(43_200));
    let alphabets: [&[LockOp]; 4] = [
        &[Start, Sync, SyncWhileBusy],
        &[Start, Echo, SyncByWhoever],
        &[Start, Work { scratch: false }, Work { scratch: true }, PayBack],
        &[Start, StartAgain, TrySync { plain: false }, TrySync { plain: true }],
    ];
    for alphabet in alphabets {
        let (played, unmemoized, _) = same_report(lock, |h| h.enumerate(alphabet).depth(2).run());
        assert!(played < unmemoized, "{alphabet:?}: {played} of {unmemoized} played");
    }
}

/// `the_closest_order_says_which_ops_it_left_out`: three in flight, a
/// refused work left out of the closest order.
#[test]
fn three_in_flight_report_the_same_with_and_without_the_memo() {
    use LockOp::*;
    let alphabet = [Start, Work { scratch: false }, Work { scratch: true }];
    let lock = || lock_on(lock_with(43_200));
    same_report(lock, |h| h.enumerate(&alphabet).depth(3).cases(200).run());
}

/// The memo is an enumeration's alone: a scan, a search and
/// `Harness::run` play each case's serial orders, two for a pair, though
/// their cases share them (one pair at two offsets of its flight, and a
/// search's one plan drawn each time). A search runs its first case
/// twice. The pair fails no check: settling a finding replays its case.
#[test]
fn a_scan_a_search_and_a_run_play_every_serial_order() {
    use LockOp::*;
    let at = |k: u16| Plan::new().op(Start).blocks(k).op(StartAgain);
    let lock = || lock_on(lock_with(43_200));
    // The serial orders `f` played, and two for each case it ran.
    let played = |f: &dyn Fn() -> u32| {
        let before = PLAYED.get();
        let cases = f();
        (PLAYED.get() - before, 2 * cases as usize)
    };
    let h = lock();
    let runs = played(&|| {
        let _ = h.run(&at(0));
        let _ = h.run(&at(1));
        2
    });
    let scan = played(&|| lock().scan(&[at(0), at(1)]).faults(Faults::none()).run().report().cases);
    let search = played(&|| lock().search(Just(at(0))).cases(3).seed(1).run().report().cases + 1);
    assert_eq!([runs, scan, search], [(4, 4), (4, 4), (8, 8)]);
}

/// On 10-block epochs: an epoch boundary at each block of each flight
/// (`a_serial_order_skips_to_the_case_s_epoch`'s skip, each order to its
/// case's epoch), and `a_state_seen_at_rest_is_still_extended_inside_its_flight`'s
/// storage key, which merges a start that released its lock.
#[test]
fn epoch_boundaries_and_a_coarse_dedup_key_report_the_same() {
    use LockOp::*;
    let lock = || lock_on(lock_with(10));
    same_report(lock, |h| h.enumerate(&[Start, Sync]).depth(2).run());
    let storage = |sim: &Sim| {
        let state = format!("{:?}", sim.view_state(LOCK, b""));
        state.bytes().fold(0u64, |h, b| h.wrapping_mul(31).wrapping_add(u64::from(b)))
    };
    same_report(lock, |h| h.enumerate(&[Start, Sync]).depth(2).key(storage).run());
}

/// The coarse key is wrong. On 10-block epochs a pair's epoch boundary
/// variants submit its first op at several heights of one epoch: starts
/// the dedup key calls equal, as it leaves the height out. `trap` reads
/// the height: placed to spring at one variant's start, it fails there,
/// in the case and in the serial order that puts it first. Keyed by the
/// dedup key, that variant takes an earlier variant's orders, where it
/// never fails: refused only under interleaving, a finding the memo-off
/// run doesn't have.
#[test]
fn starts_the_dedup_key_merges_keep_their_own_serial_orders() {
    let start = |k: i8| {
        let mut w = lock_with(10);
        let n = Gap::EpochEdge(-k).blocks_on(&w).expect("an edge is a count of blocks");
        (0..n).for_each(|_| w.step());
        w
    };
    let (first, other) = (start(0), start(2));
    assert_eq!(default_key(&first, None), default_key(&other, None));
    assert_ne!(first.height(), other.height());
    // The height the trap runs at, submitted at the second start.
    let mut plan = Plan::new().op(LockOp::Trap(0));
    plan.start = Some(Gap::EpochEdge(-2));
    let run = lock_on(lock_with(10)).run(&plan);
    let trap = run.outcomes().iter().flat_map(|o| &o.receipts).find(|r| r.method_names == ["trap"]);
    let alphabet = [LockOp::Trap(trap.unwrap().block_height), LockOp::Sync];
    let lock = || lock_on(lock_with(10));
    same_report(lock, |h| h.enumerate(&alphabet).depth(2).run());
}

/// A later start keeps the gaps before it. At depth 3 on 10-block
/// epochs, the epoch boundary variants of a trap submitted at rest after
/// another put it in at several heights, which only the gap between the
/// two fixes. Placed to spring at one of them, as above: keyed without
/// that gap (or without the epoch the orders skip to), the variant takes
/// another's serial orders.
#[test]
fn a_later_start_keeps_the_gaps_before_it() {
    let mut plan = Plan::new().op(LockOp::Trap(0)).op(LockOp::Trap(0));
    plan.steps[0].then = Gap::EpochEdge(-2);
    let run = lock_on(lock_with(10)).run(&plan);
    let second = run.outcomes()[1].receipts.iter().find(|r| r.method_names == ["trap"]);
    let alphabet = [LockOp::Trap(second.unwrap().block_height)];
    let lock = || lock_on(lock_with(10));
    same_report(lock, |h| h.enumerate(&alphabet).depth(3).run());
}

/// A fault variant keeps its own serial orders: a step's injections are
/// in the key. Under the tolerable faults, a start whose `finish` is
/// held a block has a flight a block longer, and its serial order
/// [start, trap] runs the trap a block later. Placed to spring where the
/// plain pair's [start, trap] runs it, the trap springs in a case only
/// in that extra block, where no held order springs it: refused only
/// under interleaving. Keyed without the hold, the held start takes the
/// plain one's orders, where the trap springs, and the memo-on run loses
/// the finding.
#[test]
fn a_fault_variant_keeps_its_own_serial_orders() {
    use LockOp::*;
    // Where the plain pair's serial order [start, trap] runs the trap.
    let run = lock_on(lock_with(43_200)).run(&Plan::new().op(Start).op(Trap(0)));
    let trap = run.outcomes().iter().flat_map(|o| &o.receipts).find(|r| r.method_names == ["trap"]);
    let alphabet = [Start, Trap(trap.unwrap().block_height)];
    let lock = || lock_on(lock_with(43_200));
    let (played, unmemoized, _) =
        same_report(lock, |h| h.enumerate(&alphabet).depth(2).faults(Faults::tolerable()).run());
    assert!(played < unmemoized, "{played} of {unmemoized} played");
}

// ---- yield_pay and claim_queue ----

const YIELD: &str = "yield.near";

#[derive(Clone, Debug, Serialize, Deserialize)]
enum YieldOp {
    Request,
    Respond,
}

impl Op for YieldOp {
    fn submit(&self, sim: &mut Sim) -> Result<(), Skip> {
        let method = match self {
            YieldOp::Request => "request",
            YieldOp::Respond => "respond",
        };
        sim.call("alice.near", YIELD, method).submit();
        Ok(())
    }
}

/// `a_respond_to_a_parked_yield_has_its_serial_order`: a respond in
/// flight with a parked request.
#[test]
fn a_parked_yield_reports_the_same_with_and_without_the_memo() {
    let harness = || {
        let mut sim = Sim::new();
        sim.create_account("alice.near", Balance::from_near(10));
        sim.create_account(YIELD, Balance::from_near(10));
        sim.deploy(YIELD, wat::parse_str(include_str!("../../tests/res/yield_pay.wat")).unwrap());
        let books = Books::new()
            .liability("paid", |sim: &Sim| {
                let paid = u128::from(sim.state(YIELD, b"paid").is_some());
                BTreeMap::from([("alice.near".to_owned(), paid)])
            })
            .near("yield NEAR", YIELD);
        Harness::new(sim).books(books)
    };
    // A parked yield's flight is its timeout: cut short.
    let alphabet = [YieldOp::Request, YieldOp::Respond];
    same_report(harness, |h| h.enumerate(&alphabet).depth(2).cases(40).run());
}

const QUEUE: &str = "queue.near";

#[derive(Clone, Debug, Serialize, Deserialize)]
enum QueueOp {
    Request,
    Redeem(u64),
}

impl Op for QueueOp {
    fn submit(&self, sim: &mut Sim) -> Result<(), Skip> {
        match self {
            QueueOp::Request => sim.call("alice.near", QUEUE, "request").submit(),
            QueueOp::Redeem(id) => {
                sim.call("alice.near", QUEUE, "redeem").args(id.to_le_bytes()).submit()
            }
        };
        Ok(())
    }
}

fn queued(sim: &Sim) -> Vec<u64> {
    let count = sim.view_raw(QUEUE, "count", vec![]).result.unwrap();
    let n = u64::from_le_bytes(count.try_into().unwrap());
    let key = |id: u64| [&b"q"[..], &id.to_le_bytes()].concat();
    (1..=n).filter(|&id| sim.state(QUEUE, &key(id)).is_some()).collect()
}

/// claim_queue: requests queue claims that a redeem pays.
fn queue() -> Harness<QueueOp> {
    let mut sim = Sim::new();
    sim.create_account("alice.near", Balance::from_near(10));
    sim.create_account(QUEUE, Balance::from_near(10));
    sim.deploy(QUEUE, wat::parse_str(include_str!("../../tests/res/claim_queue.wat")).unwrap());
    let claims =
        |sim: &Sim| queued(sim).into_iter().map(|id| (format!("claim #{id}"), 1)).collect();
    let open = |sim: &Sim| {
        let claim = |id| Claim { holder: format!("claim #{id}"), amount: 1, matures: None };
        queued(sim).into_iter().map(claim).collect()
    };
    let books = Books::new()
        .liability("claims", claims)
        .asset("queued", |sim: &Sim| queued(sim).len() as u128)
        .backs("claims", "queued")
        .claim("claims", "queued", open, |c: &Claim| {
            let id = c.holder.trim_start_matches("claim #").parse().unwrap();
            Plan::new().op(QueueOp::Redeem(id))
        });
    Harness::new(sim).books(books)
}

/// `liveness_is_judged_after_each_step_and_memoized_by_state`: the end
/// checks' memo by state beside the serial orders' by plan.
#[test]
fn a_claim_queue_reports_the_same_with_and_without_the_memo() {
    let alphabet = [QueueOp::Request, QueueOp::Redeem(1)];
    same_report(queue, |h| h.enumerate(&alphabet).depth(2).run());
}

// ---- the fixture, judged once per enumeration ----

/// An enumeration judges the checks on the fixture once for its cases,
/// and once more in its first case's run-again guard, which has no memo;
/// each case judged it before. `Harness::run`, a scan and a search judge
/// it in each case, as before.
#[test]
fn an_enumeration_judges_the_fixture_once() {
    use QueueOp::*;
    let alphabet = [Request, Redeem(1)];
    // The fixture's judgments while `f` runs, and the cases it says it ran.
    let judged = |f: &dyn Fn() -> u32| {
        let before = FIXTURE_JUDGED.get();
        let cases = f();
        (FIXTURE_JUDGED.get() - before, cases as usize)
    };
    let enumerate = || queue().enumerate(&alphabet).depth(2).run().report().cases;
    let once = judged(&enumerate);
    let each = {
        struct Reset;
        impl Drop for Reset {
            fn drop(&mut self) {
                FIXTURE_EACH_CASE.set(false);
            }
        }
        let _reset = Reset;
        FIXTURE_EACH_CASE.set(true);
        judged(&enumerate)
    };
    assert_eq!([once, each], [(2, 16), (17, 16)]);
    let at = |k: u16| Plan::new().op(Request).blocks(k).op(Redeem(1));
    let h = queue();
    let runs = judged(&|| {
        let _ = h.run(&at(0));
        let _ = h.run(&at(1));
        2
    });
    let scan =
        judged(&|| queue().scan(&[at(0), at(1)]).faults(Faults::none()).run().report().cases);
    // A search runs its first case twice, a guard as an enumeration's.
    let search = judged(&|| queue().search(Just(at(0))).cases(3).seed(1).run().report().cases);
    assert_eq!([runs, scan, search], [(2, 2), (2, 2), (4, 3)]);
}

/// Planted keys ([`Plan::planted`](super::Plan)) make another world than
/// the fixture: a case with them judges its own, memo or not.
#[test]
fn a_case_with_planted_keys_judges_its_own_world() {
    let unplanted = |sim: &Sim| match sim.state(QUEUE, b"planted") {
        None => Ok(()),
        Some(_) => Err("planted"),
    };
    let mut h = queue().always("nothing planted", unplanted);
    h.memo = Some(std::rc::Rc::new(super::check::Memo::new(std::rc::Rc::new(|_: &Sim| 0))));
    let plain = Plan::new().op(QueueOp::Request);
    let mut planted = plain.clone();
    planted.planted.push((QUEUE.parse().unwrap(), b"planted".to_vec()));
    assert!(!h.run_case(&plain).on_fixture);
    let run = h.run_case(&planted);
    assert!(run.on_fixture, "{run:?}");
    assert!(run.reason.as_deref().is_some_and(|r| r.contains("nothing planted")), "{run:?}");
}

/// A check failing on the fixture stops the enumeration at its first
/// case, as before, whether it is judged at every block or at the end.
/// One not judged there, or passing there and failing later, is credited
/// to every case: its counts, and a finding's last pass, as before.
#[test]
fn the_fixture_s_judgments_report_as_before() {
    use QueueOp::*;
    let alphabet = [Request, Redeem(1)];
    let enumerate = |h: &Harness<QueueOp>| h.enumerate(&alphabet).depth(2).run();
    let broken = |_: &Sim| Err::<(), _>("broken");
    // After a check that holds, so each is named by its own index.
    let holds = || queue().always("holds", |_: &Sim| Ok::<(), Fail>(()));
    let always = || holds().always("never holds", broken);
    let at_end = || holds().at_end("never holds at the end", broken);
    for (h, name) in [
        (&always as &dyn Fn() -> Harness<QueueOp>, "never holds"),
        (&at_end, "never holds at the end"),
    ] {
        assert_eq!(same_report(h, enumerate).2, (1, 1));
        let report = enumerate(&h()).report();
        assert_eq!(report.cases, 1);
        let stop = format!("the check {name:?} fails on the fixture: broken");
        assert!(report.to_string().contains(&stop), "{report}");
    }
    let credited = || {
        queue()
            .at_rest("something queued", |sim: &Sim| match queued(sim).is_empty() {
                true => Err(Fail::not_judged("nothing queued")),
                false => Ok(()),
            })
            .at_rest("nothing queued", |sim: &Sim| match queued(sim).is_empty() {
                true => Ok(()),
                false => Err("queued"),
            })
    };
    let (_, _, judged) = same_report(credited, enumerate);
    assert_eq!(judged.0, 2);
    let report = enumerate(&credited()).report().to_string();
    assert!(report.contains("something queued not judged"), "{report}");
    assert!(report.contains("nothing queued"), "{report}");
}

// ---- the token vault, cut down ----

const TOKEN: &str = "token.near";
const VAULT: &str = "vault.near";

fn res(name: &str) -> Vec<u8> {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/res").join(name);
    std::fs::read(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

fn view(sim: &Sim, id: &str, method: &str, args: serde_json::Value) -> u128 {
    sim.view(id, method, args).u128()
}

fn ft(sim: &Sim, who: &str) -> u128 {
    view(sim, TOKEN, "ft_balance_of", json!({ "account_id": who }))
}

fn shares(sim: &Sim, who: &str) -> u128 {
    view(sim, VAULT, "shares_of", json!({ "account_id": who }))
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
enum User {
    Alice,
    Bob,
}

impl User {
    fn id(self) -> &'static str {
        match self {
            User::Alice => "alice.near",
            User::Bob => "bob.near",
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
enum VaultOp {
    Deposit {
        who: User,
        amount: Amount,
    },
    Withdraw {
        who: User,
        amount: Amount,
    },
    Claim {
        who: User,
    },
    /// Two deposits of `each`, two transactions in one op.
    TwoDeposits {
        who: User,
        each: u128,
    },
}

impl Op for VaultOp {
    fn submit(&self, sim: &mut Sim) -> Result<(), Skip> {
        match *self {
            VaultOp::Deposit { who, amount } => {
                let n = amount.of(ft(sim, who.id()))?;
                sim.call(who.id(), TOKEN, "ft_transfer_call")
                    .args_json(json!({ "receiver_id": VAULT, "amount": n.to_string(), "msg": "" }))
                    .deposit(Balance::from_yoctonear(1))
                    .submit();
            }
            VaultOp::Withdraw { who, amount } => {
                let n = amount.of(shares(sim, who.id()))?;
                sim.call(who.id(), VAULT, "withdraw")
                    .args_json(json!({ "amount": n.to_string() }))
                    .deposit(Balance::from_yoctonear(1))
                    .submit();
            }
            VaultOp::Claim { who } => {
                sim.call(who.id(), VAULT, "claim_rewards").submit();
            }
            VaultOp::TwoDeposits { who, each } => {
                for _ in 0..2 {
                    let args =
                        json!({ "receiver_id": VAULT, "amount": each.to_string(), "msg": "" });
                    (sim.call(who.id(), TOKEN, "ft_transfer_call").args_json(args))
                        .deposit(Balance::from_yoctonear(1))
                        .submit();
                }
            }
        }
        Ok(())
    }
}

/// alice and bob with 1,000 tokens each, the token and the vault watched;
/// `tests/explore_vault`'s checks and books.
fn vault() -> Harness<VaultOp> {
    let world = Fixture::new()
        .account("alice.near", Balance::from_near(10))
        .account("bob.near", Balance::from_near(10))
        .contract(TOKEN, res("fungible_token.wasm"))
        .init("new_default_meta")
        .args_json(json!({ "owner_id": TOKEN, "total_supply": "2000" }))
        .contract(VAULT, res("vault.wasm"))
        .init("new")
        .args_json(json!({ "token_id": TOKEN }))
        .ft_register(TOKEN, [VAULT, "alice.near", "bob.near"])
        .ft_transfer(TOKEN, TOKEN, "alice.near", 1000)
        .ft_transfer(TOKEN, TOKEN, "bob.near", 1000)
        .token(Token::nep141(TOKEN))
        .token(Token::view(VAULT, "shares_of").supply("total_shares"))
        .build();
    let total = |sim: &Sim| view(sim, VAULT, "total_shares", json!({}));
    let rewards = |sim: &Sim| view(sim, VAULT, "total_rewards", json!({}));
    let books = Books::new()
        .liability("vault shares", move |sim: &Sim| {
            BTreeMap::from([("all".into(), total(sim) - rewards(sim))])
        })
        .asset("vault tokens", |sim: &Sim| ft(sim, VAULT))
        .backs("vault shares", "vault tokens");
    // What this op's own receipts moved: shares `out` of `who` against
    // tokens in, or the other way round.
    let moved = |d: &OpDone<VaultOp>, who: User, out: &str, into: &str| -> Result<(), Fail> {
        let given = -d.effects().token_net(out, who.id())?;
        let got = d.effects().token_net(into, who.id())?;
        (given == got).then_some(()).ok_or(Fail::new(format!("gave {given}, got {got}")))
    };
    let delivers = move |d: &OpDone<VaultOp>| match *d.op() {
        VaultOp::Withdraw { who, .. } => moved(d, who, VAULT, TOKEN),
        _ => Ok(()),
    };
    let credits = move |d: &OpDone<VaultOp>| match *d.op() {
        VaultOp::Deposit { who, .. } => moved(d, who, TOKEN, VAULT),
        _ => Ok(()),
    };
    let is = |withdraw: bool| {
        move |op: &VaultOp| match op {
            VaultOp::Withdraw { .. } => withdraw,
            VaultOp::Deposit { .. } => !withdraw,
            _ => false,
        }
    };
    Harness::new(world)
        .always("total_shares is the sum of shares", move |sim: &Sim| {
            let sum = shares(sim, "alice.near") + shares(sim, "bob.near");
            (total(sim) == sum).then_some(()).ok_or(format!("{} != {sum}", total(sim)))
        })
        .at_rest("the vault holds what it owes", move |sim: &Sim| {
            let owed = total(sim).checked_sub(rewards(sim));
            (owed == Some(ft(sim, VAULT))).then_some(()).ok_or(format!("owes {owed:?}"))
        })
        .per_op_for(is(true), "withdraw delivers what it debits", delivers)
        .per_op_for(is(false), "deposit credits what it takes", credits)
        .books(books)
}

/// The vault's 15 ops at depth 2, cut to 4: deposits, one a whole
/// balance (what it sends depends on the state it meets) and one two
/// transactions, and a withdraw.
#[test]
fn the_vault_at_depth_2_reports_the_same_with_and_without_the_memo() {
    use {Amount::*, User::*};
    let alphabet = [
        VaultOp::Deposit { who: Alice, amount: Exact(100) },
        VaultOp::Deposit { who: Bob, amount: All },
        VaultOp::Withdraw { who: Alice, amount: Exact(40) },
        VaultOp::TwoDeposits { who: Bob, each: 10 },
    ];
    let (played, unmemoized, _) = same_report(vault, |h| h.enumerate(&alphabet).depth(2).run());
    assert!(played * 2 < unmemoized, "{played} of {unmemoized} played");
}

/// The vault's 5 ops at depth 3, cut to 3 ops and 100 cases.
#[test]
fn the_vault_at_depth_3_reports_the_same_with_and_without_the_memo() {
    use {Amount::*, User::*};
    let alphabet = [
        VaultOp::Deposit { who: Alice, amount: Exact(100) },
        VaultOp::Withdraw { who: Alice, amount: Exact(40) },
        VaultOp::Claim { who: Alice },
    ];
    let (played, unmemoized, _) =
        same_report(vault, |h| h.enumerate(&alphabet).depth(3).cases(100).run());
    assert!(played * 2 < unmemoized, "{played} of {unmemoized} played");
}
