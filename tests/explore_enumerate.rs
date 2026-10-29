//! `Harness::enumerate`, the `SERIALIZABLE` built-in and `Faults::reorder`,
//! over generic demos: `tests/res/pool_sync.wat`, `staking_pool.wat`,
//! `flight_lock.wat` and `purse.wat`.
//!
//! Verification boundary: what gets scheduled (each next op at each block
//! offset inside the previous op's measured flight, an epoch boundary at
//! each offset), what the dedup key merges, and the serial-order oracle,
//! each over actual runs of the simulator. A bounded enumeration finds
//! bugs and proves nothing.

mod common;
use common::*;
use near_contract_sim::explore::{Gap, Harness, Op, Plan, Skip, Verdict};
use near_contract_sim::{Balance, KeyType, SecretKey, Sim};
use serde::{Deserialize, Serialize};

const POOL: &str = "pool.near";
const LOCK: &str = "lock.near";

/// A staking pool on 10-block epochs, alice's 100 NEAR staked in it.
fn staking() -> Sim {
    let mut sim = Sim::new();
    sim.config.epoch_length = 10;
    sim.create_account("alice.near", Balance::from_near(1000));
    sim.create_account(POOL, Balance::from_near(2));
    sim.deploy(POOL, staking_pool_wasm());
    let key = SecretKey::from_seed(KeyType::ED25519, POOL).public_key();
    sim.call(POOL, POOL, "new").args(borsh::to_vec(&key).unwrap()).transact().assert_success();
    sim.call("alice.near", POOL, "deposit_and_stake")
        .deposit(Balance::from_near(100))
        .transact()
        .assert_success();
    sim
}

#[derive(Clone, Debug, Serialize, Deserialize)]
enum StakeOp {
    /// alice stakes this many NEAR more.
    Stake(u32),
    /// alice unstakes everything.
    Unstake,
    /// The seat price moves to this many NEAR: no transaction, a knob.
    MinStake(u32),
}

impl Op for StakeOp {
    fn submit(&self, sim: &mut Sim) -> Result<(), Skip> {
        match self {
            StakeOp::Stake(n) => {
                let deposit = Balance::from_near((*n).into());
                sim.call("alice.near", POOL, "deposit_and_stake").deposit(deposit).submit();
            }
            StakeOp::Unstake => {
                sim.call("alice.near", POOL, "unstake").submit();
            }
            StakeOp::MinStake(n) => sim.config.minimum_stake = Balance::from_near((*n).into()),
        }
        Ok(())
    }
}

fn locked(sim: &Sim) -> Balance {
    sim.account(POOL).unwrap().locked
}

/// Verified first (the brief): an op that submits no transaction, only
/// sets `sim.config.minimum_stake`, runs inside a plan, mid-flight of a
/// stake, and changes its outcome: placed before the pool's restake
/// runs, the restake's `Stake` fails and the pool's lock stays at 100.
#[test]
fn an_op_with_no_transaction_runs_mid_flight_and_moves_the_outcome() {
    let h = Harness::new(staking());
    let early = Plan::new().op(StakeOp::Stake(10)).blocks(0).op(StakeOp::MinStake(1_000));
    let run = h.run(&early);
    assert_ne!(run.verdict(), Verdict::HarnessError, "{run}");
    let world: &Sim = run.world().unwrap();
    assert_eq!(locked(world), Balance::from_near(100), "the restake failed: {run}");
    let late = Plan::new().op(StakeOp::Stake(10)).op(StakeOp::MinStake(1_000));
    let run = h.run(&late);
    assert_ne!(run.verdict(), Verdict::HarnessError, "{run}");
    let world: &Sim = run.world().unwrap();
    assert_eq!(locked(world), Balance::from_near(110), "{run}");
}

fn lock() -> Sim {
    lock_with(10)
}

fn lock_with(epoch_length: u64) -> Sim {
    let mut sim = Sim::new();
    sim.config.epoch_length = epoch_length;
    sim.create_account("alice.near", Balance::from_near(100));
    sim.create_account("bob.near", Balance::from_near(100));
    let wasm = wat::parse_file(manifest_dir().join("tests/res/flight_lock.wat")).unwrap();
    sim.create_account(LOCK, Balance::from_near(10));
    sim.deploy(LOCK, wasm);
    sim
}

#[derive(Clone, Debug, Serialize, Deserialize)]
enum LockOp {
    Start,
    Sync,
    EpochBegin,
    /// bob syncs, then its submit panics: a harness error, the world moved.
    Boom,
}

impl Op for LockOp {
    fn submit(&self, sim: &mut Sim) -> Result<(), Skip> {
        let (who, method) = match self {
            LockOp::Start => ("alice.near", "start"),
            LockOp::Sync | LockOp::Boom => ("bob.near", "sync"),
            LockOp::EpochBegin => ("alice.near", "epoch_begin"),
        };
        sim.call(who, LOCK, method).submit();
        assert!(!matches!(self, LockOp::Boom), "boom");
        Ok(())
    }
}

/// Verified first: `Gap::EpochEdge(-k)` before an op puts
/// the epoch boundary `k` blocks into its flight. `epoch_begin`'s two
/// receipts run in consecutive blocks after it converts; a boundary
/// between them fails `epoch_end`, anywhere else it doesn't.
#[test]
fn an_epoch_edge_before_an_op_lands_inside_its_flight() {
    let h = Harness::new(lock());
    let failed = |k: i8| {
        let mut plan = Plan::new().op(LockOp::EpochBegin).refused();
        plan.start = Some(Gap::EpochEdge(k));
        let run = h.run(&plan);
        assert_ne!(run.verdict(), Verdict::HarnessError, "{run}");
        let out = &run.outcomes()[0];
        out.receipts.iter().any(|r| r.method_names == ["epoch_end"] && r.failure().is_some())
    };
    let fails: Vec<i8> = (-4..=1).filter(|&k| failed(k)).collect();
    assert_eq!(fails.len(), 1, "one offset puts the boundary between the receipts: {fails:?}");
}

// ---- SERIALIZABLE ----

use near_contract_sim::explore::{Books, builtin};
use std::collections::{BTreeMap, BTreeSet};

fn view_u128(sim: &Sim, method: &str) -> u128 {
    let bytes = sim.view_raw(POOL, method, vec![]).result.unwrap();
    u128::from_le_bytes(bytes.try_into().unwrap())
}

/// tests/res/pool_sync.wat, alice and bob each with 100 NEAR.
fn pool_sync() -> Sim {
    let wat = std::fs::read_to_string(manifest_dir().join("tests/res/pool_sync.wat")).unwrap();
    let mut sim = Sim::new();
    sim.create_account("alice.near", Balance::from_near(100));
    sim.create_account("bob.near", Balance::from_near(100));
    sim.create_account(POOL, Balance::from_near(10));
    sim.deploy(POOL, wat::parse_str(&wat).unwrap());
    sim.call("alice.near", POOL, "init").transact().assert_success();
    sim
}

#[derive(Clone, Debug, Serialize, Deserialize)]
enum PoolOp {
    /// `who` deposits 1 NEAR, after a sync of the pool's rewards.
    Deposit(String),
}

impl Op for PoolOp {
    fn submit(&self, sim: &mut Sim) -> Result<(), Skip> {
        if let Ok(n) = std::env::var("ENUM_ABORT_AFTER") {
            let at = SUBMITS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if at >= n.parse().unwrap() {
                std::process::abort();
            }
        }
        let PoolOp::Deposit(who) = self;
        sim.call(who, POOL, "deposit").deposit(Balance::from_near(1)).submit();
        Ok(())
    }
}

/// The pool's books: each depositor's credit, and what it booked beyond
/// them since the fixture ("unsynced rewards" it counts as its own).
fn pool_books(fixture: &Sim) -> Books<PoolOp> {
    let at_start = view_u128(fixture, "booked");
    Books::new().liability("credits", move |sim: &Sim| {
        let mut credits: BTreeMap<String, u128> = BTreeMap::new();
        for (k, v) in sim.view_state(POOL, b"c") {
            let who = String::from_utf8(k[1..].to_vec()).unwrap();
            credits.insert(who, u128::from_le_bytes(v.try_into().unwrap()));
        }
        let sum: u128 = credits.values().sum();
        let surplus = view_u128(sim, "booked").saturating_sub(at_start + sum);
        credits.insert("booked beyond the credits".into(), surplus);
        credits
    })
}

fn deposit(who: &str) -> PoolOp {
    PoolOp::Deposit(who.into())
}

/// Two deposits in flight together book the first twice, an outcome no
/// serial order of the two gives: a finding of the serial-order
/// oracle, naming the closest order. Settled apart, the same two
/// deposits are never in flight together: nothing to judge (idle).
#[test]
fn an_overlapped_deposit_has_no_serial_match() {
    let fixture = pool_sync();
    let h = Harness::new(fixture.clone()).books(pool_books(&fixture));
    let race = Plan::new().op(deposit("alice.near")).blocks(0).op(deposit("bob.near"));
    let run = h.run(&race);
    let f = run.assert_found(builtin::SERIALIZABLE, "world");
    assert!(f.message.contains("no serial order of them gives this outcome"), "{run}");
    assert!(f.message.contains("booked beyond the credits"), "{run}");
    // A second books adds its readings to the first's.
    let more = Books::new().liability("nothing", |_: &Sim| BTreeMap::new());
    let two = Harness::new(fixture.clone()).books(pool_books(&fixture)).books(more);
    let run = two.run(&race);
    let f = run.assert_found(builtin::SERIALIZABLE, "world");
    assert!(f.message.contains("booked beyond the credits"), "{run}");
    let apart = Plan::new().op(deposit("alice.near")).op(deposit("bob.near"));
    let run = h.run(&apart);
    run.assert_clean();
    let judged = run.judged().iter().find(|j| j.check == builtin::SERIALIZABLE).unwrap();
    assert_eq!((judged.ok, judged.idle), (0, 1), "{run}");
    // Idle every time: no to-do item, as settled cases order nothing.
    let audit = h.scan(&[apart]).run();
    let todo = audit.todo();
    let ids: Vec<&str> = todo.iter().map(|t| t.id.as_str()).collect();
    assert!(ids.iter().all(|id| !id.contains(builtin::SERIALIZABLE)), "{ids:?}");
    // Three in flight together are judged; four are more than the serial
    // orders tried.
    let (a, b) = (deposit("alice.near"), deposit("bob.near"));
    let three = Plan::new().op(a.clone()).blocks(0).op(b.clone()).blocks(0).op(a.clone());
    let run = h.run(&three);
    let f = run.assert_found(builtin::SERIALIZABLE, "world");
    assert!(f.message.contains("no serial order of them gives this outcome"), "{run}");
    assert!(!f.message.contains("left out"), "{run}");
    let four = three.blocks(0).op(b);
    let run = h.run(&four);
    let j = serializable(&run);
    assert_eq!((j.ok, j.failed, j.unjudged), (0, 0, 1), "{run}");
    assert!(j.why.as_deref().is_some_and(|w| w.starts_with("4 ops in flight together")), "{run}");
}

#[derive(Clone, Debug, Serialize, Deserialize)]
enum WorkOp {
    Start,
    /// bob syncs: refused while a start is in flight.
    Sync,
    /// bob works: with a start in flight, refused moving nothing
    /// (`scratch` false) or after writing a key no reader reads; else
    /// counted, and the transaction fails after.
    Work {
        scratch: bool,
    },
    /// bob works with 1 NEAR attached: with a start in flight, refused
    /// after paying it back; else counted, and kept, and the transaction
    /// fails after.
    PayBack,
    /// bob works with input 1 while anything is in flight, else 0: what
    /// it sends depends on the state it meets.
    Echo,
    /// bob syncs, but only while a start is in flight: it skips otherwise.
    SyncWhileBusy,
    /// alice syncs while anything is in flight, else bob: who calls
    /// depends on the state it meets.
    SyncByWhoever,
    /// The lock starts itself: it pays the gas.
    StartAgain,
    /// bob tries a sync: `false` while a start is in flight, no panic.
    /// `refused` reads that `false` unless `plain`.
    TrySync {
        plain: bool,
    },
}

impl Op for WorkOp {
    fn submit(&self, sim: &mut Sim) -> Result<(), Skip> {
        match self {
            WorkOp::Start => drop(sim.call("alice.near", LOCK, "start").submit()),
            WorkOp::StartAgain => drop(sim.call(LOCK, LOCK, "start").submit()),
            WorkOp::Sync => drop(sim.call("bob.near", LOCK, "sync").submit()),
            WorkOp::TrySync { .. } => drop(sim.call("bob.near", LOCK, "try_sync").submit()),
            WorkOp::Work { scratch } => {
                sim.call("bob.near", LOCK, "work").args(vec![u8::from(*scratch)]).submit();
            }
            WorkOp::PayBack => {
                let call = sim.call("bob.near", LOCK, "work").args(vec![2]);
                call.deposit(Balance::from_near(1)).submit();
            }
            WorkOp::Echo => {
                let busy = !sim.is_idle();
                sim.call("bob.near", LOCK, "work").args(vec![u8::from(busy)]).submit();
            }
            WorkOp::SyncWhileBusy => {
                if sim.state(LOCK, b"*busy").is_none() {
                    return Err("nothing in flight".into());
                }
                sim.call("bob.near", LOCK, "sync").submit();
            }
            WorkOp::SyncByWhoever => {
                let caller = if sim.is_idle() { "bob.near" } else { "alice.near" };
                sim.call(caller, LOCK, "sync").submit();
            }
        }
        Ok(())
    }

    fn refused(&self, outcomes: &[near_contract_sim::TxOutcome]) -> bool {
        matches!(self, WorkOp::TrySync { plain: false })
            && outcomes.iter().any(|o| o.value() == Some(&b"false"[..]))
    }
}

fn lock_harness<O: Op>() -> Harness<O> {
    lock_harness_on(lock())
}

fn lock_harness_on<O: Op>(sim: Sim) -> Harness<O> {
    let books = Books::new()
        .liability("work done", |sim: &Sim| {
            let v = sim.view_raw(LOCK, "worked", vec![]).result.unwrap();
            BTreeMap::from([(LOCK.to_owned(), u64::from_le_bytes(v.try_into().unwrap()).into())])
        })
        .near("lock NEAR", LOCK);
    Harness::new(sim).books(books)
}

fn serializable<O>(
    run: &near_contract_sim::explore::Run<O>,
) -> &near_contract_sim::explore::Judged {
    run.judged().iter().find(|j| j.check == builtin::SERIALIZABLE).unwrap()
}

/// A sync refused only because it landed inside a start's
/// flight succeeds in every serial order of the two: a finding, though
/// it moved nothing. Settled between the steps, it is clean. `LockOp`
/// keeps `Op::refused`'s default: only failed receipts count.
#[test]
fn an_op_refused_only_under_interleaving_is_a_finding() {
    let h = lock_harness();
    let inside = Plan::new().op(LockOp::Start).blocks(1).op(LockOp::Sync).refused();
    let run = h.run(&inside);
    let f = run.assert_found(builtin::SERIALIZABLE, "world");
    assert_eq!(f.cause.as_deref(), Some("refused only under interleaving"), "{run}");
    assert!(f.message.contains("succeeded in every serial order"), "{run}");
    let apart = Plan::new().op(LockOp::Start).op(LockOp::Sync);
    let run = h.run(&apart);
    run.assert_clean();
    assert_eq!(serializable(&run).idle, 1, "nothing in flight together: {run}");
}

/// The drop rule: a work refused inside a start's flight moved nothing,
/// and fails in every serial order too (after counting itself): left
/// out, the start alone gives the outcome, so the run is clean. One that
/// wrote a key before failing moved something, so it isn't left out, and
/// no serial order gives its outcome. One that paid its deposit back
/// moved NEAR there and back: nothing, net, so it is left out.
#[test]
fn a_failed_op_is_left_out_only_if_it_moved_nothing() {
    let h = lock_harness();
    let plan =
        |scratch| Plan::new().op(WorkOp::Start).blocks(1).op(WorkOp::Work { scratch }).refused();
    let run = h.run(&plan(false));
    assert_eq!(serializable(&run).ok, 1, "{run}");
    assert!(run.findings().iter().all(|f| f.check != builtin::SERIALIZABLE), "{run}");
    let run = h.run(&Plan::new().op(WorkOp::Start).blocks(1).op(WorkOp::PayBack).refused());
    let back = run.outcomes().iter().flat_map(|o| &o.effects().near).count();
    assert_eq!((back, serializable(&run).ok), (2, 1), "{run}");
    let run = h.run(&plan(true));
    let f = run.assert_found(builtin::SERIALIZABLE, "world");
    assert!(f.message.contains("work done: lock.near 0 here, 1 there"), "{run}");
}

/// Two starts in one block write `*busy` over itself, a
/// serial order writes it fresh: the second start's gas differs, and the
/// lock pays it (it signs that one), but the readings are net of gas, so
/// at tolerance 0 the run is clean.
#[test]
fn orders_that_differ_only_in_gas_are_one_outcome() {
    let h = lock_harness();
    let plan = Plan::new().op(WorkOp::Start).blocks(0).op(WorkOp::StartAgain);
    let run = h.run(&plan);
    run.assert_clean();
    assert_eq!(serializable(&run).ok, 1, "{run}");
}

/// A serial order ends in the case's epoch: two starts in one block, then
/// three epochs' wait before a sync, are their serial order's outcome.
#[test]
fn a_serial_order_skips_to_the_case_s_epoch() {
    let plan = Plan::new().op(LockOp::Start).blocks(0).op(LockOp::Start).epochs(3);
    let run = lock_harness().run(&plan.op(LockOp::Sync));
    let world: &Sim = run.world().unwrap();
    assert!(world.epoch_height() >= 4, "{run}");
    let j = serializable(&run);
    assert_eq!((j.ok, j.unjudged), (1, 0), "{run}");
}

/// A serial order an op skips in can't be compared: not judged, so the
/// run is incomplete, never clean. Submitted once the start has run, the
/// sync lands after its finish and succeeds; serially it skips.
#[test]
fn a_serial_order_that_skips_is_not_judged() {
    let h = lock_harness();
    let plan = Plan::new().op(WorkOp::Start).blocks(2).op(WorkOp::SyncWhileBusy);
    let run = h.run(&plan);
    assert_eq!(run.verdict(), Verdict::Incomplete, "{run}");
    let j = serializable(&run);
    assert_eq!((j.ok, j.failed, j.unjudged), (0, 0, 1), "{run}");
    assert!(j.why.as_deref().unwrap().contains("skipped: nothing in flight"), "{run}");
}

/// An op that sends other arguments in a serial order than in the case
/// can't be compared: not judged.
#[test]
fn an_op_sending_other_arguments_serially_is_not_judged() {
    let run =
        lock_harness().run(&Plan::new().op(WorkOp::Start).blocks(1).op(WorkOp::Echo).refused());
    let j = serializable(&run);
    assert_eq!((j.ok, j.failed, j.unjudged), (0, 0, 1), "{run}");
    assert!(j.why.as_deref().unwrap().contains("sent another call in serial order"), "{run}");
    // Not judged there, then a finding in the next ops in flight: the
    // finding.
    let next = Plan::new().op(WorkOp::Start).blocks(1).op(WorkOp::Echo);
    let run = lock_harness().run(&next.op(WorkOp::Start).blocks(1).op(WorkOp::Sync).refused());
    let f = run.assert_found(builtin::SERIALIZABLE, "world");
    assert_eq!(f.cause.as_deref(), Some("refused only under interleaving"), "{run}");
}

/// So can't one sent by another caller in a serial order than in the
/// case: same receiver, method and arguments, another signer.
#[test]
fn an_op_sent_by_another_caller_serially_is_not_judged() {
    let plan = Plan::new().op(WorkOp::Start).blocks(1).op(WorkOp::SyncByWhoever).refused();
    let run = lock_harness().run(&plan);
    let j = serializable(&run);
    assert_eq!((j.ok, j.failed, j.unjudged), (0, 0, 1), "{run}");
    assert!(j.why.as_deref().unwrap().contains("sent another call in serial order"), "{run}");
}

/// The closest order with a failed op left out says so; one with every
/// op in it doesn't.
#[test]
fn the_closest_order_says_which_ops_it_left_out() {
    let (refused, counted) = (WorkOp::Work { scratch: false }, WorkOp::Work { scratch: true });
    let plan = Plan::new().op(WorkOp::Start).blocks(0).op(refused).blocks(0).op(counted).refused();
    let run = lock_harness_on(lock_with(43_200)).run(&plan);
    let f = run.assert_found(builtin::SERIALIZABLE, "world");
    assert!(f.message.contains("(the failed ops that moved nothing left out)"), "{run}");
}

// ---- enumerate ----

use near_contract_sim::explore::Status;

const BOOKED: &str = "the books never exceed the balance";

/// pool_sync's own check: what it books never exceeds its balance.
fn pool_harness() -> Harness<PoolOp> {
    Harness::new(pool_sync()).at_rest(BOOKED, |sim: &Sim| {
        let (booked, balance) = (view_u128(sim, "booked"), sim.account(POOL).unwrap().amount);
        match booked <= balance.as_yoctonear() {
            true => Ok(()),
            false => Err(format!("the books hold {booked}, the balance {balance}")),
        }
    })
}

/// Depth 2 over two depositors puts the second deposit at each block of
/// the first's flight, and finds the in-flight double count; its
/// stored replay shows it again. Settled, the same pair is clean.
#[test]
fn enumerate_finds_the_in_flight_double_count_and_its_replay_shows_it() {
    let dir = std::env::temp_dir().join(format!("ncs-enumerate-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let h = pool_harness().store(&dir);
    let alphabet = [deposit("alice.near"), deposit("bob.near")];
    let audit = h.enumerate(&alphabet).depth(2).run();
    let f = audit.assert_found(BOOKED, "world");
    let steps = &f.run.plan().steps;
    assert_eq!(steps.len(), 2, "{audit}");
    assert!(
        matches!(steps[0].then, Gap::Blocks(_)),
        "the second inside the first's flight: {audit}"
    );
    let report = audit.report();
    assert!(
        report.notes.iter().any(|n| n.starts_with("flights (blocks), per op: Deposit ")),
        "{audit}"
    );
    assert!(report.notes.iter().any(|n| n.contains("proves nothing")), "{audit}");
    let replays = h.regressions();
    assert!(
        replays.iter().any(|r| r.check == BOOKED && matches!(r.status, Status::Shows(_))),
        "{replays:?}"
    );
    std::fs::remove_dir_all(&dir).unwrap();
}

/// The same enumeration twice merges the same states and keeps the
/// same cases for the same findings: the order is fixed, not a thread's.
#[test]
fn an_enumeration_runs_the_same_way_twice() {
    let h = pool_harness();
    let alphabet = [deposit("alice.near"), deposit("bob.near")];
    let run = || {
        let audit = h.enumerate(&alphabet).depth(3).run();
        let plans: Vec<String> =
            audit.found().iter().map(|f| format!("{:?}", f.run.plan())).collect();
        (audit.cases(), audit.report().notes, plans)
    };
    let first = run();
    assert!(first.1.iter().any(|n| n.contains("state(s) merged")), "{:?}", first.1);
    assert_eq!(first, run());
}

/// A budget that runs out leaves its depth `truncated:` and the
/// depths after `unexplored:`: never done.
#[test]
fn a_truncated_enumeration_is_not_done() {
    let h = pool_harness();
    let audit = h.enumerate(&[deposit("alice.near"), deposit("bob.near")]).cases(5).run();
    assert_eq!(audit.cases(), 5, "{audit}");
    let todo = audit.done().unwrap_err();
    let ids: Vec<&str> = todo.iter().map(|t| t.id.as_str()).collect();
    assert!(ids.contains(&"truncated:depth 2"), "{ids:?}");
    assert!(ids.contains(&"unexplored:depth 3"), "{ids:?}");
}

/// An enumeration says what it didn't vary. Edges asked for
/// (`Faults::default()` has them) run on each step placed at rest, so no
/// `unexplored:edges` item opens and more cases run than without them;
/// the note says what still wasn't varied. Without edges, nothing asked
/// for them: no item, and the note names everything not varied. The
/// boundary note names the holds its faults tried, none under
/// `Faults::none()`, and no epoch boundary on long epochs.
///
/// Verification boundary: an enumeration's to-do items and notes, and
/// its case count.
#[test]
fn an_enumeration_says_what_it_did_not_vary() {
    use near_contract_sim::explore::{Audit, Faults};
    use near_contract_sim::{Fault, Receipts};
    let h = pool_harness();
    let alphabet = [deposit("alice.near"), deposit("bob.near")];
    let run = |faults: Faults| h.enumerate(&alphabet).depth(1).faults(faults).run();
    let varied = |a: &Audit<PoolOp>| {
        let notes = a.report().notes;
        notes.into_iter().find(|n| n.starts_with("not varied: ")).expect("a not-varied note")
    };
    let no_item = |a: &Audit<PoolOp>| a.todo().iter().all(|t| t.id != "unexplored:edges");
    let edges = run(Faults::default());
    assert!(no_item(&edges), "{edges}");
    let note = varied(&edges);
    for what in ["callers", "inside a flight", "placed at rest", "not extended"] {
        assert!(note.contains(what), "{note}");
    }
    let starve = Faults::new(Receipts::all(), [Fault::OutOfGas]);
    let unedged = run(Faults::tolerable().and(starve));
    assert!(edges.cases() > unedged.cases(), "{} against {}", edges.cases(), unedged.cases());
    assert!(no_item(&unedged), "{unedged}");
    let note = varied(&unedged);
    for what in ["caller gas", "callers", "stub answers", "argument values"] {
        assert!(note.contains(what), "{note}");
    }
    assert!(!note.contains("placed at rest"), "{note}");
    let boundary = |a: &Audit<PoolOp>| a.report().notes.last().cloned().unwrap_or_default();
    let held = "holds over 3 block(s) and two faults on one step were not tried";
    assert!(boundary(&unedged).contains(held), "{}", boundary(&unedged));
    let none = run(Faults::none());
    assert!(no_item(&none), "{none}");
    let nothing =
        "Offsets outside the flight and holds were not tried; no epoch boundary was tried";
    assert!(boundary(&none).contains(nothing), "{}", boundary(&none));
    assert!(!boundary(&none).contains("are forced"), "{}", boundary(&none));
}

/// A seat-price op (no transaction, only `config.minimum_stake`) placed
/// inside a stake's flight fails its restake, so the pool locks less
/// than it books; after the stake settles it changes nothing.
#[test]
fn a_seat_price_change_inside_a_stake_s_flight_is_reached() {
    const LOCKS: &str = "the pool locks what it books";
    let h = Harness::new(staking()).at_rest(LOCKS, |sim: &Sim| {
        let total = sim.state(POOL, b"*total").map(|v| u128::from_le_bytes(v.try_into().unwrap()));
        let locked = locked(sim).as_yoctonear();
        match total == Some(locked) {
            true => Ok(()),
            false => Err(format!("it books {total:?}, locks {locked}")),
        }
    });
    let audit = h.enumerate(&[StakeOp::Stake(10), StakeOp::MinStake(1_000)]).depth(2).run();
    audit.assert_found(LOCKS, "world");
    let inside = audit.found().iter().any(|f| {
        let steps = &f.run.plan().steps;
        matches!(steps[0].act.op(), Some(StakeOp::Stake(10)))
            && matches!(steps[0].then, Gap::Blocks(_))
            && matches!(steps[1].act.op(), Some(StakeOp::MinStake(_)))
    });
    assert!(inside, "a finding with the seat price moved inside the stake's flight: {audit}");
    let settled = Plan::new().op(StakeOp::Stake(10)).op(StakeOp::MinStake(1_000));
    assert_eq!(
        h.run(&settled).verdict(),
        Verdict::Clean,
        "after the stake settles it changes nothing"
    );
}

/// On short epochs each new step also runs with an epoch boundary at
/// each block of its flight; one between `epoch_begin`'s two
/// receipts fails the second, a failure no one handles. On mainnet-long
/// epochs no boundary is placed, and nothing fails.
#[test]
fn an_epoch_boundary_is_placed_at_each_block_of_a_flight() {
    let audit = Harness::new(lock()).enumerate(&[LockOp::EpochBegin]).depth(1).run();
    let f =
        audit.assert_found(near_contract_sim::builtin::UNHANDLED_FAILURE, "lock.near::epoch_end");
    assert!(matches!(f.run.plan().start, Some(Gap::EpochEdge(k)) if k < 0), "{audit}");
    // The plain case, and one boundary at each of its flight's blocks and
    // just after: 0 to the flight, each its own case.
    let flight = audit.report().notes[0].rsplit(' ').next().unwrap().parse::<u64>().unwrap();
    assert_eq!(audit.cases() as u64, 1 + flight + 1, "{audit}");
    let audit = Harness::new(lock_with(43_200)).enumerate(&[LockOp::EpochBegin]).depth(1).run();
    assert!(audit.found().is_empty(), "{audit}");
    assert_eq!(audit.cases(), 1, "{audit}");
    let ids = |audit: &near_contract_sim::explore::Audit<LockOp>| -> Vec<String> {
        audit.todo().iter().map(|t| t.id.clone()).collect()
    };
    let unplaced = "unexplored:epoch boundaries".to_owned();
    let audit = Harness::new(lock()).enumerate(&[LockOp::EpochBegin]).depth(1).run();
    assert!(!ids(&audit).contains(&unplaced), "every offset fits: {audit}");
    // On 3-block epochs, the offset of 3 blocks is left out.
    let audit = Harness::new(lock_with(3)).enumerate(&[LockOp::EpochBegin]).depth(1).run();
    assert!(ids(&audit).contains(&unplaced), "{audit}");
}

/// An enumeration of no op, to depth 0, or of no case tries nothing: its
/// error.
#[test]
fn an_enumeration_of_nothing_is_an_error() {
    let h = lock_harness::<LockOp>();
    let sync = [LockOp::Sync];
    for audit in [
        h.enumerate(&[]).run(),
        h.enumerate(&sync).depth(0).run(),
        h.enumerate(&sync).cases(0).run(),
    ] {
        assert!(audit.errors().iter().any(|e| e.ends_with("it tries nothing")), "{audit}");
        assert_eq!(audit.cases(), 0, "{audit}");
        assert!(audit.todo().iter().all(|t| !t.id.starts_with("unjudged:")), "{audit}");
    }
}

/// A check failing on the fixture stops the enumeration at its first
/// case, counted, with the harness's error.
#[test]
fn a_check_failing_on_the_fixture_stops_at_the_first_case() {
    let h = Harness::new(lock()).at_rest("never", |_: &Sim| Err("never".to_owned()));
    let audit = h.enumerate(&[LockOp::Sync, LockOp::Start]).run();
    assert_eq!(audit.cases(), 1, "{audit}");
    assert!(audit.errors().iter().any(|e| e.contains("fails on the fixture")), "{audit}");
}

/// The case budget cut: the cases of the depth not run are counted, and
/// the depths after are never begun.
#[test]
fn a_cut_enumeration_counts_what_it_left() {
    let h = Harness::new(lock_with(43_200));
    let audit = h.enumerate(&[LockOp::Sync, LockOp::Start]).depth(2).cases(1).run();
    let todo = audit.todo();
    let cut =
        todo.iter().find(|t| t.id == "truncated:depth 1").unwrap_or_else(|| panic!("{audit}"));
    assert!(cut.why.contains(", 1 case(s) of it not run (from 1 state(s))"), "{}", cut.why);
    let never =
        todo.iter().find(|t| t.id == "unexplored:depth 2").unwrap_or_else(|| panic!("{audit}"));
    assert_eq!(never.why, "depth 2 never began; 1 state(s) at depth 1 left", "{audit}");
    assert!(!todo.iter().any(|t| t.id == "unexplored:depth 1"), "{audit}");
}

/// A third op goes in at each block left of the flight of the two
/// before it, counted from the first's submission: the count pins it,
/// as one counted from the second's adds cases past the flight.
#[test]
fn an_op_goes_in_only_inside_what_is_left_of_a_flight() {
    let h = Harness::new(lock_with(43_200));
    let audit = h.enumerate(&[LockOp::Start]).depth(3).run();
    assert!(audit.report().notes[0].ends_with("Start 4"), "{audit}");
    assert_eq!(audit.cases(), 21, "{audit}");
    assert_eq!(audit.report().what, "enumeration of 21 case(s) to depth 3", "{audit}");
}

/// An at-rest check over the ops sees an op that ran.
#[test]
fn an_at_rest_check_over_the_ops_sees_one_that_ran() {
    use near_contract_sim::explore::OpDone;
    let h = Harness::new(lock()).at_rest_ops("sees no op", |_: &Sim, ops: &[OpDone<LockOp>]| {
        if ops.is_empty() { Ok(()) } else { Err("saw one".to_owned()) }
    });
    let run = h.run(&Plan::new().op(LockOp::Sync));
    assert!(run.findings().iter().any(|f| f.check == "sees no op"), "{run}");
}

/// A case that is the harness's error is not extended, though the state
/// it left (bob's sync pending) is one not seen.
#[test]
fn a_harness_error_is_not_extended() {
    let h = Harness::new(lock_with(43_200));
    let audit = h.enumerate(&[LockOp::Boom]).key(|sim| u64::from(sim.is_idle())).depth(2).run();
    assert_eq!(audit.cases(), 1, "{audit}");
}

/// A skipped op leaves the state it met, nothing in flight: merged.
#[test]
fn a_seen_state_with_nothing_in_flight_is_merged() {
    let audit = lock_harness().enumerate(&[WorkOp::SyncWhileBusy]).depth(2).run();
    assert!(audit.report().notes[1].starts_with("1 state(s) merged"), "{audit}");
}

/// A variant gets no variants: faults and epoch boundaries together are
/// the cases of each alone.
#[test]
fn a_variant_gets_no_variants_of_its_own() {
    let cases = |sim: Sim, faults| {
        let h = Harness::new(sim);
        h.enumerate(&[LockOp::EpochBegin]).depth(1).faults(faults).run().cases()
    };
    let faults = cases(lock_with(43_200), near_contract_sim::explore::Faults::tolerable());
    let edges = cases(lock(), near_contract_sim::explore::Faults::none());
    assert!(faults > 1 && edges > 1, "{faults} {edges}");
    let both = cases(lock(), near_contract_sim::explore::Faults::tolerable());
    assert_eq!(both, faults + edges - 1);
}

/// An epoch boundary placed between two settled ops can land while the
/// first is still in flight: the case runs as another interleaving, and
/// a to-do counts them. How many depends on where the epoch falls: over
/// ten starting heights, from none to some.
#[test]
fn an_epoch_boundary_placed_late_is_a_to_do() {
    let late: Vec<Option<u32>> = (0..10)
        .map(|pad| {
            let mut sim = lock();
            (0..pad).for_each(|_| sim.step());
            let audit = Harness::new(sim).enumerate(&[LockOp::Start]).depth(2).run();
            let todo = audit.todo();
            let late = todo.iter().find(|t| t.id == "unexplored:epoch boundaries placed late");
            late.map(|t| t.why.split(' ').next().unwrap().parse().unwrap())
        })
        .collect();
    // A start's flight is 4 blocks: a boundary 1 to 4 blocks after the
    // first start lands inside it (a boundary in the block its last
    // receipt runs in included).
    // No to-do where none was late.
    assert_eq!(late, [None, None, None, None, None, Some(3), Some(5), Some(3), Some(1), None]);
}

/// An unstake whose receipts straddle an epoch boundary is reached:
/// a check that an unstake runs in one epoch fails.
#[test]
fn an_unstake_straddling_an_epoch_boundary_is_reached() {
    const ONE: &str = "an unstake runs in one epoch";
    let harness = |sim: Sim| {
        Harness::new(sim).per_op(ONE, |d: &near_contract_sim::explore::OpDone<StakeOp>| {
            let epochs: BTreeSet<u64> =
                d.calls().iter().map(|r| r.block_height.div_ceil(10)).collect();
            match epochs.len() {
                1 => Ok(()),
                _ => Err(format!("its receipts ran in epochs {epochs:?}")),
            }
        })
    };
    // Start where a plain unstake fits in one epoch.
    let mut sim = staking();
    let unstake = Plan::new().op(StakeOp::Unstake);
    while harness(sim.clone()).run(&unstake).verdict() != Verdict::Clean {
        sim.step();
    }
    let audit = harness(sim).enumerate(&[StakeOp::Unstake]).depth(1).run();
    let f = audit.assert_found(ONE, "Unstake");
    assert!(matches!(f.run.plan().start, Some(Gap::EpochEdge(_))), "{audit}");
}

/// The dedup key and the block height (the brief's caveat, pinned): the
/// default key leaves out the height within an epoch, so a stateless sync
/// leaves a state equal to the fixture's. It is extended only inside its
/// flight, so a trap set for the height a call after it settles runs at
/// is never sprung: missed. A key with the height finds it.
#[test]
fn the_default_key_merges_states_apart_only_in_height() {
    const SPRUNG: &str = "the trap is not sprung";
    let h = Harness::new(lock_with(43_200)).per_op(
        SPRUNG,
        |d: &near_contract_sim::explore::OpDone<Probe>| match d
            .outcomes()
            .iter()
            .all(|o| o.is_success())
        {
            true => Ok(()),
            false => Err("sprung"),
        },
    );
    // The height a trap call runs at after a sync settles.
    let run = h.run(&Plan::new().op(Probe::Sync).op(Probe::Trap(0)));
    let trap = run.outcomes().iter().flat_map(|o| &o.receipts).find(|r| r.method_names == ["trap"]);
    let at = trap.unwrap().block_height;
    let alphabet = [Probe::Sync, Probe::Trap(at)];
    let audit = h.enumerate(&alphabet).depth(2).run();
    assert!(audit.found().is_empty(), "{audit}");
    let notes = audit.report().notes;
    assert!(notes.iter().any(|n| n.contains("leaves out the block height")), "{notes:?}");
    let with_height = h.enumerate(&alphabet).depth(2).key(|sim: &Sim| sim.height()).run();
    let f = with_height.assert_found(SPRUNG, "Trap");
    assert_eq!(f.run.plan().steps[0].then, Gap::Settle, "{with_height}");
}

#[derive(Clone, Debug, Serialize, Deserialize)]
enum Probe {
    /// bob syncs: no state changes.
    Sync,
    /// bob calls `trap` with this height.
    Trap(u64),
}

impl Op for Probe {
    fn submit(&self, sim: &mut Sim) -> Result<(), Skip> {
        let at = match self {
            Probe::Sync => {
                sim.call("bob.near", LOCK, "sync").submit();
                return Ok(());
            }
            Probe::Trap(at) => *at,
        };
        sim.call("bob.near", LOCK, "trap").args(at.to_le_bytes()).submit();
        Ok(())
    }
}

// ---- Liveness after each step ----

const QUEUE: &str = "queue.near";

fn queue() -> Sim {
    let mut sim = Sim::new();
    sim.create_account("alice.near", Balance::from_near(10));
    let wat = std::fs::read_to_string(manifest_dir().join("tests/res/claim_queue.wat")).unwrap();
    sim.create_account(QUEUE, Balance::from_near(10));
    sim.deploy(QUEUE, wat::parse_str(&wat).unwrap());
    sim
}

/// The ids queued now, from 1 to the count.
fn queued(sim: &Sim) -> Vec<u64> {
    let count = sim.view_raw(QUEUE, "count", vec![]).result.unwrap();
    let n = u64::from_le_bytes(count.try_into().unwrap());
    let key = |id: u64| [&b"q"[..], &id.to_le_bytes()].concat();
    (1..=n).filter(|&id| sim.state(QUEUE, &key(id)).is_some()).collect()
}

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

/// Every enumerated prefix is its own case, so a claim's liveness
/// (`Books::claim`) is judged after each step, and its verdict is
/// memoized by the dedup key: a state reached again (a request at each
/// offset, all leaving two claims queued) runs the redeem plans once.
#[test]
fn liveness_is_judged_after_each_step_and_memoized_by_state() {
    use near_contract_sim::explore::{Books, Claim};
    use std::{cell::Cell, rc::Rc};
    let redeems = Rc::new(Cell::new(0));
    let counted = redeems.clone();
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
        .claim("claims", "queued", open, move |c: &Claim| {
            counted.set(counted.get() + 1);
            let id = c.holder.trim_start_matches("claim #").parse().unwrap();
            Plan::new().op(QueueOp::Redeem(id))
        });
    let h = Harness::new(queue()).books(books);
    let audit = h.enumerate(&[QueueOp::Request]).depth(2).run();
    audit.assert_clean();
    // One claim after the first request, two after the second at each of
    // its offsets: three redeems for the two states, not one a claim a
    // case (9), and one more for the first case, run twice without the
    // memo to check it runs the same.
    assert_eq!(audit.cases(), 5, "{audit}");
    assert_eq!(redeems.get(), 1 + 2 + 1, "{audit}");
}

/// Dedup: `start` sets `*busy` and `finish` clears
/// it, so a start at rest leaves the fixture's state. Its key is seen, but
/// the ops in flight with it are new, so an enumeration still places a
/// sync inside its flight and finds it refused only there.
#[test]
fn a_state_seen_at_rest_is_still_extended_inside_its_flight() {
    // Keyed by the lock's storage alone: the gas a start pays its
    // contract leaves it apart otherwise.
    let storage = |sim: &Sim| {
        let state = format!("{:?}", sim.view_state(LOCK, b""));
        state.bytes().fold(0u64, |h, b| h.wrapping_mul(31).wrapping_add(u64::from(b)))
    };
    let audit =
        lock_harness().enumerate(&[WorkOp::Start, WorkOp::Sync]).depth(2).key(storage).run();
    assert!(!audit.report().notes[1].starts_with("0 state"), "the start merged: {audit}");
    let f = audit.assert_found(builtin::SERIALIZABLE, "world");
    assert!(f.finding.message.contains("succeeded in every serial order"), "{audit}");
}

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

/// `SERIALIZABLE` and a parked yield: a respond submitted while the
/// request's yield is parked is in flight with it, and the serial order
/// [request, respond] leaves the yield parked between them, as the case's
/// settle does: clean. The resumed callback pays alice 1 NEAR, part of
/// the request's transaction, so the order reads each op's outcome once
/// the whole order has run. Waiting the yield out between them would time it
/// out, fail the respond and pay nothing: no serial order would match.
#[test]
fn a_respond_to_a_parked_yield_has_its_serial_order() {
    let mut sim = Sim::new();
    sim.create_account("alice.near", Balance::from_near(10));
    let wasm = wat::parse_file(manifest_dir().join("tests/res/yield_pay.wat")).unwrap();
    sim.create_account(YIELD, Balance::from_near(10));
    sim.deploy(YIELD, wasm);
    let books = Books::new()
        .liability("paid", |sim: &Sim| {
            let paid = u128::from(sim.state(YIELD, b"paid").is_some());
            BTreeMap::from([("alice.near".to_owned(), paid)])
        })
        .near("yield NEAR", YIELD);
    let h = Harness::new(sim).books(books);
    let run = h.run(&Plan::new().op(YieldOp::Request).op(YieldOp::Respond));
    let world: &Sim = run.world().unwrap();
    assert!(world.state(YIELD, b"paid").is_some(), "resumed: {run}");
    let j = serializable(&run);
    assert_eq!((j.ok, j.failed, j.unjudged), (1, 0, 0), "{run}");
}

// ---- a killed enumeration resumes ----

/// Deposits abort the process after `ENUM_ABORT_AFTER` submits: an
/// enumeration killed mid-way.
static SUBMITS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

fn pool_enumeration(h: &Harness<PoolOp>) -> near_contract_sim::explore::Enumerate<'_, PoolOp, Sim> {
    h.enumerate(&[deposit("alice.near"), deposit("bob.near")]).depth(3)
}

#[test]
#[ignore = "run by an_enumeration_killed_in_its_replay_resumes_to_the_same_report"]
fn killed_enumeration_child() {
    let Ok(path) = std::env::var("ENUM_SAVES") else { return };
    let _ = pool_enumeration(&pool_harness()).saves(path, 2).run();
}

/// What a resumed enumeration must match: cases, findings and to-do ids.
fn same(r: &near_contract_sim::explore::Report) -> (u32, Vec<String>, Vec<String>) {
    let mut rows: Vec<String> =
        r.findings.iter().map(|f| format!("{} | {} | {}", f.check, f.site, f.cases)).collect();
    rows.sort();
    let mut todo: Vec<String> = r.todo.iter().map(|t| t.id.clone()).collect();
    todo.sort();
    (r.cases, rows, todo)
}

/// Saves: an enumeration killed, then killed again while it replays the
/// cases its saves hold, keeps their count (the replay writes nothing),
/// and resumes to the report of one that ran whole.
#[test]
fn an_enumeration_killed_in_its_replay_resumes_to_the_same_report() {
    let h = pool_harness();
    let whole = pool_enumeration(&h).run().report();
    let path = std::env::temp_dir().join(format!("enum-resume-{}.json", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let kill = |after: &str| {
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "killed_enumeration_child", "--ignored", "--test-threads=1"])
            .env("ENUM_SAVES", &path)
            .env("ENUM_ABORT_AFTER", after)
            .status()
            .unwrap();
        assert!(!status.success(), "the child was meant to die mid-way");
        let text = std::fs::read_to_string(&path).unwrap();
        near_contract_sim::explore::Report::from_json(&text).unwrap().done
    };
    let done = kill("80");
    assert!(done > 4 && done + 2 < whole.cases as usize, "{done} of {}", whole.cases);
    assert_eq!(kill("12"), done, "a replay wrote its smaller count");
    let resumed = pool_enumeration(&h).saves(&path, 2).run().report();
    assert_eq!(same(&resumed), same(&whole));
    let _ = std::fs::remove_file(&path);
}

/// `Op::refused`: a try-sync that returns `false` inside a start's
/// flight, every receipt successful, is refused only under the
/// interleaving when the op says so: a finding. Settled between the
/// steps it returns `true`: clean. Without `refused` (the default) the
/// same interleaving is a success, and clean.
#[test]
fn an_op_s_refusal_without_a_failed_receipt_counts() {
    let h = lock_harness();
    let inside = |plain| Plan::new().op(WorkOp::Start).blocks(1).op(WorkOp::TrySync { plain });
    let run = h.run(&inside(false));
    assert!(run.outcomes().iter().all(|o| o.is_success()), "{run}");
    let f = run.assert_found(builtin::SERIALIZABLE, "world");
    assert_eq!(f.cause.as_deref(), Some("refused only under interleaving"), "{run}");
    h.run(&Plan::new().op(WorkOp::Start).op(WorkOp::TrySync { plain: false })).assert_clean();
    let run = h.run(&inside(true));
    run.assert_clean();
    assert_eq!(serializable(&run).ok, 1, "{run}");
}

// ---- Enumerate::first ----
//
// Verification boundary: which plans an enumeration runs when `first` is
// called (its first step from those ops, later steps from the alphabet),
// what its report says of them, and which saves file it resumes, over
// actual runs. Without `first`, nothing changes.

use near_contract_sim::explore::OpDone;
use std::cell::RefCell;
use std::rc::Rc;

/// The op sequences the lock's enumeration over Start and Sync ran to
/// `depth`, from `first` if given, as an at-rest check saw them (each
/// case's ops so far, at each rest), and its case count.
fn sequences(first: Option<&[LockOp]>, depth: usize) -> (BTreeSet<Vec<String>>, u32) {
    let seen: Rc<RefCell<BTreeSet<Vec<String>>>> = Rc::default();
    let log = seen.clone();
    let h = Harness::new(lock_with(43_200)).at_rest_ops(
        "logs the ops",
        move |_: &Sim, ops: &[OpDone<LockOp>]| {
            log.borrow_mut().insert(ops.iter().map(|o| o.name().to_owned()).collect());
            Ok::<(), String>(())
        },
    );
    let e = h.enumerate(&[LockOp::Start, LockOp::Sync]).depth(depth);
    let audit = match first {
        Some(ops) => e.first(ops).run(),
        None => e.run(),
    };
    let mut seen = seen.take();
    seen.remove(&vec![]);
    (seen, audit.cases())
}

/// `first` draws the first step from its ops and later steps from the
/// alphabet: every case starts with one of them, the second step
/// reaches an alphabet op outside them, and depth 1 runs them alone.
/// Without it, either op goes first.
#[test]
fn first_draws_the_first_step_from_its_ops_and_later_steps_from_the_alphabet() {
    let name = |s: &str| s.to_owned();
    let start = [LockOp::Start];
    let (seen, cases) = sequences(Some(&start), 2);
    assert!(seen.iter().all(|s| s[0] == "Start"), "{seen:?}");
    assert!(seen.contains(&vec![name("Start"), name("Sync")]), "{seen:?}");
    let (all, all_cases) = sequences(None, 2);
    assert!(all.iter().any(|s| s[0] == "Sync"), "{all:?}");
    assert!(cases < all_cases, "{cases} of {all_cases}");
    let (seen, cases) = sequences(Some(&start), 1);
    assert_eq!((seen, cases), (BTreeSet::from([vec![name("Start")]]), 1));
}

/// A report from a restricted run says so: its headline counts
/// the cases that ran and says the first step's op count, and a note
/// names those ops. Without `first`, neither. An empty `first`, or an
/// empty alphabet with one (even at depth 1), tries nothing.
#[test]
fn an_enumeration_from_first_ops_names_them() {
    let h = Harness::new(lock_with(43_200));
    let alphabet = [LockOp::Start, LockOp::Sync];
    let audit = h.enumerate(&alphabet).depth(2).first(&[LockOp::Start]).run();
    let report = audit.report();
    let what =
        format!("enumeration of {} case(s) to depth 2, first step from 1 op(s)", audit.cases());
    assert_eq!((report.what.as_str(), report.cases), (what.as_str(), audit.cases()), "{audit}");
    let note = "first step from 1 op(s): Start; later steps from the alphabet's 2";
    assert!(report.notes.iter().any(|n| n == note), "{:?}", report.notes);
    assert!(audit.to_string().contains(note), "{audit}");
    let all = h.enumerate(&alphabet).depth(2).run().report();
    assert_eq!(all.what, format!("enumeration of {} case(s) to depth 2", all.cases));
    assert!(all.notes.iter().all(|n| !n.starts_with("first step")), "{:?}", all.notes);
    let empty = h.enumerate(&alphabet).first(&[]).run();
    let why = "enumeration of 2 op(s) to depth 3, first step from 0 op(s), 2000 case(s): it tries \
        nothing";
    assert_eq!(empty.errors(), [why], "{empty}");
    for none in [empty, h.enumerate(&[]).depth(1).first(&alphabet).run()] {
        assert!(none.errors().iter().any(|e| e.ends_with("it tries nothing")), "{none}");
        assert_eq!(none.cases(), 0, "{none}");
    }
}

/// An enumeration split by its first ops over runs (processes, in use)
/// unions to a superset of one run's: every finding of the one run,
/// by id, and at least its cases (each part dedups on its own). The
/// union names each part's first ops.
#[test]
fn a_union_of_enumerations_split_by_first_ops_holds_one_run_s_findings() {
    let h = pool_harness();
    let alphabet = [deposit("alice.near"), deposit("bob.near")];
    let one = h.enumerate(&alphabet).depth(2).run().report();
    assert!(!one.findings.is_empty(), "{one}");
    let part = |i: usize| h.enumerate(&alphabet).depth(2).first(&alphabet[i..=i]).run().report();
    let union = h.union(&[part(0), part(1)]);
    for row in &one.findings {
        assert!(union.findings.iter().any(|u| u.id == row.id), "{} not in {union}", row.id);
    }
    assert!(union.cases >= one.cases, "{} cases, one run's {}", union.cases, one.cases);
    for who in ["alice.near", "bob.near"] {
        let named = |n: &String| n.starts_with("first step from 1 op(s): ") && n.contains(who);
        assert!(union.notes.iter().any(named), "{:?}", union.notes);
    }
}

/// The lock's enumeration to depth 2 a saves file is written or resumed
/// by: its ops, first ops and faults.
type Campaign<'a> = (&'a [LockOp], Option<&'a [LockOp]>, near_contract_sim::explore::Faults);

/// A saves file resumes only an enumeration with the same ops (in
/// order), first ops (in order, or none on both) and faults. One written
/// for any other is left and the run starts over (as for another
/// fixture), running what a run without the file runs: replayed by count
/// into another enumeration, its cases would rebuild nothing and report
/// none of the new campaign's first ones.
#[test]
fn a_saves_file_resumes_only_the_same_enumeration() {
    use near_contract_sim::explore::{Faults, Report};
    use near_contract_sim::{Fault, Receipts};
    let dir = std::env::temp_dir().join(format!("enum-first-saves-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let path = dir.join("saves.json");
    let h = Harness::new(lock_with(43_200)).store(dir.join("found"));
    let run = |(ops, first, faults): &Campaign, saves: bool| {
        let e = h.enumerate(ops).depth(2).faults(faults.clone());
        let e = if saves { e.saves(&path, 1) } else { e };
        match first {
            Some(f) => e.first(f),
            None => e,
        }
        .run()
        .report()
    };
    // Saved by `saved`, an item planted in the file, then run by `now`.
    let then = |saved: &Campaign, now: &Campaign| -> (bool, Report) {
        let _ = std::fs::remove_file(&path);
        run(saved, true);
        let mut v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let todo = serde_json::json!({ "id": "planted", "why": "planted", "close": "planted" });
        v["todo"].as_array_mut().unwrap().push(todo);
        std::fs::write(&path, v.to_string()).unwrap();
        let r = run(now, true);
        (r.todo.iter().any(|t| t.id == "planted"), r)
    };
    let (ab, ba) = ([LockOp::Start, LockOp::Sync], [LockOp::Sync, LockOp::Start]);
    let (start, sync) = ([LockOp::Start], [LockOp::Sync]);
    let none = Faults::none;
    let starve = || Faults::new(Receipts::all(), [Fault::OutOfGas]);
    let c = |ops, first, faults| -> Campaign { (ops, first, faults) };
    let resumes = [
        (c(&ab, Some(&start), none()), c(&ab, Some(&start), none())),
        (c(&ab, None, none()), c(&ab, None, none())),
        (c(&ab, None, starve()), c(&ab, None, starve())),
    ];
    for (saved, now) in &resumes {
        assert!(then(saved, now).0, "the same enumeration resumes: {now:?}");
    }
    let starts_over = [
        (c(&ab, None, none()), c(&ba, None, none())),
        (c(&ab, None, none()), c(&ab, None, starve())),
        (c(&ab, Some(&ab), none()), c(&ab, Some(&ba), none())),
        (c(&ab, Some(&start), none()), c(&ab, Some(&sync), none())),
        (c(&ab, Some(&start), none()), c(&ab, None, none())),
        (c(&ab, None, none()), c(&ab, Some(&start), none())),
    ];
    for (saved, now) in &starts_over {
        let (resumed, r) = then(saved, now);
        assert!(!resumed, "saved by {saved:?}, run by {now:?}: started over");
        let fresh = run(now, false);
        assert_eq!((r.cases, &r.what), (fresh.cases, &fresh.what), "as a run without the file");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// A saves file of a depth-2 enumeration resumes into the same one to
/// depth 3. Cases run depth by depth in a fixed order, so the
/// deeper run replays the saved cases to rebuild its states and runs and
/// reports only the cases after; with the saved part, its report is one
/// depth-3 run's.
#[test]
fn a_saves_file_resumes_into_a_deeper_enumeration() {
    let h = pool_harness();
    let alphabet = [deposit("alice.near"), deposit("bob.near")];
    let path = std::env::temp_dir().join(format!("enum-deeper-{}.json", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let shallow = h.enumerate(&alphabet).depth(2).saves(&path, 1).run();
    let deeper = h.enumerate(&alphabet).depth(3).saves(&path, 1).run();
    let whole = h.enumerate(&alphabet).depth(3).run();
    assert!(deeper.cases() > 0, "{deeper}");
    assert_eq!(deeper.cases() + shallow.cases(), whole.cases(), "only the cases after: {deeper}");
    assert_eq!(same(&deeper.report()), same(&whole.report()));
    let _ = std::fs::remove_file(&path);
}

// ---- per-step edges ----

use near_contract_sim::explore::{Audit, Axes, Faults};

const PURSE: &str = "purse.near";
const BACKED: &str = "the balance never exceeds what was deposited";

/// tests/res/purse.wat, alice with 100 NEAR.
fn purse() -> Sim {
    let mut sim = Sim::new();
    sim.create_account("alice.near", Balance::from_near(100));
    sim.create_account(PURSE, Balance::from_near(10));
    sim.deploy(PURSE, wat::parse_file(manifest_dir().join("tests/res/purse.wat")).unwrap());
    sim
}

#[derive(Clone, Debug, Serialize, Deserialize)]
enum PurseOp {
    /// alice deposits this amount.
    Deposit(u64),
    /// alice withdraws this amount.
    Withdraw(u64),
}

impl Op for PurseOp {
    fn submit(&self, sim: &mut Sim) -> Result<(), Skip> {
        let (method, n) = match self {
            PurseOp::Deposit(n) => ("deposit", n),
            PurseOp::Withdraw(n) => ("withdraw", n),
        };
        sim.call("alice.near", PURSE, method).args(n.to_le_bytes().to_vec()).submit();
        Ok(())
    }
}

/// A `PurseOp` whose only argument axis is the amount withdrawn.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(transparent)]
struct OnlyWithdrawn(PurseOp);

impl Op for OnlyWithdrawn {
    fn submit(&self, sim: &mut Sim) -> Result<(), Skip> {
        self.0.submit(sim)
    }

    fn name(&self) -> String {
        self.0.name()
    }

    fn axes(&self) -> Axes {
        Axes::only(["/Withdraw"])
    }
}

/// The purse's stored u64 under `key`; 0 if unset.
fn stored(sim: &Sim, key: &[u8]) -> u64 {
    let v = sim.view_state(PURSE, key).into_iter().next().map(|(_, v)| v);
    v.map_or(0, |v| u64::from_le_bytes(v[..8].try_into().unwrap()))
}

/// The purse's own check: its balance never exceeds what was deposited.
fn purse_harness<O: Op>() -> Harness<O> {
    Harness::new(purse()).at_rest(BACKED, |sim: &Sim| {
        let (balance, deposited) = (stored(sim, b"b"), stored(sim, b"t"));
        match balance <= deposited {
            true => Ok(()),
            false => Err(format!("the balance is {balance}, deposits {deposited}")),
        }
    })
}

fn backed<O>(audit: &Audit<O>) -> Option<&near_contract_sim::explore::Found<O>> {
    audit
        .found()
        .iter()
        .find(|f| f.finding.check == BACKED || f.folded.iter().any(|x| x.check == BACKED))
}

/// With `Faults::edges()`, each step placed at rest also runs at the
/// edges a scan tries: a withdrawal of the type's max after a deposit
/// wraps the balance above what was deposited. The plain enumeration
/// never moves the amount, and the scan's one-step edges on a withdrawal
/// alone only meet the empty purse's refusal: the bug needs the deposit
/// before it.
///
/// Verification boundary: which cases an enumeration runs and what it
/// finds there, over actual runs of the simulator.
#[test]
fn enumerate_edges_find_a_bug_only_after_an_earlier_step() {
    let h = purse_harness::<PurseOp>();
    let alphabet = [PurseOp::Deposit(5), PurseOp::Withdraw(5)];
    let plain = h.enumerate(&alphabet).depth(2).run();
    assert!(backed(&plain).is_none(), "{plain}");
    let first = h.enumerate(&alphabet).depth(1).faults(Faults::edges()).run();
    assert!(backed(&first).is_none(), "{first}");
    let alone = Plan::new().op(PurseOp::Withdraw(5));
    let scan = h.scan(&[alone]).faults(Faults::edges()).run();
    assert!(backed(&scan).is_none(), "{scan}");
    let audit = h.enumerate(&alphabet).depth(2).faults(Faults::edges()).run();
    let found = backed(&audit).unwrap_or_else(|| panic!("{audit}"));
    let at = found.label.to_string();
    assert!(at.contains(&format!("step 1, /Withdraw = {}", u64::MAX)), "{at}");
}

/// Edge cases are leaves: each (state at rest, op) adds the same V edge
/// cases, and none is extended. Over one deposit op, one state at rest
/// is extended per depth, so edges add exactly depth × V cases, and the
/// states merged are the same as without them.
///
/// Verification boundary: an enumeration's case count and merged
/// states.
#[test]
fn enumerate_edges_are_leaves() {
    let h = purse_harness::<PurseOp>();
    let run = |d: usize, faults: Faults| {
        h.enumerate(&[PurseOp::Deposit(5)]).depth(d).faults(faults).run()
    };
    let merged = |a: &Audit<PurseOp>| {
        let notes = a.report().notes;
        notes.into_iter().find(|n| n.contains("state(s) merged")).expect("a merged note")
    };
    let v = run(1, Faults::edges()).cases() - run(1, Faults::none()).cases();
    assert!(v > 3, "caller gas and the amount's edges: {v}");
    for d in 2..=3 {
        let (on, off) = (run(d, Faults::edges()), run(d, Faults::none()));
        assert_eq!(on.cases() - off.cases(), d as u32 * v, "depth {d}: {on}");
        assert_eq!(merged(&on), merged(&off), "depth {d}");
    }
}

/// `Axes::only` narrows an enumeration's edges as a scan's: with only the
/// amount withdrawn an axis, a deposit runs at its caller gas edges and
/// not at its amount's three (0, 1 and the max), and the withdrawal's max
/// still finds the wrap.
///
/// Verification boundary: an enumeration's case count and findings.
#[test]
fn axes_only_narrows_enumerate_edges() {
    let edges = Faults::edges;
    let wide = purse_harness::<PurseOp>();
    let all = wide.enumerate(&[PurseOp::Deposit(5)]).depth(1).faults(edges()).run().cases();
    let narrow = purse_harness::<OnlyWithdrawn>();
    let only = narrow.enumerate(&[OnlyWithdrawn(PurseOp::Deposit(5))]).depth(1);
    let only = only.faults(edges()).run().cases();
    assert_eq!(all - only, 3, "the amount's edges: {all} against {only}");
    assert!(only > 1, "caller gas stays an axis");
    let alphabet = [PurseOp::Deposit(5), PurseOp::Withdraw(5)].map(OnlyWithdrawn);
    let audit = narrow.enumerate(&alphabet).depth(2).faults(edges()).run();
    assert!(backed(&audit).is_some(), "{audit}");
}

/// A saves file cut among the edge cases resumes to the report of a run
/// that went whole: the leaves it holds are counted once, and not run
/// again.
///
/// Verification boundary: an enumeration's case count, findings and
/// rows across a resume.
#[test]
fn a_saves_file_cut_among_the_edges_resumes_whole() {
    let h = purse_harness::<PurseOp>();
    let alphabet = [PurseOp::Deposit(5), PurseOp::Withdraw(5)];
    let path = std::env::temp_dir().join(format!("enum-edges-{}.json", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let run = |cases: u32| {
        let e = h.enumerate(&alphabet).depth(2).faults(Faults::edges()).cases(cases);
        e.saves(&path, 1).run()
    };
    // Both plain cases, then the first two of the deposit's edges.
    let cut = run(4);
    assert_eq!(cut.cases(), 4, "{cut}");
    let rest = run(100_000);
    let whole = h.enumerate(&alphabet).depth(2).faults(Faults::edges()).run();
    assert!(rest.cases() > 0, "{rest}");
    assert_eq!(cut.cases() + rest.cases(), whole.cases(), "only the cases after: {rest}");
    // The cut run's `truncated:` items come back with its saves.
    let (cases, rows, _) = same(&rest.report());
    assert_eq!((cases, rows), {
        let (cases, rows, _) = same(&whole.report());
        (cases, rows)
    });
    assert!(backed(&rest).is_some(), "{rest}");
    let _ = std::fs::remove_file(&path);
}

/// A purse deposit that skips an amount of 0.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct Picky(u64);

impl Op for Picky {
    fn submit(&self, sim: &mut Sim) -> Result<(), Skip> {
        if self.0 == 0 {
            return Err(Skip("nothing to deposit".into()));
        }
        PurseOp::Deposit(self.0).submit(sim)
    }
}

/// An edge whose op skips wasn't tried: an open `incomplete:` item, never
/// a pass, naming the case it was an edge of.
///
/// Verification boundary: an enumeration's to-do items.
#[test]
fn an_edge_whose_op_skips_is_open_and_names_its_case() {
    let audit =
        purse_harness::<Picky>().enumerate(&[Picky(5)]).depth(2).faults(Faults::edges()).run();
    let open: Vec<&String> = audit.incomplete().iter().map(|(_, why)| why).collect();
    let skip = open.iter().find(|why| why.contains("edge not tried: op skipped"));
    let skip = skip.unwrap_or_else(|| panic!("{open:?}"));
    assert!(skip.contains("Picky(5)") && skip.contains("Picky(0)"), "{skip}");
    assert!(skip.contains("step 1, the op = 0"), "{skip}");
    assert!(audit.done().is_err(), "{audit}");
}
