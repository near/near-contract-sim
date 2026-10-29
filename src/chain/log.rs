//! The block log: every block with its outcomes, and every transaction and
//! receipt body, stored once. Outcomes, history and the supply ledger are
//! read from it. Blocks and bodies sit behind `Arc`s, so a clone copies
//! pointers.

use crate::inject::Fault;
use borsh::{BorshDeserialize, BorshSerialize};
use near_primitives::account::{Account, AccountContract};
use near_primitives::action::{Action, FunctionCallAction};
use near_primitives::chunk_apply_stats::BalanceStatsV1;
use near_primitives::errors::TxExecutionError;
use near_primitives::hash::CryptoHash;
use near_primitives::receipt::{Receipt, VersionedActionReceipt, VersionedReceiptEnum};
use near_primitives::transaction::{
    ExecutionOutcomeWithId, ExecutionStatus, SignedTransaction, Transaction,
};
use near_primitives::trie_key::TrieKey;
use near_primitives::types::{
    AccountId, Balance, ProtocolVersion, RawStateChangesWithTrieKey, StateChangeCause, StateRoot,
};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

#[derive(Clone, BorshSerialize, BorshDeserialize)]
pub(crate) struct Block {
    pub height: u64,
    pub timestamp_ns: u64,
    pub hash: CryptoHash,
    /// The block it applied on, as its header has it.
    pub prev_hash: CryptoHash,
    /// The state root it applied on, as its chunk header has it.
    pub prev_state_root: StateRoot,
    /// The version whose fees and limits it ran with.
    pub protocol_version: ProtocolVersion,
    /// The version whose gas prices it ran with, if not its own
    /// (`Config::gas_pricing`).
    pub pricing: Option<ProtocolVersion>,
    /// Conversions of the transactions it took (and refused as
    /// `ShardCongested`), then every receipt it executed, in execution order.
    pub outcomes: Vec<ExecutionOutcomeWithId>,
}

#[derive(Clone, Default, BorshSerialize, BorshDeserialize)]
pub(crate) struct Log {
    pub blocks: Vec<Arc<Block>>,
    /// Every transaction submitted, in order.
    pub roots: Vec<CryptoHash>,
    pub txs: HashMap<CryptoHash, Arc<SignedTransaction>>,
    pub receipts: HashMap<CryptoHash, Arc<Receipt>>,
    /// Outcome id -> (block, position in its outcomes).
    index: HashMap<CryptoHash, (usize, usize)>,
    /// Receipt id -> the transaction it descends from.
    pub origin: HashMap<CryptoHash, CryptoHash>,
    /// Every failure nobody learns of (`ReceiptOutcome::unhandled`), as
    /// (block, position), in execution order and so sorted.
    pub unhandled: Vec<(usize, usize)>,
    /// Every failed callback that isn't one of those and has something to
    /// reconcile (`Sim::findings`), also as sorted (block, position).
    pub failed_callbacks: Vec<(usize, usize)>,
    /// The receipts that changed their own account's state, with the first
    /// four things each changed and how many it changed: see `note_writes`.
    wrote: HashMap<CryptoHash, (Vec<String>, usize)>,
    /// Per receipt, each account whose `storage_usage` it changed, by how
    /// many bytes: see `note_writes`.
    storage: HashMap<CryptoHash, Vec<(AccountId, i64)>>,
    /// Per receipt, every contract storage key it wrote or removed, in
    /// full, with the value's length (`None`: removed): see `note_writes`.
    written: HashMap<CryptoHash, Vec<crate::outcome::WrittenKey>>,
    /// Per receipt that changed its account's code, the code before and
    /// after: see `note_writes`.
    codes: HashMap<CryptoHash, (AccountContract, AccountContract)>,
    /// The receipts the simulator delivered as callbacks
    /// (`Tx::promise_results`), whose results came from no receipt.
    pub injected: HashSet<CryptoHash>,
    /// The receipts a fault changed (`Tx::inject`), with the first one: the
    /// selected receipt's own, or `OutOfGas` for a call `PromiseFailed`
    /// failed.
    pub faulted: HashMap<CryptoHash, Fault>,
    /// The receipts a hold (`Fault::Hold`) kept back, with how many blocks
    /// later than they would have, each as it went out.
    pub held: HashMap<CryptoHash, u64>,
    /// The same for the results `Fault::HoldResult` kept back, by data id.
    pub held_results: HashMap<CryptoHash, u64>,
    /// Every transaction submitted to be delivered as a callback, with the
    /// hash of its results, so a copy can't change them.
    pub delivered: HashMap<CryptoHash, CryptoHash>,
    /// NEAR the simulator created outside the runtime, by its state writes
    /// (negative: destroyed).
    pub minted: i128,
    /// NEAR the runtime created (zero-balance subsidies, gas price deficits,
    /// epoch rewards) and burnt (gas, lost refunds, slashing).
    runtime_minted: u128,
    burnt: u128,
    /// The epoch rewards each account was paid, in all.
    pub rewarded: std::collections::BTreeMap<near_primitives::types::AccountId, u128>,
    /// The stake each epoch locks, and the proposals still to count
    /// (`epoch.rs`).
    pub epochs: super::Epochs,
    /// The watched tokens and their per-receipt moves (`watch.rs`).
    pub(super) watch: super::watch::Watch,
}

impl Log {
    /// Keeps the first body seen under a receipt's id. A receipt is first
    /// seen once the runtime has given it the result receivers of the parent
    /// that returned it.
    pub(crate) fn record(&mut self, r: &Receipt) {
        self.receipts.entry(*r.receipt_id()).or_insert_with(|| Arc::new(r.clone()));
    }

    /// Appends `block`: indexes its outcomes, hands each one's origin to the
    /// receipts it created, and lists its unhandled failures and failed
    /// callbacks, by the rules on `ReceiptOutcome::unhandled` and
    /// `Sim::findings`. Both read only what ran before the receipt (the result
    /// path's earlier links, the earlier writes), so neither changes later.
    pub(crate) fn push(&mut self, block: Block) {
        let (b, block) = (self.blocks.len(), Arc::new(block));
        self.blocks.push(block.clone());
        for (pos, o) in block.outcomes.iter().enumerate() {
            self.index.insert(o.id, (b, pos));
            // A block's outcomes come parent first, so the origin is known.
            let root = if self.txs.contains_key(&o.id) { o.id } else { self.origin[&o.id] };
            for c in &o.outcome.receipt_ids {
                self.origin.insert(*c, root);
            }
            let body = self.receipts.get(&o.id);
            if let ExecutionStatus::Failure(_) = o.outcome.status
                && !body.is_some_and(|r| is_gas_refund(r))
            {
                let action = body.and_then(|r| action_receipt(r));
                let received =
                    action.as_ref().is_some_and(|a| !a.output_data_receivers().is_empty());
                if !received && self.result_end(root) != o.id {
                    self.unhandled.push((b, pos));
                } else if action.is_some_and(|a| !a.input_data_ids().is_empty())
                    && !self.wrote_before((b, pos)).is_empty()
                {
                    self.failed_callbacks.push((b, pos));
                }
            }
        }
    }

    /// Notes each receipt of a block's `changes` that changed its own
    /// account's state, as `Sim::findings` counts it, before the block is
    /// pushed; `old` reads a key as it was before it. A change is a
    /// receipt's when nearcore committed it with cause `ReceiptProcessing`,
    /// which it does only for a receipt that succeeded, and when it changed
    /// the value (near-sdk rewrites its state on every `&mut self` call).
    /// Only a balance that falls counts: the deposits a receipt carries
    /// raise it, which is the runtime's doing, and the gas reward has its
    /// own cause. So a transfer out that a larger incoming deposit hides
    /// isn't seen.
    pub(crate) fn note_writes(
        &mut self,
        changes: &[RawStateChangesWithTrieKey],
        old: impl Fn(&TrieKey) -> Option<Vec<u8>>,
    ) {
        for c in changes {
            if !matches!(
                c.trie_key,
                TrieKey::Account { .. }
                    | TrieKey::ContractCode { .. }
                    | TrieKey::ContractData { .. }
                    | TrieKey::AccessKey { .. }
                    | TrieKey::GasKeyNonce { .. }
            ) {
                continue;
            }
            let mut prev = old(&c.trie_key);
            for ch in &c.changes {
                if let (
                    StateChangeCause::ReceiptProcessing { receipt_hash },
                    TrieKey::ContractData { account_id, key },
                ) = (&ch.cause, &c.trie_key)
                {
                    let list = self.written.entry(*receipt_hash).or_default();
                    let len = ch.data.as_ref().map(Vec::len);
                    let account = account_id.clone();
                    match list.iter_mut().find(|w| (&w.account, &w.key[..]) == (&account, &key[..]))
                    {
                        Some(w) => w.len = len,
                        None => list.push(crate::outcome::WrittenKey {
                            account,
                            key: key.to_vec(),
                            len,
                        }),
                    }
                }
                if let StateChangeCause::ReceiptProcessing { receipt_hash } = ch.cause {
                    let what = changed(&c.trie_key, prev.as_deref(), ch.data.as_deref());
                    if !what.is_empty() {
                        let (names, n) = self.wrote.entry(receipt_hash).or_default();
                        *n += what.len();
                        names.extend(what.into_iter().take(4 - names.len()));
                    }
                    if let TrieKey::Account { account_id } = &c.trie_key {
                        let account =
                            |v: Option<&[u8]>| v.and_then(|v| Account::try_from_slice(v).ok());
                        let (before, after) =
                            (account(prev.as_deref()), account(ch.data.as_deref()));
                        let code = |a: &Option<Account>| {
                            a.as_ref().map_or(AccountContract::None, |a| a.contract().into_owned())
                        };
                        let (was, is) = (code(&before), code(&after));
                        if was != is {
                            let e = self.codes.entry(receipt_hash).or_insert((was, is.clone()));
                            e.1 = is;
                        }
                        let usage = |a: &Option<Account>| {
                            a.as_ref()
                                .map_or(0, |a| i64::try_from(a.storage_usage()).unwrap_or(i64::MAX))
                        };
                        let bytes = usage(&after) - usage(&before);
                        if bytes != 0 {
                            let list = self.storage.entry(receipt_hash).or_default();
                            match list.iter_mut().find(|(a, _)| a == account_id) {
                                Some((_, n)) => *n += bytes,
                                None => list.push((account_id.clone(), bytes)),
                            }
                        }
                    }
                }
                prev = ch.data.clone();
            }
        }
    }

    pub(crate) fn written(&self, id: &CryptoHash) -> Vec<crate::outcome::WrittenKey> {
        self.written.get(id).cloned().unwrap_or_default()
    }

    pub(crate) fn storage_changes(&self, id: &CryptoHash) -> Vec<crate::effects::StorageChange> {
        let list = self.storage.get(id).map_or(&[][..], Vec::as_slice);
        (list.iter())
            .map(|(account, bytes)| crate::effects::StorageChange {
                account: account.clone(),
                bytes: *bytes,
                receipt_id: *id,
            })
            .collect()
    }

    /// The earlier receipts of `at`'s transaction that its receiver ran and
    /// that changed that account's state, in execution order.
    pub(crate) fn wrote_before(&self, at: (usize, usize)) -> Vec<(usize, usize)> {
        let o = &self.at(at).1;
        let mut wrote: Vec<_> = self
            .outcomes(self.origin[&o.id])
            .into_iter()
            .filter(|&p| {
                let w = &self.at(p).1;
                p < at
                    && w.outcome.executor_id == o.outcome.executor_id
                    && self.wrote.contains_key(&w.id)
            })
            .collect();
        wrote.sort();
        wrote
    }

    /// What receipt `id` changed of its account's state, as `note_writes`
    /// names it: the first four things, then how many more; `None` if
    /// nothing.
    pub(crate) fn wrote(&self, id: &CryptoHash) -> Option<String> {
        let (names, n) = self.wrote.get(id)?;
        let more = (*n > 4).then(|| format!(" and {} more", n - 4));
        Some(names.join(", ") + more.as_deref().unwrap_or(""))
    }

    /// How many unhandled failures and failed callbacks it lists.
    pub(crate) fn flagged(&self) -> (usize, usize) {
        (self.unhandled.len(), self.failed_callbacks.len())
    }

    /// Forgets `tx`'s outcome if its conversion was refused, and says whether
    /// it was: nearcore keeps nothing of a refused transaction, so the same one
    /// can be sent again.
    pub(crate) fn forget_refusal(&mut self, tx: &CryptoHash) -> bool {
        let refused = matches!(
            self.status(tx),
            Some(ExecutionStatus::Failure(TxExecutionError::InvalidTxError(_)))
        );
        if refused {
            self.index.remove(tx);
        }
        refused
    }

    pub(crate) fn position(&self, id: &CryptoHash) -> Option<(usize, usize)> {
        self.index.get(id).copied()
    }

    pub(crate) fn at(&self, (block, pos): (usize, usize)) -> (&Block, &ExecutionOutcomeWithId) {
        let b = &self.blocks[block];
        (b, &b.outcomes[pos])
    }

    pub(crate) fn status(&self, id: &CryptoHash) -> Option<&ExecutionStatus> {
        self.position(id).map(|at| &self.at(at).1.outcome.status)
    }

    /// `root`'s outcome, then those of its receipts that ran, depth first,
    /// each before the ones it created: nearcore's order.
    pub(crate) fn outcomes(&self, root: CryptoHash) -> Vec<(usize, usize)> {
        let (mut out, mut stack) = (vec![], vec![root]);
        while let Some(id) = stack.pop() {
            if let Some(at) = self.position(&id) {
                out.push(at);
                stack.extend(self.at(at).1.outcome.receipt_ids.iter().rev());
            }
        }
        out
    }

    /// Where `root`'s result path stands: following `SuccessReceiptId` from
    /// its outcome, the first receipt that hasn't run or returned no promise.
    pub(crate) fn result_end(&self, root: CryptoHash) -> CryptoHash {
        let mut end = root;
        while let Some(ExecutionStatus::SuccessReceiptId(next)) = self.status(&end) {
            end = *next;
        }
        end
    }

    /// The height where the code of the account the receipt at `at` ran
    /// on last changed, after its transaction began and before it, if the
    /// account had code when it began and its code now differs: an upgrade
    /// under the transaction, not a factory's first deploy, nor the same
    /// code deployed again.
    pub(crate) fn redeployed(&self, at: (usize, usize)) -> Option<u64> {
        let o = &self.at(at).1;
        let began = self.position(self.origin.get(&o.id)?)?;
        let changes: Vec<(u64, &(AccountContract, AccountContract))> = (began.0..=at.0)
            .flat_map(|b| {
                let block = &self.blocks[b];
                let ran = block.outcomes.iter().enumerate();
                ran.filter(move |(k, _)| (b, *k) > began && (b, *k) < at)
                    .filter(|(_, e)| e.outcome.executor_id == o.outcome.executor_id)
                    .filter_map(|(_, e)| Some((block.height, self.codes.get(&e.id)?)))
            })
            .collect();
        let (had, now) = (&changes.first()?.1.0, &changes.last()?.1.1);
        (*had != AccountContract::None && had != now).then(|| changes.last().unwrap().0)
    }

    /// The transaction that converted into receipt `id`, whose actions the
    /// receipt carries from its signer. The log keeps no body for a local
    /// one (to the signer itself), as nearcore doesn't.
    pub(crate) fn converted_from(&self, id: &CryptoHash) -> Option<&Transaction> {
        let t = self.origin.get(id)?;
        let conversion = &self.at(self.index[t]).1.outcome;
        (conversion.receipt_ids.first() == Some(id)).then_some(&self.txs[t].transaction)
    }

    pub(crate) fn on_balance(&mut self, b: &BalanceStatsV1) {
        self.burnt += b.tx_burnt_amount.as_yoctonear()
            + b.slashed_burnt_amount.as_yoctonear()
            + b.other_burnt_amount.as_yoctonear();
        self.runtime_minted +=
            b.subsidized_amount.as_yoctonear() + b.gas_deficit_amount.as_yoctonear();
    }

    pub(crate) fn on_reward(&mut self, to: &near_primitives::types::AccountId, r: Balance) {
        self.runtime_minted += r.as_yoctonear();
        *self.rewarded.entry(to.clone()).or_default() += r.as_yoctonear();
    }

    /// The NEAR there should be: everything minted since genesis, by state
    /// writes and by the runtime, less everything burnt.
    pub(crate) fn supply(&self) -> i128 {
        self.minted + self.runtime_minted as i128 - self.burnt as i128
    }
}

/// The changes of state its contract made when `key` goes from `old` to
/// `new`, by name, if any: see `Log::note_writes`. A storage key shows as
/// text when printable, else in hex, cut at 32 characters.
fn changed(key: &TrieKey, old: Option<&[u8]>, new: Option<&[u8]>) -> Vec<String> {
    let what = match (key, old, new) {
        _ if old == new => return vec![],
        (TrieKey::Account { .. }, Some(old), Some(new)) => {
            let [old, new] = [old, new].map(|v| Account::try_from_slice(v).expect("an account"));
            let fields = [
                (new.amount() < old.amount(), "balance"),
                (new.locked() != old.locked(), "locked balance"),
                (new.contract() != old.contract(), "contract"),
            ];
            return fields.into_iter().filter(|f| f.0).map(|f| f.1.to_string()).collect();
        }
        (TrieKey::Account { .. }, None, _) => "account created".into(),
        (TrieKey::Account { .. }, ..) => "account deleted".into(),
        (TrieKey::ContractCode { .. }, ..) => "code".into(),
        (TrieKey::ContractData { key, .. }, ..) => match std::str::from_utf8(key) {
            Ok(s) if !s.contains(char::is_control) => format!("storage {:?}", cut(s)),
            _ => format!(
                "storage 0x{}",
                cut(key.iter().map(|b| format!("{b:02x}")).collect::<String>())
            ),
        },
        (TrieKey::AccessKey { key_handle, .. }, ..) => format!("access key {}", cut(key_handle)),
        (TrieKey::GasKeyNonce { key_handle, index, .. }, ..) => {
            format!("gas key nonce {index} of {}", cut(key_handle))
        }
        _ => unreachable!("note_writes passes only these keys"),
    };
    vec![what]
}

/// `s`'s first 32 characters, and `…` if there were more.
fn cut(s: impl ToString) -> String {
    let s = s.to_string();
    let end = s.char_indices().nth(32).map_or(s.len(), |(i, _)| i);
    format!("{}{}", &s[..end], if end < s.len() { "…" } else { "" })
}

/// nearcore makes the refunded account a gas refund's signer, where a balance
/// refund's is `system` (`Receipt::new_gas_refund`, `new_balance_refund`).
pub(crate) fn is_gas_refund(r: &Receipt) -> bool {
    r.predecessor_id().is_system()
        && action_receipt(r).is_some_and(|a| a.signer_id() == r.receiver_id())
}

/// An action receipt's contents, a parked yield's included; `None` for a
/// data receipt.
pub(crate) fn action_receipt(r: &Receipt) -> Option<VersionedActionReceipt<'_>> {
    match r.versioned_receipt() {
        VersionedReceiptEnum::Action(a) | VersionedReceiptEnum::PromiseYield(a) => Some(a),
        _ => None,
    }
}

/// The function calls among `actions`, in order.
pub(crate) fn calls(actions: &[Action]) -> impl Iterator<Item = &FunctionCallAction> {
    actions.iter().filter_map(|a| match a {
        Action::FunctionCall(fc) => Some(&**fc),
        _ => None,
    })
}

pub(crate) fn method_names(actions: &[Action]) -> Vec<String> {
    calls(actions).map(|c| c.method_name.clone()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use near_primitives::types::{AccountId, Balance, RawStateChange};

    #[test]
    fn ledger_arithmetic() {
        let stats = BalanceStatsV1 {
            tx_burnt_amount: Balance::from_yoctonear(1),
            slashed_burnt_amount: Balance::from_yoctonear(10),
            other_burnt_amount: Balance::from_yoctonear(100),
            gas_deficit_amount: Balance::from_yoctonear(1_000),
            subsidized_amount: Balance::from_yoctonear(10_000),
        };
        let mut log = Log::default();
        log.on_balance(&stats);
        log.on_balance(&stats);
        assert_eq!((log.burnt, log.runtime_minted), (222, 22_000));
        log.minted = 3;
        assert_eq!(log.supply(), 3 + 22_000 - 222);
    }

    fn account(amount: u128, locked: u128, code: u8) -> Vec<u8> {
        use near_primitives::account::AccountContract;
        let contract = match code {
            0 => AccountContract::None,
            n => AccountContract::Local(CryptoHash([n; 32])),
        };
        let (amount, locked) = (Balance::from_yoctonear(amount), Balance::from_yoctonear(locked));
        borsh::to_vec(&Account::new(amount, locked, contract, 100)).unwrap()
    }

    /// An account counts when it appears or goes, or its contract, locked
    /// balance or a falling balance changes; not when its balance rises.
    #[test]
    fn account_changes_its_contract_made() {
        let key = TrieKey::Account { account_id: "a.near".parse().unwrap() };
        let ch = |old: &[u8], new: &[u8]| !changed(&key, Some(old), Some(new)).is_empty();
        let a = account(10, 0, 0);
        assert!(!ch(&a, &a));
        assert!(!ch(&a, &account(11, 0, 0)));
        assert!(ch(&a, &account(9, 0, 0)));
        assert!(ch(&a, &account(10, 1, 0)));
        assert!(ch(&a, &account(10, 0, 1)));
        assert_eq!(changed(&key, None, Some(&a)), ["account created"]);
        assert_eq!(changed(&key, Some(&a), None), ["account deleted"]);
        assert_eq!(
            changed(&key, Some(&a), Some(&account(9, 1, 1))),
            ["balance", "locked balance", "contract"]
        );
    }

    /// A storage key shows as text when printable, else in hex, and both
    /// are cut at 32 characters.
    #[test]
    fn storage_keys_by_name() {
        let ch = |key: &[u8]| {
            let key =
                TrieKey::ContractData { account_id: "a.near".parse().unwrap(), key: key.to_vec() };
            changed(&key, None, Some(b"v")).join("")
        };
        assert_eq!(ch(b"STATE"), r#"storage "STATE""#);
        assert_eq!(ch(&[b'm', 0, 1]), "storage 0x6d0001");
        assert_eq!(ch(&[b'x'; 40]), format!("storage \"{}…\"", "x".repeat(32)));
        assert_eq!(ch(&[0xab; 40]), format!("storage 0x{}…", "ab".repeat(16)));
    }

    /// `wrote` names four changes, then counts the rest.
    #[test]
    fn wrote_names_four_then_counts() {
        let mut log = Log::default();
        let (r1, r2) = (CryptoHash([1; 32]), CryptoHash([2; 32]));
        let data = |r, k: &str| RawStateChangesWithTrieKey {
            trie_key: TrieKey::ContractData {
                account_id: "a.near".parse().unwrap(),
                key: k.into(),
            },
            changes: vec![RawStateChange {
                cause: StateChangeCause::ReceiptProcessing { receipt_hash: r },
                data: Some(b"v".to_vec()),
            }],
        };
        let keys = |r, n| (0..n).map(move |i| data(r, &i.to_string()));
        log.note_writes(&keys(r1, 4).chain(keys(r2, 6)).collect::<Vec<_>>(), |_| None);
        assert_eq!(log.wrote[&r2].0.len(), 4);
        assert_eq!(
            log.wrote(&r1).as_deref(),
            Some(r#"storage "0", storage "1", storage "2", storage "3""#)
        );
        assert_eq!(
            log.wrote(&r2).as_deref(),
            Some(r#"storage "0", storage "1", storage "2", storage "3" and 2 more"#)
        );
    }

    /// Each receipt's change compares with the value before it: the second
    /// write of the same value changes nothing, and only receipts count.
    #[test]
    fn note_writes_compares_with_the_previous_value() {
        let account_id: AccountId = "a.near".parse().unwrap();
        let (r1, r2, r3) = (CryptoHash([1; 32]), CryptoHash([2; 32]), CryptoHash([3; 32]));
        let receipt = |r, v: &[u8]| RawStateChange {
            cause: StateChangeCause::ReceiptProcessing { receipt_hash: r },
            data: Some(v.to_vec()),
        };
        let data = RawStateChangesWithTrieKey {
            trie_key: TrieKey::ContractData { account_id: account_id.clone(), key: b"k".to_vec() },
            changes: vec![
                receipt(r1, b"v"),
                receipt(r2, b"v"),
                RawStateChange { cause: StateChangeCause::InitialState, data: Some(b"w".to_vec()) },
                receipt(r3, b"w"),
            ],
        };
        let queue = RawStateChangesWithTrieKey {
            trie_key: TrieKey::DelayedReceiptIndices,
            changes: vec![receipt(r2, b"x")],
        };
        let mut log = Log::default();
        log.note_writes(&[data, queue], |_| None);
        assert_eq!(log.wrote, HashMap::from([(r1, (vec![r#"storage "k""#.to_string()], 1))]));
    }
}
