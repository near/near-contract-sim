//! Gas cross-check against a real `neard` sandbox (2.14.0, protocol 88).
//!
//! `tests/res/sandbox_gas_2.14.0.json` was recorded by running the
//! same scenario (same accounts, wasm, args, prepaid gas, in the same order)
//! against near-sandbox via RPC and saving `EXPERIMENTAL_tx_status`
//! (`record_sandbox_gas`).
//!
//! Assertions:
//! - transaction conversion gas: exact match;
//! - every non-refund receipt: every gas-profile entry matches exactly,
//!   trie-node costs included. The sandbox has more accounts (`sandbox`,
//!   `near`, ...), but a receipt's trie-node charges only touch
//!   the receiver's own subtree plus a shallow path to it;
//! - refund receipts: same count, same gas.
//!
//! One data-dependent exception: `start_yield` stores
//! `format!("yielded:{}", hex(yield_id))`. The yield id (a data id) is
//! derived from block hashes, which necessarily differ between the sandbox
//! and the simulator, and hex-formatting cost (`{:02x}` padding paths)
//! depends on the byte values. So `WASM_INSTRUCTION` of that receipt is
//! allowed to differ; everything else (all host-function costs, storage
//! byte counts, trie nodes) must still match.
//!
//! Run with `--nocapture` to see the comparison table.

mod common;
use common::*;
use near_contract_sim::{Gas, Sim, TxOutcome};
use serde_json::Value;
use std::collections::BTreeSet;

/// Recorded by `record_sandbox_gas`, below.
const FIXTURE: &str = "sandbox_gas_2.14.0.json";

/// (scenario, receipt method, cost) triples that are data-dependent (see module docs).
const DATA_DEPENDENT: [(&str, &str, &str); 2] = [
    ("start_yield", "start_yield", "WASM_INSTRUCTION"),
    ("start_yield_2", "start_yield", "WASM_INSTRUCTION"),
];

const ACCOUNTS: [&str; 4] =
    ["alice.test.near", "status.test.near", "xcc.test.near", "probe.test.near"];

/// (scenario, receiver, method, args, Tgas), called by alice in this order.
const CALLS: [(&str, &str, &str, &str, u64); 5] = [
    ("set_status_1", "status.test.near", "set_status", r#"{"message":"hello"}"#, 30),
    (
        "set_status_2",
        "status.test.near",
        "set_status",
        r#"{"message":"hello world, second write"}"#,
        30,
    ),
    (
        "get_status_call",
        "status.test.near",
        "get_status",
        r#"{"account_id":"alice.test.near"}"#,
        30,
    ),
    ("probe_height", "probe.test.near", "height", "{}", 30),
    ("factorial_3", "xcc.test.near", "factorial", r#"{"n":3}"#, 300),
];

fn world() -> Sim {
    let mut sim = Sim::new();
    for (id, wasm) in
        ACCOUNTS.into_iter().zip([None, Some(status_wasm()), Some(xcc_wasm()), Some(probe_wasm())])
    {
        sim.create_account(id, near(100));
        if let Some(w) = wasm {
            sim.deploy(id, w);
        }
    }
    sim
}

fn run_scenario() -> Vec<(&'static str, TxOutcome)> {
    let mut sim = world();
    let call = |sim: &mut Sim, rcv: &str, m: &str, a: &str, tgas: u64| {
        sim.call("alice.test.near", rcv, m)
            .args(a.as_bytes().to_vec())
            .gas(Gas::from_teragas(tgas))
            .transact()
    };
    let mut out = vec![];
    for (label, rcv, m, a, g) in CALLS {
        out.push((label, call(&mut sim, rcv, m, a, g)));
    }
    let y1 = call(&mut sim, "probe.test.near", "start_yield", "{}", 100);
    out.push(("resume", call(&mut sim, "probe.test.near", "resume", "{}", 30)));
    out.push(("start_yield", sim.outcome(y1.hash)));
    out.push(("last_after_resume", call(&mut sim, "probe.test.near", "last", "{}", 30)));
    let y2 = call(&mut sim, "probe.test.near", "start_yield", "{}", 100);
    sim.advance_to(sim.height() + 205);
    sim.settle();
    out.push(("start_yield_2", sim.outcome(y2.hash)));
    out.push(("last_after_timeout", call(&mut sim, "probe.test.near", "last", "{}", 30)));
    out
}

#[test]
fn gas_matches_real_sandbox() {
    let sb: Value = serde_json::from_slice(&res(FIXTURE)).unwrap();
    assert_eq!(sb["_protocol"], world().config.protocol_version);
    assert!(
        FIXTURE.contains(sb["_version"]["version"].as_str().unwrap()),
        "recorded on another build"
    );
    let mut problems = vec![];
    println!(
        "{:<20} {:>16} {:>16} {:>9} | {:>16} {:>16} {:>13} {:>8}",
        "scenario",
        "conv sim",
        "conv sandbox",
        "conv Δ",
        "total sim",
        "total sandbox",
        "total Δ",
        "Δ %"
    );
    for (label, o) in run_scenario() {
        let s = &sb[label];
        let sb_conv = s["conversion_burnt"].as_u64().unwrap();
        let sb_total = s["total_burnt"].as_u64().unwrap();
        let conv = o.transaction.as_ref().unwrap().gas_burnt.as_gas();
        let total = o.gas_burnt().as_gas();
        let d = total as i64 - sb_total as i64;
        println!(
            "{label:<20} {conv:>16} {sb_conv:>16} {:>9} | {total:>16} {sb_total:>16} {d:>13} {:>7.4}%",
            conv as i64 - sb_conv as i64,
            d as f64 * 100.0 / sb_total as f64
        );
        if conv != sb_conv {
            problems.push(format!("{label}: conversion {conv} != {sb_conv}"));
        }
        let sb_outs = s["outcomes"].as_array().unwrap();
        let sb_refunds: Vec<_> = sb_outs
            .iter()
            .filter(|x| {
                x["profile"].as_object().unwrap().is_empty() && x["receiver"] == "alice.test.near"
            })
            .collect();
        let sb_main: Vec<_> = sb_outs
            .iter()
            .filter(|x| {
                !(x["profile"].as_object().unwrap().is_empty()
                    && x["receiver"] == "alice.test.near")
            })
            .collect();
        let main: Vec<_> = o.receipts.iter().filter(|r| !r.is_refund()).collect();
        let refunds: Vec<_> = o.receipts.iter().filter(|r| r.is_refund()).collect();
        if refunds.len() != sb_refunds.len() {
            problems.push(format!("{label}: {} refunds vs {}", refunds.len(), sb_refunds.len()));
        }
        for (r, x) in refunds.iter().zip(&sb_refunds) {
            if r.gas_burnt.as_gas() != x["gas_burnt"].as_u64().unwrap() {
                problems.push(format!(
                    "{label}: refund gas {} vs {}",
                    r.gas_burnt.as_gas(),
                    x["gas_burnt"]
                ));
            }
        }
        if main.len() != sb_main.len() {
            problems.push(format!("{label}: {} receipts vs {}", main.len(), sb_main.len()));
            continue;
        }
        for (i, (r, x)) in main.iter().zip(&sb_main).enumerate() {
            let sbp = x["profile"].as_object().unwrap();
            let keys: BTreeSet<&str> = r
                .gas_profile
                .keys()
                .map(String::as_str)
                .chain(sbp.keys().map(String::as_str))
                .collect();
            let mut allowed = 0i64;
            for k in keys {
                let a = r.gas_profile.get(k).copied().unwrap_or(0) as i64;
                let b = sbp.get(k).and_then(Value::as_u64).unwrap_or(0) as i64;
                if a == b {
                    continue;
                }
                let method = r.method_names.join(",");
                if DATA_DEPENDENT.contains(&(label, method.as_str(), k)) {
                    allowed += a - b;
                    println!(
                        "    r{i} {:<16} {k:<22} sim={a} sandbox={b} Δ={}",
                        r.method_names.join(","),
                        a - b
                    );
                } else {
                    problems.push(format!("{label} r{i}: {k} sim={a} sandbox={b}"));
                }
            }
            let rd = r.gas_burnt.as_gas() as i64 - x["gas_burnt"].as_i64().unwrap();
            if rd != allowed {
                problems.push(format!(
                    "{label} r{i}: receipt Δ {rd} not explained by allowed Δ {allowed}"
                ));
            }
        }
    }
    assert!(problems.is_empty(), "gas mismatches:\n{}", problems.join("\n"));
}

/// Records the fixture: the same scenario on a fresh sandbox at
/// `NCS_SANDBOX_RPC`, into `tests/res/` under `FIXTURE`'s name. The
/// accounts are the simulator's, patched in; the sandbox runs the calls.
///
/// ```sh
/// docker run -d --rm -p 127.0.0.1:3030:3030 nearprotocol/sandbox:<tag>
/// NCS_SANDBOX_RPC=http://127.0.0.1:3030 cargo test --test gas_vs_sandbox -- --ignored
/// ```
#[tokio::test]
#[ignore = "records against a fresh sandbox: set NCS_SANDBOX_RPC"]
async fn record_sandbox_gas() {
    use near_kit::signer::InMemorySigner;
    use near_kit::{CryptoHash, Near};
    use serde_json::json;
    let url = std::env::var("NCS_SANDBOX_RPC").unwrap();
    let sim = world();
    let key = sim.secret_key("alice.test.near").to_string();
    let alice = InMemorySigner::new("alice.test.near", key).unwrap();
    let node = Near::custom(url, "sandbox").signer(alice).build();
    let rpc = node.rpc();
    rpc.sandbox_patch_state(json!(sim.state_records(ACCOUNTS))).await.unwrap();
    rpc.sandbox_fast_forward(3).await.unwrap();
    let send = async |rcv: &str, m: &str, a: &str, tgas: u64| -> CryptoHash {
        let call = node.call(rcv, m).args_raw(a.as_bytes().to_vec()).gas(format!("{tgas} Tgas"));
        let sent = call.send().wait_until::<near_kit::transaction::Included>();
        sent.await.unwrap().transaction_hash
    };
    // Waits until every receipt of the transaction has run, final.
    let status = async |hash: CryptoHash| -> Value {
        let params = json!({"tx_hash": hash.to_string(), "sender_account_id": "alice.test.near",
                            "wait_until": "FINAL"});
        for _ in 0..120 {
            match rpc.call::<Value, Value>("EXPERIMENTAL_tx_status", params.clone()).await {
                Ok(v) => return v,
                Err(_) => tokio::time::sleep(std::time::Duration::from_millis(500)).await,
            }
        }
        panic!("transaction {hash} did not finish within a minute")
    };
    let mut out = serde_json::Map::new();
    let mut put = |label: &str, st: Value| {
        let conv = st["transaction_outcome"]["outcome"]["gas_burnt"].as_u64().unwrap();
        let outcomes: Vec<Value> = st["receipts_outcome"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| {
                let o = &r["outcome"];
                let profile: serde_json::Map<String, Value> = o["metadata"]["gas_profile"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|c| {
                        let g = &c["gas_used"];
                        let g = g.as_u64().or_else(|| g.as_str()?.parse().ok()).unwrap();
                        (c["cost"].as_str().unwrap().into(), json!(g))
                    })
                    .collect();
                json!({"receiver": o["executor_id"], "gas_burnt": o["gas_burnt"], "status": o["status"],
                       "logs": o["logs"], "profile": profile})
            })
            .collect();
        let total = conv + outcomes.iter().map(|o| o["gas_burnt"].as_u64().unwrap()).sum::<u64>();
        let entry = json!({"hash": st["transaction_outcome"]["id"], "conversion_burnt": conv,
                           "total_burnt": total, "outcomes": outcomes});
        out.insert(label.into(), entry);
    };
    for (label, rcv, m, a, g) in CALLS {
        put(label, status(send(rcv, m, a, g).await).await);
    }
    let y1 = send("probe.test.near", "start_yield", "{}", 100).await;
    put("resume", status(send("probe.test.near", "resume", "{}", 30).await).await);
    put("start_yield", status(y1).await);
    put("last_after_resume", status(send("probe.test.near", "last", "{}", 30).await).await);
    let y2 = send("probe.test.near", "start_yield", "{}", 100).await;
    rpc.sandbox_fast_forward(205).await.unwrap();
    put("start_yield_2", status(y2).await);
    put("last_after_timeout", status(send("probe.test.near", "last", "{}", 30).await).await);
    let st = rpc.call::<Value, Value>("status", json!([])).await.unwrap();
    out.insert("_protocol".into(), st["protocol_version"].clone());
    out.insert("_version".into(), st["version"].clone());
    let path = format!("{}/tests/res/{FIXTURE}", env!("CARGO_MANIFEST_DIR"));
    std::fs::write(path, serde_json::to_string_pretty(&out).unwrap() + "\n").unwrap();
}
