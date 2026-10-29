//! [`SimTransport`]: near-kit's `RpcTransport` answered in-process from a
//! `Sim`'s block log and state, so production near-kit code runs unchanged in
//! tests, unless it calls near-kit's `validators`, `block_effects`,
//! `genesis_config` or `maintenance_windows`.
//!
//! Requests parse into nearcore's own RPC types, and answers are nearcore's
//! views and per-method errors. The glue nearcore's server crate adds around
//! them is copied from `chain/jsonrpc/src/lib.rs` at the pinned tag, and says
//! so where it is.

use crate::chain::{Block, Log, VALIDATOR, ViewAt};
use crate::sim::{GENESIS_TIMESTAMP_NS, IntoAccountId, Sim};
use near_crypto::{KeyType, PublicKey};
use near_jsonrpc_primitives::errors::{RpcError, RpcErrorKind};
use near_jsonrpc_primitives::message::{Message, Request};
use near_jsonrpc_primitives::types::blocks::{RpcBlockError, RpcBlockRequest};
use near_jsonrpc_primitives::types::call_function::{RpcCallFunctionError, RpcCallFunctionRequest};
use near_jsonrpc_primitives::types::gas_price::{RpcGasPriceError, RpcGasPriceRequest};
use near_jsonrpc_primitives::types::query::{
    QueryResponseKind, RpcQueryError, RpcQueryRequest, RpcQueryResponse,
};
use near_jsonrpc_primitives::types::receipts::{
    RpcReceiptToTxError, RpcReceiptToTxRequest, RpcReceiptToTxResponse,
};
use near_jsonrpc_primitives::types::sandbox::{
    RpcSandboxFastForwardRequest, RpcSandboxFastForwardResponse, RpcSandboxPatchStateRequest,
    RpcSandboxPatchStateResponse,
};
use near_jsonrpc_primitives::types::transactions::{
    RpcSendTransactionRequest, RpcTransactionError, RpcTransactionResponse,
    RpcTransactionStatusRequest, TimeoutErrorCause,
};
use near_jsonrpc_primitives::types::view_access_key::{
    RpcViewAccessKeyError, RpcViewAccessKeyRequest,
};
use near_jsonrpc_primitives::types::view_account::{RpcViewAccountError, RpcViewAccountRequest};
use near_kit::Near;
use near_kit::rpc::{BoxFuture, RpcTransport, TransportResponse};
use near_kit::signer::InMemorySigner;
use near_parameters::RuntimeConfigStore;
use near_primitives::action::GlobalContractIdentifier;
use near_primitives::errors::{InvalidTxError, TxExecutionError};
use near_primitives::hash::CryptoHash;
use near_primitives::sharding::ShardChunkHeader;
use near_primitives::transaction::ExecutionStatus;
use near_primitives::types::{
    Balance, BlockId, BlockReference, Gas, ShardId, StateRoot, SyncCheckpoint,
};
use near_primitives::version::{PROTOCOL_VERSION, Version};
use near_primitives::views::{
    AccessKeyInfoView, AccessKeyList, BlockHeaderView, BlockView, CallResult, ChunkHeaderView,
    ContractCodeView, ExecutionOutcomeWithIdView, FinalExecutionOutcomeView,
    FinalExecutionOutcomeViewEnum, FinalExecutionOutcomeWithReceiptView, GasKeyNoncesView,
    GasPriceView, QueryRequest, ReceiptView, SignedTransactionView, StatusResponse, StatusSyncInfo,
    TxExecutionStatus,
};
use near_store::TrieUpdate;
use near_vm_runner::ContractCode;
use node_runtime::state_viewer::TrieViewer;
use node_runtime::state_viewer::errors::{
    CallFunctionError, ViewAccessKeyError, ViewAccountError, ViewContractCodeError,
    ViewGasKeyNoncesError, ViewStateError,
};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;

/// How long nearcore's RPC keeps waiting on a transaction before it answers
/// `TIMEOUT_ERROR` (`RpcPollingConfig::polling_timeout`).
const POLLING_TIMEOUT: Duration = Duration::from_secs(10);

/// A `TrieViewer` with a node's state size limit (`trie_viewer_state_size_limit`,
/// 50,000 bytes), which `TrieViewer::default()` lacks.
static VIEWER: LazyLock<TrieViewer> =
    LazyLock::new(|| TrieViewer::new(RuntimeConfigStore::new(None), Some(50_000), 100, None));

/// A node for near-kit, in process: a `Clone` handle to one [`Sim`] that
/// answers with nearcore's own views and errors.
///
/// Blocks are produced only while a request waits for a transaction (every
/// wait level but `NONE`) and on `sandbox_fast_forward`: a `send_tx` or
/// `EXPERIMENTAL_tx_status` produces blocks until the transaction is included
/// (for `INCLUDED` and `INCLUDED_FINAL`) or final, with its refunds (for every
/// executed level), until `send_tx`'s transaction is refused, or until the
/// chain is idle. A wait that finds the chain idle (behind a parked yield,
/// for one) sleeps a node's 10 s polling timeout with the handle unlocked, waits
/// once more, and then answers `TIMEOUT_ERROR` (HTTP 408), as nearcore does.
/// So other tasks can act while it sleeps, but a polling loop never sees
/// time pass on its own: wait on a transaction or fast-forward.
///
/// A request runs to its answer under the handle's lock, so requests are
/// atomic, and the only `.await` is that sleep. On a current-thread runtime
/// (`#[tokio::test]`), the same code answers the same way every run.
///
/// A view at an older block reads the state that block left, and genesis
/// is the block before the first. Blocks from before a [`Sim::cached`] load
/// are gone, as on a node that state-synced: `query` answers
/// `GARBAGE_COLLECTED_BLOCK`, the `EXPERIMENTAL_` views an `INTERNAL_ERROR`
/// that near-kit retries, and `earliest_available` is the block it loaded
/// at. The [guide](https://github.com/near/near-contract-sim/blob/main/skills/near-contract-sim/references/near-kit.md)
/// has what else it answers differently from a node.
#[derive(Clone, Debug)]
pub struct SimTransport {
    sim: Arc<Mutex<Sim>>,
    /// near-kit caches nonces per `(url, account, key)` for the whole
    /// process, so every chain gets its own.
    url: String,
}

impl SimTransport {
    /// A node running `sim`, from its current state, clock and log. Branch
    /// by making another handle: `SimTransport::new(node.with(|sim| sim.clone()))`.
    /// Assigning a snapshot into a live handle instead leaves near-kit's
    /// cached nonces above the restored chain's.
    pub fn new(sim: Sim) -> Self {
        static WORLDS: AtomicU64 = AtomicU64::new(0);
        let url = format!("sim://{}", WORLDS.fetch_add(1, Ordering::Relaxed));
        SimTransport { sim: Arc::new(Mutex::new(sim)), url }
    }

    /// This node's URL, for a near-kit client that `near` doesn't make, such
    /// as one for a chain built from state records, which holds no keys:
    /// `Near::custom(node.url(), chain_id).transport(node.clone())`.
    pub fn url(&self) -> &str {
        &self.url
    }

    /// Runs `f` on the `Sim`, for its sim-only controls and reads: the clock,
    /// state writes, `check()`, or `sim.clone()` for a snapshot. The lock
    /// isn't reentrant, so `with` or `near` inside `with` never returns. A
    /// panic inside (a failed assertion, a block that panicked) makes every
    /// later call fail, rather than run on a half-applied `Sim`.
    ///
    /// Views read the clock as soon as it moves, while `status` and `block`
    /// show the last block until the next one. So code that reads the chain's
    /// height or time over RPC should see a block first:
    /// `node.with(|sim| { sim.set_height(h); sim.step() })`.
    pub fn with<R>(&self, f: impl FnOnce(&mut Sim) -> R) -> R {
        f(&mut self.sim.lock().expect("a panic may have left the simulator half-applied"))
    }

    /// A near-kit client of this node that signs as `signer` with the key
    /// [`Sim::secret_key`] gives, and near-kit's defaults otherwise, retries
    /// included. For an account the `Sim` holds no key for, sign with
    /// `near.with_signer(..)`, which keeps the node, or make a client on
    /// [`url`](Self::url).
    ///
    /// near-kit backs off 0.5, 1 and 2 s before it retries a timed-out wait,
    /// an internal error or `ShardCongested`, a timed-out wait itself sleeps
    /// 10 s, and `sandbox_patch_state` sleeps 100 ms. That is wall time,
    /// unless the test runs on tokio's paused clock, where it's free:
    /// `#[tokio::test(start_paused = true)]` (tokio's `test-util` feature).
    #[track_caller]
    pub fn near(&self, signer: impl IntoAccountId) -> Near {
        let id = signer.into_account_id();
        let (key, chain_id) =
            self.with(|sim| (sim.keys.get(&id).cloned(), sim.config.chain_id.clone()));
        let Some(key) = key else {
            panic!(
                "no key for {id}: the Sim holds no key for it (an account a transaction made, or \
                 one that doesn't exist on chain); use Sim::create_account, Sim::set_key, \
                 with_signer or url"
            )
        };
        let signer = match InMemorySigner::new(id.as_str(), key.to_string()) {
            Ok(signer) => signer,
            Err(e) => panic!("near-kit can't read {id}'s key: {e}"),
        };
        Near::custom(self.url(), chain_id).transport(self.clone()).signer(signer).build()
    }
}

impl RpcTransport for SimTransport {
    fn post_json(
        &self,
        _url: &str,
        body: Vec<u8>,
    ) -> BoxFuture<'_, Result<TransportResponse, near_kit::rpc::RpcError>> {
        Box::pin(async move {
            let mut response = self.with(|sim| respond(sim, &body));
            // The wait found the chain idle. A node keeps polling for
            // `polling_timeout` while other clients act, then answers.
            if response.status == 408 {
                tokio::time::sleep(POLLING_TIMEOUT).await;
                response = self.with(|sim| respond(sim, &body));
            }
            Ok(response)
        })
    }
}

/// nearcore's response to `body`, with the HTTP status its server gives it
/// (`rpc_handler` and `handle_unknown_block`, `chain/jsonrpc/src/lib.rs`).
fn respond(sim: &mut Sim, body: &[u8]) -> TransportResponse {
    let (id, result, block_id) = match serde_json::from_slice::<Request>(body) {
        Ok(r) => {
            let block_id = r.params.get("block_id").and_then(Value::as_u64);
            (r.id, answer(sim, &r.method, r.params), block_id)
        }
        Err(e) => (Value::Null, Err(RpcError::parse_error(e.to_string())), None),
    };
    let head = sim.chain.log().blocks.last().map_or(0, |b| b.height);
    let status = match result.as_ref().map_err(|e| &e.error_struct) {
        Ok(_) | Err(None) => 200,
        Err(Some(RpcErrorKind::RequestValidationError(_))) => 400,
        Err(Some(RpcErrorKind::InternalError(_))) => 500,
        Err(Some(RpcErrorKind::HandlerError(e))) => match e.get("name").and_then(Value::as_str) {
            Some("TIMEOUT_ERROR") => 408,
            Some("UNKNOWN_BLOCK") if block_id.is_some_and(|h| (..head).contains(&h)) => 422,
            _ => 200,
        },
    };
    let body = serde_json::to_vec(&Message::response(id, result)).expect("a JSON-RPC response");
    TransportResponse { status, body }
}

fn answer(sim: &mut Sim, method: &str, params: Value) -> Result<Value, RpcError> {
    match method {
        "block" => {
            let r: RpcBlockRequest = parse(params)?;
            let k = resolve(sim, &r.block_reference).map_err(|id| RpcBlockError::UnknownBlock {
                error_message: not_found(id, "BLOCK"),
            })?;
            Ok(to_json(block_view(sim, k)))
        }
        "EXPERIMENTAL_call_function" => {
            let r: RpcCallFunctionRequest = parse(params)?;
            let q = QueryRequest::CallFunction {
                account_id: r.account_id,
                method_name: r.method_name,
                args: r.args,
            };
            reply(view(sim, &r.block_reference, q).map_err(RpcCallFunctionError::from))
        }
        "EXPERIMENTAL_receipt_to_tx" => {
            let r: RpcReceiptToTxRequest = parse(params)?;
            let (id, log) = (r.receipt_reference.receipt_id, sim.chain.log());
            let tx =
                log.origin.get(&id).ok_or(RpcReceiptToTxError::UnknownReceipt { receipt_id: id });
            reply(tx.map(|tx| RpcReceiptToTxResponse {
                transaction_hash: *tx,
                sender_account_id: log.txs[tx].transaction.signer_id().clone(),
            }))
        }
        "EXPERIMENTAL_tx_status" => {
            let r: RpcTransactionStatusRequest = parse(params)?;
            let hash = r.transaction_info.to_tx_hash_and_account().0;
            reply(fetch(sim, hash, r.wait_until, false))
        }
        "EXPERIMENTAL_view_access_key" => {
            let r: RpcViewAccessKeyRequest = parse(params)?;
            let q =
                QueryRequest::ViewAccessKey { account_id: r.account_id, public_key: r.public_key };
            reply(view(sim, &r.block_reference, q).map_err(RpcViewAccessKeyError::from))
        }
        "EXPERIMENTAL_view_account" => {
            let r: RpcViewAccountRequest = parse(params)?;
            let q = QueryRequest::ViewAccount { account_id: r.account_id };
            reply(view(sim, &r.block_reference, q).map_err(RpcViewAccountError::from))
        }
        "gas_price" => {
            let r: RpcGasPriceRequest = parse(params)?;
            if let Some(id) = &r.block_id {
                by_id(sim, id).ok_or_else(|| RpcGasPriceError::UnknownBlock {
                    error_message: not_found(id, "BLOCK HEADER"),
                })?;
            }
            Ok(to_json(GasPriceView { gas_price: sim.config.gas_price }))
        }
        "query" => {
            let r: RpcQueryRequest = parse(params)?;
            // The reachable half of the server's `validate_view_state_pagination`
            // (`chain/jsonrpc/src/api/mod.rs`): near-kit never asks for a proof.
            if let QueryRequest::ViewState { prefix, after_key: Some(after), .. } = &r.request
                && !after.starts_with(prefix)
            {
                return Err(RpcError::parse_error(
                    "after_key_base64 must start with prefix_base64".into(),
                ));
            }
            query(view(sim, &r.block_reference, r.request))
        }
        "sandbox_fast_forward" => {
            let r: RpcSandboxFastForwardRequest = parse(params)?;
            // A sandbox lands `delta_height` blocks past the next one and runs
            // what is in flight there, where `advance_to` runs it first.
            sim.advance_to(sim.height() + r.delta_height + 1);
            Ok(to_json(RpcSandboxFastForwardResponse {}))
        }
        "sandbox_patch_state" => {
            let r: RpcSandboxPatchStateRequest = parse(params)?;
            sim.apply_state_records(r.records);
            Ok(to_json(RpcSandboxPatchStateResponse {}))
        }
        "send_tx" => reply(send_tx(sim, parse(params)?)),
        "status" => Ok(to_json(status(sim))),
        _ => Err(RpcError::method_not_found(method.into())),
    }
}

/// The server's parse of `params` (`Params::parse`).
fn parse<T: DeserializeOwned>(params: Value) -> Result<T, RpcError> {
    serde_json::from_value(params)
        .map_err(|e| RpcError::parse_error(format!("Failed parsing args: {e}")))
}

fn reply<T: Serialize, E>(result: Result<T, E>) -> Result<Value, RpcError>
where
    RpcError: From<E>,
{
    Ok(to_json(result?))
}

fn to_json(view: impl Serialize) -> Value {
    serde_json::to_value(view).expect("an RPC view")
}

/// `query`'s answer, through the server's `process_query_response`: a
/// contract's error and a missing access key are results, not errors, and a
/// block id that names no block gets the store's message.
fn query(response: Result<RpcQueryResponse, RpcQueryError>) -> Result<Value, RpcError> {
    let in_result = |error: String, block_height: u64, block_hash: CryptoHash| {
        Ok(
            json!({"error": error, "logs": [], "block_height": block_height, "block_hash": block_hash}),
        )
    };
    match response {
        Err(RpcQueryError::ContractExecutionError {
            vm_error, block_height, block_hash, ..
        }) => in_result(vm_error, block_height, block_hash),
        Err(RpcQueryError::UnknownAccessKey { public_key, block_height, block_hash }) => {
            let error = format!("access key {public_key} does not exist while viewing");
            in_result(error, block_height, block_hash)
        }
        Err(RpcQueryError::UnknownBlock { block_reference: BlockReference::BlockId(id) }) => {
            let data = match &id {
                BlockId::Height(h) => {
                    format!("DB Not Found Error: BLOCK HEIGHT: {h} \n Cause: Unknown")
                }
                BlockId::Hash(h) => format!("DB Not Found Error: BLOCK HEADER: {h}"),
            };
            let e = RpcQueryError::UnknownBlock { block_reference: BlockReference::BlockId(id) };
            Err(RpcError::new_internal_or_handler_error(Some(data.into()), to_json(e)))
        }
        response => reply(response),
    }
}

fn send_tx(
    sim: &mut Sim,
    r: RpcSendTransactionRequest,
) -> Result<RpcTransactionResponse, RpcTransactionError> {
    let tx = r.signed_transaction;
    let hash = tx.get_hash();
    // nearcore's `check_transaction_validity_period` refuses a block hash it
    // doesn't know as expired.
    let ours = by_id(sim, &BlockId::Hash(*tx.transaction.block_hash())).is_some();
    // A pinned height or time is the first receipt's, as for a `Tx`.
    let local = tx.transaction.signer_id() == tx.transaction.receiver_id();
    if r.wait_until == TxExecutionStatus::None {
        // `send_tx_async`: queued, with nothing said about it.
        if ours {
            sim.submit_pinned(local, |chain| chain.submit(tx, None, None, vec![]));
        }
        return Ok(RpcTransactionResponse {
            final_execution_outcome: None,
            final_execution_status: TxExecutionStatus::None,
        });
    }
    if !ours {
        return Err(RpcTransactionError::InvalidTransaction { context: InvalidTxError::Expired });
    }
    sim.submit_pinned(local, |chain| chain.submit(tx, None, None, vec![]));
    fetch(sim, hash, r.wait_until, true)
}

/// The server's `tx_status_fetch`, with blocks produced while it waits: until
/// `hash` converts, for `INCLUDED` and `INCLUDED_FINAL`, or is final, for every
/// executed level (which nearcore's `tx_execution_status_meets_expectations`
/// accepts), or until the chain is idle, which is a timeout. `NONE` doesn't
/// wait. `sent` makes it `send_tx`'s wait, which also ends when its
/// transaction is refused, and whose answer has no receipt bodies.
fn fetch(
    sim: &mut Sim,
    hash: CryptoHash,
    wanted: TxExecutionStatus,
    sent: bool,
) -> Result<RpcTransactionResponse, RpcTransactionError> {
    use TxExecutionStatus::{Final, Included, IncludedFinal};
    let reached = |seen: &Option<Result<TxExecutionStatus, InvalidTxError>>| match seen {
        _ if wanted == TxExecutionStatus::None => true,
        Some(Ok(level)) => matches!(wanted, Included | IncludedFinal) || *level == Final,
        Some(Err(_)) => sent,
        None => false,
    };
    sim.settle_until(|sim| reached(&seen(sim.chain.log(), hash)));
    let now = seen(sim.chain.log(), hash);
    let done = reached(&now);
    let timeout = |cause| RpcTransactionError::TimeoutError(Some(cause));
    match now {
        Some(Ok(level)) if done => Ok(status_view(sim, hash, level, !sent)),
        Some(Err(context)) if sent => Err(RpcTransactionError::InvalidTransaction { context }),
        _ if wanted == TxExecutionStatus::None => {
            Err(RpcTransactionError::UnknownTransaction { requested_transaction_hash: hash })
        }
        Some(Ok(level)) => Err(timeout(TimeoutErrorCause::Pending {
            status: Box::new(status_view(sim, hash, level, !sent)),
        })),
        _ => Err(timeout(TimeoutErrorCause::NotObserved)),
    }
}

/// Transaction `hash` as RPC sees it: `None` until it converts (and again once
/// a refused one is sent again), its refusal, or its level.
fn seen(log: &Log, hash: CryptoHash) -> Option<Result<TxExecutionStatus, InvalidTxError>> {
    match log.status(&hash)? {
        ExecutionStatus::Failure(TxExecutionError::InvalidTxError(e)) => Some(Err(e.clone())),
        _ => Some(Ok(level(log, &log.outcomes(hash)))),
    }
}

/// The view client's `get_tx_execution_status`, every block being final:
/// `INCLUDED_FINAL` while a receipt waits to run, `EXECUTED` while only
/// refunds do (it skips the last receipt of an outcome when that is a
/// refund), then `FINAL`.
fn level(log: &Log, outcomes: &[(usize, usize)]) -> TxExecutionStatus {
    let mut level = TxExecutionStatus::Final;
    for &at in outcomes {
        let ids = &log.at(at).1.outcome.receipt_ids;
        for (i, id) in ids.iter().enumerate().filter(|(_, id)| log.position(id).is_none()) {
            let body = log.receipts.get(id);
            if i + 1 < ids.len() || !body.is_some_and(|r| r.predecessor_id().is_system()) {
                return TxExecutionStatus::IncludedFinal;
            }
            level = TxExecutionStatus::Executed;
        }
    }
    level
}

/// What `send_tx` answers (`with_receipts` false) or `EXPERIMENTAL_tx_status`
/// (true: with the receipts' bodies, all but the local one, which nearcore
/// doesn't store either).
fn status_view(
    sim: &Sim,
    hash: CryptoHash,
    level: TxExecutionStatus,
    with_receipts: bool,
) -> RpcTransactionResponse {
    let log = sim.chain.log();
    let mut views = log.outcomes(hash).into_iter().map(|at| {
        let (block, o) = log.at(at);
        ExecutionOutcomeWithIdView {
            proof: vec![],
            block_hash: block.hash,
            id: o.id,
            outcome: o.outcome.clone().into(),
        }
    });
    let outcome = FinalExecutionOutcomeView {
        status: sim.tx_status(hash),
        transaction: SignedTransactionView::from((*log.txs[&hash]).clone()),
        transaction_outcome: views.next().expect("a converted transaction"),
        receipts_outcome: views.collect(),
    };
    let outcome = if with_receipts {
        let bodies = outcome.receipts_outcome.iter().filter_map(|o| log.receipts.get(&o.id));
        let receipts = bodies.map(|r| ReceiptView::from((**r).clone())).collect();
        let outcome = FinalExecutionOutcomeWithReceiptView { final_outcome: outcome, receipts };
        FinalExecutionOutcomeViewEnum::FinalExecutionOutcomeWithReceipt(outcome)
    } else {
        FinalExecutionOutcomeViewEnum::FinalExecutionOutcome(outcome)
    };
    RpcTransactionResponse { final_execution_outcome: Some(outcome), final_execution_status: level }
}

/// Block `k` of this chain: genesis for 0, then the log's. Genesis is height
/// 0 at the genesis time, and what the first block applied on (its hash,
/// state and version), or before that block what it would apply on.
fn block(sim: &Sim, k: usize) -> Arc<Block> {
    let first = sim.chain.log().blocks.first();
    match k.checked_sub(1) {
        Some(i) => sim.chain.log().blocks[i].clone(),
        None => Arc::new(Block {
            height: 0,
            timestamp_ns: GENESIS_TIMESTAMP_NS,
            hash: first.map_or_else(|| sim.chain.prev_block_hash(&sim.config), |b| b.prev_hash),
            prev_hash: CryptoHash::default(),
            prev_state_root: first.map_or_else(|| sim.state_root(), |b| b.prev_state_root),
            protocol_version: first.map_or(sim.config.protocol_version, |b| b.protocol_version),
            pricing: first.map_or(sim.config.gas_pricing, |b| b.pricing),
            outcomes: vec![],
        }),
    }
}

/// Which block of this chain `reference` names, as `block` for `k`, or the
/// block id that names none: every finality names the latest, `genesis`
/// genesis, and `earliest_available` the earliest block whose state is kept.
fn resolve<'a>(sim: &Sim, reference: &'a BlockReference) -> Result<usize, &'a BlockId> {
    match reference {
        BlockReference::Finality(_) => Ok(sim.chain.log().blocks.len()),
        BlockReference::SyncCheckpoint(SyncCheckpoint::Genesis) => Ok(0),
        BlockReference::SyncCheckpoint(SyncCheckpoint::EarliestAvailable) => {
            Ok(earliest_available(sim))
        }
        BlockReference::BlockId(id) => by_id(sim, id).ok_or(id),
    }
}

/// The block `id` names: a height the newest block there (after a rewind),
/// and a hash its block.
fn by_id(sim: &Sim, id: &BlockId) -> Option<usize> {
    let blocks = &sim.chain.log().blocks;
    let find = |f: &dyn Fn(&Arc<Block>) -> bool| blocks.iter().rposition(f).map(|i| i + 1);
    match id {
        BlockId::Height(0) => Some(0),
        BlockId::Height(h) => find(&|b| b.height == *h),
        BlockId::Hash(h) if *h == block(sim, 0).hash => Some(0),
        BlockId::Hash(h) => find(&|b| b.hash == *h),
    }
}

/// The store's error for a block it doesn't have (`DBNotFoundErr`), which
/// names a hash by what it looked up: `BLOCK` or `BLOCK HEADER`.
fn not_found(id: &BlockId, what: &str) -> String {
    match id {
        BlockId::Height(h) => format!("BLOCK HEIGHT: {h}"),
        BlockId::Hash(h) => format!("{what}: {h}"),
    }
}

/// The earliest block whose state is kept, as nearcore's
/// `get_earliest_block_hash` finds it: the one after the latest block whose
/// state a `Sim::cached` load dropped, or genesis.
fn earliest_available(sim: &Sim) -> usize {
    let blocks = 0..sim.chain.log().blocks.len();
    blocks.rev().find(|&k| !kept(sim, view_at(sim, k).root)).map_or(0, |k| k + 1)
}

/// Where a view at block `k` runs: the latest block on the current state and
/// clock, an older one in its own environment on the state it left (the next
/// block's `prev_state_root`, with the patches applied while it was the
/// latest).
fn view_at(sim: &Sim, k: usize) -> ViewAt {
    let b = block(sim, k);
    match sim.chain.log().blocks.get(k) {
        None => sim.chain.latest(&sim.config),
        Some(next) => ViewAt {
            height: b.height,
            timestamp_ns: b.timestamp_ns,
            protocol_version: b.protocol_version,
            root: next.prev_state_root,
        },
    }
}

/// Whether the state at `root` is still there: a `Sim::cached` load keeps the
/// latest state alone, like a node that state-synced.
fn kept(sim: &Sim, root: StateRoot) -> bool {
    sim.chain.trie_at(root).retrieve_root_node().is_ok()
}

/// Block `k` as `block` answers it. Every block is final, and it has one
/// chunk: nearcore's dummy header with the block's height and gas limit and
/// the state root it applied on.
fn block_view(sim: &Sim, k: usize) -> BlockView {
    let b = block(sim, k);
    let mut chunk: ChunkHeaderView =
        ShardChunkHeader::new_dummy(b.height, ShardId::new(0), b.prev_hash, b.protocol_version)
            .into();
    chunk.height_included = b.height;
    chunk.gas_limit = sim.config.block_gas_limit.unwrap_or(Gas::MAX);
    chunk.prev_state_root = b.prev_state_root;
    let header = BlockHeaderView {
        height: b.height,
        hash: b.hash,
        prev_hash: b.prev_hash,
        timestamp: b.timestamp_ns,
        timestamp_nanosec: b.timestamp_ns,
        // The merkle root of the chunks' `prev_state_root`: one chunk's hash.
        prev_state_root: CryptoHash::hash_borsh(b.prev_state_root),
        chunks_included: 1,
        chunk_mask: vec![true],
        gas_price: sim.config.gas_price,
        total_supply: Balance::from_yoctonear(
            sim.chain.log().supply().try_into().expect("a non-negative supply"),
        ),
        last_final_block: b.hash,
        last_ds_final_block: b.hash,
        latest_protocol_version: b.protocol_version,
        ..Default::default()
    };
    BlockView { author: VALIDATOR.parse().expect("an account id"), header, chunks: vec![chunk] }
}

/// `status`: the chain's config, its latest block, whose header's state root
/// nearcore reports, and the earliest one whose state is kept.
fn status(sim: &Sim) -> StatusResponse {
    let head = block(sim, sim.chain.log().blocks.len());
    let earliest = block(sim, earliest_available(sim));
    let time = |b: &Block| {
        near_time::Utc::from_unix_timestamp_nanos(b.timestamp_ns.into())
            .expect("a u64 of nanoseconds is a date")
    };
    StatusResponse {
        version: Version { build: "near-contract-sim".into(), ..Default::default() },
        chain_id: sim.config.chain_id.clone(),
        protocol_version: head.protocol_version,
        latest_protocol_version: PROTOCOL_VERSION,
        rpc_addr: None,
        validators: vec![],
        sync_info: StatusSyncInfo {
            latest_block_hash: head.hash,
            latest_block_height: head.height,
            latest_state_root: CryptoHash::hash_borsh(head.prev_state_root),
            latest_block_time: time(&head),
            syncing: false,
            earliest_block_hash: Some(earliest.hash),
            earliest_block_height: Some(earliest.height),
            earliest_block_time: Some(time(&earliest)),
            epoch_id: None,
            epoch_start_height: None,
        },
        validator_account_id: None,
        validator_public_key: None,
        node_public_key: PublicKey::empty(KeyType::ED25519),
        node_key: None,
        uptime_sec: 0,
        genesis_hash: block(sim, 0).hash,
        detailed_debug_status: None,
    }
}

/// `request` at `reference`, as nearcore's view client runs it (`handle_query`,
/// then `NightshadeRuntime::query`), where `view_at` puts it. Viewer errors
/// map as `QueryError::from_*` maps them, and the ones the in-memory trie
/// can't produce fold into one `InternalError`, as nearcore's `unexpected =>`
/// arms do.
fn view(
    sim: &Sim,
    reference: &BlockReference,
    request: QueryRequest,
) -> Result<RpcQueryResponse, RpcQueryError> {
    let k = resolve(sim, reference)
        .map_err(|_| RpcQueryError::UnknownBlock { block_reference: reference.clone() })?;
    let b = block(sim, k);
    let (block_height, block_hash) = (b.height, b.hash);
    let at = view_at(sim, k);
    if !kept(sim, at.root) {
        return Err(RpcQueryError::GarbageCollectedBlock { block_height, block_hash });
    }
    let trie = || sim.chain.trie_at(at.root);
    let state = TrieUpdate::new(trie());
    let unknown = |requested_account_id| RpcQueryError::UnknownAccount {
        requested_account_id,
        block_height,
        block_hash,
    };
    let other =
        |e: &dyn fmt::Debug| RpcQueryError::InternalError { error_message: format!("{e:?}") };
    let code = |c: ContractCode| {
        QueryResponseKind::ViewCode(ContractCodeView { hash: *c.hash(), code: c.into_code() })
    };
    let global = |identifier| {
        VIEWER.view_global_contract_code(&state, identifier).map(code).map_err(|e| match e {
            ViewContractCodeError::NoGlobalContractCode { identifier } => {
                RpcQueryError::NoGlobalContractCode { identifier, block_height, block_hash }
            }
            e => other(&e),
        })
    };
    let kind = match request {
        QueryRequest::ViewAccount { account_id } => {
            let account = VIEWER.view_account(&state, &account_id).map_err(|e| match e {
                ViewAccountError::AccountDoesNotExist { requested_account_id } => {
                    unknown(requested_account_id)
                }
                e => other(&e),
            })?;
            QueryResponseKind::ViewAccount(account.into())
        }
        QueryRequest::ViewCode { account_id } => code(
            VIEWER
                .view_account_contract_code(
                    &state,
                    &account_id,
                    at.protocol_version,
                    &sim.config.chain_id,
                )
                .map_err(|e| match e {
                    ViewContractCodeError::AccountDoesNotExist { requested_account_id } => {
                        unknown(requested_account_id)
                    }
                    ViewContractCodeError::NoContractCode { contract_account_id } => {
                        RpcQueryError::NoContractCode {
                            contract_account_id,
                            block_height,
                            block_hash,
                        }
                    }
                    e => other(&e),
                })?,
        ),
        QueryRequest::ViewState { account_id, prefix, after_key, limit, include_proof } => {
            let after_key = after_key.as_deref().map(Vec::as_slice);
            let values = VIEWER
                .view_state(&state, &account_id, &prefix, after_key, limit, include_proof)
                .map_err(|e| match e {
                    ViewStateError::AccountDoesNotExist { requested_account_id } => {
                        unknown(requested_account_id)
                    }
                    ViewStateError::AccountStateTooLarge { requested_account_id } => {
                        RpcQueryError::TooLargeContractState {
                            contract_account_id: requested_account_id,
                            block_height,
                            block_hash,
                        }
                    }
                    e => other(&e),
                })?;
            QueryResponseKind::ViewState(values)
        }
        QueryRequest::ViewAccessKey { account_id, public_key } => {
            let key =
                VIEWER.view_access_key(&state, &account_id, &public_key).map_err(|e| match e {
                    ViewAccessKeyError::AccessKeyDoesNotExist { public_key } => {
                        RpcQueryError::UnknownAccessKey { public_key, block_height, block_hash }
                    }
                    e => other(&e),
                })?;
            QueryResponseKind::AccessKey(key.into())
        }
        QueryRequest::ViewAccessKeyList { account_id, after_key, limit } => {
            let (keys, last_key) = VIEWER
                .view_access_keys(&trie(), &account_id, after_key.as_ref(), limit)
                .map_err(|e| match e {
                    ViewAccessKeyError::TooManyAccessKeys { requested_account_id, limit } => {
                        RpcQueryError::TooManyAccessKeys {
                            requested_account_id,
                            limit,
                            block_height,
                            block_hash,
                        }
                    }
                    e => other(&e),
                })?;
            let keys = keys.into_iter().map(|(pk, key)| AccessKeyInfoView::new(pk, key.into()));
            QueryResponseKind::AccessKeyList(AccessKeyList { keys: keys.collect(), last_key })
        }
        QueryRequest::ViewGasKeyNonces { account_id, public_key } => {
            let nonces = VIEWER.view_gas_key_nonces(&state, &account_id, &public_key).map_err(
                |e| match e {
                    ViewGasKeyNoncesError::GasKeyDoesNotExist { public_key } => {
                        RpcQueryError::UnknownGasKey { public_key, block_height, block_hash }
                    }
                    e => other(&e),
                },
            )?;
            QueryResponseKind::GasKeyNonces(GasKeyNoncesView { nonces })
        }
        QueryRequest::CallFunction { account_id, method_name, args } => {
            let (result, logs) = sim.chain.view(&sim.config, at, &account_id, &method_name, &args);
            let result = result.map_err(|e| match e {
                CallFunctionError::AccountDoesNotExist { requested_account_id } => {
                    unknown(requested_account_id)
                }
                CallFunctionError::VMError { error, error_message } => {
                    RpcQueryError::ContractExecutionError {
                        vm_error: error_message,
                        error,
                        block_height,
                        block_hash,
                    }
                }
                e => other(&e),
            })?;
            QueryResponseKind::CallResult(CallResult { result, logs })
        }
        QueryRequest::ViewGlobalContractCode { code_hash } => {
            global(GlobalContractIdentifier::CodeHash(code_hash))?
        }
        QueryRequest::ViewGlobalContractCodeByAccountId { account_id } => {
            global(GlobalContractIdentifier::AccountId(account_id))?
        }
    };
    Ok(RpcQueryResponse { kind, block_height, block_hash })
}
