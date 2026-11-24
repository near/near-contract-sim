//! External calls cross-checked against a real `neard` sandbox
//! (2.15.0-rc.1, protocol 89), through JSON-RPC on both sides.
//!
//! `record_sandbox_external_calls` sends the cases below, in order, to a
//! fresh sandbox holding the simulator's accounts (patched in), and saves
//! what its RPC answered, normalized, in
//! `tests/res/sandbox_ecc_2.15.0-rc.1.json`. `external_calls_match_the_sandbox`
//! sends the same cases to `SimTransport` and compares, on every run:
//!
//! - the `call_function` view of `__external_call` before each send: the
//!   dry run's value and logs, or the refusal's error and logs;
//! - `send_tx`'s answer: an inclusion's status, the call's own outcome (gas
//!   burnt, tokens burnt, logs, status, gas profile) and every receipt's,
//!   refunds included; a repeat answered with the earlier inclusion; the
//!   error a refused call gets (an invalid transaction before the pool, a
//!   timeout after the dry run refuses it);
//! - the contracts' balances once the call's receipts have all run.
//!
//! Heights differ, since a sandbox makes blocks on its own clock: for the
//! height probe both sides record whether the run saw the block it was
//! meant to (the chunk's parent, or for a view the view's block), not the
//! numbers. Unique ids, block hashes and receipt ids are left out.

mod common;
use base64::Engine;
use common::*;
use near_contract_sim::nearcore::near_primitives::transaction::EccTransaction;
use near_contract_sim::{Gas, Sim, SimTransport};
use near_kit::rpc::RpcTransport;
use serde_json::{Map, Value, json};
use std::io::{Read, Write};

const FIXTURE: &str = "sandbox_ecc_2.15.0-rc.1.json";
const ACCOUNTS: [&str; 3] = ["alice.test.near", "probe.test.near", "bob.test.near"];
const PROBE: &str = "probe.test.near";
const START_HEIGHT: u64 = 1_000;

fn world() -> Sim {
    let mut sim = Sim::new();
    sim.advance_to(START_HEIGHT);
    for id in ACCOUNTS {
        sim.create_account(id, near(100));
    }
    sim.deploy(PROBE, wat("ecc_probe.wat"));
    sim.deploy("bob.test.near", wat("ecc_probe.wat"));
    sim
}

fn op(code: u8, rest: &[u8]) -> Vec<u8> {
    let mut p = vec![code];
    p.extend(rest);
    p
}

fn promise(tgas: u64, weight: u64, method: &str, target: &str) -> Vec<u8> {
    let mut p = op(13, &Gas::from_teragas(tgas).as_gas().to_le_bytes());
    p.extend(weight.to_le_bytes());
    p.extend(0u128.to_le_bytes());
    p.push(method.len() as u8);
    p.extend(method.as_bytes());
    p.extend(target.as_bytes());
    p
}

/// The largest loop the probe runs in an external call, found on the
/// simulator (and checked at the sandbox by the next case being refused).
fn gas_boundary() -> u64 {
    let ok = |n: u64| {
        let mut s = world();
        s.external_call(PROBE, op(14, &n.to_le_bytes())).transact().refusal().is_none()
    };
    let (mut lo, mut hi) = (0, 1 << 30);
    while hi - lo > 1 {
        let m = lo + (hi - lo) / 2;
        if ok(m) { lo = m } else { hi = m }
    }
    lo
}

/// (label, contract, payload), sent in this order.
fn cases() -> Vec<(String, &'static str, Vec<u8>)> {
    let size = |n: usize| EccTransaction::new(PROBE.parse().unwrap(), vec![0; n]).get_size();
    let fits = 20_480 - size(0) as usize;
    let n = gas_boundary();
    let mut v: Vec<(String, &str, Vec<u8>)> = vec![
        ("empty".into(), PROBE, vec![]),
        ("accept".into(), PROBE, op(0, b"hi")),
        // Within the repeat window, and before a sandbox's garbage collection
        // (5 epochs by default) prunes the first inclusion's outcome, which
        // the answer to a repeat is.
        ("accept_again".into(), PROBE, op(0, b"hi")),
        ("write".into(), PROBE, op(2, b"v")),
        ("refuse".into(), PROBE, vec![1]),
        ("refuse_after_log".into(), PROBE, vec![19]),
    ];
    for (code, name) in [
        (3, "signer_account_pk"),
        (4, "predecessor_account_id"),
        (5, "prepaid_gas"),
        (6, "random_seed"),
        (7, "promise_yield_create"),
        (8, "promise_yield_create_with_id"),
    ] {
        v.push((name.into(), PROBE, vec![code]));
    }
    v.extend([
        ("signer_account_id".into(), PROBE, vec![9]),
        ("refund_to_account_id".into(), PROBE, vec![10]),
        ("attached_deposit".into(), PROBE, vec![11]),
        ("account_balance".into(), PROBE, vec![12]),
        ("heights".into(), PROBE, vec![17]),
        ("promise_weight".into(), PROBE, promise(5, 1, "ping", "bob.test.near")),
        ("promise_zero_gas".into(), PROBE, promise(0, 1, "ping", "bob.test.near")),
        ("promise_pk".into(), PROBE, promise(10, 0, "pk", "bob.test.near")),
        ("promise_fwd_pk".into(), PROBE, promise(30, 0, "fwd", "bob.test.near")),
        ("gas_at_limit".into(), PROBE, op(14, &n.to_le_bytes())),
        ("gas_past_limit".into(), PROBE, op(14, &(n + 1).to_le_bytes())),
        ("removes_25".into(), PROBE, op(16, &25u32.to_le_bytes())),
        ("removes_26".into(), PROBE, op(16, &26u32.to_le_bytes())),
        ("size_at_limit".into(), PROBE, vec![0; fits]),
        ("size_past_limit".into(), PROBE, vec![0; fits + 1]),
        ("no_contract".into(), "alice.test.near", vec![]),
        ("no_account".into(), "nobody.test.near", vec![]),
    ]);
    v
}

enum Node {
    Sim(SimTransport),
    /// A sandbox's RPC at this `host:port`.
    Http(String),
}

impl Node {
    /// The JSON-RPC response to `method` with `params`, whole.
    async fn rpc(&self, method: &str, params: Value) -> Value {
        let request = json!({"jsonrpc": "2.0", "id": 0, "method": method, "params": params});
        let body = serde_json::to_vec(&request).unwrap();
        let bytes = match self {
            Node::Sim(node) => node.post_json("", body).await.unwrap().body,
            Node::Http(addr) => http_post(addr, &body),
        };
        serde_json::from_slice(&bytes).unwrap_or_else(|e| panic!("{method}: {e}"))
    }

    async fn ok(&self, method: &str, params: Value) -> Value {
        let r = self.rpc(method, params).await;
        r.get("result").cloned().unwrap_or_else(|| panic!("{method}: {r}"))
    }

    async fn block(&self, hash: &Value) -> Value {
        self.ok("block", json!({"block_id": hash})).await["header"].clone()
    }

    async fn balance(&self, id: &str) -> Value {
        let q = json!({"request_type": "view_account", "finality": "optimistic", "account_id": id});
        self.ok("query", q).await["amount"].clone()
    }
}

/// A plain HTTP/1.1 POST of `body` to `addr`, the response's body.
fn http_post(addr: &str, body: &[u8]) -> Vec<u8> {
    let mut s = std::net::TcpStream::connect(addr).unwrap();
    let head = format!(
        "POST / HTTP/1.1\r\nHost: {addr}\r\nContent-Type: application/json\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    s.write_all(head.as_bytes()).unwrap();
    s.write_all(body).unwrap();
    let mut out = vec![];
    s.read_to_end(&mut out).unwrap();
    let at = out.windows(4).position(|w| w == b"\r\n\r\n").expect("an HTTP response");
    let (head, rest) = (String::from_utf8_lossy(&out[..at]).to_lowercase(), &out[at + 4..]);
    if !head.contains("transfer-encoding: chunked") {
        return rest.to_vec();
    }
    let (mut body, mut rest) = (vec![], rest);
    loop {
        let eol = rest.windows(2).position(|w| w == b"\r\n").unwrap();
        let n =
            usize::from_str_radix(std::str::from_utf8(&rest[..eol]).unwrap().trim(), 16).unwrap();
        if n == 0 {
            return body;
        }
        body.extend(&rest[eol + 2..eol + 2 + n]);
        rest = &rest[eol + 4 + n..];
    }
}

fn b64(b: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(b)
}

fn unb64(s: &Value) -> Vec<u8> {
    base64::engine::general_purpose::STANDARD.decode(s.as_str().unwrap()).unwrap()
}

/// An outcome's status, a receipt id left out.
fn status(s: &Value) -> Value {
    match s.get("SuccessReceiptId") {
        Some(_) => json!("SuccessReceiptId"),
        None => s.clone(),
    }
}

fn outcome(o: &Value) -> Value {
    let o = &o["outcome"];
    let profile: Map<String, Value> = (o["metadata"]["gas_profile"].as_array())
        .into_iter()
        .flatten()
        .map(|c| {
            let g = &c["gas_used"];
            let g = g.as_u64().or_else(|| g.as_str()?.parse().ok()).unwrap();
            (c["cost"].as_str().unwrap().into(), json!(g))
        })
        .collect();
    json!({
        "executor": o["executor_id"], "gas_burnt": o["gas_burnt"], "tokens_burnt": o["tokens_burnt"],
        "logs": o["logs"], "status": status(&o["status"]), "profile": profile,
        "receipts": o["receipt_ids"].as_array().map_or(0, Vec::len),
    })
}

/// Whether op 17's answer, `value`, is `height` and `timestamp`.
fn saw(value: &[u8], height: &Value, timestamp: &Value) -> bool {
    let index = u64::from_le_bytes(value[..8].try_into().unwrap());
    let ns = u64::from_le_bytes(value[8..].try_into().unwrap());
    let ts = timestamp.as_u64().or_else(|| timestamp.as_str()?.parse().ok()).unwrap();
    Some(index) == height.as_u64() && ns == ts
}

/// Sends every case to `node` and returns what it answered, normalized.
async fn record(node: &Node) -> Map<String, Value> {
    let mut out = Map::new();
    let mut included: Vec<(String, Value)> = vec![];
    for (label, to, payload) in cases() {
        let view = json!({"request_type": "call_function", "finality": "optimistic",
                          "account_id": to, "method_name": "__external_call",
                          "args_base64": b64(&payload)});
        let r = node.rpc("query", view).await;
        let view = match (&r["result"], &r["error"]) {
            (res, Value::Null) => {
                let value: Vec<u8> = serde_json::from_value(res["result"].clone()).unwrap();
                let value = match label.as_str() {
                    "heights" => {
                        let ts = node.block(&res["block_hash"]).await["timestamp"].clone();
                        json!({"saw_its_block": saw(&value, &res["block_height"], &ts)})
                    }
                    _ => json!(b64(&value)),
                };
                json!({"ok": {"value": value, "logs": res["logs"]}})
            }
            (_, e) => match e["cause"]["name"].as_str() {
                Some("EXTERNAL_CALL_REFUSED") => json!({"refused": {
                    "error": e["cause"]["info"]["error"], "logs": e["cause"]["info"]["logs"]}}),
                _ => json!({"error": e["cause"]}),
            },
        };

        let ecc = EccTransaction::new(to.parse().unwrap(), payload);
        let send = json!({"external_call_base64": b64(&borsh::to_vec(&ecc).unwrap()),
                          "wait_until": "FINAL"});
        let r = node.rpc("send_tx", send).await;
        let send = match (&r["result"], &r["error"]) {
            (res, Value::Null) => {
                let id = res["transaction"]["transaction_id"].clone();
                assert_eq!(res["transaction"]["kind"], "external_call", "{res}");
                match included.iter().find(|(_, i)| *i == id) {
                    Some((earlier, _)) => json!({"repeat_of": earlier}),
                    None => {
                        included.push((label.clone(), id));
                        let mut call = outcome(&res["transaction_outcome"]);
                        let mut final_status = res["status"].clone();
                        if label == "heights" {
                            let block = node.block(&res["transaction_outcome"]["block_hash"]).await;
                            let parent = node.block(&block["prev_hash"]).await;
                            let value = unb64(&res["status"]["SuccessValue"]);
                            let ok = saw(&value, &parent["height"], &parent["timestamp"]);
                            final_status = json!({"saw_the_parent_block": ok});
                            call["status"] = final_status.clone();
                        }
                        let mut receipts: Vec<Value> = res["receipts_outcome"]
                            .as_array()
                            .unwrap()
                            .iter()
                            .map(outcome)
                            .collect();
                        receipts.sort_by_key(Value::to_string);
                        json!({"included": {"status": final_status, "call": call, "receipts": receipts}})
                    }
                }
            }
            (_, e) => json!({"error": e["cause"]["name"], "data": e["data"]}),
        };
        let balances = json!({"probe": node.balance(PROBE).await,
                              "bob": node.balance("bob.test.near").await});
        out.insert(label, json!({"view": view, "send": send, "balances": balances}));
    }
    out
}

#[tokio::test(start_paused = true)]
async fn external_calls_match_the_sandbox() {
    let sb: Value = serde_json::from_slice(&res(FIXTURE)).unwrap();
    assert_eq!(sb["_protocol"], world().config.protocol_version);
    let sim = record(&Node::Sim(SimTransport::new(world()))).await;
    let mut problems = vec![];
    for (label, _, _) in cases() {
        let (a, b) = (&sim[&label], &sb[&label]);
        for part in ["view", "send", "balances"] {
            if a[part] != b[part] {
                problems
                    .push(format!("{label} {part}:\n  sim     {}\n  sandbox {}", a[part], b[part]));
            }
        }
    }
    assert!(problems.is_empty(), "{} mismatches:\n{}", problems.len(), problems.join("\n"));
}

/// Records the fixture from a fresh sandbox at `NCS_SANDBOX_RPC`
/// (`http://host:port`), the simulator's accounts patched in. A manual
/// recorder, like `gas_vs_sandbox`'s: run it by hand to refresh the file.
///
/// ```sh
/// docker run -d --rm -p 127.0.0.1:3030:3030 nearprotocol/sandbox:2.15.0-rc.1
/// NCS_SANDBOX_RPC=http://127.0.0.1:3030 cargo test --test external_calls_vs_sandbox -- --ignored
/// ```
#[tokio::test]
#[ignore = "records against a fresh sandbox: set NCS_SANDBOX_RPC"]
async fn record_sandbox_external_calls() {
    let url = std::env::var("NCS_SANDBOX_RPC").unwrap();
    let node = Node::Http(url.trim_start_matches("http://").trim_end_matches('/').into());
    let sim = world();
    node.ok("sandbox_patch_state", json!({"records": sim.state_records(ACCOUNTS)})).await;
    let st = node.ok("status", json!([])).await;
    let height = st["sync_info"]["latest_block_height"].as_u64().unwrap();
    assert!(height < START_HEIGHT - 3, "not a fresh sandbox: at height {height}");
    node.ok("sandbox_fast_forward", json!({"delta_height": START_HEIGHT - height})).await;
    let mut out = record(&node).await;
    out.insert("_protocol".into(), st["protocol_version"].clone());
    out.insert("_version".into(), st["version"].clone());
    let path = format!("{}/tests/res/{FIXTURE}", env!("CARGO_MANIFEST_DIR"));
    std::fs::write(path, serde_json::to_string_pretty(&out).unwrap() + "\n").unwrap();
}
