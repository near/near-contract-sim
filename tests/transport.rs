#![cfg(feature = "near-kit")]
//! `SimTransport`: near-kit's own client code against the simulator. Every
//! test runs on tokio's paused clock, where near-kit's back-offs and a
//! timed-out wait's sleep take no wall time.

mod common;
use common::*;
use near_contract_sim::{
    AccessKey, Balance, Gas, KeyType, SecretKey, Sim, SimTransport, StateRecord,
};
use near_kit::protocol::{
    AccessKeyPermission, Action, AddKeyAction, GasKeyInfo, InvalidTxError, PublishMode,
};
use near_kit::rpc::{
    AccessKeyPermissionView, BlockReference, RpcError, RpcTransport, TxExecutionStatus,
};
use near_kit::signer::{InMemorySigner, PublicKey};
use near_kit::transaction::{Final, Submitted};
use near_kit::{AccountId, CryptoHash, Error, Near, NearToken};
use serde_json::{Value, json};
use std::time::Duration;

#[tokio::test(start_paused = true)] // back-offs and timed-out waits take no wall time
async fn near_kit_runs_on_the_sim() {
    let node = SimTransport::new(Sim::cached(probe_wasm(), probe_sim)); // any Sim
    let alice = node.near("alice.near"); // a near_kit::Near signing with the sim's key

    let t = 1_800_000_000_000_000_000; // 2027-01-15, in ns
    node.with(|sim| sim.set_timestamp_ns(t)); // sim-only controls go through the handle
    let seen: u64 = alice.view("probe.near", "ts").await.unwrap(); // near-kit, as production code calls it
    assert_eq!(seen, t);
    let out = alice.call("probe.near", "nope").await.unwrap();
    assert!(out.is_failure(), "{out:?}"); // near-kit answers a failed call with Ok(outcome)
    let hash = near_contract_sim::CryptoHash(out.transaction.hash.into()); // nearcore's hash type
    println!("{}", node.with(|sim| sim.outcome(hash))); // the sim's receipt tree of the call
    node.with(|sim| sim.findings()).assert_none(); // every call waited for FINAL, so conservation runs
}

/// A pinned time is what a call near-kit sends sees, as for a `Tx`.
#[tokio::test(start_paused = true)]
async fn a_call_near_kit_sends_sees_the_pinned_time() {
    let node = SimTransport::new(Sim::cached(probe_wasm(), probe_sim));
    let alice = node.near("alice.near");
    let t = 1_800_000_000_000_000_000;
    node.with(|sim| sim.set_timestamp_ns(t));
    let out = alice.call("probe.near", "ts").await.unwrap();
    let hash = near_contract_sim::CryptoHash(out.transaction.hash.into());
    assert_eq!(node.with(|sim| sim.outcome(hash)).json::<u64>(), t);
}

/// A request as near-kit posts it: the HTTP status and the JSON-RPC response.
async fn post(node: &SimTransport, method: &str, params: Value) -> (u16, Value) {
    let request = json!({"jsonrpc": "2.0", "id": 0, "method": method, "params": params});
    let r = node.post_json("", serde_json::to_vec(&request).unwrap()).await.unwrap();
    (r.status, serde_json::from_slice(&r.body).unwrap())
}

/// A failed request's HTTP status and the name of its cause.
async fn fails(node: &SimTransport, method: &str, params: Value) -> (u16, String) {
    let (status, r) = post(node, method, params).await;
    (status, r["error"]["cause"]["name"].as_str().expect("an error").to_string())
}

/// A failed request's `data`, which near-kit's `UnknownBlock` carries.
async fn data(node: &SimTransport, method: &str, params: Value) -> String {
    let (_, r) = post(node, method, params).await;
    r["error"]["data"].as_str().expect("an error").to_string()
}

fn rpc_error<T: std::fmt::Debug>(r: Result<T, Error>) -> RpcError {
    match r {
        Err(Error::Rpc(e)) => *e,
        r => panic!("{r:?}"),
    }
}

/// A nearcore value as its near-kit twin, through their shared text form.
fn kit<T: std::str::FromStr<Err: std::fmt::Debug>>(v: impl ToString) -> T {
    v.to_string().parse().unwrap()
}

fn id(s: &str) -> AccountId {
    kit(s)
}

fn height(node: &SimTransport) -> u64 {
    node.with(|sim| sim.height())
}

/// The status a raw `send_tx` reports at `level`.
async fn send_at(
    near: &Near,
    to: &str,
    deposit: u128,
    level: TxExecutionStatus,
) -> TxExecutionStatus {
    let call = near.call(to, "height").deposit(NearToken::from_yoctonear(deposit));
    let tx = call.finish().sign().await.unwrap();
    near.rpc().send_tx(&tx, level).await.unwrap().final_execution_status
}

#[tokio::test(start_paused = true)]
async fn account_queries_read_the_sim() {
    let node = SimTransport::new(probe_sim());
    let alice = node.near("alice.near");
    let (key, probe, amount) = node.with(|sim| {
        sim.set_state("probe.near", b"k", Some(b"v"));
        let key = sim.secret_key("alice.near").public_key();
        (key, sim.account("probe.near").unwrap(), sim.balance("bob.near"))
    });
    let account = alice.account("probe.near").await.unwrap();
    assert_eq!(account.code_hash.to_string(), probe.code_hash.to_string());
    assert_eq!(account.storage_usage, probe.storage_usage);
    assert_eq!(
        alice.balance("bob.near").await.unwrap().total.as_yoctonear(),
        amount.as_yoctonear()
    );
    assert!(alice.account_exists("bob.near").await.unwrap());
    assert!(!alice.account_exists("nobody.near").await.unwrap());
    let seen: u64 = alice.view("probe.near", "height").await.unwrap();
    assert_eq!(seen, height(&node));
    let keys = alice.access_keys("alice.near").await.unwrap().keys;
    assert_eq!(
        keys.iter().map(|k| k.public_key.to_string()).collect::<Vec<_>>(),
        [key.to_string()]
    );
    assert_eq!(alice.contract_code("probe.near").await.unwrap().code, probe_wasm());
    let (rpc, at) = (alice.rpc(), BlockReference::final_());
    let state = rpc.view_state(&id("probe.near"), b"k", None, None, at).await.unwrap().values;
    assert_eq!((state[0].key.as_slice(), state[0].value.as_slice()), (&b"k"[..], &b"v"[..]));
    assert_eq!(rpc.view_access_key(&id("alice.near"), &kit(&key), at).await.unwrap().nonce, 0);
}

/// Global contracts published and used, and a gas key added and funded, with
/// near-kit's own sends.
#[tokio::test(start_paused = true)]
async fn global_contracts_and_gas_keys() {
    let node = SimTransport::new(probe_sim());
    let (alice, bob) = (node.near("alice.near"), node.near("bob.near"));
    let published =
        [(status_wasm(), PublishMode::Immutable), (probe_wasm(), PublishMode::Updatable)];
    for (wasm, mode) in published {
        let out = bob.publish(wasm, mode).await.unwrap();
        assert!(out.is_success(), "{out:?}");
    }
    let hash = CryptoHash::hash(&status_wasm());
    assert_eq!(alice.global_contract(hash).await.unwrap().code, status_wasm());
    assert_eq!(alice.global_contract("bob.near").await.unwrap().code, probe_wasm());
    assert!(alice.deploy_from("bob.near").await.unwrap().is_success());
    assert!(bob.deploy_from(hash).await.unwrap().is_success());
    let (a, b) =
        (alice.account("alice.near").await.unwrap(), alice.account("bob.near").await.unwrap());
    assert_eq!(
        (a.global_contract_account_id, b.global_contract_hash),
        (Some(id("bob.near")), Some(hash))
    );
    let seen: u64 = alice.view("alice.near", "height").await.unwrap();
    assert_eq!(seen, height(&node), "alice.near runs bob.near's probe");

    let gas_key: PublicKey = kit(SecretKey::from_seed(KeyType::ED25519, "gas").public_key());
    let info = GasKeyInfo { balance: NearToken::ZERO, num_nonces: 3 };
    let access_key = near_kit::protocol::AccessKey {
        nonce: 0,
        permission: AccessKeyPermission::GasKeyFullAccess(info),
    };
    let add = Action::AddKey(AddKeyAction { public_key: gas_key.clone(), access_key });
    let fund = Action::transfer_to_gas_key(gas_key.clone(), NearToken::from_near(1));
    let out = alice.transaction("alice.near").add_action(add).add_action(fund).await.unwrap();
    assert!(out.is_success(), "{out:?}");
    let (rpc, at) = (alice.rpc(), BlockReference::final_());
    let key = rpc.view_access_key(&id("alice.near"), &gas_key, at).await.unwrap().permission;
    let AccessKeyPermissionView::GasKeyFullAccess { balance, num_nonces } = key else {
        panic!("{key:?}")
    };
    assert_eq!((balance, num_nonces), (NearToken::from_near(1), 3));
    let nonces = rpc.view_gas_key_nonces(&id("alice.near"), &gas_key, at).await.unwrap();
    assert_eq!(nonces.nonces.len(), 3);
}

/// Every block is final and has one chunk; `status` and `gas_price` answer
/// from the chain's parameters and its last block.
#[tokio::test(start_paused = true)]
async fn blocks_and_status_are_the_logs() {
    let node = SimTransport::new(probe_sim());
    let rpc = node.near("alice.near").rpc().clone();
    let (h, time, root, supply) = node.with(|sim| {
        let held = ["alice.near", "bob.near", "probe.near"].map(|a| sim.balance(a).as_yoctonear());
        (sim.height(), sim.timestamp_ns(), sim.state_root(), held.iter().sum::<u128>())
    });
    let head = rpc.block(BlockReference::final_()).await.unwrap();
    let header = &head.header;
    assert_eq!(
        (header.height, header.timestamp, &header.timestamp_nanosec),
        (h, time, &time.to_string())
    );
    assert_eq!((header.last_final_block, header.last_ds_final_block), (header.hash, header.hash));
    assert_eq!((header.chunks_included, &header.chunk_mask[..]), (1, &[true][..]));
    assert_eq!(
        (header.gas_price.as_yoctonear(), header.total_supply.as_yoctonear()),
        (100_000_000, supply)
    );
    assert_eq!((header.latest_protocol_version, head.author.as_str()), (88, "validator.near"));
    let chunk = &head.chunks[0];
    assert_eq!((chunk.height_created, chunk.height_included), (h, h));
    assert_eq!(chunk.gas_limit, 1_000_000_000_000_000);
    let prev = rpc.block(BlockReference::at_height(h - 1)).await.unwrap();
    assert_eq!((header.prev_hash, chunk.prev_block_hash), (prev.header.hash, prev.header.hash));
    // The next block's chunk applies on the state block h left.
    node.with(|sim| sim.step());
    let after = rpc.block(BlockReference::at_height(h + 1)).await.unwrap();
    assert_eq!(after.chunks[0].prev_state_root.to_string(), root.to_string());
    let merkle = near_contract_sim::CryptoHash::hash_borsh(root);
    assert_eq!(after.header.prev_state_root.to_string(), merkle.to_string());
    assert_eq!(rpc.block(BlockReference::at_hash(header.hash)).await.unwrap().header.height, h);
    let genesis = rpc.block(BlockReference::genesis()).await.unwrap();
    assert_eq!((genesis.header.height, genesis.header.timestamp), (0, 1_600_000_000_000_000_000));
    let by_hash = rpc.block(BlockReference::at_hash(genesis.header.hash)).await.unwrap();
    assert_eq!((by_hash.header.height, by_hash.header.prev_hash), (0, CryptoHash::ZERO));
    let by_height = rpc.block(BlockReference::at_height(0)).await.unwrap();
    assert_eq!(by_height.header.hash, genesis.header.hash);
    let status = rpc.status().await.unwrap();
    let sync = &status.sync_info;
    assert_eq!((sync.latest_block_height, sync.latest_block_hash), (h + 1, after.header.hash));
    assert_eq!((status.chain_id.as_str(), status.genesis_hash), ("localnet", genesis.header.hash));
    assert_eq!((status.protocol_version, status.version.build.as_str()), (88, "near-contract-sim"));
    assert_eq!(sync.latest_state_root, Some(after.header.prev_state_root));
    let earliest = (sync.earliest_block_height, sync.earliest_block_hash);
    assert_eq!(earliest, (Some(0), Some(genesis.header.hash)), "every state is kept");
    assert_eq!(
        rpc.gas_price(Some(&header.hash)).await.unwrap().gas_price.as_yoctonear(),
        100_000_000
    );
    node.with(|sim| sim.config.gas_price = Balance::from_yoctonear(7));
    assert_eq!(rpc.gas_price(None).await.unwrap().gas_price.as_yoctonear(), 7);
    let unknown = rpc.gas_price(Some(&CryptoHash::ZERO)).await.unwrap_err();
    assert!(matches!(unknown, RpcError::UnknownBlock(_)), "{unknown:?}");
    // A new seed or version is the next block's: genesis stays what the first
    // block applied on.
    node.with(|sim| (sim.config.seed, sim.config.protocol_version) = (7, 86));
    let again = rpc.block(BlockReference::genesis()).await.unwrap().header;
    assert_eq!((again.hash, again.latest_protocol_version), (genesis.header.hash, 88));
    assert_eq!(rpc.status().await.unwrap().genesis_hash, genesis.header.hash);
}

/// Each of near-kit's sends lands as the sim's own would.
#[tokio::test(start_paused = true)]
async fn sends_land_in_the_sim() {
    let node = SimTransport::new(probe_sim());
    let (alice, bob) = (node.near("alice.near"), node.near("bob.near"));
    let before = node.with(|sim| sim.balance("bob.near"));
    let sent = alice.transfer("bob.near", NearToken::from_yoctonear(5)).await.unwrap();
    assert_eq!(
        node.with(|sim| sim.balance("bob.near")),
        before.saturating_add(Balance::from_yoctonear(5))
    );
    assert!(sent.is_success());
    let queued = alice.call("probe.near", "height").send().wait_until::<Submitted>().await.unwrap();
    assert!(!node.with(|sim| sim.is_idle()), "queued, nothing produced");
    let k1 = SecretKey::from_seed(KeyType::ED25519, "k1").public_key();
    let out =
        alice.transaction("sub.alice.near").create_account().transfer(NearToken::from_near(5));
    let out = out.add_full_access_key(kit(&k1)).deploy(status_wasm()).await.unwrap();
    assert!(out.is_success(), "{out:?}");
    let sub = node.with(|sim| sim.account("sub.alice.near").unwrap());
    assert_eq!(sub.amount, near(5));
    assert!(node.with(|sim| sim.access_key("sub.alice.near", &k1)).is_some());
    let k2 = SecretKey::from_seed(KeyType::ED25519, "k2").public_key();
    alice.add_full_access_key(kit(&k2)).await.unwrap();
    assert!(node.with(|sim| sim.access_key("alice.near", &k2)).is_some());
    alice.delete_key(kit(&k2)).await.unwrap();
    assert!(node.with(|sim| sim.access_key("alice.near", &k2)).is_none());
    // A meta transaction: alice signs, bob relays and pays.
    let signed = alice.transaction("probe.near").call("height").finish();
    let signed = signed.delegate(Default::default()).await.unwrap().signed_delegate_action;
    let relayed = bob.transaction("alice.near").signed_delegate_action(signed).await.unwrap();
    assert!(relayed.is_success(), "{relayed:?}");
    node.with(|sim| {
        let relayed = sim.outcome(near_contract_sim::CryptoHash(relayed.transaction.hash.into()));
        let call = relayed.receipt("height").unwrap();
        assert_eq!(
            (call.predecessor_id.as_str(), call.receiver_id.as_str()),
            ("alice.near", "probe.near")
        );
        let queued = sim.outcome(near_contract_sim::CryptoHash(queued.transaction_hash.into()));
        assert!(queued.is_success());
        sim.findings().assert_none();
    });
}

/// near-kit signs with a secp256k1 key from the sim, and the runtime takes the
/// signature.
#[tokio::test(start_paused = true)]
async fn a_secp256k1_key_signs() {
    let node = SimTransport::new(probe_sim());
    let key = SecretKey::from_seed(KeyType::SECP256K1, "carol");
    node.with(|sim| sim.create_account_with_key("carol.near", near(10), &key));
    let out = node.near("carol.near").transfer("bob.near", NearToken::from_yoctonear(1)).await;
    assert!(out.unwrap().is_success());
}

/// `NONE` produces no block. `INCLUDED` answers at conversion, at the level
/// nearcore reports there: `INCLUDED_FINAL` while a receipt waits to run,
/// `EXECUTED` while only a refund does (a call to the signer itself). An
/// executed level waits until the refunds landed.
#[tokio::test(start_paused = true)]
async fn waits_end_at_their_level() {
    let node = SimTransport::new(probe_sim());
    let (alice, probe) = (node.near("alice.near"), node.near("probe.near"));
    let h = height(&node);
    let queued = alice.call("probe.near", "height").send().wait_until::<Submitted>();
    queued.await.unwrap();
    assert_eq!(height(&node), h);
    use TxExecutionStatus::{Executed, Final as AtFinal, Included, IncludedFinal};
    assert_eq!(send_at(&alice, "probe.near", 0, Included).await, IncludedFinal);
    assert_eq!(height(&node), h + 1, "the block that converted both");
    assert_eq!(send_at(&probe, "probe.near", 0, Included).await, Executed);
    // A call that fails returns its deposit before its gas: two refunds.
    assert_eq!(send_at(&alice, "alice.near", 1, Included).await, IncludedFinal);
    assert_eq!(
        send_at(&alice, "probe.near", 0, TxExecutionStatus::ExecutedOptimistic).await,
        AtFinal
    );
    let h = height(&node);
    assert_eq!(send_at(&alice, "probe.near", 0, IncludedFinal).await, IncludedFinal);
    assert_eq!(height(&node), h + 1, "INCLUDED_FINAL is met at conversion too");
    assert_eq!(send_at(&alice, "probe.near", 0, Executed).await, AtFinal);
    node.with(|sim| {
        assert!(sim.is_idle());
        sim.findings().assert_none();
    });
}

/// `send_tx` answers with the outcome alone; `EXPERIMENTAL_tx_status` adds
/// the receipts, all but the local one, and `EXPERIMENTAL_receipt_to_tx`
/// finds the transaction back.
#[tokio::test(start_paused = true)]
async fn statuses_and_receipts() {
    let node = SimTransport::new(probe_sim());
    let probe = node.near("probe.near");
    let sent = probe.call("probe.near", "call_missing").await.unwrap();
    assert!(sent.receipts.is_empty() && !sent.receipts_outcome.is_empty());
    let hash = sent.transaction.hash;
    let status = probe.tx_status(&hash, "probe.near").wait_until::<Final>().await.unwrap();
    assert_eq!(status.receipts_outcome.len(), sent.receipts_outcome.len());
    assert_eq!(status.receipts.len(), status.receipts_outcome.len() - 1, "no local receipt");
    let ids = |o: &near_kit::rpc::FinalExecutionOutcome| {
        o.receipts_outcome.iter().map(|r| r.id).collect::<Vec<_>>()
    };
    assert_eq!(ids(&status), ids(&sent));
    let mut sim_order = node.with(|sim| {
        let out = sim.outcome(near_contract_sim::CryptoHash(hash.into()));
        out.receipts.iter().map(|r| r.receipt_id.to_string()).collect::<Vec<_>>()
    });
    let mut kit_order: Vec<String> = ids(&sent).iter().map(ToString::to_string).collect();
    assert_ne!(kit_order, sim_order, "nearcore's order is depth first, not by block");
    kit_order.sort();
    sim_order.sort();
    assert_eq!(kit_order, sim_order);
    let found = probe.rpc().receipt_to_tx(&status.receipts_outcome[2].id).await.unwrap();
    assert_eq!((found.transaction_hash, found.sender_account_id.as_str()), (hash, "probe.near"));
    let unknown = probe.rpc().receipt_to_tx(&hash).await.unwrap_err();
    assert!(matches!(unknown, RpcError::UnknownReceipt(_)), "{unknown:?}");
}

/// near-kit's typed errors, from the answers a node gives.
#[tokio::test(start_paused = true)]
async fn errors_reach_near_kit_typed() {
    let node = SimTransport::new(probe_sim());
    let (alice, bob) = (node.near("alice.near"), node.near("bob.near"));
    let stranger: PublicKey = kit(SecretKey::from_seed(KeyType::ED25519, "x").public_key());
    node.with(|sim| {
        sim.set_state("probe.near", b"big", Some(&[0; 60_000]));
        let key = |i: u32| SecretKey::from_seed(KeyType::ED25519, &i.to_string()).public_key();
        let bob = || "bob.near".parse().unwrap();
        sim.apply_state_records(
            (0..100).map(|i| StateRecord::access_key(bob(), key(i), AccessKey::full_access())),
        );
    });
    let rpc = alice.rpc();
    let at = BlockReference::final_();
    let e = rpc_error(alice.account("nobody.near").await);
    assert!(matches!(e, RpcError::AccountNotFound { .. }), "{e:?}");
    let e = rpc.view_access_key(&id("alice.near"), &stranger, at).await.unwrap_err();
    assert!(matches!(e, RpcError::AccessKeyNotFound { .. }), "{e:?}");
    let e = rpc_error(alice.contract_code("alice.near").await);
    assert!(matches!(e, RpcError::ContractNotDeployed { .. }), "{e:?}");
    let e = rpc_error(alice.contract_code("nobody.near").await);
    assert!(matches!(e, RpcError::AccountNotFound { .. }), "{e:?}");
    let e = rpc_error(alice.view::<Value>("probe.near", "nope").await);
    assert!(matches!(e, RpcError::MethodNotFound { .. }), "{e:?}");
    let e = rpc_error(alice.view::<Value>("nobody.near", "nope").await);
    assert!(matches!(e, RpcError::AccountNotFound { .. }), "{e:?}");
    let e = rpc_error(alice.global_contract(CryptoHash::ZERO).await);
    assert!(matches!(e, RpcError::GlobalContractNotFound { .. }), "{e:?}");
    let e = rpc.view_gas_key_nonces(&id("alice.near"), &stranger, at).await.unwrap_err();
    assert!(matches!(e, RpcError::GasKeyNotFound { .. }), "{e:?}");
    let e = rpc.view_state(&id("nobody.near"), b"", None, None, at).await.unwrap_err();
    assert!(matches!(e, RpcError::AccountNotFound { .. }), "{e:?}");
    let e = rpc.view_state(&id("probe.near"), b"", None, None, at).await.unwrap_err();
    assert!(matches!(e, RpcError::ContractStateTooLarge { .. }), "{e:?}");
    let e = rpc.view_access_key_list_page(&id("bob.near"), None, None, at).await.unwrap_err();
    assert!(matches!(e, RpcError::TooManyAccessKeys { limit: 100, .. }), "{e:?}");
    assert_eq!(bob.access_keys("bob.near").await.unwrap().keys.len(), 101, "paginated");
    assert!(alice.access_keys("nobody.near").await.unwrap().keys.is_empty(), "not an error");
    let e = rpc_error(alice.balance("alice.near").at_block(10_000).await);
    assert!(matches!(e, RpcError::UnknownBlock(_)), "{e:?}");
    let e = rpc_error(alice.validators().await);
    assert!(matches!(e, RpcError::Network { status_code: Some(400), .. }), "{e:?}");
}

/// The HTTP statuses of nearcore's server, which near-kit's retries follow,
/// `query`'s errors that come back as results, and the store's messages for a
/// block it doesn't have.
#[tokio::test(start_paused = true)]
async fn statuses_are_the_servers() {
    let node = SimTransport::new(probe_sim());
    let head = height(&node);
    let method = fails(&node, "validators", json!([null])).await;
    assert_eq!(method, (400, "METHOD_NOT_FOUND".into()));
    let parse = fails(&node, "block", json!({"finality": "soon"})).await;
    assert_eq!(parse, (400, "PARSE_ERROR".into()));
    let page = json!({"request_type": "view_state", "finality": "final", "account_id": "probe.near",
                      "prefix_base64": "AA==", "after_key_base64": "AQ=="});
    assert_eq!(fails(&node, "query", page).await, (400, "PARSE_ERROR".into()));
    let garbage = node.post_json("", b"{".to_vec()).await.unwrap();
    assert_eq!(garbage.status, 400);
    let account = json!({"finality": "final", "account_id": "nobody.near"});
    assert_eq!(
        fails(&node, "EXPERIMENTAL_view_account", account).await,
        (200, "UNKNOWN_ACCOUNT".into())
    );
    let ahead = json!({"request_type": "view_account", "block_id": head + 5,
                       "account_id": "alice.near"});
    assert_eq!(fails(&node, "query", ahead.clone()).await, (200, "UNKNOWN_BLOCK".into()));
    let no_height = format!("DB Not Found Error: BLOCK HEIGHT: {} \n Cause: Unknown", head + 5);
    assert_eq!(data(&node, "query", ahead).await, no_height);
    assert_eq!(data(&node, "block", json!({"block_id": head + 5})).await, no_height);
    let zero = CryptoHash::ZERO;
    let at_zero =
        json!({"request_type": "view_account", "block_id": zero, "account_id": "alice.near"});
    let no_header = format!("DB Not Found Error: BLOCK HEADER: {zero}");
    assert_eq!(data(&node, "query", at_zero).await, no_header);
    assert_eq!(data(&node, "gas_price", json!([zero])).await, no_header + " \n Cause: Unknown");
    let no_block = format!("DB Not Found Error: BLOCK: {zero} \n Cause: Unknown");
    assert_eq!(data(&node, "block", json!({"block_id": zero})).await, no_block);
    let call = json!({"request_type": "call_function", "finality": "final", "account_id": "probe.near",
                      "method_name": "nope", "args_base64": ""});
    let (status, r) = post(&node, "query", call).await;
    let error = r["result"]["error"].as_str().unwrap();
    assert!(status == 200 && error.contains("MethodNotFound"), "{r}");
    let pk = SecretKey::from_seed(KeyType::ED25519, "x").public_key();
    let key = json!({"request_type": "view_access_key", "finality": "final", "account_id": "alice.near",
                     "public_key": pk});
    let (status, r) = post(&node, "query", key).await;
    assert_eq!(
        (status, &r["result"]["error"]),
        (200, &json!(format!("access key {pk} does not exist while viewing")))
    );
}

/// A parked yield keeps a call from ever reaching its level. Each attempt
/// polls 10 s and answers 408 with the pending status, and near-kit gives up
/// after its retries: 43.5 s, as against a node. Once the yield fires, the
/// status is final.
#[tokio::test(start_paused = true)]
async fn a_parked_yield_times_out_the_wait() {
    let node = SimTransport::new(probe_sim());
    let alice = node.near("alice.near");
    let start = tokio::time::Instant::now();
    let e = rpc_error(alice.call("probe.near", "start_yield").await);
    assert!(matches!(e, RpcError::RequestTimeout { .. }), "{e:?}");
    assert_eq!(start.elapsed(), Duration::from_millis(43_500));
    let hash = node.with(|sim| *sim.txs().last().unwrap());
    let status = |wait_until: &str| json!({"tx_hash": hash.to_string(), "sender_account_id": "alice.near", "wait_until": wait_until});
    let (code, r) = post(&node, "EXPERIMENTAL_tx_status", status("FINAL")).await;
    let info = &r["error"]["cause"]["info"];
    assert_eq!((code, &info["cause"]), (408, &json!("PENDING")));
    assert_eq!(info["status"]["final_execution_status"], "INCLUDED_FINAL");
    assert!(info["status"]["receipts"].as_array().is_some_and(|r| !r.is_empty()), "{info}");
    let (code, r) = post(&node, "EXPERIMENTAL_tx_status", status("NONE")).await;
    assert_eq!((code, &r["result"]["final_execution_status"]), (200, &json!("INCLUDED_FINAL")));
    let expires_at = node.with(|sim| sim.yields()[0].expires_at);
    node.with(|sim| sim.advance_to(expires_at));
    let fired = alice.tx_status(&kit(hash), "alice.near").wait_until::<Final>().await.unwrap();
    assert!(fired.is_success());
    let last: String = alice.view("probe.near", "last").await.unwrap();
    assert_eq!(last, "timeout");
    let unknown = json!({"tx_hash": CryptoHash::ZERO.to_string(), "sender_account_id": "alice.near",
                         "wait_until": "FINAL"});
    let (code, r) = post(&node, "EXPERIMENTAL_tx_status", unknown.clone()).await;
    assert_eq!((code, &r["error"]["cause"]["info"]["cause"]), (408, &json!("NOT_OBSERVED")));
    let mut unknown = unknown;
    unknown["wait_until"] = json!("NONE");
    assert_eq!(
        fails(&node, "EXPERIMENTAL_tx_status", unknown).await,
        (200, "UNKNOWN_TRANSACTION".into())
    );
}

/// A call waiting on a yield and a task resuming it, on one runtime: the
/// wait's 10 s of polling let the responder, which looks every 5 s, act.
#[tokio::test(start_paused = true)]
async fn a_responder_resumes_the_yield_a_call_waits_on() {
    let node = SimTransport::new(probe_sim());
    let (alice, bob) = (node.near("alice.near"), node.near("bob.near"));
    let responder = async {
        loop {
            tokio::time::sleep(Duration::from_secs(5)).await;
            let last: String = bob.view("probe.near", "last").await.unwrap();
            if last.starts_with("yielded") {
                return bob.call("probe.near", "resume").await.unwrap();
            }
        }
    };
    let call = alice.call("probe.near", "start_yield").into_future();
    let (called, resumed) = tokio::join!(call, responder);
    assert!(called.unwrap().is_success() && resumed.is_success());
    let last: String = bob.view("probe.near", "last").await.unwrap();
    assert_eq!(last, "resumed:\"hello\"");
}

/// Sending a transaction again is sending the same one, as with nearcore's
/// `send_tx`: a converted one isn't queued again, and a refused one is,
/// since nearcore keeps nothing of it. One signed against a block this world
/// never had is refused as expired, as a node refuses a block hash it doesn't
/// know, or dropped at `NONE`.
#[tokio::test(start_paused = true)]
async fn a_transaction_sent_again_is_the_same_one() {
    let node = SimTransport::new(probe_sim());
    let alice = node.near("alice.near");
    let tx = alice.transfer("bob.near", NearToken::from_yoctonear(1)).sign().await.unwrap();
    let first = alice.rpc().send_tx(&tx, TxExecutionStatus::Final).await.unwrap();
    let n = node.with(|sim| sim.txs().len());
    let again = alice.rpc().send_tx(&tx, TxExecutionStatus::Final).await.unwrap();
    let block = |r: near_kit::rpc::RawTransactionResponse| {
        r.outcome.unwrap().transaction_outcome.block_hash
    };
    assert_eq!(block(first), block(again));
    assert_eq!(node.with(|sim| sim.txs().len()), n);

    node.with(|sim| sim.set_balance("bob.near", Balance::ZERO));
    let bob = node.near("bob.near");
    let tx = bob.transfer("alice.near", NearToken::from_near(1)).sign().await.unwrap();
    let e = bob.rpc().send_tx(&tx, TxExecutionStatus::Final).await.unwrap_err();
    assert!(matches!(e, RpcError::InvalidTx(InvalidTxError::NotEnoughBalance { .. })), "{e:?}");
    let by_hash = json!({"tx_hash": tx.get_hash().to_string(), "sender_account_id": "bob.near",
                         "wait_until": "NONE"});
    assert_eq!(
        fails(&node, "EXPERIMENTAL_tx_status", by_hash).await,
        (200, "UNKNOWN_TRANSACTION".into())
    );
    node.with(|sim| sim.set_balance("bob.near", near(10)));
    let sent = bob.rpc().send_tx(&tx, TxExecutionStatus::Final).await.unwrap();
    assert!(sent.outcome.unwrap().is_success());
    assert_eq!(node.with(|sim| sim.txs().len()), n + 1, "listed once");

    let foreign =
        alice.transfer("bob.near", NearToken::from_yoctonear(1)).sign_offline(CryptoHash::ZERO, 99);
    let foreign = foreign.await.unwrap();
    let e = alice.rpc().send_tx(&foreign, TxExecutionStatus::Final).await.unwrap_err();
    assert!(matches!(e, RpcError::InvalidTx(InvalidTxError::Expired)), "{e:?}");
    let dropped = alice.rpc().send_tx(&foreign, TxExecutionStatus::None).await.unwrap();
    assert_eq!(dropped.final_execution_status, TxExecutionStatus::None);
    assert!(node.with(|sim| sim.is_idle()), "nothing queued");
}

/// A signature is not part of the transaction hash. A corrected signature
/// must replace the refused body, while the transaction stays listed once.
#[tokio::test(start_paused = true)]
async fn a_corrected_signature_can_retry_the_same_transaction() {
    let node = SimTransport::new(probe_sim());
    let alice = node.near("alice.near");
    let tx = alice.transfer("bob.near", NearToken::from_yoctonear(1)).sign().await.unwrap();
    let mut bad = tx.clone();
    bad.signature = near_kit::signer::Signature::Ed25519([0; 64]);
    assert_eq!(bad.get_hash(), tx.get_hash());
    let before = node.with(|sim| {
        let pk = sim.secret_key("alice.near").public_key();
        (sim.account("alice.near"), sim.account("bob.near"), sim.access_key("alice.near", &pk))
    });
    let n = node.with(|sim| sim.txs().len());
    let e = alice.rpc().send_tx(&bad, TxExecutionStatus::Final).await.unwrap_err();
    assert!(matches!(e, RpcError::InvalidTx(InvalidTxError::InvalidSignature)), "{e:?}");
    node.with(|sim| {
        let pk = sim.secret_key("alice.near").public_key();
        assert_eq!(
            (sim.account("alice.near"), sim.account("bob.near"), sim.access_key("alice.near", &pk)),
            before
        );
    });
    for _ in 0..2 {
        let sent = alice.rpc().send_tx(&tx, TxExecutionStatus::Final).await.unwrap();
        assert!(sent.outcome.unwrap().is_success());
    }
    node.with(|sim| {
        assert_eq!(sim.txs().len(), n + 1);
        assert_eq!(
            sim.balance("bob.near"),
            before.1.unwrap().amount.checked_add(Balance::from_yoctonear(1)).unwrap()
        );
        sim.findings().assert_none();
    });
}

/// near-kit sends the same bytes again after `ShardCongested`, and they go
/// through once the congestion clears: here, a task lifts the chunk gas limit
/// while near-kit backs off.
#[tokio::test(start_paused = true)]
async fn a_transfer_refused_as_congested_goes_through_when_sent_again() {
    let node = SimTransport::new(probe_sim());
    node.with(|sim| {
        sim.config.block_gas_limit = Some(Gas::from_teragas(10));
        for _ in 0..400 {
            sim.call("bob.near", "probe.near", "height").gas(Gas::from_teragas(1_000)).submit();
        }
        sim.step();
        sim.step();
    });
    let lift = async {
        tokio::time::sleep(Duration::from_millis(100)).await;
        node.with(|sim| sim.config.block_gas_limit = None);
    };
    let transfer = node.near("alice.near").transfer("bob.near", NearToken::from_yoctonear(1));
    let start = tokio::time::Instant::now();
    let (sent, ()) = tokio::join!(transfer.into_future(), lift);
    assert!(sent.unwrap().is_success());
    // Refused twice, with near-kit backing off 0.5 and then 1 s: the third
    // attempt goes through.
    assert_eq!(start.elapsed(), Duration::from_millis(1_500));
    let refused = node.with(|sim| {
        let log = sim.txs().iter().map(|h| sim.outcome(h));
        log.filter(|o| o.failure().is_some_and(|f| f.to_string().contains("congest"))).count()
    });
    assert_eq!(refused, 0, "a resent transaction's outcome is its last attempt's");
}

/// near-kit reads the nonce on chain, so a transaction the sim queued with the
/// same key takes it first: the chunk refuses near-kit's with `InvalidNonce`,
/// and near-kit signs again with the next nonce.
#[tokio::test(start_paused = true)]
async fn near_kit_signs_again_past_an_invalid_nonce() {
    let node = SimTransport::new(probe_sim());
    let queued = node.with(|sim| sim.call("alice.near", "probe.near", "height").submit());
    let out = node.near("alice.near").call("probe.near", "height").await.unwrap();
    assert_eq!((out.transaction.nonce, out.is_success()), (2, true));
    assert!(node.with(|sim| sim.outcome(queued).is_success()));
}

/// A view at an older block reads the state that block left, and genesis is
/// the block before the first. A `Sim::cached` load keeps the latest state
/// alone, like a node that state-synced: older blocks are gone, and
/// `earliest_available` is the loaded one.
#[tokio::test(start_paused = true)]
async fn views_at_older_blocks() {
    let node = SimTransport::new(probe_sim());
    let alice = node.near("alice.near");
    let before = height(&node);
    alice.transfer("bob.near", NearToken::from_near(1)).await.unwrap();
    let then = alice.account("alice.near").at_block(before).await.unwrap();
    assert_eq!((then.amount, then.block_height), (NearToken::from_near(1_000), before));
    let hash = alice.rpc().block(BlockReference::at_height(before)).await.unwrap().header.hash;
    assert_eq!(alice.account("alice.near").at_block_hash(hash).await.unwrap().amount, then.amount);
    let seen: u64 = alice.view("probe.near", "height").at_block(before).await.unwrap();
    assert_eq!(seen, before);
    let rpc = alice.rpc();
    let genesis = rpc.view_account(&id("probe.near"), BlockReference::genesis()).await.unwrap();
    assert_eq!((genesis.block_height, genesis.amount), (0, NearToken::from_near(100)));
    let e = rpc.view_code(&id("probe.near"), BlockReference::genesis()).await.unwrap_err();
    assert!(matches!(e, RpcError::ContractNotDeployed { .. }), "no code before the deploy: {e:?}");
    let earliest = rpc.view_account(&id("probe.near"), BlockReference::earliest_available()).await;
    assert_eq!(earliest.unwrap().block_height, 0, "every state is kept");

    let load = || Sim::cached(probe_wasm(), probe_sim);
    load();
    let node = SimTransport::new(load());
    let alice = node.near("alice.near");
    assert!(alice.account("alice.near").at_block(height(&node)).await.is_ok());
    let e =
        alice.rpc().view_code(&id("probe.near"), BlockReference::at_height(1)).await.unwrap_err();
    assert!(matches!(e, RpcError::UnknownBlock(_)), "{e:?}");
    let gone = json!({"block_id": 1, "account_id": "alice.near"});
    assert_eq!(
        fails(&node, "EXPERIMENTAL_view_account", gone).await,
        (500, "INTERNAL_ERROR".into())
    );
    let gone = json!({"request_type": "view_account", "block_id": 1, "account_id": "alice.near"});
    assert_eq!(fails(&node, "query", gone).await, (200, "GARBAGE_COLLECTED_BLOCK".into()));
    let (rpc, loaded) = (alice.rpc(), height(&node));
    let earliest = rpc.view_account(&id("alice.near"), BlockReference::earliest_available()).await;
    assert_eq!(earliest.unwrap().block_height, loaded, "the loaded state is the earliest kept");
    assert_eq!(rpc.status().await.unwrap().sync_info.earliest_block_height, Some(loaded));
}

/// Heights are the log's: after `set_height`, `status` and `block` still show
/// the last block, while a view already runs at the new height. A skipped
/// height has no block, and `sandbox_fast_forward(n)` lands `n` blocks past
/// the next one.
#[tokio::test(start_paused = true)]
async fn heights_are_the_logs() {
    let node = SimTransport::new(probe_sim());
    let alice = node.near("alice.near");
    let head = height(&node);
    node.with(|sim| sim.set_height(head + 100));
    assert_eq!(alice.rpc().status().await.unwrap().sync_info.latest_block_height, head);
    let seen = alice.view::<u64>("probe.near", "height").with_metadata().await.unwrap();
    assert_eq!((seen.result, seen.block_height), (head + 100, head));
    node.with(|sim| sim.step());
    let skipped = fails(&node, "block", json!({"block_id": head + 50})).await;
    assert_eq!(skipped, (422, "UNKNOWN_BLOCK".into()));
    let ahead = fails(&node, "block", json!({"block_id": head + 500})).await;
    assert_eq!(ahead, (200, "UNKNOWN_BLOCK".into()));
    alice.rpc().sandbox_fast_forward(5).await.unwrap();
    assert_eq!(height(&node), head + 101 + 6);
}

/// Every node is a world of its own, with its own URL, so near-kit's nonce
/// cache doesn't carry from one to another: worlds from one fixture sign the
/// same first transaction.
#[tokio::test(start_paused = true)]
async fn worlds_from_one_fixture_sign_alike() {
    let fixture = probe_sim();
    let first = |sim: Sim| {
        let alice = SimTransport::new(sim).near("alice.near");
        async move { alice.transfer("bob.near", NearToken::from_yoctonear(1)).await.unwrap() }
    };
    let (a, b) = (first(fixture.clone()).await, first(fixture).await);
    assert_eq!(a.transaction.hash, b.transaction.hash);
}

/// A world from state records holds no keys, so a client of its own, on the
/// world's URL, signs there.
#[tokio::test(start_paused = true)]
async fn a_client_of_its_own_on_the_worlds_url() {
    use near_contract_sim::nearcore::near_primitives::account::{Account, AccountContract};
    let id: near_contract_sim::AccountId = "carol.near".parse().unwrap();
    let key = SecretKey::from_seed(KeyType::ED25519, "carol");
    let account = Account::new(near(10), Balance::ZERO, AccountContract::None, 0);
    let mut sim = Sim::new();
    sim.apply_state_records([
        StateRecord::Account { account_id: id.clone(), account },
        StateRecord::access_key(id, key.public_key(), AccessKey::full_access()),
    ]);
    let node = SimTransport::new(sim);
    let signer = InMemorySigner::new("carol.near", key.to_string()).unwrap();
    let carol = Near::custom(node.url(), "localnet").transport(node.clone()).signer(signer).build();
    assert!(carol.transfer("carol.near", NearToken::from_yoctonear(1)).await.unwrap().is_success());
}

/// `sandbox_patch_state` writes at once, as `apply_state_records` does.
#[tokio::test(start_paused = true)]
async fn patches_write_at_once() {
    let node = SimTransport::new(probe_sim());
    let rpc = node.near("alice.near").rpc().clone();
    let record =
        json!([{"Data": {"account_id": "probe.near", "data_key": "a2V5", "value": "dmFsdWU="}}]);
    rpc.sandbox_patch_state(record).await.unwrap();
    assert_eq!(
        node.with(|sim| sim.view_state("probe.near", b"key")),
        [(b"key".to_vec(), b"value".to_vec())]
    );
}

/// A world set up on the sim and patched into a sandbox, where the same
/// near-kit scenario burns the same gas. It needs a fresh sandbox of its own
/// (`nearprotocol/sandbox:2.14.0`) at `NCS_SANDBOX_RPC`, as CI's
/// differential job gives it.
#[tokio::test]
#[ignore = "needs a fresh sandbox: set NCS_SANDBOX_RPC"]
async fn a_sim_world_on_a_sandbox() {
    let url = std::env::var("NCS_SANDBOX_RPC").unwrap();
    let mut sim = Sim::new();
    for id in ["sandbox", "near"] {
        sim.create_account(id, near(1_000)); // the sandbox's own accounts, for the same trie
    }
    sim.create_account("alice.near", near(100));
    sim.deploy("status.near", status_wasm());
    let hi = json!({"message": "hi"});
    sim.call("alice.near", "status.near", "set_status").args_json(hi).transact().assert_success();

    let records = sim.state_records(["alice.near", "status.near"]);
    let key = sim.secret_key("alice.near").to_string();
    let alice = InMemorySigner::new("alice.near", key).unwrap();
    let sandbox = Near::custom(url, "sandbox").signer(alice).build();
    sandbox.rpc().sandbox_patch_state(json!(records)).await.unwrap();
    let node = SimTransport::new(sim);
    assert_eq!(set_status(&node.near("alice.near")).await, set_status(&sandbox).await);
}

/// alice.near sets her status and reads it back: the gas of the transaction
/// and of each receipt, and the status.
async fn set_status(near: &Near) -> (Vec<near_kit::Gas>, String) {
    let set = near.call("status.near", "set_status").args(json!({"message": "hello"}));
    let out = set.send().wait_until::<Final>().await.unwrap();
    assert!(out.is_success(), "{out:?}");
    let outcomes = std::iter::once(&out.transaction_outcome).chain(&out.receipts_outcome);
    let gas = outcomes.map(|o| o.outcome.gas_burnt).collect();
    let args = json!({"account_id": "alice.near"});
    (gas, near.view("status.near", "get_status").args(args).await.unwrap())
}
