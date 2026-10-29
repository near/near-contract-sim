//! Runs a differential scenario over JSON-RPC (near-kit's `RpcClient` for
//! transport, raw JSON for everything else), against a real sandbox node or
//! in process against `SimTransport`, and returns the raw results in the same
//! shape `run_sim` produces.

use crate::fuzz_common::*;
use near_kit::protocol::{Action, Transaction};
use near_kit::rpc::{RetryConfig, RpcClient, RpcError};
use near_kit::signer::{PublicKey, SecretKey};
use near_kit::{AccountId, CryptoHash, Gas, NearToken};
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap};
use std::str::FromStr;
use std::time::{Duration, Instant};

/// Blocks without DAG growth after which a tx with unexecuted receipts
/// (parked yields never execute) counts as settled. Not 2-3: a loaded
/// sandbox misses chunks and receipts then lag several blocks.
const QUIET_BLOCKS: u64 = 10;

pub struct Sandbox {
    rpc: RpcClient,
    /// Blocks come from `sandbox_fast_forward`, not the wall clock.
    in_process: bool,
    pub root: String,
    pub root_key: SecretKey,
    heights: HashMap<String, u64>,
    /// Latest known status per tx hash (see `refresh`).
    pub snap: HashMap<String, Value>,
}

/// Every receipt the DAG references has executed.
fn done(st: &Value) -> bool {
    let receipts = st["receipts_outcome"].as_array().unwrap();
    let ids: std::collections::HashSet<&str> =
        receipts.iter().map(|r| r["id"].as_str().unwrap()).collect();
    std::iter::once(&st["transaction_outcome"])
        .chain(receipts)
        .flat_map(|o| o["outcome"]["receipt_ids"].as_array().unwrap())
        .all(|x| ids.contains(x.as_str().unwrap()))
}

fn kit_actions(actions: &[TxAction]) -> Vec<Action> {
    actions
        .iter()
        .map(|a| match a {
            TxAction::Call { program_hex, gas, deposit } => Action::function_call(
                "run",
                unhex(program_hex),
                Gas::from_gas(*gas),
                NearToken::from_yoctonear(deposit.parse().unwrap()),
            ),
            TxAction::Transfer { amount } => {
                Action::transfer(NearToken::from_yoctonear(amount.parse().unwrap()))
            }
        })
        .collect()
}

fn account(s: &str) -> AccountId {
    s.parse().unwrap_or_else(|e| panic!("account id {s}: {e:?}"))
}

impl Sandbox {
    /// A node at `url`.
    pub fn node(url: &str, root: &str, root_key: &str) -> Self {
        // No client-side retries: unknown (aged-out) txs are expected and
        // must fail fast; the harness retries where it needs to.
        let rpc = RpcClient::with_retry_config(url, RetryConfig::none());
        Sandbox::new(rpc, false, root, root_key)
    }

    /// The simulator behind `node`, driven by the same code.
    #[cfg(feature = "near-kit")]
    pub fn sim(node: near_contract_sim::SimTransport, root: &str, root_key: &str) -> Self {
        let node = std::sync::Arc::new(node);
        let rpc = RpcClient::with_transport_and_retry_config("sim", node, RetryConfig::none());
        Sandbox::new(rpc, true, root, root_key)
    }

    fn new(rpc: RpcClient, in_process: bool, root: &str, root_key: &str) -> Self {
        Sandbox {
            rpc,
            in_process,
            root: root.into(),
            root_key: SecretKey::from_str(root_key).expect("root key"),
            heights: HashMap::new(),
            snap: HashMap::new(),
        }
    }

    pub async fn call(&self, method: &str, params: Value) -> Result<Value, String> {
        self.rpc.call::<Value, Value>(method, params).await.map_err(|e| e.to_string())
    }

    pub async fn call_ok(&self, method: &str, params: Value) -> Value {
        let mut last = String::new();
        for _ in 0..50 {
            match self.call(method, params.clone()).await {
                Ok(v) => return v,
                Err(e) => last = e,
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        panic!("{method} failed: {last}")
    }

    pub async fn status(&self) -> Result<Value, String> {
        self.call("status", json!([])).await
    }

    pub async fn height(&self) -> u64 {
        self.call_ok("block", json!({"finality": "optimistic"})).await["header"]["height"]
            .as_u64()
            .unwrap()
    }

    /// Wait for `n` more blocks. In process they come at once; a node makes
    /// them in real time, and this panics if it stalls for 5 minutes.
    pub async fn wait_blocks(&self, n: u64) {
        if self.in_process {
            // A sandbox lands `n - 1` blocks past the next one.
            return self.rpc.sandbox_fast_forward(n - 1).await.unwrap();
        }
        let h0 = self.height().await;
        let (mut last, mut t) = (h0, Instant::now());
        loop {
            let h = self.height().await;
            if h >= h0 + n {
                return;
            }
            if h != last {
                (last, t) = (h, Instant::now());
            } else if t.elapsed() > Duration::from_secs(300) {
                panic!("sandbox stalled at height {h}");
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    async fn block_height(&mut self, hash: &str) -> u64 {
        if let Some(h) = self.heights.get(hash) {
            return *h;
        }
        let h = self.call_ok("block", json!({"block_id": hash})).await["header"]["height"]
            .as_u64()
            .unwrap();
        self.heights.insert(hash.to_string(), h);
        h
    }

    /// Sign locally and submit the exact bytes (resubmitting the same bytes
    /// is idempotent). Returns the tx hash or the RPC error (invalid tx).
    pub async fn submit(
        &self,
        signer: &str,
        key: &SecretKey,
        receiver: &str,
        actions: Vec<Action>,
    ) -> Result<String, String> {
        let group = vec![(signer.to_string(), receiver.to_string(), actions)];
        let (hash, b64) = self.sign_all(group, |_| key.clone()).await.remove(0);
        self.send(&b64).await.map(|()| hash)
    }

    /// Sign transactions for back-to-back submission: one head block hash,
    /// nonces counted up locally per signer (a signer may appear twice).
    async fn sign_all(
        &self,
        txs: Vec<(String, String, Vec<Action>)>,
        key_of: impl Fn(&str) -> SecretKey,
    ) -> Vec<(String, String)> {
        // The head, not the final block: right after a fast-forward the final
        // block can be from before the jump, i.e. already past the tx
        // validity window, and the node silently drops the tx.
        let block = self.call_ok("block", json!({"finality": "optimistic"})).await;
        let block_hash = CryptoHash::from_str(block["header"]["hash"].as_str().unwrap()).unwrap();
        let mut nonces: HashMap<String, u64> = HashMap::new();
        let mut out = vec![];
        for (signer, receiver, actions) in txs {
            let key = key_of(&signer);
            let pk: PublicKey = key.public_key();
            let nonce = match nonces.get(&signer) {
                Some(n) => n + 1,
                None => {
                    let ak = self
                        .call_ok(
                            "query",
                            json!({"request_type": "view_access_key", "finality": "optimistic",
                                   "account_id": signer, "public_key": pk.to_string()}),
                        )
                        .await;
                    ak["nonce"].as_u64().unwrap() + 1
                }
            };
            nonces.insert(signer.clone(), nonce);
            let tx = Transaction::new(
                account(&signer),
                pk,
                nonce,
                account(&receiver),
                block_hash,
                actions,
            );
            let signed = tx.sign(&key);
            out.push((signed.get_hash().to_string(), signed.to_base64()));
        }
        out
    }

    /// Submit a signed transaction (base64 borsh). Resubmitting the same
    /// bytes is idempotent. An invalid tx comes back as the RPC's structured
    /// error, in `Debug` form so the `InvalidTxError` variant names are in it.
    pub async fn send(&self, b64: &str) -> Result<(), String> {
        let params = json!({"signed_tx_base64": b64, "wait_until": "NONE"});
        for i in 0.. {
            match self.rpc.call::<Value, Value>("send_tx", params.clone()).await {
                Ok(_) => return Ok(()),
                Err(e @ (RpcError::InvalidTx(_) | RpcError::InvalidTransaction { .. })) => {
                    return Err(format!("{e:?}"));
                }
                Err(e) if i > 20 => return Err(format!("{e:?}")),
                Err(_) => tokio::time::sleep(Duration::from_millis(200)).await,
            }
        }
        unreachable!()
    }

    /// `sandbox_patch_state` in batches, then wait for it to land.
    pub async fn patch_records(&self, records: &[Value]) {
        for chunk in records.chunks(2_000) {
            self.call_ok("sandbox_patch_state", json!({"records": chunk})).await;
        }
        self.wait_blocks(3).await;
    }

    /// Re-fetch the status of every tx and keep the latest one we got.
    /// Test sandboxes retain 100 epochs: the image's default 5 can forget a
    /// transaction while a callback from a later yield is still pending.
    /// A snapshot preserves the last response, not outcomes RPC never
    /// returned. Returns whether each transaction is known and whether
    /// every referenced receipt has executed.
    pub async fn refresh(&mut self, txs: &[(String, String)]) -> Vec<(bool, bool)> {
        let mut out = vec![];
        for (hash, signer) in txs {
            let st = self
                .call(
                    "EXPERIMENTAL_tx_status",
                    json!({"tx_hash": hash, "sender_account_id": signer, "wait_until": "NONE"}),
                )
                .await
                .ok()
                // A tx the chunk found invalid (e.g. a strict-mode nonce gap,
                // which the RPC doesn't check) has a failed status and no
                // receipts; anything else without receipts isn't in yet.
                .filter(|st| {
                    st["receipts_outcome"].as_array().is_some_and(|r| !r.is_empty())
                        || st["status"].get("Failure").is_some()
                });
            let Some(st) = st else {
                let known = self.snap.contains_key(hash);
                out.push((known, known && done(&self.snap[hash])));
                continue;
            };
            let n = st["receipts_outcome"].as_array().unwrap().len();
            let prev = self.snap.get(hash).map(|v| v["receipts_outcome"].as_array().unwrap().len());
            if prev != Some(n) || !self.snap[hash]["status"].eq(&st["status"]) {
                let full = self.with_heights(st).await;
                self.snap.insert(hash.clone(), full);
            }
            out.push((true, done(&self.snap[hash])));
        }
        out
    }

    /// Wait until the receipt DAGs of all given txs are settled: every
    /// referenced receipt executed, or no DAG grew for `QUIET_BLOCKS`. All
    /// txs, not just the latest: a resume in a later tx wakes callbacks that
    /// live in an earlier tx's DAG.
    pub async fn quiesce(&mut self, txs: &[(String, String)]) {
        self.quiesce_within(txs, None).await;
    }

    /// `quiesce`, but with `unknown_limit`, give up once some tx's status has
    /// stayed unknown for that many blocks (the node dropped it) and return
    /// false.
    pub async fn quiesce_within(
        &mut self,
        txs: &[(String, String)],
        unknown_limit: Option<u64>,
    ) -> bool {
        let (mut last, mut stable_since) = (usize::MAX, 0);
        let mut unknown_since = None;
        let t0 = Instant::now();
        for waited in 0.. {
            // In process a wait is one block, and a scenario settles within
            // 20: a transport that never does fails here, not in 10 minutes.
            assert!(!self.in_process || waited < 1_000, "quiesce: 1000 blocks: {txs:?}");
            assert!(t0.elapsed() < Duration::from_secs(600), "quiesce timeout: {txs:?}");
            let r = self.refresh(txs).await;
            if let Some(limit) = unknown_limit
                && !r.iter().all(|(known, _)| *known)
            {
                let h = self.height().await;
                if h >= *unknown_since.get_or_insert(h) + limit {
                    return false;
                }
            }
            if r.iter().all(|(known, _)| *known) {
                if r.iter().all(|(_, done)| *done) {
                    return true;
                }
                let total: usize = txs
                    .iter()
                    .map(|(h, _)| self.snap[h]["receipts_outcome"].as_array().unwrap().len())
                    .sum();
                let h = self.height().await;
                if total != last {
                    (last, stable_since) = (total, h);
                } else if h >= stable_since + QUIET_BLOCKS {
                    return true;
                }
            }
            self.wait_blocks(1).await;
        }
        unreachable!()
    }

    /// Attach every outcome's block height (and lift the gas profile to where
    /// the normalizer expects it).
    async fn with_heights(&mut self, mut st: Value) -> Value {
        let mut to = st["transaction_outcome"].take();
        let mut ro: Vec<Value> = st["receipts_outcome"].take().as_array().unwrap().clone();
        for o in std::iter::once(&mut to).chain(ro.iter_mut()) {
            let h = self.block_height(o["block_hash"].as_str().unwrap()).await;
            o["block_height"] = json!(h);
            let profile = o["outcome"]["metadata"]["gas_profile"].clone();
            o["outcome"]["gas_profile"] = profile;
        }
        st["transaction_outcome"] = to;
        st["receipts_outcome"] = json!(ro);
        st
    }

    pub async fn view_account(&self, id: &str) -> Value {
        match self
            .call(
                "query",
                json!({"request_type": "view_account", "finality": "optimistic", "account_id": id}),
            )
            .await
        {
            Ok(a) => json!({
                "amount": a["amount"], "locked": a["locked"],
                "storage_usage": a["storage_usage"], "code_hash": a["code_hash"],
            }),
            Err(_) => Value::Null,
        }
    }

    async fn view_state(&self, id: &str) -> Vec<(Vec<u8>, Vec<u8>)> {
        let r = self
            .call_ok(
                "query",
                json!({"request_type": "view_state", "finality": "optimistic",
                       "account_id": id, "prefix_base64": ""}),
            )
            .await;
        r["values"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| (unb64(v["key"].as_str().unwrap()), unb64(v["value"].as_str().unwrap())))
            .collect()
    }

    /// Run one scenario (its accounts must not exist yet).
    pub async fn run(&mut self, sc: &Scenario) -> Value {
        let keys: BTreeMap<&str, SecretKey> =
            sc.keys.iter().map(|(a, k)| (a.as_str(), SecretKey::from_str(k).unwrap())).collect();
        let balance = NearToken::from_yoctonear(sc.start_balance.parse().unwrap());
        let root = self.root.clone();
        let root_key = self.root_key.clone();
        for id in sc.names.signers() {
            let mut actions = vec![
                Action::create_account(),
                Action::transfer(balance),
                Action::add_full_access_key(keys[id.as_str()].public_key()),
            ];
            if sc.names.contracts.contains(&id) {
                actions.push(Action::deploy_contract(fuzz_wasm()));
            }
            let h = self.submit(&root, &root_key, &id, actions).await.expect("setup tx");
            self.quiesce(&[(h, root.clone())]).await;
        }
        let mut setup = BTreeMap::new();
        for id in sc.names.signers() {
            setup.insert(id.clone(), self.view_account(&id).await);
        }

        let mut hashes: Vec<Option<(String, String)>> = vec![];
        let mut warnings: Vec<String> = vec![];
        let mut steps = vec![];
        for step in &sc.steps {
            let out = match step {
                Step::TxGroup { txs } => {
                    // Signed up front and sent back to back, so they land in
                    // as few chunks as the node's pool allows.
                    let group = txs
                        .iter()
                        .map(|(s, r, a)| (s.clone(), r.clone(), kit_actions(a)))
                        .collect();
                    let signed = self.sign_all(group, |s| keys[s].clone()).await;
                    let mut first = None;
                    for (i, ((signer, _, _), (hash, b64))) in txs.iter().zip(signed).enumerate() {
                        let r = self.send(&b64).await.map(|()| hash.clone());
                        if let Err(e) = &r {
                            warnings.push(format!("step {} group tx {i} send: {e}", steps.len()));
                        }
                        hashes.push(r.as_ref().ok().map(|h| (h.clone(), signer.clone())));
                        first.get_or_insert(r);
                    }
                    let all: Vec<_> = hashes.iter().flatten().cloned().collect();
                    self.quiesce(&all).await;
                    match first.unwrap() {
                        Ok(h) => json!({"kind": "Tx", "status": self.snap[&h]["status"]}),
                        Err(e) => json!({"kind": "Tx", "error": e}),
                    }
                }
                Step::Tx { signer, receiver, actions } => {
                    let acts = kit_actions(actions);
                    match self.submit(signer, &keys[signer.as_str()], receiver, acts).await {
                        Ok(h) => {
                            let mut all: Vec<_> = hashes.iter().flatten().cloned().collect();
                            all.push((h.clone(), signer.clone()));
                            self.quiesce(&all).await;
                            let st = self.snap[&h].clone();
                            hashes.push(Some((h, signer.clone())));
                            json!({"kind": "Tx", "status": st["status"]})
                        }
                        Err(e) => {
                            hashes.push(None);
                            json!({"kind": "Tx", "error": e})
                        }
                    }
                }
                Step::View { account: a, .. } | Step::Digest { account: a } => {
                    let (method, args) = match step {
                        Step::View { program_hex, .. } => ("run", unhex(program_hex)),
                        _ => ("digest", vec![]),
                    };
                    let kind = if method == "run" { "View" } else { "Digest" };
                    match self
                        .call(
                            "query",
                            json!({"request_type": "call_function", "finality": "optimistic",
                                   "account_id": a, "method_name": method, "args_base64": b64(&args)}),
                        )
                        .await
                    {
                        Ok(r) if r.get("error").is_some() => json!({"kind": kind, "error": r["error"]}),
                        Ok(r) => {
                            let bytes: Vec<u8> = r["result"]
                                .as_array()
                                .unwrap()
                                .iter()
                                .map(|x| x.as_u64().unwrap() as u8)
                                .collect();
                            json!({"kind": kind, "result": b64(&bytes), "logs": r["logs"]})
                        }
                        Err(e) => json!({"kind": kind, "error": e}),
                    }
                }
                Step::Patch { account: a, key_hex, value_hex: Some(v) } => {
                    // Mirror Sim::set_state: sandbox_patch_state writes the
                    // record but doesn't touch storage_usage, so patch the
                    // account record with the recomputed usage as well.
                    let acc = self
                        .call_ok(
                            "query",
                            json!({"request_type": "view_account", "finality": "optimistic", "account_id": a}),
                        )
                        .await;
                    let (key, val) = (unhex(key_hex), unhex(v));
                    let old = self.view_state(a).await.into_iter().find(|(k, _)| *k == key);
                    let mut usage = acc["storage_usage"].as_u64().unwrap();
                    if let Some((k, v)) = old {
                        usage -= 40 + (k.len() + v.len()) as u64;
                    }
                    usage += 40 + (key.len() + val.len()) as u64;
                    self.call_ok(
                        "sandbox_patch_state",
                        json!({"records": [
                            {"Account": {"account_id": a, "account": {
                                "amount": acc["amount"], "locked": acc["locked"],
                                "code_hash": acc["code_hash"], "storage_usage": usage, "version": "V1"}}},
                            {"Data": {"account_id": a, "data_key": b64(&key), "value": b64(&val)}},
                        ]}),
                    )
                    .await;
                    self.wait_blocks(2).await;
                    json!({"kind": "Patch"})
                }
                Step::Wait { blocks } => {
                    let start = self.height().await;
                    // In chunks, refreshing snapshots before outcomes age out.
                    let all: Vec<_> = hashes.iter().flatten().cloned().collect();
                    let mut left = *blocks;
                    while left > 0 {
                        let n = left.min(20);
                        self.wait_blocks(n).await;
                        self.refresh(&all).await;
                        left -= n;
                    }
                    self.quiesce(&all).await;
                    // Anything parked during the wait must not reach its
                    // 200-block timeout before the final reads, or the two
                    // sides compare different moments.
                    let overshoot = self.height().await.saturating_sub(start + blocks);
                    if overshoot > 150 {
                        warnings
                            .push(format!("sandbox overshot the final wait by {overshoot} blocks"));
                    }
                    json!({"kind": "Wait"})
                }
                s => panic!("step not supported by the sandbox: {s:?}"),
            };
            steps.push(out);
        }
        let mut txs = vec![];
        let all: Vec<_> = hashes.iter().flatten().cloned().collect();
        self.refresh(&all).await;
        for h in &hashes {
            txs.push(match h {
                Some((h, _)) => self.snap[h].clone(),
                None => Value::Null,
            });
        }
        let mut accounts = BTreeMap::new();
        for id in sc.names.all_accounts() {
            accounts.insert(id.clone(), self.view_account(&id).await);
        }
        let mut state = BTreeMap::new();
        for c in &sc.names.contracts {
            let kv: Vec<Value> =
                self.view_state(c).await.iter().map(|(k, v)| json!([b64(k), b64(v)])).collect();
            state.insert(c.clone(), Value::Array(kv));
        }
        json!({"setup_accounts": setup, "steps": steps, "txs": txs, "accounts": accounts,
               "state": state, "harness_warnings": warnings})
    }
}
