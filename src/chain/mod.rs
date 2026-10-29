//! The private core: nearcore's `Runtime::apply` over an in-memory trie, one
//! chunk per block on a single shard, and the block log. Receipts a block
//! produces are the next block's incoming receipts, which is what a
//! single-shard chain does, unless a hold keeps one back (`order.rs`).

mod apply;
mod callback;
mod epoch;
mod fault;
mod log;
mod order;
mod pending;
mod reply;
mod state;
mod watch;

#[cfg(feature = "near-kit")]
pub(crate) use apply::VALIDATOR;
pub(crate) use epoch::Epochs;
pub(crate) use log::{Block, Log, action_receipt, calls, is_gas_refund, method_names};
pub(crate) use reply::BoundReply;
pub(crate) use state::{VIEWER, prepaid_gas, yield_timeout};

use crate::inject::{Fault, Receipts};
use apply::Carry;
use borsh::BorshDeserialize;
use fault::Faulting;
use near_parameters::{RuntimeConfig, RuntimeConfigStore};
use near_primitives::errors::{ActionError, ActionErrorKind, FunctionCallError, TxExecutionError};
use near_primitives::hash::{CryptoHash, hash};
use near_primitives::receipt::{Receipt, ReceiptOrStateStoredReceipt, ReceiptSource};
use near_primitives::shard_layout::{ShardLayout, ShardUId};
use near_primitives::transaction::{ExecutionStatus, SignedTransaction};
use near_primitives::trie_key::TrieKey;
use near_primitives::types::{
    Balance, Gas, ProtocolVersion, RawStateChangesWithTrieKey, StateRoot,
};
use near_primitives::version::{MIN_SUPPORTED_PROTOCOL_VERSION, PROTOCOL_VERSION};
use near_store::ShardTries;
use near_store::test_utils::TestTriesBuilder;
use near_store::trie::AccessOptions;
use std::collections::hash_map::Entry;
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, LazyLock};
use std::time::Duration;

/// Chain parameters, as `sim.config`. Each block reads them when it is
/// produced, so a change applies from the next block.
#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Config {
    /// The version whose fees, limits and wasm config the next block runs
    /// with; changing it runs no migrations. The pinned nearcore runs 84 to
    /// 88, and its `PROTOCOL_VERSION`, 88, is the default.
    /// [`runtime_config`](Self::runtime_config) and
    /// [`Sim::step`](crate::Sim::step) panic on any other.
    pub protocol_version: ProtocolVersion,
    /// What `env::chain_id()` returns: `localnet` by default. Views read it
    /// at once, and blocks from the next one.
    pub chain_id: String,
    /// Mainnet's minimum by default, 1e8 yocto per gas.
    pub gas_price: Balance,
    /// The chunk gas limit: mainnet's 1000 Tgas by default; `None` lifts it.
    pub block_gas_limit: Option<Gas>,
    /// How far the timestamp moves per height, skipped heights included:
    /// 1 s by default.
    pub block_time: Duration,
    /// Block hashes and random seeds derive from it: 0 by default.
    pub seed: u64,
    /// Blocks per epoch, mainnet's 43,200 by default, and at least 3: under
    /// 3, a stake's return and the epoch a call runs in shift. Epoch `e`
    /// starts at block `(e - 1) * epoch_length + 1` (genesis is in epoch 1),
    /// and `env::epoch_height()` there is `e`, numbered as on a long-running
    /// chain; a fresh node's first two epochs share height 1. Set it before
    /// the first block: blocks panic if it changes after, and blocks, views
    /// and `Sim::advance_epochs` panic if it is under 3.
    pub epoch_length: u64,
    /// What each account with stake in an epoch earns for it, flat and
    /// whatever its stake, added to its locked balance at the next epoch's
    /// first block as nearcore adds validator rewards. A node's amount is an
    /// inflation share; don't read reward math from this. 0 by default.
    pub epoch_reward: Balance,
    /// What nearcore's `action_stake` checks a positive stake against,
    /// once the account can pay for it: under it, the `Stake` action fails
    /// with nearcore's own
    /// [`InsufficientStake`](crate::ActionErrorKind::InsufficientStake),
    /// whether it raises, keeps or lowers what is locked; a stake of 0
    /// (leaving) is never checked. On a network it is the next epoch's seat
    /// price over `minimum_stake_divisor`, so it moves only at epoch
    /// boundaries: change it there to model a drifting seat price (a change
    /// mid-epoch is one no network makes). Only that check reads it: the
    /// sim's epochs still accept every positive proposal, with no seat
    /// price and no kickouts. 0 by default, so any positive stake passes,
    /// as before; a real seat price depends on a validator set the sim
    /// doesn't model.
    pub minimum_stake: Balance,
    /// How many blocks [`Sim::settle`](crate::Sim::settle) and
    /// [`Sim::advance_to`](crate::Sim::advance_to) produce before deciding
    /// the chain is stuck in a receipt loop: 10,000 by default.
    pub max_blocks: u64,
    /// The gas ceilings blocks and views run under: the protocol
    /// version's own by default ([`GasLimits::Protocol`]), or
    /// [`GasLimits::Legacy300`] when the environment sets
    /// `NEAR_SIM_GAS_LIMITS=legacy300` (`protocol` is the default), so a
    /// built test binary can run under either.
    pub gas_limits: GasLimits,
    /// The protocol version whose gas prices the next block runs with:
    /// `None` (the default) for [`protocol_version`](Self::protocol_version)'s
    /// own. `Some(n)` takes from nearcore's `RuntimeConfigStore` config for
    /// `n` what gas *prices*: the host-function costs (`ext_costs`, trie
    /// node accesses among them), the wasm op and memory costs, and the
    /// action fees. Everything else stays `protocol_version`'s: limits,
    /// features, which reads charge a key's path, the linear-op costs
    /// (placeholders before the ops existed), storage staking, refund
    /// penalties. To price a defect at an older (or newer) version's
    /// costs, as a sandbox on that version would, while contracts run on
    /// the pinned semantics: an approximation, labeled as such on every
    /// report made under it, and never merged into a default one. Any `n` the
    /// store holds (it keeps a config per change since genesis).
    pub gas_pricing: Option<ProtocolVersion>,
}

/// The gas ceilings a [`Config`] runs under: what one transaction may
/// attach (`max_total_prepaid_gas`), what one receipt may burn
/// (`max_gas_burnt`), and what a view may burn (a node's
/// `max_gas_burnt_view`, which is `max_gas_burnt` unless the node sets
/// it). Every ceiling the sim and its exploration use comes from here,
/// through [`Config::runtime_config`].
#[non_exhaustive]
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize,
)]
pub enum GasLimits {
    /// The protocol version's: 1 PGas for all three on every version the
    /// pinned nearcore runs (raised in protocol 83).
    #[default]
    Protocol,
    /// 300 Tgas for all three, the ceilings before protocol 83: to audit
    /// against the old limit.
    Legacy300,
    /// These ceilings: `prepaid` per transaction, `burnt` per receipt and
    /// per view.
    Custom {
        /// `max_total_prepaid_gas`.
        prepaid: Gas,
        /// `max_gas_burnt`, and the views' limit.
        burnt: Gas,
    },
}

/// 300 Tgas: what wallets and SDKs attach to a call, so
/// [`Tx::call`](crate::Tx::call)'s default; and every ceiling before
/// protocol 83, so [`GasLimits::Legacy300`]'s.
pub(crate) const WALLET_GAS: Gas = Gas::from_teragas(300);

impl GasLimits {
    /// `NEAR_SIM_GAS_LIMITS`: `legacy300`, or `protocol` and unset for
    /// [`Protocol`](Self::Protocol). Panics on anything else.
    #[track_caller]
    fn from_env() -> GasLimits {
        match std::env::var("NEAR_SIM_GAS_LIMITS").as_deref() {
            Err(_) | Ok("protocol") => GasLimits::Protocol,
            Ok("legacy300") => GasLimits::Legacy300,
            Ok(other) => panic!("NEAR_SIM_GAS_LIMITS={other:?}: expected protocol or legacy300"),
        }
    }

    /// (prepaid, burnt) overriding the protocol's; `None` for
    /// [`Protocol`](Self::Protocol).
    pub(crate) fn overrides(self) -> Option<(Gas, Gas)> {
        match self {
            GasLimits::Protocol => None,
            GasLimits::Legacy300 => Some((WALLET_GAS, WALLET_GAS)),
            GasLimits::Custom { prepaid, burnt } => Some((prepaid, burnt)),
        }
    }

    /// `the protocol max`, `the Legacy300 max`, `the custom max`: how
    /// findings name the ceiling.
    pub fn name(self) -> &'static str {
        match self {
            GasLimits::Protocol => "the protocol max",
            GasLimits::Legacy300 => "the Legacy300 max",
            GasLimits::Custom { .. } => "the custom max",
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        Config {
            protocol_version: PROTOCOL_VERSION,
            chain_id: "localnet".into(),
            gas_price: Balance::from_yoctonear(100_000_000),
            block_gas_limit: Some(Gas::from_teragas(1_000)),
            block_time: Duration::from_secs(1),
            seed: 0,
            epoch_length: 43_200,
            epoch_reward: Balance::ZERO,
            minimum_stake: Balance::ZERO,
            max_blocks: 10_000,
            gas_limits: GasLimits::from_env(),
            gas_pricing: None,
        }
    }
}

impl Config {
    /// Fees, limits and wasm config of `protocol_version`, from nearcore's
    /// own `RuntimeConfigStore`, with [`gas_pricing`](Self::gas_pricing)'s
    /// prices and [`gas_limits`](Self::gas_limits) applied. Panics on a
    /// version the pinned nearcore can't run.
    #[track_caller]
    pub fn runtime_config(&self) -> &'static Arc<RuntimeConfig> {
        let v = self.protocol_version;
        let supported = MIN_SUPPORTED_PROTOCOL_VERSION..=PROTOCOL_VERSION;
        assert!(
            supported.contains(&v),
            "config.protocol_version {v} is not supported (supported: {supported:?})"
        );
        limited_config(v, self.gas_limits, self.gas_pricing)
    }

    /// What one transaction may attach: `max_total_prepaid_gas`. The
    /// chunk's gas limit doesn't bound it: that limit counts a
    /// transaction's conversion gas, not its prepaid gas.
    pub(crate) fn gas_ceiling(&self) -> Gas {
        self.runtime_config().wasm_config.limit_config.max_total_prepaid_gas
    }

    pub(crate) fn block_time_ns(&self) -> u64 {
        self.block_time.as_nanos().try_into().expect("config.block_time exceeds u64 nanoseconds")
    }

    #[track_caller]
    fn epoch_length(&self) -> u64 {
        let n = self.epoch_length;
        assert!(
            n >= 3,
            "config.epoch_length is {n}: an epoch needs at least 3 blocks (under 3, a stake's \
             return and the epoch a call runs in shift)"
        );
        n
    }
}

/// Panics unless a transaction submitted again carries the `same` as it
/// did (`was`: how it differed): its hash doesn't cover them.
#[track_caller]
fn resubmitted(h: &CryptoHash, same: bool, was: &str) {
    assert!(
        same,
        "transaction {h} was submitted before {was}: its hash doesn't cover them, so a copy must \
         have the same"
    );
}

/// How the simulator delivers a transaction's receipt (`callback.rs`).
#[derive(Clone, Debug, borsh::BorshSerialize)]
pub(crate) enum Delivery {
    /// As its receiver's callback to itself, reading these promise results
    /// in order (`None`: the promise failed).
    Callback(Vec<Option<Vec<u8>>>),
    /// As a receipt from this predecessor.
    From(near_primitives::types::AccountId),
}

/// Where a view runs: a block's height, time and protocol version, over a
/// state root.
#[derive(Clone, Copy)]
pub(crate) struct ViewAt {
    pub height: u64,
    pub timestamp_ns: u64,
    pub protocol_version: ProtocolVersion,
    pub root: StateRoot,
}

/// Unchecked: a block's version was checked when it ran. The protocol's
/// own limits: what fees read (no fee depends on a gas ceiling).
pub(crate) fn runtime_config(v: ProtocolVersion) -> &'static Arc<RuntimeConfig> {
    static STORE: LazyLock<RuntimeConfigStore> = LazyLock::new(|| RuntimeConfigStore::new(None));
    STORE.get_config(v)
}

/// [`runtime_config`] of `v` with `pricing`'s gas prices (see
/// [`Config::gas_pricing`]): `v`'s own for `None`. Built once per
/// (version, pricing) and kept for the process.
pub(crate) fn priced_config(
    v: ProtocolVersion,
    pricing: Option<ProtocolVersion>,
) -> &'static Arc<RuntimeConfig> {
    let Some(p) = pricing else { return runtime_config(v) };
    type Configs = HashMap<(ProtocolVersion, ProtocolVersion), &'static Arc<RuntimeConfig>>;
    static PRICED: LazyLock<std::sync::Mutex<Configs>> = LazyLock::new(Default::default);
    let mut built = PRICED.lock().unwrap_or_else(|e| e.into_inner());
    built.entry((v, p)).or_insert_with(|| {
        let (pinned, prices) = (runtime_config(v), runtime_config(p));
        let mut rc = RuntimeConfig::clone(pinned);
        let mut wasm = (*rc.wasm_config).clone();
        wasm.ext_costs = prices.wasm_config.ext_costs.clone();
        wasm.regular_op_cost = prices.wasm_config.regular_op_cost;
        wasm.grow_mem_cost = prices.wasm_config.grow_mem_cost;
        rc.wasm_config = Arc::new(wasm);
        let mut fees = (*rc.fees).clone();
        fees.action_fees = prices.fees.action_fees.clone();
        rc.fees = Arc::new(fees);
        Box::leak(Box::new(Arc::new(rc)))
    })
}

/// [`priced_config`] with `limits` applied to a clone, built once per
/// (version, limits, pricing) and kept for the process.
fn limited_config(
    v: ProtocolVersion,
    limits: GasLimits,
    pricing: Option<ProtocolVersion>,
) -> &'static Arc<RuntimeConfig> {
    let Some((prepaid, burnt)) = limits.overrides() else { return priced_config(v, pricing) };
    type Key = (ProtocolVersion, GasLimits, Option<ProtocolVersion>);
    type Configs = HashMap<Key, &'static Arc<RuntimeConfig>>;
    static LIMITED: LazyLock<std::sync::Mutex<Configs>> = LazyLock::new(Default::default);
    let mut built = LIMITED.lock().unwrap_or_else(|e| e.into_inner());
    built.entry((v, limits, pricing)).or_insert_with(|| {
        let mut rc = RuntimeConfig::clone(priced_config(v, pricing));
        let mut wasm = (*rc.wasm_config).clone();
        wasm.limit_config.max_total_prepaid_gas = prepaid;
        wasm.limit_config.max_gas_burnt = burnt;
        rc.wasm_config = Arc::new(wasm);
        Box::leak(Box::new(Arc::new(rc)))
    })
}

#[derive(Clone)]
pub(crate) struct Chain {
    tries: ShardTries,
    shard_uid: ShardUId,
    root: StateRoot,
    /// Height and timestamp of the last block, or what `Sim::set_height` and
    /// `Sim::set_timestamp_ns` made them since.
    pub height: u64,
    pub timestamp_ns: u64,
    /// Transactions for the next block, in submission order.
    mempool: Vec<CryptoHash>,
    /// Receipts for the next block.
    incoming: Vec<Receipt>,
    /// Receipts holds keep back (`order.rs`), in the order they go out, each
    /// with what it still waits for and the block (by count) it was held in.
    holding: VecDeque<(Receipt, order::Wait, usize)>,
    carry: Carry,
    /// Transactions whose receipt the simulator delivers otherwise than a
    /// node would: see `callback.rs`.
    callbacks: HashMap<CryptoHash, Delivery>,
    /// Transactions submitted with injections (`Tx::inject`), or tracked
    /// (`lenient`): see `fault.rs`.
    faults: HashMap<CryptoHash, Faulting>,
    /// The receipts every other transaction spawned, in the order they
    /// left their blocks: what a `HoldUntil` in a later one counts
    /// (`Receipts::in_tx`). Not snapshotted.
    history: HashMap<CryptoHash, Vec<crate::fault::Spawned>>,
    /// Transactions that bind stub replies (`Tx::stub_reply`), and the
    /// receipts waiting on one: see `reply.rs`.
    replies: HashMap<CryptoHash, Vec<BoundReply>>,
    awaiting: Vec<reply::Awaiting>,
    /// Per transaction, stub and method, the calls a bound reply answered
    /// so far: which of its queued replies the next one takes.
    answered: HashMap<(CryptoHash, near_primitives::types::AccountId, String), usize>,
    log: Log,
}

impl Chain {
    pub(crate) fn new(height: u64, timestamp_ns: u64) -> Self {
        let shard_uid = ShardLayout::single_shard().shard_uids().next().unwrap();
        Chain {
            tries: TestTriesBuilder::new().build(),
            shard_uid,
            root: StateRoot::default(),
            height,
            timestamp_ns,
            mempool: vec![],
            incoming: vec![],
            holding: VecDeque::new(),
            carry: Carry::new(shard_uid.shard_id()),
            callbacks: HashMap::new(),
            faults: HashMap::new(),
            history: HashMap::new(),
            replies: HashMap::new(),
            awaiting: vec![],
            answered: HashMap::new(),
            log: Log::default(),
        }
    }

    pub(crate) fn log(&self) -> &Log {
        &self.log
    }

    /// The last block's hash: the next block's `prev_block_hash`, and the
    /// `block_hash` a transaction signed now references.
    pub(crate) fn prev_block_hash(&self, config: &Config) -> CryptoHash {
        self.log.blocks.last().map_or_else(|| block_hash(config.seed, 0), |b| b.hash)
    }

    /// Queues `tx` for the next block, its receipt delivered as `results`
    /// says if some (see `callback.rs`), and with `injections` if some
    /// (see `fault.rs` and `order.rs`; an empty list tracks its receipts),
    /// and with the stub replies it binds (see `reply.rs`). A transaction
    /// already submitted isn't queued again, as in nearcore's mempool,
    /// unless its conversion was refused. `roots` lists it once. The hash
    /// covers neither `results`, `injections` nor `replies`, so a copy must
    /// come with the same ones: panics if it doesn't.
    #[track_caller]
    pub(crate) fn submit(
        &mut self,
        tx: SignedTransaction,
        results: Option<Delivery>,
        injections: Option<Vec<(Receipts, Fault)>>,
        replies: Vec<BoundReply>,
    ) -> CryptoHash {
        let h = tx.get_hash();
        let before = self.replies.get(&h).map_or(&[][..], Vec::as_slice);
        if self.log.txs.contains_key(&h) {
            resubmitted(&h, before == replies.as_slice(), "with other stub replies");
        }
        let h = self.queue(tx, results, injections);
        if !replies.is_empty() {
            self.replies.insert(h, replies);
        }
        h
    }

    #[track_caller]
    fn queue(
        &mut self,
        tx: SignedTransaction,
        results: Option<Delivery>,
        injections: Option<Vec<(Receipts, Fault)>>,
    ) -> CryptoHash {
        let h = tx.get_hash();
        self.check_in_tx(injections.as_deref().unwrap_or_default());
        let digest = results.as_ref().map(|r| hash(&borsh::to_vec(r).expect("borsh")));
        if self.log.forget_refusal(&h) {
            // The hash covers the transaction, not its signature: a retry
            // can correct a signature that the runtime refused.
            self.log.txs.insert(h, Arc::new(tx));
            self.mempool.push(h);
        } else if let Entry::Vacant(e) = self.log.txs.entry(h) {
            e.insert(Arc::new(tx));
            self.log.roots.push(h);
            self.mempool.push(h);
        } else {
            let before = self.log.delivered.get(&h);
            let was = match (before, &digest) {
                (None, _) => "as a plain call",
                (Some(_), None) => "with promise results or a predecessor",
                (Some(_), Some(_)) => "with other promise results or another predecessor",
            };
            resubmitted(&h, before == digest.as_ref(), was);
            let before = self.faults.get(&h).map_or(&[][..], |f| &f.injections);
            let same = before == injections.as_deref().unwrap_or_default();
            resubmitted(&h, same, "with other injections");
            return h;
        }
        match injections {
            Some(injections) => {
                self.history.remove(&h);
                self.faults.insert(h, Faulting::new(injections));
            }
            None => {
                self.faults.remove(&h);
                self.history.insert(h, vec![]);
            }
        }
        match (digest, results) {
            (Some(d), Some(r)) => {
                self.log.delivered.insert(h, d);
                self.callbacks.insert(h, r);
            }
            _ => {
                self.log.delivered.remove(&h);
                self.callbacks.remove(&h);
            }
        }
        h
    }

    /// Panics unless each `Receipts::in_tx` is a `HoldUntil`'s and names a
    /// transaction submitted before.
    #[track_caller]
    fn check_in_tx(&self, injections: &[(Receipts, Fault)]) {
        for (at, fault) in injections {
            assert!(
                at.tx_of().is_none(),
                "{fault} at {at}: an injection selects its own transaction's receipts; \
                 in_tx names whose receipt a Fault::HoldUntil waits for"
            );
            if let Fault::HoldUntil(until) = fault
                && let Some(t) = until.tx_of()
            {
                assert!(
                    self.log.txs.contains_key(&t),
                    "{fault} at {at}: {t} names no transaction submitted before this one"
                );
            }
        }
    }

    pub(crate) fn queued(&self) -> impl Iterator<Item = &SignedTransaction> {
        self.mempool.iter().map(|h| &*self.log.txs[h])
    }

    /// Applies block `height + 1`, one `block_time` after the last, and makes
    /// it the last.
    pub(crate) fn produce_block(&mut self, config: &Config) {
        let apply_state = self.apply_state(config);
        self.note_awaiting();
        let bound = self.bind(config, &apply_state);
        let prev_state_root = self.root;
        let (e, length) = (apply_state.epoch_height, config.epoch_length);
        if self.log.epochs.unrewarded(e, config.epoch_reward) {
            note_unrewarded(e);
        }
        let validators = self.log.epochs.enter(e, length, config.epoch_reward);
        self.height = apply_state.block_height;
        self.timestamp_ns = apply_state.block_timestamp;
        let incoming = std::mem::take(&mut self.incoming);
        let (txs, mut outcomes) = self.take_chunk_transactions(config);
        let mut res = self.apply_chunk(config, &apply_state, &validators, &incoming, txs);
        // Before the log records the bodies, so a delivered callback's, and a
        // faulted receipt's, is its own.
        self.rewrite_callbacks(outcomes.iter().chain(&res.outcomes), &mut res.outgoing_receipts);
        let buffered: Vec<Receipt> =
            res.state_changes.iter().filter_map(buffered_receipt).collect();
        let hold = self.apply_faults(&mut res.outcomes, &mut res.outgoing_receipts, &buffered);
        let before = self.trie();
        self.log.note_writes(&res.state_changes, |k| {
            before.get(&k.to_vec(), AccessOptions::DEFAULT).expect("storage")
        });
        self.watch_block(config, prev_state_root, &res);
        self.apply_changes(&res.trie_changes);
        self.log.on_balance(&res.stats.balance);
        // What the epoch boundary paid out, which the runtime's balance stats
        // leave out. A staked account can't be deleted
        // (`DeleteAccountStaking`), so each reward reached its account.
        validators
            .iter()
            .flat_map(|v| v.validator_rewards.iter())
            .for_each(|(to, r)| self.log.on_reward(to, *r));
        let next = self.epoch_height(config, self.height.saturating_add(1));
        self.log.epochs.propose(next, &res.validator_proposals);
        // Each receipt is recorded in the block that creates it: as it goes
        // out; as it enters the outgoing buffer, in state, if congestion
        // control holds it back for a later block; or, for an instant receipt,
        // which never goes out, as it runs (nearcore's `is_instant_receipt`:
        // the PromiseYield receipt a yield parks in state, and a receipt whose
        // only action is DeleteAccount and that waits on no data). A local one
        // (a transaction's to its own signer) never goes out, delayed or not,
        // so it gets no body, as in nearcore: it carries the transaction's
        // actions.
        let instant = res.processed_receipts.iter().filter(|p| p.source == ReceiptSource::Instant);
        for r in instant.map(|p| &p.receipt).chain(&buffered).chain(&res.outgoing_receipts) {
            self.log.record(r);
        }
        self.carry.update(&res);
        outcomes.extend(std::mem::take(&mut res.outcomes));
        let ran: Vec<CryptoHash> = outcomes.iter().map(|o| o.id).collect();
        let refused = self.hold_back(std::mem::take(&mut res.outgoing_receipts), &hold, &ran);
        self.unbind(config, bound, &ran);
        self.log.push(Block {
            height: self.height,
            timestamp_ns: self.timestamp_ns,
            hash: block_hash(config.seed, self.log.blocks.len() as u64 + 1),
            prev_hash: apply_state.prev_block_hash,
            prev_state_root,
            protocol_version: config.protocol_version,
            pricing: config.gas_pricing,
            outcomes,
        });
        if let Some(refused) = refused {
            self.refuse_hold(refused);
        }
    }

    /// The epoch block `h` runs in by `config`, 1 for genesis. Panics if
    /// `config.epoch_length` is under 3 or isn't the one the chain's epochs
    /// were counted in.
    #[track_caller]
    pub(crate) fn epoch_height(&self, config: &Config, h: u64) -> u64 {
        let e = h.saturating_sub(1) / config.epoch_length() + 1;
        self.log.epochs.check_length(config.epoch_length);
        e
    }

    /// The first block of epoch `e`, 1 for epoch 1; panics where
    /// `epoch_height` does, and for epoch 0 or an overflow.
    #[track_caller]
    pub(crate) fn epoch_start(&self, config: &Config, e: u64) -> u64 {
        let h = e.checked_sub(1).and_then(|e| e.checked_mul(config.epoch_length()));
        let h = h.and_then(|h| h.checked_add(1)).expect("epoch_start: epoch 0 or an overflow");
        self.log.epochs.check_length(config.epoch_length);
        h
    }

    /// Nothing queued or in flight: no transaction waits for a block, and no
    /// receipt for the next one, delayed, buffered or held.
    pub(crate) fn is_settled(&self) -> bool {
        self.is_quiet() && self.holding.is_empty()
    }

    /// Settled but for held receipts.
    fn is_quiet(&self) -> bool {
        self.mempool.is_empty() && self.incoming.is_empty() && self.carry.is_empty()
    }
}

fn call_error(status: &ExecutionStatus) -> Option<&FunctionCallError> {
    match status {
        ExecutionStatus::Failure(TxExecutionError::ActionError(ActionError {
            kind: ActionErrorKind::FunctionCallError(e),
            ..
        })) => Some(e),
        _ => None,
    }
}

/// Panics if the VM failed a call, a view's included, for want of a wasm
/// instance. nearcore's wasmtime engine pools 1000 per process, and
/// returns freed ones to the pool in batches of 500; a call holds one
/// while it runs, and a `Sim` or its clones hold none between calls. A call
/// that finds none fails with a `LinkError`, which would pass for the
/// contract's failure where a node would have run it.
#[track_caller]
pub(crate) fn assert_vm_room(e: &FunctionCallError) {
    let FunctionCallError::LinkError { msg } = e else { return };
    if msg.starts_with("maximum concurrent limit of") || msg == "failed to acquire execution slot" {
        panic!(
            "the VM ran out of wasm instances ({msg}): nearcore's engine has 1000 for the \
             whole test process, and too many calls ran at once. The test process failed, not \
             the contract: run fewer test threads (--test-threads=4), or nextest, which runs \
             each test in a process of its own"
        );
    }
}

/// The hash of the `n`th block (genesis's for `n = 0`). It counts blocks,
/// not heights, so hashes stay unique when the height moves backward.
fn block_hash(seed: u64, n: u64) -> CryptoHash {
    hash(&[seed.to_le_bytes(), n.to_le_bytes()].concat())
}

fn buffered_receipt(c: &RawStateChangesWithTrieKey) -> Option<Receipt> {
    let TrieKey::BufferedReceipt { .. } = c.trie_key else { return None };
    let v = c.changes.last()?.data.as_deref()?;
    Some(ReceiptOrStateStoredReceipt::try_from_slice(v).expect("a buffered receipt").into_receipt())
}

/// Says once per process that an epoch began with stake and
/// `config.epoch_reward` at 0: a staking contract then sees no reward,
/// and one that skips a sync with nothing to distribute takes another
/// path than on a network.
fn note_unrewarded(epoch: u64) {
    use std::sync::atomic::{AtomicBool, Ordering};
    static NOTED: AtomicBool = AtomicBool::new(false);
    if !NOTED.swap(true, Ordering::Relaxed) {
        eprintln!(
            "near-contract-sim: epoch {epoch} began with stake and config.epoch_reward = 0: no \
             staking reward is paid, so a contract that skips a zero-reward sync times its \
             work differently than on a network; set config.epoch_reward (printed once)"
        );
    }
}

#[cfg(test)]
mod tests {
    use crate::sim::tests::{park, probe_sim, stale};
    use crate::{AccountId, Balance, Failure, Sim, builtin};
    use near_primitives::types::StateChangeCause;

    /// A transaction without injections never enters `faults`, which a
    /// block's fault pass scans per data receipt: only `history` records
    /// what it spawned, for a later `HoldUntil` (`Receipts::in_tx`).
    #[test]
    fn a_plain_transaction_is_recorded_but_not_faulted() {
        use crate::{Reply, Stub};
        let mut sim = probe_sim();
        sim.deploy_stub("bob.near", Stub::new().on("ping", Reply::json("pong")));
        let ask = Reply::call("bob.near", "ping", ()).then(Reply::json("asked"));
        sim.deploy_stub("app.near", Stub::new().on("ask", ask));
        let out = sim.call("alice.near", "app.near", "ask").transact();
        out.assert_success();
        assert!(sim.chain.faults.is_empty());
        assert_eq!(sim.chain.history.len(), sim.txs().len(), "every plain transaction");
        // The call to bob.near: not the transaction's own receipt, nor the
        // refunds.
        let spawned = sim.chain.spawned(&out.hash);
        assert_eq!(spawned.iter().map(|s| s.site()).collect::<Vec<_>>(), ["bob.near::ping"]);
    }

    /// Past a stale timeout-queue head (a resumed yield's entry), the chain
    /// is busy: the next block pops the head. Yet nothing is queued or in
    /// flight, so a snapshot takes it, and a loaded one is just as busy, and
    /// conservation runs.
    #[test]
    fn past_a_stale_head_the_chain_is_busy_yet_settled() {
        let mut sim = probe_sim();
        let expires_at = stale(&mut sim);
        sim.set_height(expires_at);
        assert!(sim.yields().is_empty() && !sim.is_idle());
        let (key, setups) = (std::time::SystemTime::now(), std::cell::Cell::new(0));
        let cached = || {
            crate::Sim::cached(key, || {
                setups.set(setups.get() + 1);
                sim.clone()
            })
        };
        assert!(!cached().is_idle() && !cached().is_idle());
        assert_eq!(setups.get(), 1, "saved, then loaded");
        sim.findings().assert_none();
        sim.chain.log.minted += 1;
        let vs = sim.findings();
        let [v] = &vs[..] else { panic!("{vs:?}") };
        assert_eq!(v.check, builtin::CONSERVATION);
        assert!(v.message.ends_with("(off by -1 yocto)"), "{v}");
    }

    /// Conservation runs while a yield is parked: its callback receipt holds
    /// its prepaid gas and fees, so the books balance exactly once it parks,
    /// once it is resumed and once it times out. A balance written past the
    /// ledger is caught while a yield is parked.
    #[test]
    fn conservation_counts_what_a_parked_yield_holds() {
        let mut sim = probe_sim();
        let held = |sim: &Sim| sim.chain.total_supply() as i128 - sim.chain.log.supply();
        park(&mut sim);
        assert!(sim.chain.is_settled() && held(&sim) == 0);
        sim.call("alice.near", "probe.near", "resume").transact().assert_success();
        assert!(sim.yields().is_empty() && held(&sim) == 0);
        let expires_at = park(&mut sim);
        assert_eq!(held(&sim), 0);
        sim.advance_to(expires_at);
        sim.settle();
        assert_eq!(sim.view("probe.near", "last", ()).json::<String>(), "timeout");
        assert_eq!(held(&sim), 0);
        sim.findings().assert_none();

        park(&mut sim);
        let alice: AccountId = "alice.near".parse().unwrap();
        let mut tu = sim.chain.trie_update();
        let mut acc = near_store::get_account(&tu, &alice).unwrap().unwrap();
        acc.set_amount(acc.amount().checked_add(Balance::from_yoctonear(1)).unwrap());
        near_store::set_account(&mut tu, alice, &acc);
        tu.commit(StateChangeCause::InitialState);
        sim.chain.apply_changes(&tu.finalize().unwrap().trie_changes);
        assert_eq!(sim.yields().len(), 1);
        let vs = sim.findings();
        let [v] = &vs[..] else { panic!("{vs:?}") };
        assert_eq!(v.check, builtin::CONSERVATION);
        assert!(v.message.ends_with("(off by 1 yocto)"), "{v}");
    }

    /// `findings()` reports the unhandled failures, then conservation, so
    /// filtering out a known failure leaves the conservation finding.
    #[test]
    fn conservation_is_reported_after_the_unhandled_failures() {
        let mut sim = Sim::new();
        for id in ["alice.near", "relayer.near"] {
            sim.create_account(id, Balance::from_near(10));
        }
        sim.call("alice.near", "nobody.near", "m").relay("relayer.near").assert_success();
        sim.chain.log.minted += 1;
        let vs = sim.findings();
        let checks: Vec<_> = vs.iter().map(|v| v.check.as_str()).collect();
        assert_eq!(checks, [builtin::UNHANDLED_FAILURE, builtin::CONSERVATION], "{vs:?}");
        assert!(vs[1].tx.is_none() && vs[1].receipt_id.is_none());
        let known = |v: &crate::Finding| {
            let Some((tx, id)) = v.tx.zip(v.receipt_id) else { return false };
            let r = sim.outcome(tx).receipts.into_iter().find(|r| r.receipt_id == id).unwrap();
            matches!(r.failure(), Some(Failure::AccountDoesNotExist(_)))
        };
        let vs: Vec<_> = vs.into_iter().filter(|v| !known(v)).collect();
        let [v] = &vs[..] else { panic!("{vs:?}") };
        assert_eq!(v.check, builtin::CONSERVATION);
    }

    /// A call the VM failed for want of a wasm instance panics, saying so;
    /// any other failure is the contract's.
    #[test]
    fn running_out_of_wasm_instances_panics() {
        use super::{assert_vm_room, call_error};
        use near_primitives::errors::{
            ActionError, ActionErrorKind, FunctionCallError, TxExecutionError,
        };
        use near_primitives::transaction::ExecutionStatus;
        let link = |msg: &str| FunctionCallError::LinkError { msg: msg.into() };
        let failed = |e: FunctionCallError| {
            let kind = ActionErrorKind::FunctionCallError(e);
            ExecutionStatus::Failure(TxExecutionError::ActionError(ActionError {
                index: None,
                kind,
            }))
        };
        let pool = link("maximum concurrent limit of 1000 for memory stripe 0 reached");
        assert_eq!(call_error(&failed(pool.clone())), Some(&pool));
        assert_eq!(call_error(&ExecutionStatus::SuccessValue(vec![])), None);
        let missing =
            ActionErrorKind::AccountDoesNotExist { account_id: "x.near".parse().unwrap() };
        let missing = TxExecutionError::ActionError(ActionError { index: None, kind: missing });
        assert_eq!(call_error(&ExecutionStatus::Failure(missing)), None);
        assert_vm_room(&link("unknown or invalid import"));
        assert_vm_room(&FunctionCallError::ExecutionError("maximum concurrent limit of".into()));
        for e in [pool, link("failed to acquire execution slot")] {
            let err = std::panic::catch_unwind(|| assert_vm_room(&e)).unwrap_err();
            let m = err.downcast_ref::<String>().unwrap();
            assert!(m.starts_with("the VM ran out of wasm instances ("), "{m}");
            assert!(m.contains("--test-threads=4"), "{m}");
        }
    }

    #[test]
    fn total_supply_counts_locked_and_gas_key_balances() {
        use near_primitives::account::{AccessKey, Account, AccountContract};
        use near_primitives::state_record::StateRecord;
        let mut sim = Sim::new();
        let before = sim.chain.total_supply();
        let mut key = AccessKey::gas_key_full_access(1);
        key.gas_key_info_mut().unwrap().balance = Balance::from_yoctonear(50);
        let pk = near_crypto::SecretKey::from_seed(near_crypto::KeyType::ED25519, "k").public_key();
        sim.apply_state_records([
            StateRecord::Account {
                account_id: "a.near".parse().unwrap(),
                account: Account::new(
                    Balance::from_yoctonear(100),
                    Balance::from_yoctonear(7),
                    AccountContract::None,
                    0,
                ),
            },
            StateRecord::access_key("a.near".parse().unwrap(), pk, key),
        ]);
        assert_eq!(sim.chain.total_supply() - before, 157);
    }
}
