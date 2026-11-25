//! External contract calls (NEP-667): what a node checks before a call
//! enters its pool, the pool, what a chunk producer packs of it, and the
//! window in which a node drops a repeat of an accepted call.
//!
//! nearcore's runtime runs the calls: `process_external_call` in apply, and
//! `dry_run_external_call`, which a producer runs before packing one. The
//! simulator, as the node, decides which calls go into a chunk. It mirrors
//! near-chain's `pack_eccs` (not a dependency): signed transactions first,
//! then one call per contract while the chunk's rules on calls
//! (`ExternalCallsValidity`) and their shares of compute and storage proof
//! (`ExternalCallsInChunk`) leave room, each dry-run on a fresh recorder
//! over the state the chunk applies on, with what the calls accepted before
//! it wrote, and only accepted calls packed. A forced call skips the
//! producer's choices (the dry run, the repeat, congestion and contract
//! checks) and keeps only the chunk's rules, so apply alone decides.

use super::log::SeenCall;
use super::{Chain, Config, ViewAt};
use crate::cache::contract_cache;
use crate::ecc::ExternalCallRefusal;
use near_crypto::PublicKey;
use near_primitives::account::{AccessKey, Account};
use near_primitives::apply::ApplyChunkReason;
use near_primitives::errors::{InvalidTxError, TxExecutionError};
use near_primitives::hash::CryptoHash;
use near_primitives::transaction::{
    EccTransaction, ExternalCallsValidity, SignedTransaction, TransactionEnvelope,
};
use near_primitives::types::{AccountId, EpochId, NonceIndex};
use near_primitives::utils::create_ecc_outcome_id;
use near_runtime::config::tx_cost;
use near_runtime::{
    ApplyState, EccDryRun, EccDryRunContext, ExternalCallInclusion, ExternalCallsInChunk,
    PendingConstraints, PrevBlockContext, TxAuthorization, TxVerdict, call_can_run_at_account,
    dry_run_external_call_with_output, ecc_cost_for_account, get_signer_and_authorization,
    validate_envelope, validate_transaction, verify_and_charge_tx_ephemeral, verify_ecc_ephemeral,
};
use near_store::Trie;
use near_store::trie::update::TrieUpdate;
use near_store::{get_account, get_gas_key_nonce};
use std::collections::HashMap;
use std::sync::Arc;

/// For this many blocks after a contract accepts a call, a node drops a
/// resend of it unrun: near-chain's `EXTERNAL_CALL_REPEAT_WINDOW`.
pub(crate) const REPEAT_WINDOW: u64 = 1_000;

/// One send of an external call, in the order sent.
#[derive(Clone)]
pub(crate) struct Send {
    pub ecc: Arc<EccTransaction>,
    pub forced: bool,
    pub verdict: Verdict,
}

/// What came of a send.
#[derive(Clone, Debug)]
pub(crate) enum Verdict {
    /// In the pool.
    Pending,
    /// In a chunk, under this unique id.
    Included(CryptoHash),
    /// Dropped by the node's checks or the producer's dry run.
    Refused(ExternalCallRefusal),
    /// Dropped as a repeat of the accepted inclusion with this unique id.
    Repeat(CryptoHash),
}

fn refused(error: impl Into<TxExecutionError>) -> Verdict {
    Verdict::Refused(ExternalCallRefusal { error: error.into(), logs: vec![] })
}

impl Chain {
    /// Sends `ecc`, as `send_tx` does: a node answers a repeat of a call it
    /// accepted within the window with that inclusion, refuses a call
    /// whose validity, contract or fees fail its checks, and pools the
    /// rest. A call sent while the same one (by hash) is in the pool
    /// shares its send. A forced call skips the checks. Returns the send.
    pub(crate) fn send_ecc(&mut self, config: &Config, ecc: EccTransaction, forced: bool) -> usize {
        let pending = |s: &Send| matches!(s.verdict, Verdict::Pending) && !s.forced;
        if !forced
            && let Some(&i) = self
                .ecc_pool
                .iter()
                .find(|&&i| pending(&self.sends[i]) && self.sends[i].ecc.hash() == ecc.hash())
        {
            return i;
        }
        let verdict = match forced {
            true => Self::check_forced_ecc(config, &ecc),
            false => self.check_ecc(config, &ecc),
        };
        let i = self.sends.len();
        if matches!(verdict, Verdict::Pending) {
            self.ecc_pool.push(i);
        }
        self.sends.push(Send { ecc: Arc::new(ecc), forced, verdict });
        i
    }

    pub(crate) fn send(&self, i: usize) -> &Send {
        &self.sends[i]
    }

    /// near-chain's checks of a call before its pool (`process_tx`): a
    /// repeat goes nowhere, then `check_before_run`.
    fn check_ecc(&self, config: &Config, ecc: &EccTransaction) -> Verdict {
        if let Some(unique) = self.repeat_of(ecc) {
            return Verdict::Repeat(unique);
        }
        match check_before_run(config, &self.trie(), ecc) {
            Ok(()) => Verdict::Pending,
            Err(e) => refused(e),
        }
    }

    /// A `call_function` view of `__external_call` (near-chain's
    /// `view_external_call`): the checks before the pool, then the dry run
    /// of the call in a chunk on top of block `at`, which it sees as its
    /// block, with no repeat check and no signed transaction before it.
    /// Returns the run's value and logs, or why the call would be refused.
    pub(crate) fn view_ecc(
        &self,
        config: &Config,
        at: ViewAt,
        ecc: &EccTransaction,
    ) -> Result<(Vec<u8>, Vec<String>), Box<ExternalCallRefusal>> {
        let refusal =
            |e: InvalidTxError| Box::new(ExternalCallRefusal { error: e.into(), logs: vec![] });
        let trie = self.trie_at(at.root);
        check_before_run(config, &trie, ecc).map_err(refusal)?;
        let rc = config.runtime_config();
        let hash = self.log.blocks.iter().rev().find(|b| b.height == at.height).map(|b| b.hash);
        let prev_block = PrevBlockContext {
            hash: hash.unwrap_or_else(|| self.prev_block_hash(config)),
            height: at.height,
            epoch_id: EpochId::default(),
            epoch_height: self.epoch_height(config, at.height),
            timestamp: at.timestamp_ns,
        };
        let context = EccDryRunContext {
            shard_id: self.shard_uid.shard_id(),
            epoch_id: EpochId::default(),
            protocol_version: config.protocol_version,
            config: Arc::clone(rc),
            config_store: Arc::clone(config.runtime_config_store()),
            vm_generation: config.runtime_config_store().vm_generation(config.protocol_version),
            gas_price: config.gas_price,
            prev_block,
            cache: Some(contract_cache()),
            apply_reason: ApplyChunkReason::ViewTrackedShard,
        };
        let mut shares = ExternalCallsInChunk::new(rc, config.block_gas_limit);
        let mut state = TrieUpdate::new(trie.recording_reads_new_recorder());
        let out = dry_run_external_call_with_output(
            &mut state,
            context,
            &super::apply::SimEpochInfo::new(config),
            &mut shares,
            None,
            ecc,
        )
        .unwrap_or_else(|e| panic!("dry run of an external call failed: {e:?}"));
        match out.dry_run {
            EccDryRun::Accepted { .. } => Ok((out.return_value, out.logs)),
            EccDryRun::Refused(error) => {
                Err(Box::new(ExternalCallRefusal { error, logs: out.logs }))
            }
        }
    }

    /// The one check of a forced call: a chunk holding a call that fails
    /// its own validity (its size, or a protocol before external calls)
    /// fails validation (near-chain's `pre_validate_chunk_state_witness`),
    /// so no producer can include it.
    fn check_forced_ecc(config: &Config, ecc: &EccTransaction) -> Verdict {
        match ecc.check_valid_for_config(config.runtime_config(), config.protocol_version) {
            Ok(()) => Verdict::Pending,
            Err(e) => refused(e),
        }
    }

    /// The unique id of the accepted inclusion `ecc` repeats, if its
    /// contract accepted it in a block at most `REPEAT_WINDOW` below the
    /// last (blocks are final at once here): the first such of the ones
    /// near-chain's `ExternalCallCache` keeps.
    fn repeat_of(&self, ecc: &EccTransaction) -> Option<CryptoHash> {
        let seen = self.log.seen_eccs.get(ecc.hash_without_state_init())?;
        let inside =
            |&(h, _): &(u64, CryptoHash)| h <= self.height && self.height - h <= REPEAT_WINDOW;
        let mut inside = seen.accepted().filter(inside);
        inside.next().map(|(_, unique)| unique)
    }

    pub(crate) fn has_pooled_eccs(&self) -> bool {
        !self.ecc_pool.is_empty()
    }

    /// The calls a chunk producer packs into the chunk `apply_state` runs,
    /// after the signed transactions `signed`, which take `signed_size`
    /// bytes. Each packed call is registered under its unique id, the
    /// outcome id apply gives it; the dry run's refusals and the node's
    /// discards leave the pool with their reason; the rest stay, in order.
    pub(super) fn take_chunk_eccs(
        &mut self,
        config: &Config,
        apply_state: &ApplyState,
        signed: &[SignedTransaction],
        signed_size: u64,
    ) -> Vec<EccTransaction> {
        if self.ecc_pool.is_empty() {
            return vec![];
        }
        let rc = config.runtime_config();
        let mut validity = ExternalCallsValidity::new(rc);
        let mut shares = ExternalCallsInChunk::new(rc, apply_state.gas_limit);
        let mut size_left =
            (rc.witness_config.combined_transactions_size_limit as u64).saturating_sub(signed_size);
        let congested = self.shard_congested(config);
        let mut state = TrieUpdate::new(self.trie().recording_reads_new_recorder());
        let charged = SignerCharges::new(self, config, apply_state, signed);
        let (mut packed, mut left) = (vec![], vec![]);
        for i in std::mem::take(&mut self.ecc_pool) {
            let send = &self.sends[i];
            let ecc = Arc::clone(&send.ecc);
            // The chunk holds one call per contract: the others wait.
            if validity.has_call_to(ecc.contract_id()) {
                left.push(i);
                continue;
            }
            // A full chunk leaves every call for the next.
            if !validity.has_room() || !shares.has_room() {
                left.push(i);
                continue;
            }
            // A repeat leaves the pool even if it doesn't fit.
            if !send.forced
                && let Some(unique) = self.repeat_of(&ecc)
            {
                self.sends[i].verdict = Verdict::Repeat(unique);
                continue;
            }
            if ecc.get_size() > size_left {
                left.push(i);
                continue;
            }
            let Ok(room) = validity.reserve(&ecc) else {
                left.push(i);
                continue;
            };
            if send.forced {
                room.commit();
                size_left -= ecc.get_size();
                packed.push((i, (*ecc).clone()));
                continue;
            }
            if let Some(e) = &congested {
                self.sends[i].verdict = refused(e.clone());
                continue;
            }
            if !call_can_run_at_account(&state, &ecc).expect("storage") {
                let contract_id = ecc.contract_id().clone();
                self.sends[i].verdict = refused(InvalidTxError::ContractNotFound { contract_id });
                continue;
            }
            // Each run records on a recorder of its own, as a producer's does.
            state.trie = state.trie.recording_reads_new_recorder();
            let before = shares.clone();
            let loaded = charged.account(ecc.contract_id());
            let context = dry_run_context(config, apply_state);
            let out = dry_run_external_call_with_output(
                &mut state,
                context,
                &super::apply::SimEpochInfo::new(config),
                &mut shares,
                loaded,
                &ecc,
            )
            .unwrap_or_else(|e| panic!("dry run of an external call failed: {e:?}"));
            match out.dry_run {
                // The chunk takes no other call to this contract, so no
                // later run sees the account the call charged.
                EccDryRun::Accepted { installed_state_init, .. } => {
                    room.commit();
                    // A state init the account ignored is stripped, as a
                    // producer strips it; the unique id leaves it out.
                    let ecc = match ecc.state_init().is_some() && !installed_state_init {
                        true => ecc.without_state_init(),
                        false => (*ecc).clone(),
                    };
                    size_left -= ecc.get_size();
                    packed.push((i, ecc));
                }
                EccDryRun::Refused(error) => {
                    drop(room);
                    shares = before;
                    self.sends[i].verdict =
                        Verdict::Refused(ExternalCallRefusal { error, logs: out.logs });
                }
            }
        }
        self.ecc_pool = left;
        let height = apply_state.block_height;
        packed
            .into_iter()
            .map(|(i, ecc)| {
                let unique = create_ecc_outcome_id(ecc.hash_without_state_init(), height);
                assert!(
                    !self.log.eccs.contains_key(&unique),
                    "external call {} to {} was included at height {height} before: its unique \
                     id, {unique}, is its hash and the height, so a rewind (set_height) that \
                     includes it at that height again repeats the id; include it at another \
                     height",
                    ecc.hash_without_state_init(),
                    ecc.contract_id(),
                );
                self.log.eccs.insert(unique, Arc::new(ecc.clone()));
                self.log.roots.push(unique);
                self.history.insert(unique, vec![]);
                self.sends[i].verdict = Verdict::Included(unique);
                ecc
            })
            .collect()
    }

    /// Notes the chunk's calls for the repeat window, as near-chain's
    /// `ExternalCallCache::remember` does: each call's first accepted
    /// inclusion stays while inside the window, and its latest replaces
    /// the one before.
    pub(super) fn remember_eccs(&mut self, height: u64, inclusions: &[ExternalCallInclusion]) {
        for inc in inclusions {
            let latest = (height, inc.unique_id);
            let earlier = self.log.seen_eccs.get(&inc.hash_without_state_init);
            let first_accepted = match earlier.and_then(|s| s.first_accepted) {
                Some(first) if height.saturating_sub(first.0) <= REPEAT_WINDOW => Some(first),
                _ => inc.accepted.then_some(latest),
            };
            let seen = SeenCall { first_accepted, latest, latest_accepted: inc.accepted };
            self.log.seen_eccs.insert(inc.hash_without_state_init, seen);
        }
    }
}

/// near-chain's checks of a call before its run, on `trie`: its validity
/// (`validate_tx`), then its contract's (`check_ecc_at_state`): the account
/// can pay the fees and has a contract to run, or a state init that
/// installs one.
fn check_before_run(
    config: &Config,
    trie: &Trie,
    ecc: &EccTransaction,
) -> Result<(), InvalidTxError> {
    let rc = config.runtime_config();
    let envelope = TransactionEnvelope::Unsigned(ecc.clone());
    validate_envelope(rc, envelope, config.protocol_version).map_err(|(e, _)| e)?;
    let account = get_account(trie, ecc.contract_id()).expect("storage");
    let cost = ecc_cost_for_account(rc, ecc, account.as_ref(), config.gas_price)
        .map_err(|_| InvalidTxError::CostOverflow)?;
    verify_ecc_ephemeral(account.as_ref(), ecc, &cost)?;
    if !call_can_run_at_account(trie, ecc).expect("storage") {
        let contract_id = ecc.contract_id().clone();
        return Err(InvalidTxError::ContractNotFound { contract_id });
    }
    Ok(())
}

/// What a producer's dry run sees: the chunk's parent block and config,
/// as apply runs the call with them.
fn dry_run_context(config: &Config, apply_state: &ApplyState) -> EccDryRunContext {
    EccDryRunContext {
        shard_id: apply_state.shard_id,
        epoch_id: apply_state.epoch_id,
        protocol_version: apply_state.current_protocol_version,
        config: Arc::clone(&apply_state.config),
        config_store: Arc::clone(config.runtime_config_store()),
        vm_generation: apply_state.vm_generation,
        gas_price: apply_state.gas_price,
        prev_block: apply_state.prev_block.clone(),
        cache: Some(contract_cache()),
        apply_reason: ApplyChunkReason::UpdateTrackedShard,
    }
}

/// The accounts of the chunk's signers after the signed transactions that
/// pass their checks paid for themselves, as apply has them loaded when it
/// reaches the chunk's calls (near-chain's `SignerOverlay`): a call to a
/// contract that signs one of them sees that balance. Only an account with
/// such a transaction is loaded; a call to another reads its own.
struct SignerCharges {
    accounts: HashMap<AccountId, Account>,
}

impl SignerCharges {
    fn new(
        chain: &Chain,
        config: &Config,
        apply_state: &ApplyState,
        signed: &[SignedTransaction],
    ) -> Self {
        let rc = config.runtime_config();
        let trie = chain.trie();
        let mut accounts: HashMap<AccountId, Account> = HashMap::new();
        let mut keys: HashMap<(AccountId, PublicKey), AccessKey> = HashMap::new();
        let mut nonces: HashMap<(AccountId, PublicKey, NonceIndex), u64> = HashMap::new();
        for tx in signed {
            let Ok(validated) =
                validate_transaction(rc, tx.clone(), apply_state.current_protocol_version)
            else {
                continue;
            };
            let t = &tx.transaction;
            let (id, pk) = (t.signer_id().clone(), t.public_key().clone());
            let Ok((stored, authorization)) = get_signer_and_authorization(&trie, &validated)
            else {
                continue;
            };
            let Ok(cost) = tx_cost(rc, t, apply_state.gas_price) else { continue };
            let account = accounts.get(&id).cloned().unwrap_or(stored);
            let authorization = match authorization {
                TxAuthorization::AccessKey(k) => TxAuthorization::AccessKey(
                    keys.get(&(id.clone(), pk.clone())).cloned().unwrap_or(k),
                ),
                TxAuthorization::GasKey { access_key, nonce_index } => TxAuthorization::GasKey {
                    access_key: keys.get(&(id.clone(), pk.clone())).cloned().unwrap_or(access_key),
                    nonce_index,
                },
                a => a,
            };
            let nonce = |index| {
                let stored = || get_gas_key_nonce(&trie, &id, &pk, index);
                nonces.get(&(id.clone(), pk.clone(), index)).map_or_else(stored, |n| Ok(Some(*n)))
            };
            let verdict = verify_and_charge_tx_ephemeral(
                rc,
                &account,
                authorization.as_tx_authorization_ref(),
                t,
                &cost,
                Some(apply_state.block_height),
                &PendingConstraints::default(),
                nonce,
            )
            .expect("storage");
            let TxVerdict::Success(result) = verdict else { continue };
            let (mut account, mut key) = (account, authorization.into_access_key());
            result.apply(&mut account, key.as_mut()).expect("a consistent verdict");
            if let Some((index, n)) = result.gas_key_nonce_update() {
                nonces.insert((id.clone(), pk.clone(), index), n);
            }
            if let Some(key) = key {
                keys.insert((id.clone(), pk), key);
            }
            accounts.insert(id, account);
        }
        SignerCharges { accounts }
    }

    fn account(&self, id: &AccountId) -> Option<Account> {
        self.accounts.get(id).cloned()
    }
}
