//! The trie: the one write path, which counts what it mints and seeds the
//! stake of the locked balances it writes, insert-only commits, the yield
//! timeout queue, and views at a block.

use super::apply::SimEpochInfo;
use super::{Chain, Config, GasLimits, Log, ViewAt, action_receipt, assert_vm_room, priced_config};
use crate::cache::contract_cache;
use borsh::BorshDeserialize;
use near_parameters::{ActionCosts, RuntimeConfig};
use near_primitives::account::{AccessKey, Account};
use near_primitives::action::Action;
use near_primitives::hash::CryptoHash;
use near_primitives::receipt::{PromiseYieldTimeout, Receipt};
use near_primitives::trie_key::{TrieKey, col};
use near_primitives::types::{AccountId, Balance, EpochId, Gas, StateChangeCause, StateRoot};
use near_store::trie::AccessOptions;
use near_store::trie::update::TrieUpdate;
use near_store::{Trie, TrieChanges};
use node_runtime::config::{total_deposit, total_prepaid_exec_fees, total_prepaid_gas};
use node_runtime::state_viewer::errors::CallFunctionError;
use node_runtime::state_viewer::{TrieViewer, ViewApplyState};
use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};

/// The gas the creator of an action receipt to `receiver` prepaid on top
/// of the send fees it burnt: its attached gas and exec fees,
/// `new_action_receipt`'s included (nearcore's `total_prepaid_gas` and
/// `total_prepaid_exec_fees`).
pub(crate) fn prepaid_gas(rc: &RuntimeConfig, actions: &[Action], receiver: &AccountId) -> Gas {
    let fees = [
        total_prepaid_gas(actions).expect("gas overflow"),
        total_prepaid_exec_fees(rc, actions, receiver).expect("fee overflow").gas,
        rc.fees.fee(ActionCosts::new_action_receipt).exec_fee().gas,
    ];
    fees.into_iter().fold(Gas::ZERO, |sum, g| sum.checked_add(g).expect("gas overflow"))
}

/// nearcore's `TrieViewer::default()`, with no state size limit, built once:
/// each call builds a `RuntimeConfigStore`, all protocol versions' parameters.
pub(crate) static VIEWER: LazyLock<TrieViewer> = LazyLock::new(TrieViewer::default);

/// The viewer calls run under: [`VIEWER`], whose limit is the protocol's
/// `max_gas_burnt`, or one with a node's `max_gas_burnt_view` set to the
/// limits' burnt gas, built once per limits.
fn viewer(limits: GasLimits) -> &'static TrieViewer {
    let Some((_, burnt)) = limits.overrides() else { return &VIEWER };
    static LIMITED: LazyLock<Mutex<HashMap<GasLimits, &'static TrieViewer>>> =
        LazyLock::new(Default::default);
    let mut built = LIMITED.lock().unwrap_or_else(|e| e.into_inner());
    built.entry(limits).or_insert_with(|| {
        let store = near_parameters::RuntimeConfigStore::new(None);
        Box::leak(Box::new(TrieViewer::new(store, None, 100, Some(burnt))))
    })
}

impl Chain {
    pub(crate) fn trie(&self) -> Trie {
        self.trie_at(self.root)
    }

    /// The trie at `root`, the current one or an older one: commits only
    /// insert, so older roots stay readable.
    pub(crate) fn trie_at(&self, root: StateRoot) -> Trie {
        self.tries.get_trie_for_shard(self.shard_uid, root)
    }

    pub(crate) fn trie_update(&self) -> TrieUpdate {
        self.tries.new_trie_update(self.shard_uid, self.root)
    }

    pub(crate) fn state_root(&self) -> CryptoHash {
        self.root
    }

    /// The latest block as a view sees it: the clock, the last block's
    /// protocol version (`config`'s before the first block) and the current
    /// state.
    pub(crate) fn latest(&self, config: &Config) -> ViewAt {
        let protocol_version =
            self.log.blocks.last().map_or(config.protocol_version, |b| b.protocol_version);
        ViewAt {
            height: self.height,
            timestamp_ns: self.timestamp_ns,
            protocol_version,
            root: self.root,
        }
    }

    /// Calls `method` of `contract` at `at` as RPC's `call_function` does,
    /// with its logs.
    pub(crate) fn view(
        &self,
        config: &Config,
        at: ViewAt,
        contract: &AccountId,
        method: &str,
        args: &[u8],
    ) -> (Result<Vec<u8>, CallFunctionError>, Vec<String>) {
        let tu = TrieUpdate::new(self.trie_at(at.root));
        self.run_view(config, viewer(config.gas_limits), tu, at, (contract, method, args))
    }

    /// The code `id` ran at `at`, its own or a global contract's; `None`
    /// without.
    pub(crate) fn code(&self, config: &Config, at: ViewAt, id: &AccountId) -> Option<Vec<u8>> {
        let tu = TrieUpdate::new(self.trie_at(at.root));
        let code =
            VIEWER.view_account_contract_code(&tu, id, at.protocol_version, &config.chain_id);
        code.ok().map(|c| c.into_code())
    }

    /// Runs `call` on `tu` under `viewer`, in block `at` (its root unread).
    pub(super) fn run_view(
        &self,
        config: &Config,
        viewer: &TrieViewer,
        tu: TrieUpdate,
        at: ViewAt,
        (contract, method, args): (&AccountId, &str, &[u8]),
    ) -> (Result<Vec<u8>, CallFunctionError>, Vec<String>) {
        let state = ViewApplyState {
            block_height: at.height,
            prev_block_hash: self.prev_block_hash(config),
            shard_id: self.shard_uid.shard_id(),
            epoch_id: EpochId::default(),
            epoch_height: self.epoch_height(config, at.height),
            block_timestamp: at.timestamp_ns,
            current_protocol_version: at.protocol_version,
            cache: Some(contract_cache()),
        };
        let mut logs = vec![];
        let epoch_info = SimEpochInfo::new(config);
        let result =
            viewer.call_function(tu, state, contract, method, args, &mut logs, &epoch_info);
        if let Err(CallFunctionError::VMError { error, .. }) = &result {
            assert_vm_room(error);
        }
        (result, logs)
    }

    /// The `expires_at` of the timeout queue's head, stale (its yield already
    /// resumed) or not. nearcore pops the head while it is due and stops at
    /// the first entry still ahead, so no timeout behind the head fires
    /// before it.
    pub(crate) fn timeout_due(&self) -> Option<u64> {
        let tu = self.trie_update();
        let idx = near_store::get_promise_yield_indices(&tu).expect("storage");
        // An empty queue has no entry at `first_index`.
        yield_timeout(&tu, idx.first_index).map(|t| t.expires_at)
    }

    /// The NEAR in every account and gas key, and in every receipt parked
    /// in state: a yield's callback, and a callback that waits on one.
    pub(crate) fn total_supply(&self) -> u128 {
        let trie = self.trie();
        let mut total = 0;
        for c in [col::ACCOUNT, col::ACCESS_KEY, col::PROMISE_YIELD_RECEIPT, col::POSTPONED_RECEIPT]
        {
            let mut it = trie.disk_iter().expect("storage");
            it.seek_prefix([c]).expect("storage");
            for item in it {
                let (k, v) = item.expect("storage");
                total += match c {
                    col::PROMISE_YIELD_RECEIPT | col::POSTPONED_RECEIPT => self.parked(&v),
                    _ => held(&k, Some(&v)),
                };
            }
        }
        total
    }

    /// The NEAR a receipt parked in state holds, which nearcore pays out of
    /// it when it runs (`refund_unspent_gas_and_deposits`): its deposits,
    /// and the gas it prepaid, its attached gas and exec fees at the last
    /// block's fees, at the price that gas was bought. A contract made it,
    /// so it is no refund and holds no delegate's send fees.
    fn parked(&self, v: &[u8]) -> u128 {
        let r = Receipt::try_from_slice(v).expect("a parked receipt");
        let a = action_receipt(&r).expect("an action receipt");
        let actions = a.actions();
        let last = self.log.blocks.last().expect("a block parked it");
        let rc = priced_config(last.protocol_version, last.pricing);
        let gas = prepaid_gas(rc, actions, r.receiver_id()).as_gas();
        total_deposit(actions).expect("deposit overflow").as_yoctonear()
            + u128::from(gas) * a.gas_price().as_yoctonear()
    }

    /// Commits what `f` writes, and adds what it mints to the ledger: for
    /// every account and access key written, the NEAR held under it now
    /// minus before (negative when a write destroys some). An account it
    /// gives `locked` where it had none is staked that much (`Epochs::seed`)
    /// unless it is still staked or proposing; a nonzero `locked` it
    /// changes leaves the stakes as they were.
    pub(crate) fn write(&mut self, config: &Config, f: impl FnOnce(&mut TrieUpdate)) {
        let mut tu = self.trie_update();
        f(&mut tu);
        tu.commit(StateChangeCause::InitialState);
        let r = tu.finalize().expect("finalize");
        for c in &r.state_changes {
            let key = c.trie_key.to_vec();
            let old = r.trie.get(&key, AccessOptions::DEFAULT).expect("storage");
            let new = c.changes.last().and_then(|change| change.data.as_deref());
            self.log.minted += held(&key, new) as i128 - held(&key, old.as_deref()) as i128;
            // Locked where there was none: staked, as a genesis validator is
            // (`Epochs::seed`).
            if let TrieKey::Account { account_id } = &c.trie_key
                && locked(old.as_deref()).is_zero()
                && !locked(new).is_zero()
            {
                let now = self.epoch_height(config, self.height);
                self.log.epochs.seed(account_id, locked(new), now);
            }
        }
        self.apply_changes(&r.trie_changes);
    }

    /// Insertions only: nodes of older roots stay readable, which is what
    /// lets a clone (a snapshot) keep working after this chain moves on.
    pub(super) fn apply_changes(&mut self, changes: &TrieChanges) {
        let mut su = self.tries.store_update();
        self.tries.apply_insertions(changes, self.shard_uid, &mut su);
        self.root = changes.new_root;
        su.commit();
    }

    pub(crate) fn entries(&self) -> Vec<(Vec<u8>, Vec<u8>)> {
        self.trie().disk_iter().expect("storage").map(|e| e.expect("storage")).collect()
    }

    /// Restores a saved chain: its trie from `entries`, which must make
    /// exactly `root` (`None` if they don't), and its log.
    pub(crate) fn load(
        &mut self,
        entries: Vec<(Vec<u8>, Vec<u8>)>,
        root: CryptoHash,
        log: Log,
    ) -> Option<()> {
        let entries = entries.into_iter().map(|(k, v)| (k, Some(v)));
        self.apply_changes(&self.trie().update(entries, AccessOptions::DEFAULT).ok()?);
        self.log = log;
        (self.root == root).then_some(())
    }
}

/// NEAR held under a raw trie entry: an account's amount + locked, or a gas
/// key's balance. Gas-key nonces share the access-key column but don't parse
/// as an access key.
fn held(key: &[u8], value: Option<&[u8]>) -> u128 {
    match (key[0], value) {
        (col::ACCOUNT, Some(v)) => {
            let a = Account::try_from_slice(v).expect("account record");
            a.amount().as_yoctonear() + a.locked().as_yoctonear()
        }
        (col::ACCESS_KEY, Some(v)) => AccessKey::try_from_slice(v)
            .ok()
            .and_then(|k| k.gas_key_info().map(|g| g.balance.as_yoctonear()))
            .unwrap_or(0),
        _ => 0,
    }
}

/// An account record's locked balance, 0 for none.
fn locked(account: Option<&[u8]>) -> Balance {
    account.map_or(Balance::ZERO, |v| Account::try_from_slice(v).expect("account record").locked())
}

pub(crate) fn yield_timeout(tu: &TrieUpdate, index: u64) -> Option<PromiseYieldTimeout> {
    near_store::get(tu, &TrieKey::PromiseYieldTimeout { index }).expect("storage")
}
