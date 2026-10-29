//! The block clock: `height()` and `timestamp_ns()` are the last block's, as
//! RPC reports the latest block. Timestamps can go backward, and the height
//! can be set anywhere, genesis included.

mod common;
use common::*;
use near_contract_sim::{Balance, Busy, Sim};
use std::time::Duration;

const JAN_2020: u64 = 1_577_836_800_000_000_000; // 2020-01-01T00:00:00Z

const JAN_2031: u64 = 1_924_992_000_000_000_000; // 2031-01-01T00:00:00Z

const JAN_2019: u64 = 1_546_300_800_000_000_000; // 2019-01-01T00:00:00Z

const SEC: u64 = 1_000_000_000;

/// `ts()` executed in a real receipt; returns (value seen by contract, block ts of that receipt).
fn call_ts(sim: &mut Sim) -> (u64, u64) {
    let out = sim.call("alice.near", "probe.near", "ts").transact();
    out.assert_success();
    let r = out.receipt("ts").unwrap();
    (out.json::<u64>(), r.block_timestamp_ns)
}

#[test]
fn set_timestamp_2020_contract_sees_it() {
    let mut sim = probe_sim();
    sim.set_timestamp_ns(JAN_2020);
    // A view sees the last block's time.
    assert_eq!(sim.view("probe.near", "ts", ()).json::<u64>(), JAN_2020);
    // So does the next call: its receipt runs in a block at exactly that time.
    let (seen, receipt_ts) = call_ts(&mut sim);
    assert_eq!(seen, receipt_ts);
    assert_eq!(seen, JAN_2020);
}

/// A pinned time or height is what the next transaction's first receipt
/// sees, whether it calls another account (converting a block before) or
/// its own signer (running in the block it converts in); the chain goes on
/// from there.
#[test]
fn the_next_call_sees_the_pinned_time_and_height() {
    for signer in ["alice.near", "probe.near"] {
        let mut sim = probe_sim();
        sim.set_height(1_000_000);
        sim.set_timestamp_ns(JAN_2020);
        assert_eq!(sim.view("probe.near", "height", ()).json::<u64>(), 1_000_000);
        let height: u64 = sim.call(signer, "probe.near", "height").transact().json();
        assert_eq!(height, 1_000_000, "{signer}");
        sim.set_height(2_000_000);
        let height: u64 = sim.call(signer, "probe.near", "height").transact().json();
        assert_eq!(height, 2_000_000, "{signer}");
        sim.set_timestamp_ns(JAN_2031);
        let (seen, _) = call_ts(&mut sim);
        assert_eq!(seen, JAN_2031, "{signer}");
    }
}

/// The pin lasts until the next transaction or block: a block produced
/// first follows the pinned one, and a transaction then converts in the
/// block after that.
#[test]
fn a_block_before_the_call_spends_the_pin() {
    let mut sim = probe_sim();
    sim.set_height(1_000);
    sim.set_timestamp_ns(JAN_2020);
    sim.step();
    assert_eq!((sim.height(), sim.timestamp_ns()), (1_001, JAN_2020 + SEC));
    let height: u64 = sim.call("alice.near", "probe.near", "height").transact().json();
    assert_eq!(height, 1_003);
    // Only the first of several transactions lands on the pin.
    sim.set_height(2_000);
    let first = sim.call("alice.near", "probe.near", "height").submit();
    let second = sim.call("probe.near", "probe.near", "height").submit();
    sim.settle();
    assert_eq!(sim.outcome(first).json::<u64>(), 2_000);
    assert_eq!(sim.outcome(second).json::<u64>(), 1_999);
}

/// A pin no first receipt can land on panics when the transaction is
/// submitted, before anything is queued: a call to another account needs
/// two blocks after the last, so neither height 1 nor a time under two
/// block times leaves room.
#[test]
fn a_pin_with_no_room_before_it_panics_at_submit() {
    use std::panic::{AssertUnwindSafe, catch_unwind};
    let msg = |f: &mut dyn FnMut()| {
        let err = catch_unwind(AssertUnwindSafe(f)).unwrap_err();
        err.downcast_ref::<String>().cloned().unwrap_or_default()
    };
    let mut sim = probe_sim();
    sim.set_height(1);
    let got = msg(&mut || {
        sim.call("alice.near", "probe.near", "height").submit();
    });
    assert!(got.starts_with("set_height(1): the next transaction's first receipt runs"), "{got}");
    assert!(sim.is_idle());
    let mut sim = probe_sim();
    sim.set_timestamp_ns(SEC);
    let got = msg(&mut || {
        sim.call("alice.near", "probe.near", "ts").submit();
    });
    assert!(got.starts_with("set_timestamp_ns(1000000000): the next transaction's"), "{got}");
    // A call to its own signer needs one block: height 1 and one block time do.
    let mut sim = probe_sim();
    sim.set_height(1);
    sim.set_timestamp_ns(SEC);
    let out = sim.call("probe.near", "probe.near", "ts").transact();
    assert_eq!(out.json::<u64>(), SEC);
}

#[test]
fn jump_forward() {
    let mut sim = probe_sim();
    sim.set_timestamp_ns(JAN_2020);
    call_ts(&mut sim);
    sim.set_timestamp_ns(JAN_2031);
    sim.set_height(10_000_000);
    let (seen, _) = call_ts(&mut sim);
    assert_eq!(seen, JAN_2031);
    let r = sim.view("probe.near", "height", ()).json::<u64>();
    assert_eq!(r, sim.height());
}

/// Going backward: nearcore's runtime does not enforce timestamp
/// monotonicity (the chain/consensus layer does), so the contract simply
/// observes the earlier value. The simulator allows it on purpose (e.g. to
/// replay a fixture's validity window) - but a real chain can never do this.
#[test]
fn set_past_timestamp_after_later_one() {
    let mut sim = probe_sim();
    sim.set_timestamp_ns(JAN_2031);
    let (later, _) = call_ts(&mut sim);
    sim.set_timestamp_ns(JAN_2019);
    let (earlier, _) = call_ts(&mut sim);
    assert_eq!(later, JAN_2031);
    assert_eq!(earlier, JAN_2019);
    // The clock keeps ticking from the past value: the refund's block is the
    // last.
    assert_eq!(sim.timestamp_ns(), JAN_2019 + SEC);
}

/// A view sees `height()` and `timestamp_ns()`, the last block's, as an RPC
/// view of the latest block does. A transaction submitted then converts in
/// block `height() + 1`, and its call to another account runs in
/// `height() + 2`.
#[test]
fn a_view_sees_the_last_block_and_a_transaction_the_next() {
    let mut sim = probe_sim();
    sim.advance_by(Duration::from_secs(86_400));
    let (h, t) = (sim.height(), sim.timestamp_ns());
    assert_eq!(sim.view("probe.near", "height", ()).json::<u64>(), h);
    assert_eq!(sim.view("probe.near", "ts", ()).json::<u64>(), t);
    let out = sim.call("alice.near", "probe.near", "height").transact();
    let tx = out.transaction.as_ref().unwrap();
    assert_eq!((tx.block_height, tx.block_timestamp_ns), (h + 1, t + SEC));
    assert_eq!(out.json::<u64>(), h + 2);
}

/// Genesis is block 0: a fresh `Sim` starts there, and `set_height(2)` goes
/// back to it for a call to another account. The block a transaction
/// converts in is 1 at the earliest (at 0 the runtime's nonce upper bound,
/// `height * 1e6`, would refuse every transaction).
#[test]
fn genesis_is_block_0() {
    let mut sim = sim_with_users();
    assert_eq!(sim.height(), 0);
    sim.advance_to(10);
    sim.set_height(2);
    let out = sim.tx("alice.near", "bob.near").transfer(near(1)).transact();
    assert_eq!(out.assert_success().transaction.as_ref().unwrap().block_height, 1);
}

#[test]
#[should_panic(expected = "advance_to(0): block 1 is already the last")]
fn advance_to_a_past_block_panics() {
    let mut sim = Sim::new();
    sim.step();
    sim.advance_to(0);
}

/// A block moves the clock one height and one `block_time`; setting the time
/// leaves the height alone.
#[test]
fn step_moves_the_clock_one_block() {
    let mut sim = Sim::new();
    let t0 = sim.timestamp_ns();
    sim.step();
    assert_eq!((sim.height(), sim.timestamp_ns()), (1, t0 + SEC));
    sim.set_timestamp_ns(t0 + 3_600 * SEC);
    assert_eq!(sim.height(), 1, "time moved, height did not");
}

#[test]
fn config_block_time_gas_price_and_protocol_version() {
    let t0 = 1_700_000_000_000_000_000;
    let price = Balance::from_yoctonear(300_000_000);
    let mut sim = Sim::new();
    sim.config.block_time = Duration::from_millis(600);
    sim.config.gas_price = price;
    sim.config.protocol_version = 86;
    sim.set_height(49);
    sim.set_timestamp_ns(t0);
    // config.runtime_config() is the real config for the chosen protocol version.
    assert_eq!(
        sim.config.runtime_config().wasm_config.limit_config.max_total_prepaid_gas,
        near_contract_sim::nearcore::near_parameters::RuntimeConfigStore::new(None)
            .get_config(86)
            .wasm_config
            .limit_config
            .max_total_prepaid_gas
    );
    sim.step();
    assert_eq!((sim.height(), sim.timestamp_ns()), (50, t0 + 600_000_000));

    // tokens burnt = gas burnt * gas price, for the configured price and after changing it.
    sim.create_account("alice.near", near(10));
    for p in [price, Balance::from_yoctonear(1_000_000_000)] {
        sim.config.gas_price = p;
        let out =
            sim.tx("alice.near", "alice.near").transfer(Balance::from_yoctonear(1)).transact();
        out.assert_success();
        let t = out.transaction.as_ref().unwrap();
        assert_eq!(t.tokens_burnt.as_yoctonear(), t.gas_burnt.as_gas() as u128 * p.as_yoctonear());
    }
}

#[test]
fn advancing_near_the_timestamp_limit_counts_only_the_remaining_blocks() {
    let mut sim = Sim::new();
    sim.set_height(1);
    sim.set_timestamp_ns(u64::MAX - 2);
    sim.config.block_time = Duration::from_nanos(1);
    sim.advance_to(2);
    assert_eq!((sim.height(), sim.timestamp_ns()), (2, u64::MAX - 1));
    sim.advance_to(3);
    assert_eq!((sim.height(), sim.timestamp_ns()), (3, u64::MAX));
}

#[test]
fn clock_overflow_panics_before_advancing_or_taking_transactions() {
    use std::panic::{AssertUnwindSafe, catch_unwind};
    for (height, timestamp, block_time, advance) in [
        (u64::MAX, 0, Duration::ZERO, None),
        (0, u64::MAX, Duration::from_nanos(1), None),
        (0, 0, Duration::from_secs(u64::MAX), None),
        (0, u64::MAX - 1, Duration::from_nanos(1), Some(2)),
        (0, 0, Duration::from_nanos(2), Some(u64::MAX)),
    ] {
        let mut sim = sim_with_users();
        let tx = sim.tx("alice.near", "bob.near").transfer(near(1)).submit();
        sim.set_height(height);
        sim.set_timestamp_ns(timestamp);
        sim.config.block_time = block_time;
        let root = sim.state_root();
        sim.advance_to(height); // A no-op must not try to advance an invalid clock.
        assert!(
            catch_unwind(AssertUnwindSafe(|| match advance {
                Some(h) => sim.advance_to(h),
                None => sim.step(),
            }))
            .is_err()
        );
        assert_eq!((sim.height(), sim.timestamp_ns(), sim.state_root()), (height, timestamp, root));
        assert!(sim.outcome(tx).transaction.is_none(), "transaction was taken");
        sim.set_height(0);
        sim.set_timestamp_ns(0);
        sim.config.block_time = Duration::from_secs(1);
        sim.settle();
        sim.outcome(tx).assert_success(); // It stayed queued through the panic.
    }
}

/// `advance_by(d)` lands on the first height at least `d` later: `d`
/// rounded up to whole blocks, at whatever `block_time` is.
#[test]
fn advance_by_rounds_up_to_whole_blocks() {
    let mut sim = Sim::new();
    let step = |sim: &mut Sim, d: Duration| {
        let (h, t) = (sim.height(), sim.timestamp_ns());
        sim.advance_by(d);
        (sim.height() - h, sim.timestamp_ns() - t)
    };
    assert_eq!(step(&mut sim, Duration::from_secs(7 * 86_400)), (7 * 86_400, 7 * 86_400 * SEC));
    assert_eq!(step(&mut sim, Duration::ZERO), (0, 0));
    assert_eq!(step(&mut sim, Duration::from_nanos(1)), (1, SEC));
    sim.config.block_time = Duration::from_millis(600);
    assert_eq!(step(&mut sim, Duration::from_millis(1_200)), (2, 1_200_000_000));
    assert_eq!(step(&mut sim, Duration::from_millis(1_201)), (3, 1_800_000_000));
}

/// Like `advance_to`, `advance_by` fires a yield timeout it passes.
#[test]
fn advance_by_fires_a_yield_timeout_on_the_way() {
    let mut sim = probe_sim();
    sim.call("alice.near", "probe.near", "start_yield").transact().assert_success();
    let expires_at = sim.yields()[0].expires_at;
    sim.advance_by(Duration::from_secs(expires_at - sim.height() - 1));
    assert_eq!(sim.yields().len(), 1);
    sim.advance_by(Duration::from_secs(1));
    assert!(sim.yields().is_empty());
}

/// `advance_by` panics before producing a block or taking a transaction: on
/// a paused clock, past the last height or timestamp, or with a
/// `block_time` that doesn't fit; a zero `d` is a no-op even then.
#[test]
fn advance_by_panics_before_advancing() {
    use std::panic::{AssertUnwindSafe, catch_unwind};
    let ns = Duration::from_nanos;
    for (height, timestamp, block_time, d, msg) in [
        (
            0,
            0,
            Duration::ZERO,
            ns(1),
            "advance_by(1ns): config.block_time is zero, the clock is paused",
        ),
        (u64::MAX - 1, 0, ns(1), ns(2), "advance_by: height overflow"),
        (0, 0, ns(1), Duration::MAX, "advance_by: height overflow"),
        (0, u64::MAX - 1, ns(1), ns(2), "advance_to: timestamp overflow"),
        (0, 0, Duration::from_secs(u64::MAX), ns(1), "config.block_time exceeds u64 nanoseconds"),
    ] {
        let mut sim = sim_with_users();
        let tx = sim.tx("alice.near", "bob.near").transfer(near(1)).submit();
        sim.set_height(height);
        sim.set_timestamp_ns(timestamp);
        sim.config.block_time = block_time;
        let root = sim.state_root();
        sim.advance_by(Duration::ZERO);
        let err = catch_unwind(AssertUnwindSafe(|| sim.advance_by(d))).unwrap_err();
        let got = err.downcast_ref::<String>().cloned();
        let got = got.or_else(|| err.downcast_ref::<&str>().map(|s| s.to_string()));
        assert!(got.as_ref().is_some_and(|m| m.starts_with(msg)), "{got:?}");
        assert_eq!((sim.height(), sim.timestamp_ns(), sim.state_root()), (height, timestamp, root));
        assert!(sim.outcome(tx).transaction.is_none(), "transaction was taken");
    }
}

/// Paused: every block at one time, while heights, and the yield timeouts
/// counted in them, go on.
#[test]
fn pausing_the_clock() {
    let mut sim = probe_sim();
    sim.call("alice.near", "probe.near", "start_yield").transact().assert_success();
    sim.pause_clock_at(JAN_2020);
    sim.advance_to(sim.yields()[0].expires_at);
    sim.settle();
    assert_eq!(sim.timestamp_ns(), JAN_2020);
    assert_eq!(sim.view("probe.near", "last", ()).json::<String>(), "timeout");
}

/// A paused clock holds for a transaction's later blocks too: its
/// conversion at +1, its receipt at +2 and the refund at +3 all run at
/// `ns`, and the contract reads `ns`.
#[test]
fn a_paused_clock_holds_for_every_block_of_a_transaction() {
    let mut sim = probe_sim();
    sim.pause_clock_at(JAN_2020);
    let h = sim.height();
    let out = sim.call("alice.near", "probe.near", "ts").transact();
    assert_eq!(out.json::<u64>(), JAN_2020);
    let conversion = out.transaction.as_ref().unwrap();
    assert_eq!(conversion.block_height, h + 1);
    let ts = out.receipts.iter().map(|r| (r.block_height, r.block_timestamp_ns));
    assert_eq!(ts.collect::<Vec<_>>(), [(h + 2, JAN_2020), (h + 3, JAN_2020)]);
    assert_eq!(conversion.block_timestamp_ns, JAN_2020);
    assert_eq!((sim.height(), sim.timestamp_ns()), (h + 3, JAN_2020));
}

/// `advance_to` moves heights while paused, the past is allowed, and
/// `advance_by` panics before producing a block.
#[test]
fn heights_move_while_paused_and_advance_by_panics() {
    use std::panic::{AssertUnwindSafe, catch_unwind};
    let mut sim = probe_sim();
    sim.pause_clock_at(JAN_2031);
    sim.advance_to(sim.height() + 100);
    assert_eq!(sim.timestamp_ns(), JAN_2031);
    sim.pause_clock_at(JAN_2019);
    assert_eq!(call_ts(&mut sim), (JAN_2019, JAN_2019));
    let (h, root) = (sim.height(), sim.state_root());
    let r = catch_unwind(AssertUnwindSafe(|| sim.advance_by(Duration::from_secs(1))));
    assert!(panic_message(r).starts_with("advance_by(1s): the clock is paused (pause_clock_at)"));
    assert_eq!((sim.height(), sim.timestamp_ns(), sim.state_root()), (h, JAN_2019, root));
    sim.advance_by(Duration::ZERO);
}

/// `resume_clock` restores the block time the first pause saved, however
/// many pauses followed, and is a no-op on a running clock.
#[test]
fn resume_clock_restores_the_block_time() {
    let mut sim = Sim::new();
    sim.resume_clock();
    assert_eq!(sim.config.block_time, Duration::from_secs(1));
    sim.pause_clock_at(JAN_2020);
    sim.pause_clock_at(JAN_2020 + SEC);
    assert_eq!(sim.config.block_time, Duration::ZERO);
    sim.step();
    assert_eq!(sim.timestamp_ns(), JAN_2020 + SEC);
    sim.resume_clock();
    assert_eq!(sim.config.block_time, Duration::from_secs(1));
    sim.step();
    assert_eq!(sim.timestamp_ns(), JAN_2020 + 2 * SEC);
    sim.advance_by(Duration::from_secs(5));
    assert_eq!(sim.timestamp_ns(), JAN_2020 + 7 * SEC);
    sim.resume_clock();
    assert_eq!(sim.config.block_time, Duration::from_secs(1));
}

/// A paused clock survives a `Sim::cached` load, and so does what
/// `resume_clock` restores.
#[test]
fn a_cached_sim_keeps_its_paused_clock() {
    let mut sim = Sim::cached("paused", || {
        let mut sim = Sim::new();
        sim.config.block_time = Duration::from_millis(600);
        sim.pause_clock_at(JAN_2020);
        sim
    });
    sim.step();
    assert_eq!(sim.timestamp_ns(), JAN_2020);
    sim.resume_clock();
    sim.step();
    assert_eq!(sim.timestamp_ns(), JAN_2020 + 600_000_000);
}

/// Each `ping` calls `ping` on its own account with all its gas left, so a
/// transaction keeps one receipt in flight for about a hundred blocks,
/// until the gas runs out.
fn receipt_loop() -> Sim {
    let mut sim = sim_with_users();
    let wasm = wat::parse_str(
        r#"(module
  (import "env" "current_account_id" (func $me (param i64)))
  (import "env" "register_len" (func $len (param i64) (result i64)))
  (import "env" "read_register" (func $read (param i64 i64)))
  (import "env" "promise_batch_create" (func $create (param i64 i64) (result i64)))
  (import "env" "promise_batch_action_function_call_weight"
    (func $call (param i64 i64 i64 i64 i64 i64 i64 i64)))
  (memory 1)
  (export "memory" (memory 0))
  (data (i32.const 0) "ping")
  (func (export "ping") (local $p i64)
    (call $me (i64.const 0))
    (call $read (i64.const 0) (i64.const 64))
    (local.set $p (call $create (call $len (i64.const 0)) (i64.const 64)))
    (call $call (local.get $p) (i64.const 4) (i64.const 0) (i64.const 0) (i64.const 0)
                (i64.const 128) (i64.const 0) (i64.const 1))))"#,
    )
    .unwrap();
    sim.deploy("loop.near", wasm);
    sim
}

fn pings(sim: &Sim, tx: near_contract_sim::CryptoHash) -> Vec<u64> {
    let out = sim.outcome(tx);
    out.receipts.iter().filter(|r| r.method_names == ["ping"]).map(|r| r.block_height).collect()
}

/// `run_until(n, Sim::is_idle)` on a receipt loop returns `Busy` after its
/// budget, and the chain goes on: it still steps, and with room it settles.
#[test]
fn settling_a_receipt_loop_within_a_budget_is_busy_and_the_chain_still_steps() {
    let mut sim = receipt_loop();
    let tx = sim.call("alice.near", "loop.near", "ping").submit();
    let h = sim.height();
    assert!(matches!(sim.run_until(5, Sim::is_idle), Err(Busy { blocks: 5, idle: false, .. })));
    assert_eq!(sim.height(), h + 5);
    sim.step();
    assert_eq!(sim.height(), h + 6);
    let n = sim.run_until(1_000, Sim::is_idle).unwrap();
    assert!(n > 20, "the loop ran {n} more blocks");
    assert!(sim.is_idle() && pings(&sim, tx).len() as u64 > n);
    assert_eq!(sim.run_until(5, Sim::is_idle), Ok(0));
}

/// `settle()` is `run_until(config.max_blocks, Sim::is_idle)`, and panics with what
/// is still pending.
#[test]
fn settle_panics_on_a_receipt_loop_listing_the_pending_receipts() {
    use std::panic::{AssertUnwindSafe, catch_unwind};
    let mut sim = receipt_loop();
    let tx = sim.call("alice.near", "loop.near", "ping").submit();
    sim.config.max_blocks = 4;
    let h = sim.height();
    let msg = panic_message(catch_unwind(AssertUnwindSafe(|| sim.settle())));
    assert!(
        msg.starts_with("not idle after 4 blocks (config.max_blocks; a receipt loop?)"),
        "{msg}"
    );
    assert!(msg.contains("loop.near::ping from loop.near (receipt "), "{msg}");
    assert!(msg.contains(&format!(", tx {tx}): in flight")), "{msg}");
    assert_eq!(sim.height(), h + 4);
}

/// `run_until` checks its predicate before each block and stops on the
/// exact block it turns true, one block at a time.
#[test]
fn run_until_stops_on_the_exact_block() {
    let mut sim = receipt_loop();
    let tx = sim.call("alice.near", "loop.near", "ping").submit();
    let h = sim.height();
    let n = sim.run_until(100, |sim| pings(sim, tx).len() == 3).unwrap();
    let blocks = pings(&sim, tx);
    assert_eq!(blocks.len(), 3);
    assert_eq!(sim.height(), blocks[2]);
    assert_eq!(n, blocks[2] - h);
    assert_eq!(sim.run_until(100, |sim| pings(sim, tx).len() == 3), Ok(0));
    // True on the budget's last block: Ok, not Busy.
    let h = sim.height();
    assert_eq!(sim.run_until(3, |sim| sim.height() == h + 3), Ok(3));
}

/// On an idle chain `run_until` still steps block by block, skipping no
/// height, and reports `idle` when it gives up.
#[test]
fn run_until_on_an_idle_chain_is_busy_and_idle() {
    let mut sim = probe_sim();
    sim.call("alice.near", "probe.near", "start_yield").transact().assert_success();
    let h = sim.height();
    assert!(matches!(sim.run_until(4, |_| false), Err(Busy { blocks: 4, idle: true, .. })));
    assert_eq!(sim.height(), h + 4);
    assert_eq!(sim.yields().len(), 1);
}

/// Only a transaction the chain queues spends the pin: a copy of one
/// already queued queues nothing and leaves it, and a copy the chain
/// refuses (other promise results than the first's) panics before the
/// last block moves.
#[test]
fn only_a_queued_transaction_spends_the_pin() {
    let mut sim = probe_sim();
    fn call(sim: &mut Sim) -> near_contract_sim::Tx<'_> {
        sim.call("alice.near", "probe.near", "height").nonce(1_000)
    }
    let first = call(&mut sim).submit();
    sim.set_height(100);
    assert_eq!(call(&mut sim).submit(), first);
    let refused = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        call(&mut sim).promise_results([]).submit();
    }));
    assert!(panic_message(refused).contains("was submitted before"));
    assert_eq!(sim.height(), 100, "the pin is still there");
    let second = sim.call("alice.near", "probe.near", "height").submit();
    sim.settle();
    assert_eq!(sim.outcome(second).json::<u64>(), 100);
    assert_eq!(sim.outcome(first).json::<u64>(), 100, "queued before, converted in the same block");
}
