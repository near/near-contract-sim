//! Congested and overfull chunks on the single simulated shard: the chunk
//! gas limit and the delayed-receipt queue, the state-witness size limits,
//! the bandwidth scheduler's per-block receipt bytes, yield timeouts in full
//! chunks, and what a chunk producer does with transactions (per-chunk gas
//! and size budget, `ShardCongested` rejection).
//!
//! Every limit here is nearcore's own (the default protocol's config): the simulator
//! only feeds `Runtime::apply` and picks the chunk's transactions.

mod fuzz_common;
use fuzz_common::*;
use near_contract_sim::explore::{Faults, Harness, Op as XOp, Plan, Skip, plans};
use near_contract_sim::{
    Balance, CryptoHash, ExecutionStatus, Fault, FinalExecutionStatus, Gas, InvalidTxError,
    Receipts, Sim, TxExecutionError, TxOutcome,
};
use proptest::prelude::Just;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::panic::{AssertUnwindSafe, catch_unwind};

fn sim_with(limit: Option<Gas>, users: usize) -> Sim {
    let mut sim = Sim::new();
    sim.config.block_gas_limit = limit;
    for c in ["c.near", "d.near"] {
        sim.create_account(c, Balance::from_near(1_000_000));
        sim.deploy(c, fuzz_wasm());
    }
    for i in 0..users {
        sim.create_account(format!("u{i}.near"), Balance::from_near(10_000));
    }
    sim
}

fn run_call(sim: &mut Sim, signer: &str, prog: &[Op], gas: u64) -> CryptoHash {
    sim.call(signer, "c.near", "run").args(encode(prog)).gas(Gas::from_gas(gas)).submit()
}

/// Height at which the tx's first (non-refund) receipt ran.
fn first_receipt_height(o: &TxOutcome) -> u64 {
    let t = o.transaction.as_ref().expect("signed tx");
    let ExecutionStatus::SuccessReceiptId(id) = &t.status else { panic!("{:?}", t.status) };
    o.receipts.iter().find(|r| r.receipt_id == *id).unwrap().block_height
}

/// Gas burnt per block height over the given outcomes (conversions included).
fn gas_per_height(outs: &[TxOutcome]) -> BTreeMap<u64, (u64, u64)> {
    let mut m: BTreeMap<u64, (u64, u64)> = BTreeMap::new();
    for o in outs {
        for r in o.transaction.iter().chain(&o.receipts) {
            let e = m.entry(r.block_height).or_default();
            e.0 += r.gas_burnt.as_gas();
            e.1 = e.1.max(r.gas_burnt.as_gas());
        }
    }
    m
}

/// 12 transactions of ~270 Tgas each land in one block. The chunk gas limit
/// (1000 Tgas) lets 4 receipts run per block (the check is made before each
/// receipt, so the 4th overshoots); the rest wait in the delayed queue and
/// run in FIFO order, and `run` drains it.
#[test]
fn heavy_burst_spills_into_delayed_queue_in_fifo_order() {
    let mut sim = sim_with(Some(Gas::from_teragas(1_000)), 12);
    let hs: Vec<_> = (0..12)
        .map(|i| run_call(&mut sim, &format!("u{i}.near"), &[Op::BurnGas(16_000)], 300 * TGAS))
        .collect();
    let conv = sim.height() + 1;
    sim.settle();
    assert!(sim.is_idle());
    let outs: Vec<_> = hs.iter().map(|h| sim.outcome(*h)).collect();
    let heights: Vec<u64> = outs
        .iter()
        .map(|o| {
            o.assert_success();
            assert_eq!(o.transaction.as_ref().unwrap().block_height, conv);
            first_receipt_height(o)
        })
        .collect();
    // 4 per block, FIFO: conv+1 x4, conv+2 x4, conv+3 x4.
    let want: Vec<u64> = (0..12).map(|i| conv + 1 + i / 4).collect();
    assert_eq!(heights, want);
    // Every chunk stayed within the limit up to its last receipt.
    for (h, (sum, max)) in gas_per_height(&outs) {
        assert!(sum - max < 1_000 * TGAS, "block {h}: {sum} gas");
    }
    // Refunds follow one block after their receipt; nothing else lingers.
    assert_eq!(sim.height(), conv + 4);
}

/// `config.block_gas_limit` is the chunk gas limit: a lower limit spreads
/// the same load over more blocks, `None` runs everything in one block.
#[test]
fn block_gas_limit_sets_the_chunk_limit() {
    let spread = |limit: Option<Gas>| {
        let mut sim = sim_with(limit, 6);
        let hs: Vec<_> = (0..6)
            .map(|i| run_call(&mut sim, &format!("u{i}.near"), &[Op::BurnGas(3_000)], 100 * TGAS))
            .collect();
        sim.settle();
        let hs: Vec<u64> = hs.iter().map(|h| first_receipt_height(&sim.outcome(*h))).collect();
        hs.iter().map(|h| h - hs[0]).collect::<Vec<_>>()
    };
    // ~50 Tgas per receipt.
    assert_eq!(spread(None), vec![0; 6]);
    assert_eq!(spread(Some(Gas::from_teragas(1_000))), vec![0; 6]);
    assert_eq!(spread(Some(Gas::from_teragas(100))), vec![0, 0, 1, 1, 2, 2]);
    assert_eq!(spread(Some(Gas::from_teragas(1))), vec![0, 1, 2, 3, 4, 5]);
}

/// Local receipts (signer == receiver) run in the conversion block, but only
/// within the chunk gas limit: the rest go to the delayed queue as well.
#[test]
fn local_receipts_respect_the_chunk_limit() {
    let mut sim = sim_with(Some(Gas::from_teragas(1_000)), 0);
    let hs: Vec<_> = (0..6)
        .map(|_| {
            sim.call("c.near", "c.near", "run")
                .args(encode(&[Op::BurnGas(16_000)]))
                .gas(Gas::from_teragas(300))
                .submit()
        })
        .collect();
    let conv = sim.height() + 1;
    sim.settle();
    let heights: Vec<u64> = hs.iter().map(|h| first_receipt_height(&sim.outcome(*h))).collect();
    assert_eq!(heights, vec![conv, conv, conv, conv, conv + 1, conv + 1]);
}

/// Receipts a chunk produces for the (only) shard go through the bandwidth
/// scheduler (`max_shard_bandwidth` 4.5 MB per block), buffered ones first.
/// Six ~1.2 MB transactions (a chunk takes three: 4 MiB of transactions)
/// each forward their payload to d.near. A block sends three such receipts,
/// not four, and the block a forward is made in has already sent three: the
/// next three transactions' calls, or the forwards buffered the block
/// before. So every forward waits a block in the buffer, yet it counts
/// against its call's gas at once: it took the call's unused gas by weight.
/// `run` waits for the buffer to drain.
#[test]
fn outgoing_receipt_bytes_are_buffered_beyond_the_bandwidth_grant() {
    let mut sim = sim_with(Some(Gas::from_teragas(1_000)), 6);
    let hs: Vec<_> = (0..6u8)
        .map(|i| {
            let inner = encode(&[Op::YieldResume { slot: 9, payload: vec![i; 1_200_000] }]);
            let prog = [Op::Call {
                to: "d.near".into(),
                args: inner,
                deposit: 0,
                gas: 30 * TGAS,
                weight: 1,
            }];
            run_call(&mut sim, &format!("u{i}.near"), &prog, 300 * TGAS)
        })
        .collect();
    let start = sim.height() + 1;
    sim.advance_to(start + 1);
    for h in &hs[..3] {
        let o = sim.outcome(*h);
        let c = o.receipts.iter().find(|r| r.receiver_id.as_str() == "c.near").unwrap();
        assert_eq!(c.headroom(), Gas::ZERO, "{o}");
    }
    sim.settle();
    assert!(sim.is_idle());
    let mut ran: BTreeMap<u64, usize> = BTreeMap::new();
    for h in &hs {
        let o = sim.outcome(*h);
        o.assert_success();
        let c = o.receipts.iter().find(|r| r.receiver_id.as_str() == "c.near").unwrap();
        let d = o
            .receipts
            .iter()
            .find(|r| r.receiver_id.as_str() == "d.near" && !r.is_refund())
            .unwrap();
        assert_eq!(d.logs, vec!["y-"]);
        assert_eq!(d.block_height, c.block_height + 2, "buffered for a block");
        for r in [c, d] {
            *ran.entry(r.block_height - start).or_default() += 1;
        }
    }
    // Three calls a block in the two blocks after the first conversions, then
    // three forwards a block.
    assert_eq!(ran, [(1, 3), (2, 3), (3, 3), (4, 3)].into());
}

/// Six ~1.2 MB forwards, as above, the last transaction's made `n`th: each
/// waits a block in the outgoing buffer.
fn buffered_forwards(sim: &mut Sim) -> Vec<Op> {
    for i in 0..5u8 {
        run_call(sim, &format!("u{i}.near"), &forward(i), 300 * TGAS);
    }
    forward(5)
}

fn forward(i: u8) -> Vec<Op> {
    let inner = encode(&[Op::YieldResume { slot: 9, payload: vec![i; 1_200_000] }]);
    vec![Op::Call { to: "d.near".into(), args: inner, deposit: 0, gas: 30 * TGAS, weight: 1 }]
}

/// Six ~1.2 MB forwards submitted together, as one op: congestion control
/// holds each one's forward back a block in the outgoing buffer. With
/// `true`, each first makes a small call too.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct Forwards(bool);

impl XOp for Forwards {
    fn submit(&self, sim: &mut Sim) -> Result<(), Skip> {
        let small =
            Op::Call { to: "d.near".into(), args: encode(&[]), deposit: 0, gas: TGAS, weight: 1 };
        for i in 0..6u8 {
            let prog = [self.0.then(|| small.clone()).as_slice(), &forward(i)].concat();
            run_call(sim, &format!("u{i}.near"), &prog, 300 * TGAS);
        }
        Ok(())
    }
}

/// A receipt congestion control holds back leaves its block in a later one,
/// past where faults apply: `Tx::inject` panics rather than not fault it; a
/// scan, which can't count it, lists it as not tried, with why; and a
/// search's sampled fault that fits nothing else runs the case without one,
/// counted as untargeted, never as a faulted pass.
#[test]
fn a_held_back_receipt_cant_be_faulted() {
    let mut sim = sim_with(Some(Gas::from_teragas(1_000)), 6);
    let last = buffered_forwards(&mut sim);
    let r = catch_unwind(AssertUnwindSafe(|| {
        let tx =
            sim.call("u5.near", "c.near", "run").args(encode(&last)).gas(Gas::from_teragas(300));
        tx.inject(Receipts::method("run"), Fault::OutOfGas).transact()
    }));
    let msg = r.unwrap_err();
    let msg = msg.downcast_ref::<String>().unwrap();
    assert!(msg.starts_with("congestion control held back receipt "), "{msg}");

    let harness = Harness::new(sim_with(Some(Gas::from_teragas(1_000)), 6)).at_rest(
        "every forward succeeds",
        |sim: &Sim| {
            for hash in sim.txs() {
                let outcome = sim.outcome(hash);
                if !outcome.is_success() {
                    return Err(outcome.to_string());
                }
            }
            Ok(())
        },
    );
    let plan = Plan::new().op(Forwards(false));
    harness.run(&plan).assert_clean();
    let starve = || Faults::new(Receipts::call("d.near", "run"), [Fault::OutOfGas]);
    // A fault fits no receipt congestion control holds back, so the scan
    // has no variant past the plan as written.
    let report = harness.scan(std::slice::from_ref(&plan)).faults(starve()).run();
    assert_eq!((report.cases(), report.found().len()), (1, 0), "{report}");
    // A hold still fits a receipt of theirs that left its block: one each
    // to a small call ahead of the forward.
    let hold = Faults::new(Receipts::all(), [Fault::Hold(1)]);
    let report = harness.scan(&[Plan::new().op(Forwards(true))]).faults(hold).run();
    assert!(report.cases() > 1, "{report}");
    let report = harness
        .search(plans(Just(Forwards(false))).len(1..=1).faults(starve().per_case(1.0)))
        .seed(0x5eed)
        .cases(1)
        .run();
    assert!(report.found().is_empty() && report.errors().is_empty(), "{report}");
}

/// Six calls, each with a ~1.2 MB callback, submitted together.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct BigCallbacks;

impl XOp for BigCallbacks {
    fn submit(&self, sim: &mut Sim) -> Result<(), Skip> {
        for i in 0..6u8 {
            let then = encode(&[Op::YieldResume { slot: 9, payload: vec![i; 1_200_000] }]);
            let call = Op::Call {
                to: "d.near".into(),
                args: encode(&[]),
                deposit: 0,
                gas: 5 * TGAS,
                weight: 0,
            };
            let prog = [call, Op::Then { args: then, gas: 30 * TGAS, weight: 1 }];
            run_call(sim, &format!("u{i}.near"), &prog, 300 * TGAS);
        }
        Ok(())
    }
}

/// A callback congestion control holds back is past where faults apply:
/// each 1.2 MB callback waits a block in the buffer, so a scan that would
/// starve callbacks lists every one as held back, and tries none.
#[test]
fn a_held_back_callback_is_not_tried() {
    let harness = Harness::new(sim_with(Some(Gas::from_teragas(1_000)), 6));
    let plan = Plan::new().op(BigCallbacks);
    let starve = Faults::new(Receipts::callbacks(), [Fault::OutOfGas]);
    let report = harness.scan(&[plan]).faults(starve).run();
    assert_eq!((report.cases(), report.found().len()), (1, 0), "{report}");
}

/// State-witness limits need the trie reads recorded, as a chunk producer
/// does. A receipt reading more than `per_receipt_storage_proof_size_limit`
/// (4 MB) fails; the state is left untouched.
#[test]
fn receipt_over_the_storage_proof_limit_fails() {
    let mut sim = sim_with(Some(Gas::from_teragas(1_000)), 1);
    for i in 0..12u8 {
        sim.set_state("c.near", &[b'b', i], Some(&vec![i; 400_000]));
    }
    // storage_write returns (so reads) the evicted value: 12 x 400 kB.
    let prog: Vec<Op> =
        (0..12u8).map(|i| Op::Write { key: vec![b'b', i], value: vec![1] }).collect();
    let h = run_call(&mut sim, "u0.near", &prog, 300 * TGAS);
    sim.settle();
    let msg = sim.outcome(h).assert_failure().to_string();
    assert!(
        msg.contains("Size of the recorded trie storage proof has exceeded the allowed limit"),
        "{msg}"
    );
    assert_eq!(sim.view_state("c.near", &[b'b', 0])[0].1.len(), 400_000);
    // Under the limit it works.
    let prog: Vec<Op> =
        (0..6u8).map(|i| Op::Write { key: vec![b'b', i], value: vec![1] }).collect();
    let h = run_call(&mut sim, "u0.near", &prog, 300 * TGAS);
    sim.settle();
    sim.outcome(h).assert_success();
}

/// Past `main_storage_proof_size_soft_limit` (4 MB recorded in the chunk)
/// the runtime stops taking receipts and delays the rest, even with gas to
/// spare.
#[test]
fn chunk_state_witness_soft_limit_delays_receipts() {
    let mut sim = sim_with(Some(Gas::from_teragas(1_000)), 8);
    for i in 0..16u8 {
        sim.set_state("c.near", &[b'b', i], Some(&vec![i; 300_000]));
    }
    let hs: Vec<_> = (0..8u8)
        .map(|i| {
            let prog = [
                Op::Write { key: vec![b'b', 2 * i], value: vec![1] },
                Op::Write { key: vec![b'b', 2 * i + 1], value: vec![1] },
            ];
            run_call(&mut sim, &format!("u{i}.near"), &prog, 50 * TGAS)
        })
        .collect();
    sim.settle();
    let outs: Vec<_> = hs.iter().map(|h| sim.outcome(*h)).collect();
    let heights: Vec<u64> = outs
        .iter()
        .map(|o| {
            o.assert_success();
            first_receipt_height(o)
        })
        .collect();
    // 8 x 0.6 MB: at most ~7 fit under 4 MB, far below the gas limit.
    assert!(heights.windows(2).all(|w| w[0] <= w[1]), "{heights:?}");
    assert!(heights[7] > heights[0], "no receipt was delayed: {heights:?}");
    let used: u64 = gas_per_height(&outs)[&heights[0]].0;
    assert!(used < 500 * TGAS, "{used}");
}

/// A yield timeout that falls due in a full chunk is skipped by the runtime
/// (timeouts are processed last, within the same gas limit) and fires in the
/// next block. Overdue, it keeps the chain busy, so `run` produces that
/// block: a chain would.
#[test]
fn yield_timeout_skipped_by_a_full_chunk_still_fires() {
    // At a gas price below `min_gas_purchase_price` every receipt refunds
    // the price difference, and that refund alone keeps the sim going. At
    // or above it, a receipt that burns all of its gas leaves nothing behind.
    let mut sim = Sim::new();
    sim.config.gas_price = Balance::from_yoctonear(1_000_000_000);
    sim.config.block_gas_limit = Some(Gas::from_teragas(1_000));
    sim.create_account("c.near", Balance::from_near(1_000_000));
    sim.deploy("c.near", fuzz_wasm());
    for i in 0..4 {
        sim.create_account(format!("u{i}.near"), Balance::from_near(10_000));
    }
    let y = sim
        .call("u0.near", "c.near", "run")
        .args(encode(&[Op::YieldCreate {
            slot: 0,
            args: encode(&[Op::Results]),
            gas: 5 * TGAS,
            weight: 1,
        }]))
        .transact();
    let expires = sim.yields()[0].expires_at;
    sim.advance_to(expires - 2);
    // Converted at expires-1; at `expires` four receipts burn all of their
    // 300 Tgas (no refunds, nothing left in flight) and fill the chunk.
    for i in 0..4 {
        run_call(&mut sim, &format!("u{i}.near"), &[Op::BurnGas(50_000)], 300 * TGAS);
    }
    sim.step();
    sim.step();
    assert_eq!(sim.height(), expires);
    assert_eq!(sim.yields().len(), 1, "the full chunk skipped the timeout");
    assert!(!sim.is_idle(), "an overdue timeout keeps the chain busy");
    sim.settle();
    assert!(sim.yields().is_empty(), "timeout never fired: {:?}", sim.yields());
    let o = sim.outcome(y.hash);
    assert!(o.is_success(), "{:?}", o.status);
    // Processed one block late; its PromiseResume runs the block after.
    assert_eq!(o.receipts.iter().find(|r| r.logs == ["n1|x"]).unwrap().block_height, expires + 2);
}

/// Many yields resumed in the same block as a heavy load: every callback
/// runs, callbacks and timeouts respect the chunk limit, nothing is lost.
#[test]
fn yield_heavy_load_under_congestion() {
    let mut sim = sim_with(Some(Gas::from_teragas(300)), 30);
    // 30 yields with user-chosen ids on c.near (ids 0..30).
    let ys: Vec<TxOutcome> = (0..30u8)
        .map(|i| {
            let cb = encode(&[Op::Results, Op::BurnGas(500)]);
            sim.call(format!("u{i}.near"), "c.near", "run")
                .args(encode(&[Op::YieldCreateWithId {
                    id: i,
                    args: cb,
                    deposit: 0,
                    gas: 20 * TGAS,
                    weight: 1,
                }]))
                .transact()
        })
        .collect();
    assert_eq!(sim.yields().len(), 30);
    // Resume the first 20 from one tx each, all in the same block, next to
    // heavy filler; the last 10 time out.
    let fill: Vec<_> = (0..10)
        .map(|i| run_call(&mut sim, &format!("u{i}.near"), &[Op::BurnGas(8_000)], 200 * TGAS))
        .collect();
    let res: Vec<_> = (0..20u8)
        .map(|i| {
            run_call(
                &mut sim,
                &format!("u{}.near", 10 + i % 20),
                &[Op::YieldResumeWithId { id: i, payload: vec![i] }],
                20 * TGAS,
            )
        })
        .collect();
    sim.settle();
    for h in fill.iter().chain(&res) {
        sim.outcome(*h).assert_success();
    }
    assert_eq!(sim.yields().len(), 10);
    sim.advance_to(sim.height() + 200);
    sim.settle();
    assert!(sim.yields().is_empty());
    for (i, y) in ys.iter().enumerate() {
        let o = sim.outcome(y.hash);
        assert!(o.is_success(), "yield {i}: {:?}", o.status);
        let cb = o
            .receipts
            .iter()
            .find(|r| r.logs.first().is_some_and(|l| l.starts_with("n1")))
            .unwrap();
        let want = if i < 20 { format!("n1|+{:02x}", i) } else { "n1|x".into() };
        assert_eq!(cb.logs[0], want, "yield {i}");
    }
}

fn is_shard_congested(o: &TxOutcome) -> bool {
    matches!(
        &o.status,
        FinalExecutionStatus::Failure(TxExecutionError::InvalidTxError(
            InvalidTxError::ShardCongested { .. }
        ))
    )
}

/// A chunk producer drops new transactions to a shard whose congestion level
/// (from the previous chunk) is at least `reject_tx_congestion_threshold`
/// (0.8); RPC answers `ShardCongested`. Incoming congestion is the delayed
/// queue's prepaid gas over `max_congestion_incoming_gas` (400 PGas): 400
/// receipts with 1000 Tgas prepaid each, behind a 10 Tgas chunk limit. A
/// relay dropped this way never ran its delegate, which keeps its nonce.
#[test]
fn congested_shard_rejects_new_transactions() {
    let n = 400;
    let mut sim = sim_with(Some(Gas::from_teragas(10)), n);
    let hs: Vec<_> =
        (0..n).map(|i| run_call(&mut sim, &format!("u{i}.near"), &[], 1_000 * TGAS)).collect();
    sim.step(); // conversion
    sim.step(); // ~10 run, the rest is delayed
    let late = run_call(&mut sim, "u0.near", &[], 10 * TGAS);
    let sign = |sim: &mut Sim| {
        let h = sim.height();
        sim.tx("u1.near", "u2.near").transfer(Balance::from_yoctonear(1)).sign_delegate(h + 100)
    };
    let delegate = sign(&mut sim);
    let relay = sim.tx("u2.near", "u1.near").action(delegate.clone()).submit();
    sim.step();
    assert!(is_shard_congested(&sim.outcome(relay)));
    assert_eq!(sign(&mut sim).delegate_action.nonce, delegate.delegate_action.nonce + 1);
    let o = sim.outcome(late);
    assert!(is_shard_congested(&o), "{:?}", o.status);
    let FinalExecutionStatus::Failure(TxExecutionError::InvalidTxError(
        InvalidTxError::ShardCongested { congestion_level, .. },
    )) = o.status
    else {
        unreachable!()
    };
    assert!(*congestion_level >= 0.8, "{congestion_level}");
    assert!(o.receipts.is_empty());
    // Everything accepted before still executes; once the queue drains the
    // shard takes transactions again.
    sim.settle();
    for h in &hs {
        sim.outcome(*h).assert_success();
    }
    let again = run_call(&mut sim, "u0.near", &[], 10 * TGAS);
    sim.settle();
    sim.outcome(again).assert_success();
}

/// A chunk takes transactions until their conversion gas reaches
/// `process_tx_limit()` (500 Tgas when uncongested); the rest waits for the
/// next block, in order.
#[test]
fn chunk_transaction_gas_budget_carries_over() {
    let n = 1_800;
    let mut sim = sim_with(None, n);
    let hs: Vec<CryptoHash> =
        (0..n).map(|i| run_call(&mut sim, &format!("u{i}.near"), &[], 5 * TGAS)).collect();
    let first = sim.height() + 1;
    sim.settle();
    let outs: Vec<_> = hs.iter().map(|h| sim.outcome(h)).collect();
    let conv_gas = outs[0].transaction.as_ref().unwrap().gas_burnt.as_gas();
    let fit = (500 * TGAS).div_ceil(conv_gas) as usize;
    assert!(fit < n);
    for (i, o) in outs.iter().enumerate() {
        o.assert_success();
        let want = if i < fit { first } else { first + 1 };
        assert_eq!(o.transaction.as_ref().unwrap().block_height, want, "tx {i}");
    }
}

/// Four transactions of exactly 1 MiB fill `combined_transactions_size_limit`
/// (4 MiB) to the byte and all fit; the next one waits.
#[test]
fn chunk_transaction_size_budget_is_inclusive() {
    check_transaction_size_budget(1 << 20, [0, 0, 0, 0, 1]);
    // A transaction exactly at its own limit is valid and still consumes
    // chunk space: only two of these fit in the first 4 MiB chunk.
    let max = near_contract_sim::Config::default()
        .runtime_config()
        .wasm_config
        .limit_config
        .max_transaction_size as usize;
    check_transaction_size_budget(max, [0, 0, 1, 1, 1]);
}

fn check_transaction_size_budget(bytes: usize, expected: [u64; 5]) {
    use near_contract_sim::nearcore::near_crypto::Signature;
    use near_contract_sim::nearcore::near_primitives::action::FunctionCallAction;
    use near_contract_sim::nearcore::near_primitives::transaction::{
        SignedTransaction, Transaction, TransactionV0,
    };
    use near_contract_sim::{Action, KeyType, PublicKey};
    let mut sim = sim_with(None, 5);
    let prog = |m: usize| encode(&[Op::YieldResume { slot: 9, payload: vec![1; m] }]);
    // Wire size of one of these transactions, as the chunk producer counts it.
    let size = |args: Vec<u8>| {
        let tx = Transaction::V0(TransactionV0 {
            nonce: 1,
            signer_id: "u0.near".parse().unwrap(),
            public_key: PublicKey::empty(KeyType::ED25519),
            receiver_id: "c.near".parse().unwrap(),
            block_hash: CryptoHash::default(),
            actions: vec![Action::FunctionCall(Box::new(FunctionCallAction {
                method_name: "run".into(),
                args,
                gas: Gas::from_teragas(50),
                deposit: Balance::ZERO,
            }))],
        });
        SignedTransaction::new(Signature::empty(KeyType::ED25519), tx)
            .size_for_limits(sim.config.protocol_version)
    };
    let (s0, s1) = (size(prog(0)), size(prog(1)));
    assert_eq!(s1, s0 + 1);
    let m = bytes - s0 as usize;
    assert_eq!(size(prog(m)), bytes as u64);
    let mut hs: Vec<_> = (0..4)
        .map(|i| {
            run_call(
                &mut sim,
                &format!("u{i}.near"),
                &[Op::YieldResume { slot: 9, payload: vec![1; m] }],
                50 * TGAS,
            )
        })
        .collect();
    hs.push(run_call(&mut sim, "u4.near", &[], 50 * TGAS));
    let first = sim.height() + 1;
    sim.settle();
    let conv: Vec<u64> = hs
        .iter()
        .map(|h| sim.outcome(*h).transaction.as_ref().unwrap().block_height - first)
        .collect();
    assert_eq!(conv, expected);
}

/// And until their size would pass `combined_transactions_size_limit` (4 MiB).
/// The one left over stays queued: it has no conversion outcome yet.
#[test]
fn chunk_transaction_size_budget_carries_over() {
    let mut sim = sim_with(None, 4);
    let prog = [Op::YieldResume { slot: 9, payload: vec![1; 1_200_000] }];
    let hs: Vec<_> =
        (0..4).map(|i| run_call(&mut sim, &format!("u{i}.near"), &prog, 50 * TGAS)).collect();
    let first = sim.height() + 1;
    sim.step();
    assert!(sim.outcome(hs[2]).transaction.is_some() && sim.outcome(hs[3]).transaction.is_none());
    sim.settle();
    let conv: Vec<u64> = hs
        .iter()
        .map(|h| {
            let o = sim.outcome(*h);
            o.assert_success();
            o.transaction.as_ref().unwrap().block_height - first
        })
        .collect();
    assert_eq!(conv, vec![0, 0, 0, 1]);
}

/// An invalid transaction can exceed the entire chunk's size budget. It
/// must reach runtime validation rather than keep the queue stuck forever.
#[test]
fn oversized_transactions_do_not_block_later_transactions() {
    for version in [84, 87] {
        let mut sim = Sim::new();
        sim.config.protocol_version = version;
        for id in ["alice.near", "bob.near"] {
            sim.create_account(id, Balance::from_near(100));
        }
        let mut large = sim.tx("alice.near", "bob.near");
        // Each action is legal on its own; their combined size is not.
        for _ in 0..5 {
            large = large.call("unused").args(vec![0; 1 << 20]).gas(Gas::from_teragas(1));
        }
        let bad = large.submit();
        let good = sim.tx("alice.near", "bob.near").transfer(Balance::from_yoctonear(1)).submit();
        sim.step();
        let out = sim.outcome(bad);
        assert!(
            matches!(
                out.status,
                FinalExecutionStatus::Failure(TxExecutionError::InvalidTxError(
                    InvalidTxError::TransactionSizeExceeded { .. }
                ))
            ),
            "{out}"
        );
        assert!(out.receipts.is_empty());
        assert!(sim.outcome(good).transaction.is_some(), "valid tail never selected");
        sim.settle();
        sim.outcome(good).assert_success();
        assert_eq!(
            sim.balance("bob.near"),
            Balance::from_near(100).checked_add(Balance::from_yoctonear(1)).unwrap()
        );
        sim.findings().assert_none();
    }
}
