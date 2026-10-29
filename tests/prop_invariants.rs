//! Property tests: random scenarios of fuzz-contract programs (nested
//! calls, callbacks, `promise_and`, transfers, account creation, yields with
//! runtime and user-chosen ids, panics, gas burning) across 3 contracts and
//! 2 users, interleaved with signed txs, same-block tx groups, views,
//! `set_state` (write and delete), `advance_to` (short and past yield
//! timeouts), `set_timestamp_ns` (incl. the past and relative moves) and
//! `set_height` (incl. backward).
//!
//! Case count: `NCS_INV_CASES` (default 16 per property). Long run:
//! `NCS_INV_CASES=2000 cargo test --release --test prop_invariants`.

mod fuzz_common;
use fuzz_common::*;
use near_contract_sim::{CryptoHash, ExecutionStatus, Sim, TxOutcome};
use proptest::prelude::*;
use std::collections::{BTreeMap, HashSet};

fn scenarios() -> BoxedStrategy<Scenario> {
    scenario_strategy("p".into(), StepConfig { sim_only: true, depth: 2 }, 10)
}

/// Height far above anything `SetHeight` generates: every yield has expired.
const END_HEIGHT: u64 = 20_000_000;

/// Run the whole scenario, then flush every pending yield.
fn run_all(sc: &Scenario) -> SimRunner {
    let mut r = SimRunner::new(sc);
    for s in &sc.steps {
        r.step(s);
    }
    flush(&mut r.sim);
    r
}

/// Time out every pending yield (timed-out callbacks may yield again, so
/// repeat; programs nest at most a few levels deep). Past every expiry the
/// timeout queue's head is overdue, however `SetHeight` ordered it, so the
/// first `run` pops the whole queue.
fn flush(sim: &mut Sim) {
    sim.set_height(END_HEIGHT.max(sim.height()));
    for _ in 0..8 {
        sim.settle();
        match sim.yields().iter().map(|y| y.expires_at).max() {
            Some(h) => sim.advance_to(h),
            None => return,
        }
    }
}

fn fingerprint(r: &SimRunner) -> Vec<String> {
    let mut v = vec![r.sim.state_root().to_string()];
    for h in r.roots() {
        let o = r.sim.outcome(h);
        v.push(format!("{h} {:?}", o.status));
        for x in o.transaction.iter().chain(&o.receipts) {
            v.push(format!(
                "  {} {} h{} t{} g{} b{} {:?} {:?} {:?}",
                x.receipt_id,
                x.receiver_id,
                x.block_height,
                x.block_timestamp_ns,
                x.gas_burnt.as_gas(),
                x.tokens_burnt.as_yoctonear(),
                x.logs,
                x.status,
                x.receipt_ids
            ));
        }
    }
    v.push(format!("{:?}", r.sim.yields()));
    v
}

/// Determinism: the same scenario run twice in fresh simulators gives an
/// identical state root and identical outcomes (ids, heights, timestamps,
/// gas, tokens, logs, statuses) and pending-yield sets.
#[test]
fn prop_determinism() {
    runner()
        .run(&scenarios(), |sc| {
            let a = fingerprint(&run_all(&sc));
            let b = fingerprint(&run_all(&sc));
            prop_assert_eq!(a, b);
            Ok(())
        })
        .unwrap();
}

fn total_balance(sim: &Sim, names: &Names) -> u128 {
    names
        .all_accounts()
        .iter()
        .filter_map(|a| sim.account(a))
        .map(|a| a.amount.as_yoctonear() + a.locked.as_yoctonear())
        .sum()
}

fn all_outcomes(sim: &Sim, roots: &[CryptoHash]) -> Vec<TxOutcome> {
    roots.iter().map(|h| sim.outcome(h)).collect()
}

fn tokens_burnt(outs: &[TxOutcome]) -> u128 {
    outs.iter().map(|o| o.tokens_burnt().as_yoctonear()).sum()
}

/// Receipt ids referenced by some outcome of the tree but never executed.
fn unexecuted(o: &TxOutcome) -> Vec<CryptoHash> {
    let executed: HashSet<CryptoHash> =
        o.transaction.iter().chain(&o.receipts).map(|r| r.receipt_id).collect();
    o.transaction
        .iter()
        .chain(&o.receipts)
        .flat_map(|r| r.receipt_ids.iter().copied())
        .filter(|id| !executed.contains(id))
        .collect()
}

/// The end of `o`'s result path: following `SuccessReceiptId` from its
/// conversion to the first outcome that returned no promise.
fn result_end(o: &TxOutcome) -> CryptoHash {
    let all: Vec<_> = o.transaction.iter().chain(&o.receipts).collect();
    let mut end = o.hash;
    while let Some(ExecutionStatus::SuccessReceiptId(next)) =
        all.iter().find(|r| r.receipt_id == end).map(|r| &r.status)
    {
        end = *next;
    }
    end
}

/// Step-by-step invariants (checked after every step, i.e. at quiescence):
///
/// - **Token conservation.** Whenever nothing is in flight and no yield is
///   parked, `Σ(amount + locked)` over every account the scenario can touch
///   has changed by exactly the sum of `tokens_burnt` over every outcome
///   (conversion and receipts) of every transaction: an oracle of its own
///   beside `check()`'s conservation, which reads the runtime's balance
///   stats.
/// - **Outcome completeness.** With no pending yields every receipt id listed
///   by any outcome has an outcome and no transaction is pending; with
///   pending yields, the number of listed-but-unexecuted receipts is at least
///   the number of pending yields and a transaction is pending only then.
/// - **Pending yields** are parked on scenario contracts, with the `run`
///   callback, and expire exactly `yield_timeout_length_in_blocks` after
///   a height at which a receipt ran on that account.
/// - **Views are pure**: a view never changes the state root, and `digest`
///   equals the direct `view_state` reads of `k0..k7`.
/// - **storage_usage** of each contract = its usage right after setup
///   + Σ(num_extra_bytes_record + |key| + |value|) over its `view_state`.
/// - **Clock model.** Every logged `block_timestamp`/`block_height`
///   (`e0`/`e1`) equals the executing receipt's block timestamp/height; the
///   first receipt of a tx runs in the conversion block if signer == receiver
///   (local receipt), else in the next block; views see `sim.height()` /
///   `sim.timestamp_ns()`, and a tx converts in block `height() + 1`; the
///   epoch height (`e2`) is the executing block's epoch.
/// - **Gas.** Conversion: `tokens_burnt == gas_burnt * gas_price`. Every
///   non-refund receipt: `gas_burnt > 0`, `tokens_burnt >= gas_burnt *
///   gas_price` and the gas profile sums to at most `gas_burnt` (it omits the
///   exec fees of the receipt's own actions, like RPC). Refund receipts:
///   `tokens_burnt == 0`.
/// - **Unhandled failures.** Once every yield is flushed, an `unhandled`
///   receipt failed and doesn't end its transaction's result path. The log
///   decided it when the receipt ran, before the rest of that path had, so
///   this checks the answer against the finished tree. (Whether a callback
///   receives the failure is fixed by the receipt's body when it runs;
///   `tests/invariants.rs` covers each way.)
#[test]
fn prop_step_invariants() {
    runner()
        .run(&scenarios(), |sc| {
            let mut r = SimRunner::new(&sc);
            let start = total_balance(&r.sim, &sc.names);
            let base_usage: BTreeMap<String, u64> = sc
                .names
                .contracts
                .iter()
                .map(|c| (c.clone(), r.sim.account(c).unwrap().storage_usage))
                .collect();
            let steps: Vec<Option<&Step>> = sc.steps.iter().map(Some).chain([None]).collect();
            let mut tx_heights: BTreeMap<CryptoHash, (u64, bool)> = BTreeMap::new();
            // A height `SetHeight` pinned that no block or transaction spent.
            let mut pinned: Option<u64> = None;
            for (i, s) in steps.into_iter().enumerate() {
                let root_before = r.sim.state_root();
                let (h_before, t_before) = (r.sim.height(), r.sim.timestamp_ns());
                let out = match s {
                    Some(s) => r.step(s),
                    None => {
                        flush(&mut r.sim);
                        StepOut::None
                    }
                };
                let ctx = format!("step {i}: {s:?}");
                // Record conversion heights of new txs. A pinned height is
                // the first one's first receipt's: it converts there, or a
                // block before for a call to another account.
                let txs: Vec<(bool, CryptoHash)> = match (s, &out) {
                    (Some(Step::Tx { signer, receiver, .. }), StepOut::Tx(hs)) => {
                        vec![(signer == receiver, hs[0])]
                    }
                    (Some(Step::TxGroup { txs }), StepOut::Tx(hs)) => {
                        txs.iter().zip(hs).map(|((sg, rc, _), h)| (sg == rc, *h)).collect()
                    }
                    _ => vec![],
                };
                let converts = match (pinned, txs.first()) {
                    (Some(h), Some((local, _))) => h - u64::from(!local) - 1,
                    _ => h_before,
                } + 1;
                for (local, h) in &txs {
                    tx_heights.insert(*h, (converts, *local));
                }
                pinned = match s {
                    Some(Step::SetHeight { height }) if r.sim.height() == *height => Some(*height),
                    _ if txs.is_empty() && r.sim.height() == h_before => pinned,
                    _ => None,
                };
                // Views.
                if let StepOut::View(v) = &out {
                    prop_assert_eq!(
                        root_before,
                        r.sim.state_root(),
                        "view changed state root; {}",
                        ctx
                    );
                    for l in v.logs() {
                        if let Some(x) = l.strip_prefix("e0:") {
                            prop_assert_eq!(
                                u64::from_str_radix(x, 16).unwrap(),
                                t_before,
                                "view ts; {}",
                                ctx
                            );
                        }
                        if let Some(x) = l.strip_prefix("e1:") {
                            prop_assert_eq!(
                                u64::from_str_radix(x, 16).unwrap(),
                                h_before,
                                "view height; {}",
                                ctx
                            );
                        }
                    }
                    if let Some(Step::Digest { account }) = s {
                        let got: Vec<Option<Vec<u8>>> =
                            borsh::from_slice(v.result.as_ref().unwrap()).unwrap();
                        let direct: BTreeMap<Vec<u8>, Vec<u8>> =
                            r.sim.view_state(account, b"k").into_iter().collect();
                        let want: Vec<Option<Vec<u8>>> =
                            (0..8u8).map(|i| direct.get(&vec![b'k', b'0' + i]).cloned()).collect();
                        prop_assert_eq!(got, want, "digest != view_state; {}", ctx);
                    }
                }

                let outs = all_outcomes(&r.sim, r.roots());
                let pending = r.sim.yields();
                prop_assert!(r.sim.is_idle(), "not idle after step; {}", ctx);

                // Completeness.
                let unexec: usize = outs.iter().map(|o| unexecuted(o).len()).sum();
                if pending.is_empty() {
                    prop_assert_eq!(
                        unexec,
                        0,
                        "unexecuted receipts without pending yields; {}",
                        ctx
                    );
                    for o in &outs {
                        prop_assert!(
                            !o.is_pending(),
                            "Pending without pending yields: {}; {}",
                            o.hash,
                            ctx
                        );
                    }
                } else {
                    prop_assert!(
                        unexec >= pending.len(),
                        "{} unexecuted < {} pending; {}",
                        unexec,
                        pending.len(),
                        ctx
                    );
                }
                // A receipt changes only its receiver's storage, as curves
                // read it.
                for o in &outs {
                    for c in &o.effects().storage {
                        let r = o.receipts.iter().find(|r| r.receipt_id == c.receipt_id);
                        let receiver = r.map(|r| &r.receiver_id);
                        prop_assert_eq!(receiver, Some(&c.account), "{:?}; {}", c, ctx);
                    }
                }
                // Pending yields.
                let limits = &r.sim.config.runtime_config().wasm_config.limit_config;
                let timeout = limits.yield_timeout_length_in_blocks;
                for y in &pending {
                    prop_assert!(
                        sc.names.contracts.contains(&y.receiver_id.to_string()),
                        "{:?}",
                        y
                    );
                    prop_assert_eq!(&y.callback_methods, &vec!["run".to_string()], "{:?}", y);
                    let created_at = y.expires_at - timeout;
                    let ran_there = outs.iter().flat_map(|o| &o.receipts).any(|x| {
                        x.receiver_id == y.receiver_id
                            && x.block_height == created_at
                            && !x.is_refund()
                    });
                    prop_assert!(
                        ran_there,
                        "yield {:?} expires_at-timeout matches no receipt; {}",
                        y,
                        ctx
                    );
                }

                // Conservation.
                if pending.is_empty() {
                    let now = total_balance(&r.sim, &sc.names) as i128;
                    let expect = start as i128 - tokens_burnt(&outs) as i128;
                    prop_assert_eq!(now, expect, "conservation (Δ={}); {}", now - expect, ctx);
                }

                // storage_usage.
                let extra =
                    r.sim.config.runtime_config().fees.storage_usage_config.num_extra_bytes_record;
                for c in &sc.names.contracts {
                    let data: u64 = r
                        .sim
                        .view_state(c, b"")
                        .iter()
                        .map(|(k, v)| extra + (k.len() + v.len()) as u64)
                        .sum();
                    let acc = r.sim.account(c).unwrap();
                    prop_assert_eq!(
                        acc.storage_usage,
                        base_usage[c] + data,
                        "storage_usage of {}; {}",
                        c,
                        ctx
                    );
                }
            }

            // Clock model + gas, over all final outcomes.
            let price = r.sim.config.gas_price.as_yoctonear();
            let epoch_length = r.sim.config.epoch_length;
            for o in all_outcomes(&r.sim, r.roots()) {
                if let Some(t) = &o.transaction {
                    prop_assert_eq!(
                        t.tokens_burnt.as_yoctonear(),
                        t.gas_burnt.as_gas() as u128 * price
                    );
                    if let Some((h, local)) = tx_heights.get(&o.hash) {
                        prop_assert_eq!(t.block_height, *h);
                        if let ExecutionStatus::SuccessReceiptId(first) = &t.status {
                            let first = o.receipts.iter().find(|x| x.receipt_id == *first).unwrap();
                            prop_assert_eq!(
                                first.block_height,
                                h + if *local { 0 } else { 1 },
                                "first receipt height"
                            );
                        }
                    }
                }
                for x in &o.receipts {
                    for l in &x.logs {
                        if let Some(v) = l.strip_prefix("e0:") {
                            prop_assert_eq!(
                                u64::from_str_radix(v, 16).unwrap(),
                                x.block_timestamp_ns
                            );
                        }
                        if let Some(v) = l.strip_prefix("e1:") {
                            prop_assert_eq!(u64::from_str_radix(v, 16).unwrap(), x.block_height);
                        }
                        if let Some(v) = l.strip_prefix("e2:") {
                            let epoch = x.block_height.saturating_sub(1) / epoch_length + 1;
                            prop_assert_eq!(u64::from_str_radix(v, 16).unwrap(), epoch);
                        }
                    }
                    if x.is_refund() {
                        prop_assert_eq!(x.tokens_burnt.as_yoctonear(), 0, "refund tokens_burnt");
                        continue;
                    }
                    prop_assert!(x.gas_burnt.as_gas() > 0, "zero gas receipt {:?}", x);
                    prop_assert!(
                        x.tokens_burnt.as_yoctonear() >= x.gas_burnt.as_gas() as u128 * price,
                        "tokens_burnt < gas_burnt*price: {:?}",
                        x
                    );
                    // The profile leaves out the exec fees of the receipt's
                    // own actions (as RPC does), so it can only be smaller.
                    let sum: u64 = x.gas_profile.values().sum();
                    prop_assert!(
                        sum <= x.gas_burnt.as_gas(),
                        "gas profile sum > gas_burnt; {:?}",
                        x
                    );
                }
            }
            // Everything flushed at the end.
            prop_assert!(r.sim.yields().is_empty());
            for o in all_outcomes(&r.sim, r.roots()) {
                prop_assert!(!o.is_pending(), "{:?}", o.status);
                let end = result_end(&o);
                for x in o.receipts.iter().filter(|x| x.unhandled) {
                    prop_assert!(x.failure().is_some() && x.receipt_id != end, "{}", o);
                }
            }
            Ok(())
        })
        .unwrap();
}

/// `headroom()` is the gas nearcore refunds a successful receipt: its
/// attached gas and prepaid exec fees less the gas it used, which counts
/// what it prepaid the receipts it created (`refund_unspent_gas_and_deposits`).
/// At a gas price no lower than `min_gas_purchase_price`, gas is bought and
/// burnt at the same price, so the refund receipts carry exactly
/// `headroom() * gas_price`. Random scenarios cover nested calls with and
/// without gas weights, callbacks, `promise_and`, transfers, account
/// creation and yields.
#[test]
fn prop_headroom_is_the_gas_refund() {
    runner()
        .run(&scenarios(), |sc| {
            let mut sim = Sim::new();
            sim.config.gas_price = sim.config.runtime_config().min_gas_purchase_price;
            let price = sim.config.gas_price.as_yoctonear();
            let mut r = SimRunner::with_sim(sim, &sc);
            for s in &sc.steps {
                r.step(s);
            }
            flush(&mut r.sim);
            for o in all_outcomes(&r.sim, r.roots()) {
                for x in o.receipts.iter().filter(|x| x.is_success() && !x.is_refund()) {
                    let refunded: u128 = o
                        .receipts
                        .iter()
                        .filter(|c| c.is_refund() && x.receipt_ids.contains(&c.receipt_id))
                        .map(|c| c.deposit.as_yoctonear())
                        .sum();
                    prop_assert_eq!(refunded, x.headroom().as_gas() as u128 * price, "{}", o);
                }
            }
            Ok(())
        })
        .unwrap();
}

/// BurnGas rounds of congested programs in property tests (~2, ~8 and
/// ~25 Tgas), against a small chunk gas limit so that bursts overfill chunks
/// without costing much CPU.
const PROP_BURNS: &[u32] = &[100, 500, 1_500];

/// Invariants under congestion: random bursts of heavy transactions (plus
/// nested calls, callbacks, yields, storage) against a chunk gas limit of
/// 10, 30 or 80 Tgas, so receipts spill into the delayed queue and yield
/// timeouts land in full chunks.
///
/// - `run` terminates after every step and leaves the sim idle;
/// - determinism (two runs give the same fingerprint);
/// - completeness and token conservation (as in `prop_step_invariants`);
/// - every chunk stays within the limit up to its last receipt: at every
///   height, the gas of all outcomes minus the largest one is below the
///   limit (the runtime checks the limit before each receipt, and compute
///   usage is never below gas);
/// - FIFO: within a burst, the first receipts of non-local transactions
///   run at non-decreasing heights in submission order.
#[test]
fn prop_congested_invariants() {
    let names = Names::new("g", 3, 6);
    let strat = (
        congested_scenario_strategy_under(names.clone(), PROP_BURNS, 6),
        prop::sample::select(vec![10u64, 30, 80]),
    );
    runner()
        .run(&strat, |(sc, limit_tgas)| {
            let limit = near_contract_sim::Gas::from_teragas(limit_tgas);
            let run = || {
                let mut sim = Sim::new();
                sim.config.block_gas_limit = Some(limit);
                let mut r = SimRunner::with_sim(sim, &sc);
                let start = total_balance(&r.sim, &sc.names);
                let mut groups = vec![];
                for s in &sc.steps {
                    let out = r.step(s);
                    assert!(r.sim.is_idle());
                    if let (Step::TxGroup { txs }, StepOut::Tx(hs)) = (s, out) {
                        let local: Vec<bool> = txs.iter().map(|(a, b, _)| a == b).collect();
                        groups.push((hs, local));
                    }
                }
                flush(&mut r.sim);
                (r, start, groups)
            };
            let (r, start, groups) = run();
            prop_assert_eq!(fingerprint(&r), fingerprint(&run().0));

            let outs = all_outcomes(&r.sim, r.roots());
            prop_assert!(r.sim.yields().is_empty());
            for o in &outs {
                prop_assert!(unexecuted(o).is_empty(), "unexecuted receipts in {}", o.hash);
                prop_assert!(!o.is_pending(), "{:?}", o.status);
            }
            let now = total_balance(&r.sim, &sc.names) as i128;
            let expect = start as i128 - tokens_burnt(&outs) as i128;
            prop_assert_eq!(now, expect, "conservation (Δ={})", now - expect);

            let mut per_height: BTreeMap<u64, (u64, u64)> = BTreeMap::new();
            for x in outs.iter().flat_map(|o| o.transaction.iter().chain(&o.receipts)) {
                let e = per_height.entry(x.block_height).or_default();
                e.0 += x.gas_burnt.as_gas();
                e.1 = e.1.max(x.gas_burnt.as_gas());
            }
            for (h, (sum, max)) in per_height {
                prop_assert!(sum - max < limit.as_gas(), "height {}: {} gas", h, sum);
            }

            for (hs, local) in groups {
                let heights: Vec<u64> = hs
                    .iter()
                    .zip(local)
                    .filter(|(_, l)| !l)
                    .filter_map(|(h, _)| {
                        let o = r.sim.outcome(h);
                        let ExecutionStatus::SuccessReceiptId(first) =
                            &o.transaction.as_ref()?.status
                        else {
                            return None;
                        };
                        o.receipts.iter().find(|x| x.receipt_id == *first).map(|x| x.block_height)
                    })
                    .collect();
                prop_assert!(heights.windows(2).all(|w| w[0] <= w[1]), "not FIFO: {:?}", heights);
            }
            Ok(())
        })
        .unwrap();
}
