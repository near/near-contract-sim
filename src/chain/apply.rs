//! What one block takes from the chain around it: the `ApplyState`, an
//! epoch, the chunk's transactions, and what the previous chunk hands on.

use super::{Chain, Config};
use super::{assert_vm_room, call_error};
use crate::cache::contract_cache;
use near_primitives::apply::ApplyChunkReason;
use near_primitives::bandwidth_scheduler::BlockBandwidthRequests;
use near_primitives::congestion_info::{
    BlockCongestionInfo, CongestionControl, CongestionInfo, ExtendedCongestionInfo,
    RejectTransactionReason, ShardAcceptsTransactions,
};
use near_primitives::errors::{EpochError, InvalidTxError};
use near_primitives::hash::{CryptoHash, hash};
use near_primitives::receipt::Receipt;
use near_primitives::shard_layout::ShardLayout;
use near_primitives::transaction::{ExecutionOutcomeWithId, SignedTransaction};
use near_primitives::types::{AccountId, Balance, EpochId, EpochInfoProvider, Gas, ShardId};
use node_runtime::config::tx_cost;
use node_runtime::{
    ApplyResult, ApplyState, Runtime, SignedValidPeriodTransactions, ValidatorAccountsUpdate,
};

impl Chain {
    /// Runs the chunk: `txs` and the `incoming` receipts. Panics where
    /// nearcore would refuse the block.
    pub(super) fn apply_chunk(
        &self,
        config: &Config,
        apply_state: &ApplyState,
        validators: &Option<ValidatorAccountsUpdate>,
        incoming: &[Receipt],
        txs: Vec<SignedTransaction>,
    ) -> ApplyResult {
        let e = apply_state.epoch_height;
        let taken: Vec<CryptoHash> = txs.iter().map(SignedTransaction::get_hash).collect();
        // Record reads like a chunk producer does (state witness): past the
        // soft limit the runtime delays the remaining receipts, and a single
        // receipt reading more than `per_receipt_storage_proof_size_limit`
        // fails.
        let trie = self.trie().recording_reads_with_proof_size_limit(
            apply_state.config.witness_config.main_storage_proof_size_soft_limit,
        );
        let res = Runtime::new()
            .apply(
                trie,
                validators,
                apply_state,
                incoming,
                SignedValidPeriodTransactions::new(txs, vec![true; taken.len()]),
                &SimEpochInfo::new(config),
                Default::default(),
            )
            .unwrap_or_else(|err| match &validators {
                // nearcore's FATAL staking check: an account holds less
                // locked than the stake it keeps.
                Some(_) => panic!(
                    "Runtime::apply failed at the block that applies epoch {e}'s boundary: \
                     {err:?}. If this is nearcore's staking invariant, a staked account's \
                     locked balance is below its stake: did a direct write \
                     (`apply_state_records`) lower it?"
                ),
                None => panic!("Runtime::apply failed: {err:?}"),
            });
        // nearcore drops a transaction only when the chunk's tokens burnt
        // overflow u128 (`process_transactions`), after sending or queueing
        // its receipt, which the log can't place without the transaction's
        // outcome.
        if let Some(h) = taken.iter().find(|&h| res.outcomes.iter().all(|o| o.id != *h)) {
            panic!("the runtime dropped transaction {h}: the chunk's tokens burnt overflow u128");
        }
        res.outcomes.iter().filter_map(|o| call_error(&o.outcome.status)).for_each(assert_vm_room);
        res
    }

    /// The state the next block applies with: block `height + 1`, one
    /// `block_time` after the last.
    pub(super) fn apply_state(&self, config: &Config) -> ApplyState {
        let height = self.height.checked_add(1).expect("block height overflow");
        ApplyState {
            apply_reason: ApplyChunkReason::UpdateTrackedShard,
            block_height: height,
            prev_block_hash: self.prev_block_hash(config),
            shard_id: self.shard_uid.shard_id(),
            // Every epoch's id is the default: nearcore's runtime only keys
            // `EpochInfoProvider` lookups with it, which answer the same for
            // every epoch here, and no host function reads it.
            epoch_id: EpochId::default(),
            epoch_height: self.epoch_height(config, height),
            gas_price: config.gas_price,
            block_timestamp: self
                .timestamp_ns
                .checked_add(config.block_time_ns())
                .expect("block timestamp overflow"),
            gas_limit: config.block_gas_limit,
            random_seed: self.random_seed(config, height),
            current_protocol_version: config.protocol_version,
            config: config.runtime_config().clone(),
            next_wasm_config: None,
            cache: Some(contract_cache()),
            is_new_chunk: true,
            save_receipt_to_tx: false,
            congestion_info: self.carry.congestion_info.clone(),
            bandwidth_requests: self.carry.bandwidth_requests.clone(),
            trie_access_tracker_state: Default::default(),
            on_post_state_ready: None,
        }
    }

    /// Block `height`'s random seed, from the config's seed, the height and
    /// how many blocks were produced at that height before (0 unless a
    /// rewind revisits it): the same whether the chain stepped or jumped
    /// there.
    fn random_seed(&self, config: &Config, height: u64) -> CryptoHash {
        let n = self.log.blocks.iter().filter(|b| b.height == height).count() as u64;
        let parts = [config.seed, height, n].map(u64::to_le_bytes);
        hash(&[b"random".as_slice(), &parts.concat()].concat())
    }

    /// What a chunk producer would put in this chunk (nearcore's
    /// `prepare_transactions`, in submission order): nothing if the shard's
    /// congestion level (from the previous chunk) is at or above
    /// `reject_tx_congestion_threshold` -- those transactions fail with
    /// `ShardCongested`, which is what RPC answers, and come back as the
    /// second list -- otherwise transactions until their conversion gas
    /// reaches `process_tx_limit()` or their size would pass
    /// `combined_transactions_size_limit`. The rest waits for the next block.
    pub(super) fn take_chunk_transactions(
        &mut self,
        config: &Config,
    ) -> (Vec<SignedTransaction>, Vec<ExecutionOutcomeWithId>) {
        let rc = config.runtime_config();
        let cc = CongestionControl::new(rc.congestion_control_config, self.carry.congestion(), 0);
        if let ShardAcceptsTransactions::No(reason) = cc.shard_accepts_transactions() {
            let congestion_level = match reason {
                RejectTransactionReason::IncomingCongestion { congestion_level }
                | RejectTransactionReason::OutgoingCongestion { congestion_level }
                | RejectTransactionReason::MemoryCongestion { congestion_level } => {
                    congestion_level
                }
                RejectTransactionReason::MissedChunks { .. } => unreachable!("no missed chunks"),
            };
            let shard_id = self.shard_uid.shard_id().into();
            let err = InvalidTxError::ShardCongested { shard_id, congestion_level };
            let refused = std::mem::take(&mut self.mempool)
                .iter()
                .map(|h| ExecutionOutcomeWithId::failed(&self.log.txs[h], err.clone()))
                .collect();
            return (vec![], refused);
        }
        let (gas_limit, size_limit) =
            (cc.process_tx_limit(), rc.witness_config.combined_transactions_size_limit as u64);
        let (mut gas, mut size, mut take) = (Gas::ZERO, 0u64, 0);
        for tx in self.queued() {
            let tx_size = tx.size_for_limits(config.protocol_version);
            // These cannot execute. Let the runtime report its validation
            // error without an un-packable head blocking the whole queue.
            if tx_size > rc.wasm_config.limit_config.max_transaction_size {
                take += 1;
                continue;
            }
            if gas >= gas_limit || size + tx_size > size_limit {
                break;
            }
            if let Ok(cost) = tx_cost(rc, &tx.transaction, config.gas_price) {
                gas = gas.saturating_add(cost.gas_burnt);
            }
            size += tx_size;
            take += 1;
        }
        let txs = self.mempool.drain(..take).map(|h| (*self.log.txs[&h]).clone()).collect();
        (txs, vec![])
    }
}

/// The epoch as the runtime and views see it: one shard, and one validator
/// with a fixed stake, whatever the epoch. It holds the chain id and the
/// minimum stake.
pub(super) struct SimEpochInfo {
    chain_id: String,
    minimum_stake: Balance,
}

impl SimEpochInfo {
    pub(super) fn new(config: &Config) -> Self {
        SimEpochInfo { chain_id: config.chain_id.clone(), minimum_stake: config.minimum_stake }
    }
}

pub(crate) const VALIDATOR: &str = "validator.near";
const VALIDATOR_STAKE: Balance = Balance::from_near(50_000_000);

impl EpochInfoProvider for SimEpochInfo {
    fn validator_stake(&self, _: &EpochId, a: &AccountId) -> Result<Option<Balance>, EpochError> {
        Ok((a.as_str() == VALIDATOR).then_some(VALIDATOR_STAKE))
    }
    fn validator_total_stake(&self, _: &EpochId) -> Result<Balance, EpochError> {
        Ok(VALIDATOR_STAKE)
    }
    fn minimum_stake(&self, _: &CryptoHash) -> Result<Balance, EpochError> {
        Ok(self.minimum_stake)
    }
    fn chain_id(&self) -> String {
        self.chain_id.clone()
    }
    fn shard_layout(&self, _: &EpochId) -> Result<ShardLayout, EpochError> {
        Ok(ShardLayout::single_shard())
    }
}

/// What a chunk hands the next one: its congestion info and bandwidth
/// requests, which the runtime needs back every block even on one shard
/// (the bandwidth scheduler throttles receipts a shard sends itself).
#[derive(Clone)]
pub(super) struct Carry {
    shard_id: ShardId,
    congestion_info: BlockCongestionInfo,
    bandwidth_requests: BlockBandwidthRequests,
}

impl Carry {
    pub(crate) fn new(shard_id: ShardId) -> Self {
        let congestion_info = [(shard_id, ExtendedCongestionInfo::default())].into_iter().collect();
        Carry {
            shard_id,
            congestion_info: BlockCongestionInfo::new(congestion_info),
            bandwidth_requests: BlockBandwidthRequests::empty(),
        }
    }

    pub(crate) fn update(&mut self, res: &ApplyResult) {
        let requests = [(self.shard_id, res.bandwidth_requests.clone())].into_iter().collect();
        self.bandwidth_requests = BlockBandwidthRequests { shards_bandwidth_requests: requests };
        if let Some(ci) = res.congestion_info {
            let info = ExtendedCongestionInfo { missed_chunks_count: 0, congestion_info: ci };
            self.congestion_info.insert(self.shard_id, info);
        }
    }

    /// The shard's congestion as the previous chunk left it.
    pub(crate) fn congestion(&self) -> CongestionInfo {
        self.congestion_info.get(&self.shard_id).expect("the shard's own").congestion_info
    }

    /// No receipt waiting in state to run or to be sent: the congestion
    /// info's receipt bytes count delayed and buffered receipts alike.
    pub(crate) fn is_empty(&self) -> bool {
        self.congestion().receipt_bytes() == 0
    }
}
