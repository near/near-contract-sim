//! Differential test: random scenarios run in the simulator and in a real
//! `neard` sandbox, then compared. The sandbox side is a JSON-RPC runner
//! (`differential/sandbox.rs`), which also runs in process against
//! `SimTransport`: `differential_vs_transport` compares that with the
//! simulator's direct API on every `cargo test --all-features`.
//!
//! `differential_vs_sandbox` is ignored by default; it needs a running
//! sandbox (the `nearprotocol/sandbox` Docker image matching the pinned
//! nearcore, i.e. 2.14.0):
//!
//! ```sh
//! tests/differential/run.sh 40 1
//! ```
//!
//! The runner keeps 100 epochs of history: the image's default 5 can prune
//! a transaction before a later transaction's yield timeout resumes its
//! callback. For an existing sandbox, set `gc_num_epochs_to_keep` to 100
//! and restart it before running with `NCS_SANDBOX_RPC`.
//!
//! Other knobs:
//! `NCS_DIFF_CASES` (random scenarios, default 20), `NCS_DIFF_SEED`
//! (default 1), `NCS_DIFF_CONGESTED` (random congested scenarios, default 2),
//! `NCS_DIFF_FIXED` (`0` leaves out the fixed scenarios, to replay an older
//! seed's order; the feature ones below have `NCS_DIFF_FEATURES`),
//! `NCS_DIFF_POPULATE` (mainnet-like accounts patched in first, default 0),
//! `NCS_DIFF_SCENARIOS` (replay a saved `scenarios.json`), `NCS_DIFF_ONLY`
//! (rerun scenario indices, `3,17`), `NCS_DIFF_DIR` (where results go),
//! `NCS_SANDBOX_ROOT` (root account, default `sandbox`),
//! `NCS_SANDBOX_ROOT_KEY` (default: the image's deterministic key, seed =
//! root account id), `NCS_SANDBOX_GENESIS_ACCOUNTS` (default `sandbox,near`).
//! `run.sh` takes `NCS_SANDBOX_IMAGE_TAG` and `NCS_SANDBOX_PORT`.
//!
//! Scenarios come from the property-test strategies, restricted to steps both
//! sides support, and run one after another on the same sandbox chain. The
//! simulator mirrors that: one `Sim` holding the sandbox's genesis accounts
//! runs every scenario in the same order. Trie-node gas depends on every key
//! in the trie, so both tries need the same key set for exact gas.
//!
//! Compared, per scenario:
//! - account views right after setup (amount, locked, storage_usage, code hash);
//! - per step: the tx final status at quiescence, view results/logs/errors;
//! - per tx after the final wait: final status and the whole receipt
//!   tree (DFS order from the tx outcome): executor, gas_burnt, tokens_burnt,
//!   logs, status, children, full gas profile, and the block-height delta to
//!   the parent receipt (counted, not compared, for callbacks that waited
//!   for a yield or dependency: the wait spans however many blocks the
//!   sandbox made between steps, and so does which refunds of other
//!   transactions an `account_balance` read in such a callback sees);
//! - final account views and contract state (`view_state`).
//!
//! Normalized away (legitimately chain-specific): logged block timestamp,
//! height, epoch height and random seed; receipt ids (replaced by tree
//! indices); the runtime-generated yield data ids stored under `Y*` keys.
//!
//! After the random scenarios, hand-written ones cover gas keys, global
//! contracts, NEP-616 state inits, meta transactions and secp256k1 /
//! ML-DSA-65 keys (see `differential/features.rs`); `NCS_DIFF_FEATURES=0`
//! skips them.

mod fuzz_common;
use fuzz_common::*;
use near_contract_sim::{Balance, Failure, KeyType, SecretKey, Sim};
use proptest::strategy::{Strategy, ValueTree};
use proptest::test_runner::{Config, RngAlgorithm, TestRng, TestRunner};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::PathBuf;

#[path = "differential/sandbox.rs"]
mod sandbox;

#[path = "differential/features.rs"]
mod features;

fn env_or(k: &str, d: &str) -> String {
    std::env::var(k).unwrap_or_else(|_| d.to_string())
}

/// Blocks to wait at the end of a scenario that left yields parked (> the
/// 200-block yield timeout), so they time out on both sides. A real wait,
/// not `sandbox_fast_forward`: after fast-forwards the sandbox soon reports
/// earlier txs as UNKNOWN_TRANSACTION (the image's 60-block epochs and GC),
/// and the simulator fires each timeout in its own block too, so timeouts
/// and the receipts they spawn interleave the same way on both sides.
const FINAL_WAIT: u64 = 210;

/// `n` random scenarios, then `congested` random congested ones (bursts of
/// heavy transactions overfilling chunks), then, with `fixed`, the
/// hand-written ones of `fixed_scenarios`.
pub fn generate(n: usize, congested: usize, fixed: bool, seed: u64, root: &str) -> Vec<Scenario> {
    let mut seed_bytes = [0u8; 32];
    seed_bytes[..8].copy_from_slice(&seed.to_le_bytes());
    let mut runner = TestRunner::new_with_rng(
        Config::default(),
        TestRng::from_seed(RngAlgorithm::ChaCha, &seed_bytes),
    );
    let mut out: Vec<Scenario> = (0..n)
        .map(|i| {
            let strat = scenario_strategy_under(
                Names::under(&format!("d{seed}x{i}"), root, 3, 2),
                StepConfig { sim_only: false, depth: 2 },
                8,
            );
            strat.new_tree(&mut runner).unwrap().current()
        })
        .collect();
    for i in 0..congested {
        // ~70, ~135 or ~250 Tgas of BurnGas per transaction, 6-15 per burst
        // against the 1000 Tgas chunk limit.
        let strat = congested_scenario_strategy_under(
            Names::under(&format!("d{seed}g{i}"), root, 3, 6),
            &[4_000, 8_000, 15_000],
            4,
        );
        out.push(strat.new_tree(&mut runner).unwrap().current());
    }
    if fixed {
        out.insert(0, late_yield_scenario(seed, root));
        out.extend(fixed_scenarios(seed, root));
    }
    for sc in &mut out {
        sc.steps.push(Step::Wait { blocks: FINAL_WAIT });
    }
    out
}

fn call(ops: &[Op], gas: u64) -> Vec<TxAction> {
    vec![TxAction::Call { program_hex: hex(&encode(ops)), gas, deposit: "0".into() }]
}

/// A later transaction's timeout resumes a yield created by an older
/// transaction's timeout. The old transaction must remain queryable past
/// the sandbox image's default 240-300 block history window.
fn late_yield_scenario(seed: u64, root: &str) -> Scenario {
    let names = Names::under(&format!("d{seed}h"), root, 1, 1);
    let tx = |ops: &[Op]| Step::Tx {
        signer: names.users[0].clone(),
        receiver: names.contracts[0].clone(),
        actions: call(ops, 100 * TGAS),
    };
    let late = Op::YieldCreate {
        slot: 0,
        args: encode(&[Op::Log("late callback".into())]),
        gas: 10 * TGAS,
        weight: 0,
    };
    let first =
        tx(&[Op::YieldCreate { slot: 1, args: encode(&[late]), gas: 40 * TGAS, weight: 0 }]);
    let second = tx(&[Op::YieldCreateWithId {
        id: 0,
        args: encode(&[Op::YieldResume { slot: 0, payload: vec![] }]),
        deposit: 0,
        gas: 10 * TGAS,
        weight: 0,
    }]);
    Scenario::new(names, vec![first, Step::Wait { blocks: 100 }, second])
}

#[test]
fn late_yield_callback_outlives_the_default_history_window() {
    let mut sc = late_yield_scenario(1, "test.near");
    sc.steps.push(Step::Wait { blocks: FINAL_WAIT });
    let (_, result) = run_sim(Sim::new(), &mut sc);
    let tx = &result["txs"][0];
    let first = tx["transaction_outcome"]["block_height"].as_u64().unwrap();
    let late = tx["receipts_outcome"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["outcome"]["logs"] == json!(["late callback"]))
        .expect("the old transaction's callback ran");
    assert!(late["block_height"].as_u64().unwrap() - first > 300);
}

/// Hand-written overfull-chunk scenarios:
///
/// - state-witness limits: 16 x 300 kB of contract state, then one receipt
///   rewriting 14 of those values (storage_write reads the evicted value:
///   4.2 MB, over `per_receipt_storage_proof_size_limit`, fails on both
///   sides), then a burst of 8 receipts each rewriting 2 (0.6 MB each, past
///   the chunk's 4 MB soft limit together);
/// - receipt bytes: 6 transactions of ~1.2 MB (3 fit a chunk's 4 MiB of
///   transactions), each forwarding its payload to another contract, past
///   the 4.5 MB per-block bandwidth.
pub fn fixed_scenarios(seed: u64, root: &str) -> Vec<Scenario> {
    let names = Names::under(&format!("d{seed}w"), root, 1, 8);
    let c = names.contracts[0].clone();
    let mut steps: Vec<Step> = (0..4u8)
        .map(|t| {
            let ops: Vec<Op> = (0..4u8)
                .map(|j| Op::Write { key: vec![b'b', 4 * t + j], value: vec![t; 300_000] })
                .collect();
            Step::Tx {
                signer: names.users[t as usize].clone(),
                receiver: c.clone(),
                actions: call(&ops, 300 * TGAS),
            }
        })
        .collect();
    let ops: Vec<Op> =
        (0..14u8).map(|i| Op::Write { key: vec![b'b', i], value: vec![1] }).collect();
    steps.push(Step::Tx {
        signer: names.users[4].clone(),
        receiver: c.clone(),
        actions: call(&ops, 300 * TGAS),
    });
    let txs = (0..8u8)
        .map(|i| {
            let ops = [
                Op::Write { key: vec![b'b', 2 * i], value: vec![2] },
                Op::Write { key: vec![b'b', 2 * i + 1], value: vec![2] },
            ];
            (names.users[i as usize].clone(), c.clone(), call(&ops, 100 * TGAS))
        })
        .collect();
    steps.push(Step::TxGroup { txs });
    let witness = Scenario {
        start_balance: (200 * NEAR).to_string(),
        congested: true,
        ..Scenario::new(names, steps)
    };

    let names = Names::under(&format!("d{seed}b"), root, 2, 6);
    let txs = (0..6u8)
        .map(|i| {
            let inner = encode(&[Op::YieldResume { slot: 9, payload: vec![i; 1_200_000] }]);
            let ops = [Op::Call {
                to: names.contracts[1].clone(),
                args: inner,
                deposit: 0,
                gas: 30 * TGAS,
                weight: 1,
            }];
            (names.users[i as usize].clone(), names.contracts[0].clone(), call(&ops, 300 * TGAS))
        })
        .collect();
    let bytes = Scenario { congested: true, ..Scenario::new(names, vec![Step::TxGroup { txs }]) };
    vec![witness, bytes]
}

/// Calls into the populated big contract (`NCS_DIFF_POPULATE`): trie-node
/// gas on a mainnet-shaped trie must match exactly.
pub fn trie_scenario(seed: u64, root: &str, big: &str, n: usize) -> Scenario {
    let names = Names::under(&format!("d{seed}t"), root, 0, 1);
    let u = names.users[0].clone();
    let progs: Vec<Vec<Op>> = vec![
        vec![Op::Read { key: big_contract_holder_key(big, n, 0) }],
        vec![Op::Write { key: big_contract_holder_key(big, n, 1), value: vec![5; 16] }],
        (0..8u64)
            .map(|i| {
                let mut key = b"v".to_vec();
                key.extend((i * 97 % n.max(1) as u64).to_le_bytes());
                Op::Write { key, value: vec![6; 64] }
            })
            .collect(),
        vec![
            Op::Read { key: b"missing".to_vec() },
            Op::Remove { key: big_contract_holder_key(big, n, 2) },
        ],
    ];
    let mut steps: Vec<Step> = progs
        .iter()
        .map(|p| Step::Tx {
            signer: u.clone(),
            receiver: big.to_string(),
            actions: call(p, 100 * TGAS),
        })
        .collect();
    steps.push(Step::Wait { blocks: FINAL_WAIT });
    Scenario::new(names, steps)
}

fn account_json(sim: &Sim, id: &str) -> Value {
    match sim.account(id) {
        None => Value::Null,
        Some(a) => json!({
            "amount": a.amount.as_yoctonear().to_string(),
            "locked": a.locked.as_yoctonear().to_string(),
            "storage_usage": a.storage_usage,
            "code_hash": a.code_hash.to_string(),
        }),
    }
}

/// Run one scenario in `sim` (which keeps going across scenarios, like the
/// sandbox chain); same result shape as the sandbox runner.
/// The final `Wait` shrinks to 2 blocks when no yield is parked (the sandbox
/// then gets the same, shorter scenario).
pub fn run_sim(sim: Sim, sc: &mut Scenario) -> (Sim, Value) {
    let mut r = SimRunner::with_sim(sim, sc);
    let setup: BTreeMap<String, Value> =
        sc.names.signers().iter().map(|a| (a.clone(), account_json(&r.sim, a))).collect();
    let mut steps = vec![];
    let last = sc.steps.len() - 1;
    for (i, s) in sc.steps.clone().iter().enumerate() {
        let s = if i == last && r.sim.yields().is_empty() {
            sc.steps[last] = Step::Wait { blocks: 2 };
            &sc.steps[last]
        } else {
            s
        };
        let out = r.step(s);
        steps.push(match (s, out) {
            (_, StepOut::Tx(hs)) => {
                let status = json!(r.sim.outcome(hs[0]).status);
                json!({"kind": "Tx", "status": norm_final_status(&status)})
            }
            (s, StepOut::View(v)) => {
                let kind = if matches!(s, Step::Digest { .. }) { "Digest" } else { "View" };
                match &v.result {
                    Ok(b) => json!({"kind": kind, "result": b64(b), "logs": v.logs()}),
                    Err(f) => json!({"kind": kind, "error": vm_error(f)}),
                }
            }
            (s, StepOut::None) => json!({"kind": serde_json::to_value(s).unwrap()["kind"]}),
        });
    }
    let txs: Vec<Value> = r.roots().iter().map(|h| sim_tx_json(&r.sim.outcome(h))).collect();
    let accounts: BTreeMap<String, Value> =
        sc.names.all_accounts().iter().map(|a| (a.clone(), account_json(&r.sim, a))).collect();
    let state: BTreeMap<String, Value> = sc
        .names
        .contracts
        .iter()
        .map(|c| {
            let kv: Vec<Value> = r
                .sim
                .view_state(c.as_str(), b"")
                .iter()
                .map(|(k, v)| json!([b64(k), b64(v)]))
                .collect();
            (c.clone(), Value::Array(kv))
        })
        .collect();
    let v = json!({"setup_accounts": setup, "steps": steps, "txs": txs, "accounts": accounts, "state": state});
    (r.sim, v)
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct Mismatch {
    pub scenario: String,
    pub what: String,
    pub sim: Value,
    pub sandbox: Value,
}

fn norm_state(v: &Value) -> Value {
    let Some(a) = v.as_array() else { return v.clone() };
    let mut m = BTreeMap::new();
    for kv in a {
        let k = unb64(kv[0].as_str().unwrap());
        let val = unb64(kv[1].as_str().unwrap());
        let shown = if k.first() == Some(&b'Y') {
            format!("<data id, {} bytes>", val.len())
        } else {
            hex(&val)
        };
        m.insert(hex(&k), shown);
    }
    json!(m)
}

/// A view's `Failure` in the words of nearcore's `VMError`, which the
/// sandbox reports.
fn vm_error(f: &Failure) -> String {
    match f {
        Failure::Panic(m) => format!("GuestPanic {{ panic_msg: {m:?} }}"),
        Failure::OutOfGas => "GasExceeded".into(),
        Failure::Host(m) => match m.strip_suffix(" is not allowed in view calls") {
            Some(name) => format!("ProhibitedInView {{ method_name: {name:?} }}"),
            None => m.clone(),
        },
        f => format!("{f:?}"),
    }
}

/// Pull the error kind out of a view error (sim: `vm_error`; sandbox: RPC
/// error string), e.g. `ProhibitedInView { method_name: "storage_write" }`.
fn view_error_kind(e: &str) -> String {
    for pat in [
        "ProhibitedInView",
        "GuestPanic",
        "GasExceeded",
        "GasLimitExceeded",
        "MethodNotFound",
        "InvalidAccountId",
    ] {
        if let Some(i) = e.find(pat) {
            let rest = &e[i..];
            let end = rest.find(')').unwrap_or(rest.len());
            return rest[..end].replace('\\', "").to_string();
        }
    }
    e.to_string()
}

fn norm_view(v: &Value) -> Value {
    if let Some(e) = v.get("error") {
        let s = e.as_str().map(str::to_string).unwrap_or_else(|| e.to_string());
        return json!({"error": view_error_kind(&s)});
    }
    let logs: Vec<String> = v["logs"]
        .as_array()
        .map_or(vec![], |a| a.iter().map(|l| normalize_log(l.as_str().unwrap())).collect());
    json!({"result": v["result"], "logs": logs})
}

const TRIE_NODE_COSTS: [&str; 2] = ["TOUCHING_TRIE_NODE", "READ_CACHED_TRIE_NODE"];

/// The two receipts' gas differs only by trie-node charges: every other
/// profile entry is equal and the `gas_burnt` difference is the trie-node
/// difference. That is what running in different chunks does: nodes already
/// touched in the chunk are charged as cached (the same 2.28 Ggas since
/// protocol 82, so only the profile split moves), and a value written
/// earlier in the same chunk is read back without touching the trie at all.
fn trie_nodes_only(u: &NormNode, v: &NormNode) -> bool {
    let split = |n: &NormNode| {
        let (mut trie, mut rest) = (0i128, BTreeMap::new());
        for (k, g) in &n.gas_profile {
            if TRIE_NODE_COSTS.contains(&k.as_str()) {
                trie += *g as i128;
            } else {
                rest.insert(k.clone(), *g);
            }
        }
        (trie, rest)
    };
    let ((tu, ru), (tv, rv)) = (split(u), split(v));
    ru == rv && u.gas_burnt as i128 - v.gas_burnt as i128 == tu - tv
}

/// `logs` with the `account_balance` values (`ea:`) blanked.
fn balance_blind(logs: &[String]) -> Vec<&str> {
    logs.iter().map(|l| if l.starts_with("ea:") { "ea:*" } else { l.as_str() }).collect()
}

/// Compare one tx's normalized receipt trees; returns `(what, sim, sandbox)`
/// per difference. Differences that legitimately vary go to `timing`.
pub fn compare_tx(
    p: &str,
    label: &str,
    x: &NormTx,
    y: &NormTx,
    congested: bool,
    timing: &mut Vec<String>,
) -> Vec<(String, Value, Value)> {
    let mut out = vec![];
    if x.status != y.status {
        out.push((format!("{label} final status"), x.status.clone(), y.status.clone()));
    }
    if x.nodes.len() != y.nodes.len() {
        out.push((
            format!("{label} receipt count"),
            serde_json::to_value(x).unwrap(),
            serde_json::to_value(y).unwrap(),
        ));
        return out;
    }
    for (n, (u, v)) in x.nodes.iter().zip(&y.nodes).enumerate() {
        let fields: [(&str, Value, Value); 8] = [
            ("executor", json!(u.executor), json!(v.executor)),
            ("gas_burnt", json!(u.gas_burnt), json!(v.gas_burnt)),
            ("tokens_burnt", json!(u.tokens_burnt), json!(v.tokens_burnt)),
            ("logs", json!(u.logs), json!(v.logs)),
            ("status", u.status.clone(), v.status.clone()),
            ("children", json!(u.children), json!(v.children)),
            ("gas_profile", json!(u.gas_profile), json!(v.gas_profile)),
            ("dh", json!(u.dh), json!(v.dh)),
        ];
        for (f, a, b) in fields {
            // A receipt runs 1 block after its parent (0 for a local
            // receipt). Larger deltas are callbacks waiting for a
            // yield resume/timeout or a dependency; they depend on how
            // many blocks the sandbox produced between steps (it keeps
            // producing blocks in real time), so they're only counted.
            // So is a sandbox receipt running *later* than the sim's:
            // a loaded sandbox misses chunks, which only delays.
            // In a congested scenario the two chains pack the same load
            // into different chunks (the sandbox receives transactions
            // over time), which moves receipts between blocks and changes
            // their trie-node charges (see `trie_nodes_only`), and the
            // sandbox's gas price rises with full chunks: all counted as
            // timing, not compared.
            let (du, dv) = (u.dh.unwrap_or(0), v.dh.unwrap_or(0));
            let timed = (du >= 2 && dv >= 2) || dv > du || congested;
            if f == "dh" && timed {
                if a != b {
                    timing.push(format!(
                        "{p} {label} node {n}: dh sim {:?} sandbox {:?}",
                        u.dh, v.dh
                    ));
                }
                continue;
            }
            // Such a receipt's block also decides which of other
            // transactions' refunds its account has received when it
            // reads `account_balance`: a yield timeout of a later
            // transaction can run before or after an earlier one's last
            // refund, depending on the step spacing. Only that value is
            // let go; the final balances, compared exactly, still catch a
            // wrong or missing refund.
            if f == "logs" && timed && a != b && balance_blind(&u.logs) == balance_blind(&v.logs) {
                timing.push(format!("{p} {label} node {n}: account_balance read"));
                continue;
            }
            if congested && f == "tokens_burnt" {
                if a != b {
                    timing.push(format!("{p} {label} node {n}: tokens_burnt (gas price)"));
                }
                continue;
            }
            if congested
                && (f == "gas_burnt" || f == "gas_profile")
                && a != b
                && trie_nodes_only(u, v)
            {
                timing.push(format!("{p} {label} node {n}: {f} (chunk trie-node cache)"));
                continue;
            }
            if a != b {
                out.push((format!("{label} node {n} ({}) {f}", u.executor), a, b));
            }
        }
    }
    out
}

pub fn compare(sc: &Scenario, sim: &Value, sb: &Value, timing: &mut Vec<String>) -> Vec<Mismatch> {
    let p = sc.names.prefix.clone();
    let mut out = vec![];
    let mut mm = |what: String, a: &Value, b: &Value| {
        out.push(Mismatch { scenario: p.clone(), what, sim: a.clone(), sandbox: b.clone() })
    };
    for w in sb["harness_warnings"].as_array().into_iter().flatten() {
        mm("harness".into(), &Value::Null, w);
    }
    if let Some(e) = sb.get("error") {
        mm("sandbox runner error".into(), &Value::Null, e);
        return out;
    }
    for (id, a) in sim["setup_accounts"].as_object().unwrap() {
        let b = &sb["setup_accounts"][id];
        for f in ["amount", "locked", "storage_usage", "code_hash"] {
            if a[f] != b[f] {
                mm(format!("setup account {id}.{f}"), &a[f], &b[f]);
            }
        }
    }
    let sim_steps = sim["steps"].as_array().unwrap();
    let sb_steps = sb["steps"].as_array().unwrap();
    for (side, steps) in [("sim", sim_steps), ("sandbox", sb_steps)] {
        if steps.len() != sc.steps.len() {
            mm(format!("{side} step count"), &json!(sc.steps.len()), &json!(steps.len()));
        }
    }
    for (i, (a, b)) in sim_steps.iter().zip(sb_steps).enumerate() {
        if a["kind"] != b["kind"] {
            mm(format!("step {i} kind"), &a["kind"], &b["kind"]);
            continue;
        }
        match a["kind"].as_str().unwrap() {
            "Tx" => {
                if let Some(e) = b.get("error") {
                    mm(format!("step {i} tx send"), &a["status"], e);
                    continue;
                }
                let bs = norm_final_status(&b["status"]);
                if a["status"] != bs {
                    mm(format!("step {i} tx status at quiescence"), &a["status"], &bs);
                }
            }
            "View" | "Digest" => {
                let (x, y) = (norm_view(a), norm_view(b));
                if x != y {
                    mm(format!("step {i} {}", a["kind"]), &x, &y);
                }
            }
            _ => {}
        }
    }
    let sim_txs = sim["txs"].as_array().unwrap();
    let sb_txs = sb["txs"].as_array().unwrap();
    let expected_txs: usize = sc
        .steps
        .iter()
        .map(|s| match s {
            Step::Tx { .. } => 1,
            Step::TxGroup { txs } => txs.len(),
            _ => 0,
        })
        .sum();
    for (side, txs) in [("sim", sim_txs), ("sandbox", sb_txs)] {
        if txs.len() != expected_txs {
            mm(format!("{side} tx count"), &json!(expected_txs), &json!(txs.len()));
        }
    }
    for (t, (a, b)) in sim_txs.iter().zip(sb_txs).enumerate() {
        if a.is_null() || b.is_null() {
            mm(format!("tx {t} missing result"), a, b);
            continue;
        }
        for (w, x, y) in compare_tx(
            &p,
            &format!("tx {t}"),
            &normalize_tx(a),
            &normalize_tx(b),
            sc.congested,
            timing,
        ) {
            mm(w, &x, &y);
        }
    }
    for id in sc.names.all_accounts() {
        let (a, b) = (&sim["accounts"][&id], &sb["accounts"][&id]);
        if a.is_null() != b.is_null() {
            mm(format!("final account {id} existence"), a, b);
            continue;
        }
        if a.is_null() {
            continue;
        }
        for f in ["amount", "locked", "storage_usage", "code_hash"] {
            if a[f] != b[f] {
                if sc.congested && f == "amount" {
                    timing.push(format!("{p} final account {id}.amount (gas price)"));
                    continue;
                }
                mm(format!("final account {id}.{f}"), &a[f], &b[f]);
            }
        }
    }
    for c in &sc.names.contracts {
        let (a, b) = (norm_state(&sim["state"][c]), norm_state(&sb["state"][c]));
        if a != b {
            mm(format!("final state {c}"), &a, &b);
        }
    }
    out
}

/// Both sides' raw results: per scenario, then per feature scenario.
#[derive(Default)]
struct Results {
    sims: Vec<Value>,
    sandboxes: Vec<Value>,
    feature_sims: Vec<Value>,
    feature_sandboxes: Vec<Value>,
}

/// A chain holding the sandbox's genesis accounts (balances don't matter:
/// only the key set shapes the trie).
fn genesis(accounts: &str) -> Sim {
    let mut sim = Sim::new();
    for a in accounts.split(',').filter(|a| !a.is_empty()) {
        sim.create_account(a, Balance::from_near(1_000_000));
    }
    sim
}

/// Runs every scenario in `sim` and through `sb`, in order. The feature
/// scenarios run on the same chain and `Sim`, after the plain random
/// scenarios and before the congested ones: full chunks raise the sandbox's
/// gas price, and the feature scenarios compare tokens burnt exactly.
fn run_both(
    mut sim: Sim,
    sb: &mut sandbox::Sandbox,
    rt: &tokio::runtime::Runtime,
    scenarios: &mut [Scenario],
    features: &[features::Feature],
) -> Results {
    let mut r = Results::default();
    let mut features_left = features;
    for (i, sc) in scenarios.iter_mut().enumerate() {
        if sc.congested {
            sim = run_features(sim, sb, rt, std::mem::take(&mut features_left), &mut r);
        }
        let (s, v) = run_sim(sim, sc);
        sim = s;
        r.sims.push(v);
        r.sandboxes.push(rt.block_on(sb.run(sc)));
        eprintln!("scenario {i} ({}) done", sc.names.prefix);
    }
    run_features(sim, sb, rt, features_left, &mut r);
    r
}

fn run_features(
    mut sim: Sim,
    sb: &mut sandbox::Sandbox,
    rt: &tokio::runtime::Runtime,
    features: &[features::Feature],
    r: &mut Results,
) -> Sim {
    for f in features {
        let (s, v) = features::run_sim(sim, f);
        sim = s;
        r.feature_sims.push(v);
        r.feature_sandboxes.push(rt.block_on(features::run_sandbox(sb, f)));
        eprintln!("feature scenario {} ({}) done", f.name, f.prefix);
    }
    sim
}

/// Every mismatch, the timing differences (counted, not compared) and
/// counts of what ran.
fn compare_all(
    scenarios: &[Scenario],
    features: &[features::Feature],
    r: &Results,
) -> (Vec<Mismatch>, Vec<String>, BTreeMap<&'static str, usize>) {
    assert_eq!(r.sims.len(), scenarios.len(), "sim scenario count");
    assert_eq!(r.sandboxes.len(), scenarios.len(), "sandbox scenario count");
    assert_eq!(r.feature_sims.len(), features.len(), "sim feature count");
    assert_eq!(r.feature_sandboxes.len(), features.len(), "sandbox feature count");
    let mut all = vec![];
    let mut timing = vec![];
    let mut stats: BTreeMap<&str, usize> = BTreeMap::new();
    for ((sc, sim), sb) in scenarios.iter().zip(&r.sims).zip(&r.sandboxes) {
        for t in sim["txs"].as_array().unwrap() {
            *stats.entry("txs").or_default() += 1;
            *stats.entry("receipts").or_default() +=
                t["receipts_outcome"].as_array().unwrap().len();
            let key =
                match t["status"].as_object().and_then(|o| o.keys().next().cloned()).as_deref() {
                    Some("SuccessValue") => "tx success",
                    Some("Failure") => "tx failure",
                    _ => "tx other",
                };
            *stats.entry(key).or_default() += 1;
        }
        let m = compare(sc, sim, sb, &mut timing);
        if m.is_empty() {
            *stats.entry("scenarios identical").or_default() += 1;
        }
        all.extend(m);
    }
    for ((f, a), b) in features.iter().zip(&r.feature_sims).zip(&r.feature_sandboxes) {
        let m = features::compare(f, a, b, &mut timing);
        if m.is_empty() {
            *stats.entry("feature scenarios identical").or_default() += 1;
        }
        all.extend(m);
    }
    (all, timing, stats)
}

fn print_mismatches(all: &[Mismatch]) {
    for m in all.iter().take(40) {
        println!("[{}] {}\n   sim:     {}\n   sandbox: {}", m.scenario, m.what, m.sim, m.sandbox);
    }
}

/// Seed 362: a later tx's yield timeout read `account_balance` one block
/// before an earlier tx's last refund in the sim and ten blocks after it in
/// the sandbox, which had made more blocks between the two steps. Rerunning
/// the sim with the sandbox's spacing reproduced the sandbox's value.
#[test]
fn a_balance_read_after_a_wait_is_timing_and_nothing_else_is() {
    let tx = |dh: i64, logs: &[&str]| {
        let node = |dh, logs: Vec<String>| NormNode {
            executor: "c.near".into(),
            gas_burnt: 1,
            tokens_burnt: "0".into(),
            logs,
            status: json!({"SuccessValue": ""}),
            children: vec![],
            dh,
            gas_profile: BTreeMap::new(),
        };
        let mut root = node(None, vec![]);
        root.children = vec![Some(1)];
        let status = json!({"SuccessValue": ""});
        NormTx {
            status,
            nodes: vec![root, node(Some(dh), logs.iter().map(|l| l.to_string()).collect())],
        }
    };
    let check = |dh, sim: &[&str], sb: &[&str]| {
        let mut timing = vec![];
        let out = compare_tx("p", "tx 0", &tx(dh, sim), &tx(dh, sb), false, &mut timing);
        (out.len(), timing.len())
    };
    let (lo, hi) = (
        ["w0", "ea:0000000000082f2b6a4ee6fc036060ff"],
        ["w0", "ea:0000000000082f41256b01a61006c4ff"],
    );
    // A yield timeout (201 blocks after its parent on both sides).
    assert_eq!(check(201, &lo, &hi), (0, 1));
    // A receipt that ran right after its parent: no excuse.
    assert_eq!(check(1, &lo, &hi), (1, 0));
    // After a wait, any other log difference still counts.
    assert_eq!(check(201, &["w0", lo[1]], &["w1", hi[1]]), (1, 0));
    assert_eq!(check(201, &["r-"], &["r00"]), (1, 0));
}

#[test]
fn comparison_rejects_missing_group_results_and_truncated_output() {
    let sc = Scenario::new(
        Names::new("compare", 0, 0),
        vec![Step::TxGroup { txs: vec![("u.near".into(), "u.near".into(), vec![]); 2] }],
    );
    let tx = json!({
        "transaction_outcome": null, "receipts_outcome": [], "status": {"SuccessValue": ""},
    });
    let complete = json!({
        "setup_accounts": {}, "steps": [{"kind": "Tx", "status": {"SuccessValue": ""}}],
        "txs": [tx, tx], "accounts": {}, "state": {},
    });
    assert!(compare(&sc, &complete, &complete, &mut vec![]).is_empty());
    for (field, value, expected) in [
        ("txs", json!([tx, null]), "tx 1 missing result"),
        ("txs", json!([tx]), "sandbox tx count"),
        ("steps", json!([]), "sandbox step count"),
        ("steps", json!([{"kind": "Wait"}]), "step 0 kind"),
    ] {
        let mut incomplete = complete.clone();
        incomplete[field] = value;
        let mismatches = compare(&sc, &complete, &incomplete, &mut vec![]);
        assert!(mismatches.iter().any(|m| m.what == expected), "{mismatches:?}");
    }
    // Both runners omitting the same data must not turn a truncated run green.
    let mut empty = complete;
    empty["steps"] = json!([]);
    empty["txs"] = json!([]);
    assert_eq!(compare(&sc, &empty, &empty, &mut vec![]).len(), 4);
}

#[test]
#[ignore = "needs a running sandbox: set NCS_SANDBOX_RPC (see module docs)"]
fn differential_vs_sandbox() {
    let url =
        std::env::var("NCS_SANDBOX_RPC").expect("set NCS_SANDBOX_RPC, e.g. http://localhost:3030");
    let root = env_or("NCS_SANDBOX_ROOT", "sandbox");
    let root_key = std::env::var("NCS_SANDBOX_ROOT_KEY")
        .unwrap_or_else(|_| SecretKey::from_seed(KeyType::ED25519, &root).to_string());
    let n: usize = env_or("NCS_DIFF_CASES", "20").parse().unwrap();
    let seed: u64 = env_or("NCS_DIFF_SEED", "1").parse().unwrap();
    let dir = PathBuf::from(env_or(
        "NCS_DIFF_DIR",
        std::env::temp_dir().join(format!("ncs-diff-{seed}")).to_str().unwrap(),
    ));
    std::fs::create_dir_all(&dir).unwrap();

    let congested: usize = env_or("NCS_DIFF_CONGESTED", "2").parse().unwrap();
    let fixed = env_or("NCS_DIFF_FIXED", "1") == "1";
    let populate: usize = env_or("NCS_DIFF_POPULATE", "0").parse().unwrap();
    let big = format!("big.{root}");
    let mut scenarios = match std::env::var("NCS_DIFF_SCENARIOS") {
        // Replay scenarios saved by an earlier run (`scenarios.json`), e.g. a
        // hand-minimized one.
        Ok(f) => serde_json::from_slice(&std::fs::read(f).unwrap()).unwrap(),
        Err(_) => {
            let mut v = generate(n, congested, fixed, seed, &root);
            if populate > 0 {
                v.insert(0, trie_scenario(seed, &root, &big, populate));
            }
            v
        }
    };
    // NCS_DIFF_ONLY=3,17: rerun just these scenario indices (for minimizing).
    if let Ok(only) = std::env::var("NCS_DIFF_ONLY") {
        let keep: Vec<usize> = only.split(',').map(|x| x.trim().parse().unwrap()).collect();
        scenarios = scenarios
            .into_iter()
            .enumerate()
            .filter(|(i, _)| keep.contains(i))
            .map(|(_, s)| s)
            .collect();
    }
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    let mut sb = sandbox::Sandbox::node(&url, &root, &root_key);
    let status = rt.block_on(sb.status()).expect("sandbox not reachable");
    println!("sandbox {} protocol {}", status["version"]["version"], status["protocol_version"]);

    // The simulator starts with the sandbox's genesis accounts.
    let mut sim = genesis(&env_or("NCS_SANDBOX_GENESIS_ACCOUNTS", "sandbox,near"));
    // NCS_DIFF_POPULATE=n: give both chains a mainnet-shaped trie first (n
    // accounts with keys, every 10th with some contract data, plus
    // `big.<root>` holding n token-like entries),
    // with the same records, so every scenario runs on it.
    if populate > 0 {
        let ids = mainnet_like_ids(populate, "pop");
        let mut accounts = account_records(&ids);
        accounts.extend(contract_state_records(&ids));
        let contract = big_contract_records(&big, &fuzz_wasm(), populate);
        let json = |r: &[near_contract_sim::StateRecord]| -> Vec<Value> {
            r.iter().map(|x| serde_json::to_value(x).unwrap()).collect()
        };
        rt.block_on(sb.patch_records(&json(&accounts)));
        rt.block_on(sb.patch_records(&json(&contract)));
        sim.apply_state_records(accounts);
        sim.apply_state_records(contract);
        println!("populated both chains: {populate} accounts, {big} with {populate} entries");
    }
    let features = if env_or("NCS_DIFF_FEATURES", "1") == "0" {
        vec![]
    } else {
        features::scenarios(&format!("f{seed}"), &root)
    };
    let r = run_both(sim, &mut sb, &rt, &mut scenarios, &features);
    let (all, timing, stats) = compare_all(&scenarios, &features, &r);
    std::fs::write(dir.join("features_sim.json"), serde_json::to_vec(&r.feature_sims).unwrap())
        .unwrap();
    let feature_sandboxes = serde_json::to_vec(&r.feature_sandboxes).unwrap();
    std::fs::write(dir.join("features_sandbox.json"), feature_sandboxes).unwrap();
    std::fs::write(dir.join("scenarios.json"), serde_json::to_vec(&scenarios).unwrap()).unwrap();
    std::fs::write(dir.join("sim.json"), serde_json::to_vec(&r.sims).unwrap()).unwrap();
    std::fs::write(dir.join("sandbox.json"), serde_json::to_vec(&r.sandboxes).unwrap()).unwrap();
    std::fs::write(dir.join("mismatches.json"), serde_json::to_vec_pretty(&all).unwrap()).unwrap();
    println!(
        "differential: {} scenarios + {} feature scenarios, {stats:?}",
        scenarios.len(),
        features.len()
    );
    println!("timing differences (counted, not compared): {}", timing.len());
    println!("details in {}", dir.display());
    print_mismatches(&all);
    assert!(all.is_empty(), "{} mismatches (see {}/mismatches.json)", all.len(), dir.display());
}

/// The sandbox runner over `SimTransport` against the simulator's direct API.
/// The differential job runs the same client code against a real node, so a
/// mismatch there is the simulator's or the transport's, and this says which.
/// No Docker: CI's test job runs it.
#[cfg(feature = "near-kit")]
#[test]
fn differential_vs_transport() {
    let (root, seed) = ("sandbox", 1);
    let root_key = SecretKey::from_seed(KeyType::ED25519, root).to_string();
    let node = near_contract_sim::SimTransport::new(genesis("sandbox,near"));
    let mut sb = sandbox::Sandbox::sim(node, root, &root_key);
    let mut scenarios = generate(6, 1, false, seed, root);
    // near-kit derives and signs ML-DSA-65 keys in pure Rust, about a minute
    // of a debug build's time: the key types run against the sandbox alone.
    let features = features::scenarios(&format!("f{seed}"), root);
    let features: Vec<_> = features.into_iter().filter(|f| f.name != "kt").collect();
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .start_paused(true)
        .build()
        .unwrap();
    let r = run_both(genesis("sandbox,near"), &mut sb, &rt, &mut scenarios, &features);
    let (all, timing, stats) = compare_all(&scenarios, &features, &r);
    println!("{stats:?}, timing differences: {timing:?}");
    print_mismatches(&all);
    assert!(all.is_empty(), "{} mismatches", all.len());
}
