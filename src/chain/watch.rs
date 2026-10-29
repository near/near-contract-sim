//! Per-receipt token deltas for watched tokens ([`Sim::watch`](crate::Sim::watch)).
//!
//! After `Runtime::apply` and before its changes are committed, each receipt
//! that ran on a watched token and changed its contract storage is measured:
//! the token's balance view (per holder) and supply view run on the state
//! just before the receipt and just after it, both in the block's own
//! context, so a view that depends on the clock attributes no drift to the
//! receipt. Both states are the block's pre-state with the token account's
//! changes, in execution order, up to that receipt. The views write nothing
//! and burn no gas the chain counts: a watch changes no state, outcome or
//! receipt.
//!
//! It is exact because a receipt changes only its own account's state, and
//! a view reads only its own account's.
//!
//! Holders are every account in state, the ones a contract created too,
//! plus the declared ones (ids that aren't accounts) and, for a
//! multi-token id, those its standard events named with that id. Within a
//! block, a receipt's "before" is the previous one's "after" when the
//! token's state between them is the same. A pure token's ([`Token::pure`])
//! last measurement also carries over to the next block that starts on the
//! state root it was taken on, so a direct write in between drops it.
//!
//! Cost: (holders + 1 with a supply view) views after each writing
//! receipt, and as many before it unless reused.

use super::{Chain, Config, Log, VIEWER, ViewAt};
use crate::effects::{Delta, Token, TokenKey, TokenMove, token_label};
use crate::outcome::ReceiptOutcome;
use borsh::{BorshDeserialize, BorshSerialize};
use near_primitives::hash::CryptoHash;
use near_primitives::trie_key::trie_key_parsers::parse_account_id_from_account_key;
use near_primitives::trie_key::{TrieKey, col};
use near_primitives::types::{
    AccountId, RawStateChange, RawStateChangesWithTrieKey, StateChangeCause, StateRoot,
};
use near_store::trie::AccessOptions;
use near_store::trie::update::TrieUpdate;
use node_runtime::ApplyResult;
use std::collections::{BTreeSet, HashMap, HashSet};

/// The watched tokens and what they measured, kept in the log (so a saved
/// `Sim` keeps them).
#[derive(Clone, Default, BorshSerialize, BorshDeserialize)]
pub(crate) struct Watch {
    /// Each watched token, with the index of the first block it measured.
    tokens: Vec<(Token, usize)>,
    /// Each measured receipt's token moves, nonzero ones only.
    moves: HashMap<CryptoHash, Vec<TokenMove>>,
    /// Per receipt, the tokens whose moves it couldn't measure in full, and
    /// why.
    unobserved: HashMap<CryptoHash, Vec<(TokenKey, String)>>,
    /// Each pure token's last measurement ([`Token::pure`]), while it
    /// holds.
    cache: HashMap<TokenKey, Cached>,
    /// The multi-tokens whose ids standard events name are watched as
    /// they appear ([`Token::discover`]): one template per contract.
    discover: Vec<Token>,
    /// This block's (receipt, token id) pairs whose `mt_mint` first named
    /// the id: its views before that receipt read 0.
    minted: HashSet<(CryptoHash, String)>,
    /// The views it ran: its cost, which a unit test bounds.
    views: u64,
}

/// A pure token's views on the state with root `root`: they hold in any
/// later block that starts on that root.
#[derive(Clone, BorshSerialize, BorshDeserialize)]
struct Cached {
    root: StateRoot,
    holders: Vec<AccountId>,
    values: Vec<Measured>,
}

/// The token account's keys a block changed, as they stand at some point
/// in it: (index in the block's keys, value), for those whose value differs
/// from the block's pre-state.
type State<'a> = Vec<(usize, Option<&'a [u8]>)>;

impl Chain {
    /// Watches `token` from the next block on, replacing an earlier watch
    /// of the same contract (and token id).
    pub(crate) fn watch(&mut self, token: Token) {
        let w = &mut self.log.watch;
        if token.discover {
            w.discover.retain(|t| t.id != token.id);
            w.discover.push(Token { token_ids: vec![], ..token.clone() });
            if token.token_ids.is_empty() {
                return;
            }
        }
        for token in token.split() {
            w.tokens.retain(|(t, _)| t.key() != token.key());
            w.cache.remove(&token.key());
            w.tokens.push((token, self.log.blocks.len()));
        }
    }

    /// The block hook: notes the accounts contracts created, then measures
    /// each receipt that changed a watched token's storage. `prev` is the
    /// state root the block applied on; `res` is not committed yet, and is
    /// left as it is.
    pub(super) fn watch_block(&mut self, config: &Config, prev: StateRoot, res: &ApplyResult) {
        self.discover_block(res);
        if self.log.watch.tokens.is_empty() {
            return;
        }
        let pos: HashMap<CryptoHash, usize> =
            res.outcomes.iter().enumerate().map(|(i, o)| (o.id, i)).collect();
        let mut universe = None;
        let trie = self.trie_at(prev);
        for (token, _) in self.log.watch.tokens.clone() {
            let keys: Vec<&RawStateChangesWithTrieKey> = res
                .state_changes
                .iter()
                .filter(|c| owner(&c.trie_key) == Some(&token.id))
                .collect();
            let old: Vec<Option<Vec<u8>>> = keys
                .iter()
                .map(|c| trie.get(&c.trie_key.to_vec(), AccessOptions::DEFAULT).expect("storage"))
                .collect();
            let at = |upto| state_at(&keys, &old, &pos, upto);
            // What a view can read of the state: for a pure token, its
            // storage and code only.
            let same = |a: &State, b: &State| same(&keys, token.pure, a, b);
            let cached = self.log.watch.cache.remove(&token.key()).filter(|c| c.root == prev);
            let writers = writers(&keys, &old, &pos);
            if writers.is_empty() {
                // Its storage kept its values: a pure measurement still holds.
                if let Some(c) = cached.filter(|_| same(&at(usize::MAX), &vec![])) {
                    let c = Cached { root: res.state_root, ..c };
                    self.log.watch.cache.insert(token.key(), c);
                }
                continue;
            }
            let universe = universe.get_or_insert_with(|| self.universe(prev, res));
            let mut holders: Vec<AccountId> = universe.clone();
            holders.extend(token.holders.iter().filter(|h| !universe.contains(h)).cloned());
            let mut last = cached.filter(|c| c.holders == holders).map(|c| (vec![], c.values));
            for (r, p) in writers {
                let before = at(p);
                let mut before = match last.take() {
                    Some((state, values)) if same(&state, &before) => values,
                    _ => {
                        let values = self.measure(config, prev, &keys, &before, &token, &holders);
                        self.log.watch.views += values.len() as u64;
                        values
                    }
                };
                // An id this receipt's own `mt_mint` first named: nothing
                // of it existed before, whatever the views say there.
                let minted = (token.token_id())
                    .is_some_and(|id| self.log.watch.minted.contains(&(r, id.clone())));
                if minted {
                    before.iter_mut().filter(|b| b.is_err()).for_each(|b| *b = Ok(0));
                }
                let after_state = at(p + 1);
                let after = self.measure(config, prev, &keys, &after_state, &token, &holders);
                self.log.watch.views += after.len() as u64;
                self.record(r, &token, &holders, &before, &after);
                last = Some((after_state, after));
            }
            if let Some((state, values)) = last.filter(|_| token.pure)
                && same(&state, &at(usize::MAX))
            {
                let c = Cached { root: res.state_root, holders, values };
                self.log.watch.cache.insert(token.key(), c);
            }
        }
    }

    /// What this block's standard events tell the watch: each discovering
    /// multi-token's ids they name, watched from this block on, the ids an
    /// `mt_mint` first named (by receipt), and the holders they name, for
    /// every watched token of the contract that logged them.
    fn discover_block(&mut self, res: &ApplyResult) {
        let w = &mut self.log.watch;
        w.minted.clear();
        if w.discover.is_empty() && w.tokens.is_empty() {
            return;
        }
        let since = self.log.blocks.len();
        for o in &res.outcomes {
            let contract = &o.outcome.executor_id;
            for e in o.outcome.logs.iter().filter_map(|l| standard_event(l)) {
                let template = w.discover.iter().find(|t| &t.id == contract);
                if let Some(t) = template.filter(|_| e.standard == "nep245") {
                    for id in &e.token_ids {
                        let key = (contract.clone(), Some(id.clone()));
                        if !w.tokens.iter().any(|(x, _)| x.key() == key) {
                            let token = Token { token_ids: vec![id.clone()], ..t.clone() };
                            w.tokens.push((token, since));
                            if e.event == "mt_mint" {
                                w.minted.insert((o.id, id.clone()));
                            }
                        }
                    }
                }
                // A holder joins the ids its event names (all of the
                // contract's, if it names none): measuring every holder
                // for every id costs ids x holders views per write.
                let named = |t: &Token| {
                    e.token_ids.is_empty() || t.token_id().is_none_or(|id| e.token_ids.contains(id))
                };
                for (t, _) in w.tokens.iter_mut().filter(|(t, _)| &t.id == contract && named(t)) {
                    for h in &e.holders {
                        if !t.holders.contains(h) {
                            t.holders.push(h.clone());
                        }
                    }
                }
            }
        }
    }

    /// Every account in state before the block or created in it, sorted:
    /// the ones contracts created too.
    fn universe(&self, prev: StateRoot, res: &ApplyResult) -> Vec<AccountId> {
        let mut ids = BTreeSet::new();
        let trie = self.trie_at(prev);
        let mut it = trie.disk_iter().expect("storage");
        it.seek_prefix([col::ACCOUNT]).expect("storage");
        for item in it {
            let (k, _) = item.expect("storage");
            ids.insert(parse_account_id_from_account_key(&k).expect("an account key"));
        }
        for c in &res.state_changes {
            if let TrieKey::Account { account_id } = &c.trie_key {
                ids.insert(account_id.clone());
            }
        }
        ids.into_iter().collect()
    }

    /// Each holder's balance, then the supply if declared, on the block's
    /// pre-state with `state` (the token account's keys as they stand at
    /// some point of the block) applied.
    fn measure(
        &self,
        config: &Config,
        prev: StateRoot,
        keys: &[&RawStateChangesWithTrieKey],
        state: &State,
        token: &Token,
        holders: &[AccountId],
    ) -> Vec<Measured> {
        let view = |method: &str, args: Vec<u8>| {
            let mut tu = TrieUpdate::new(self.trie_at(prev));
            for &(i, v) in state {
                let key = keys[i].trie_key.clone();
                match v {
                    Some(v) => tu.set(key, v.to_vec()),
                    None => tu.remove(key),
                }
            }
            let call = format!("{}.{method}({})", token.id, String::from_utf8_lossy(&args));
            let call = match token.token_id() {
                Some(_) => format!("{call} [{}]", token_label(&token.key())),
                None => call,
            };
            self.view_in_block(config, tu, &token.id, method, &args)
                .and_then(|v| amount(&v))
                .map_err(|e| format!("{call} {e}"))
        };
        let args = |holder: Option<&AccountId>| token_args(token, holder);
        let mut values: Vec<Measured> =
            holders.iter().map(|h| view(&token.balance, args(Some(h)))).collect();
        if let Some(s) = &token.supply {
            values.push(view(s, args(None)));
        }
        values
    }

    /// Runs view `method` on `tu`, at the block being produced.
    fn view_in_block(
        &self,
        config: &Config,
        tu: TrieUpdate,
        contract: &AccountId,
        method: &str,
        args: &[u8],
    ) -> Result<Vec<u8>, String> {
        let at = ViewAt {
            height: self.height,
            timestamp_ns: self.timestamp_ns,
            protocol_version: config.protocol_version,
            root: self.root,
        };
        let (result, _) = self.run_view(config, &VIEWER, tu, at, (contract, method, args));
        result.map_err(|e| format!("failed: {e}"))
    }

    /// Stores receipt `r`'s moves of `token`, and why they're unobserved if
    /// a view failed or the holders' sum misses the supply's change.
    fn record(
        &mut self,
        r: CryptoHash,
        token: &Token,
        holders: &[AccountId],
        before: &[Measured],
        after: &[Measured],
    ) {
        let mut moves = vec![];
        let mut failed = None;
        let who = holders.iter().map(Some).chain(token.supply.as_ref().map(|_| None));
        for ((holder, b), a) in who.zip(before).zip(after) {
            match (b, a) {
                (Ok(b), Ok(a)) if a != b => moves.push(TokenMove {
                    token: token.id.clone(),
                    token_id: token.token_id().cloned(),
                    holder: holder.cloned(),
                    delta: Delta::between(*b, *a),
                    receipt_id: r,
                }),
                (Ok(_), Ok(_)) => {}
                (Err(e), _) | (_, Err(e)) => {
                    failed.get_or_insert_with(|| format!("receipt {r}: {e}"));
                }
            }
        }
        let why = failed.or_else(|| {
            token.supply.as_ref()?;
            // Checked: a token whose balances sum past u128 says so rather
            // than crash the block.
            let sum = |m: &[TokenMove], holder: bool| -> Option<Delta> {
                let mut ds = m.iter().filter(|m| m.holder.is_some() == holder).map(|m| m.delta);
                ds.try_fold(Delta::ZERO, Delta::checked_add)
            };
            let label = token_label(&token.key());
            let (Some(held), Some(supply)) = (sum(&moves, true), sum(&moves, false)) else {
                return Some(format!(
                    "receipt {r} changed the watched holders' {label} by more than u128 holds"
                ));
            };
            (held != supply).then(|| {
                let missing =
                    supply.checked_add(-held).map_or("more than u128".into(), |d| d.to_string());
                format!(
                    "receipt {r} changed the watched holders' {label} by {held} and the supply \
                     by {supply}: {missing} moved to or from holders not watched (an id that \
                     isn't an account?); add them with Token::holders",
                )
            })
        });
        let w = &mut self.log.watch;
        if !moves.is_empty() {
            w.moves.entry(r).or_default().extend(moves);
        }
        if let Some(why) = why {
            w.unobserved.entry(r).or_default().push((token.key(), why));
        }
    }
}

/// A standard event a receipt logged (`EVENT_JSON:`): its standard, its
/// name, the token ids and the holders (`owner_id`, `old_owner_id`,
/// `new_owner_id`) its data names.
struct StandardEvent {
    standard: String,
    event: String,
    token_ids: Vec<String>,
    holders: Vec<AccountId>,
}

fn standard_event(log: &str) -> Option<StandardEvent> {
    let v: serde_json::Value = serde_json::from_str(log.strip_prefix("EVENT_JSON:")?).ok()?;
    let text = |k: &str| v.get(k).and_then(|s| s.as_str()).map(str::to_owned);
    let mut e = StandardEvent {
        standard: text("standard")?,
        event: text("event")?,
        token_ids: vec![],
        holders: vec![],
    };
    for d in v.get("data").and_then(|d| d.as_array()).into_iter().flatten() {
        let ids = d.get("token_ids").and_then(|t| t.as_array()).into_iter().flatten();
        for id in ids.filter_map(|i| i.as_str()) {
            if !e.token_ids.iter().any(|x| x == id) {
                e.token_ids.push(id.to_owned());
            }
        }
        for k in ["owner_id", "old_owner_id", "new_owner_id"] {
            let id = d.get(k).and_then(|s| s.as_str()).and_then(|s| s.parse::<AccountId>().ok());
            if let Some(id) = id.filter(|id| !e.holders.contains(id)) {
                e.holders.push(id);
            }
        }
    }
    Some(e)
}

/// A view's amount, or why there is none.
type Measured = Result<u128, String>;

impl Log {
    /// The views the watch has run: its cost. Each receipt that writes a
    /// watched token's storage costs (holders + 1 with a supply view) views
    /// after it, and as many before it unless reused.
    #[cfg(test)]
    pub(crate) fn watch_views(&self) -> u64 {
        self.watch.views
    }

    /// The token moves receipt `id` made, as measured.
    pub(crate) fn token_moves(&self, id: &CryptoHash) -> Vec<TokenMove> {
        self.watch.moves.get(id).cloned().unwrap_or_default()
    }

    /// For each watched token, `None` if `receipts` (one transaction's
    /// that ran) were all measured, or why not: a receipt ran on it before
    /// it was watched, or one's moves are unobserved.
    /// Also the watched tokens with a supply view.
    pub(crate) fn token_verdicts(
        &self,
        receipts: &[ReceiptOutcome],
    ) -> (HashMap<TokenKey, Option<String>>, HashSet<TokenKey>) {
        let w = &self.watch;
        let supplied = w.tokens.iter().filter(|(t, _)| t.supply.is_some()).map(|(t, _)| t.key());
        let verdicts = w
            .tokens
            .iter()
            .map(|(t, since)| {
                let early = receipts.iter().find(|r| {
                    r.receiver_id == t.id
                        && self.position(&r.receipt_id).is_some_and(|(b, _)| b < *since)
                });
                let why = match early {
                    Some(r) => Some(format!(
                        "{} is watched from block #{} on, and receipt {} ran on it before, at \
                         #{}",
                        token_label(&t.key()),
                        self.blocks.get(*since).map_or("(next)".into(), |b| b.height.to_string()),
                        r.receipt_id,
                        r.block_height,
                    )),
                    None => receipts.iter().find_map(|r| {
                        let u = w.unobserved.get(&r.receipt_id)?;
                        u.iter().find(|(id, _)| *id == t.key()).map(|(_, why)| why.clone())
                    }),
                };
                (t.key(), why)
            })
            .collect();
        (verdicts, supplied.collect())
    }
}

/// Whether states `a` and `b` agree on what the token's views can read:
/// for a pure token, its storage and code only.
fn same(keys: &[&RawStateChangesWithTrieKey], pure: bool, a: &State, b: &State) -> bool {
    let read = |s: &State| -> Vec<(usize, Option<Vec<u8>>)> {
        let account = |i: usize| matches!(keys[i].trie_key, TrieKey::Account { .. });
        s.iter()
            .filter(|(i, _)| !(pure && account(*i)))
            .map(|(i, v)| (*i, v.map(<[u8]>::to_vec)))
            .collect()
    };
    read(a) == read(b)
}

/// The receipts that changed a value of the token's storage, with their
/// positions in the block, in execution order. `keys` are the token
/// account's changed keys, `old` their values before the block.
fn writers(
    keys: &[&RawStateChangesWithTrieKey],
    old: &[Option<Vec<u8>>],
    pos: &HashMap<CryptoHash, usize>,
) -> Vec<(CryptoHash, usize)> {
    let mut writers = BTreeSet::new();
    for (c, old) in keys.iter().zip(old) {
        if !matches!(c.trie_key, TrieKey::ContractData { .. }) {
            continue;
        }
        let mut was = old.as_deref();
        for ch in &c.changes {
            if let StateChangeCause::ReceiptProcessing { receipt_hash } = ch.cause
                && let Some(&p) = pos.get(&receipt_hash)
                && was != ch.data.as_deref()
            {
                writers.insert((p, receipt_hash));
            }
            was = ch.data.as_deref();
        }
    }
    writers.into_iter().map(|(p, r)| (r, p)).collect()
}

/// The account whose state `key` is, for the keys a view can read: its
/// account record, code and contract storage.
fn owner(key: &TrieKey) -> Option<&AccountId> {
    match key {
        TrieKey::Account { account_id }
        | TrieKey::ContractCode { account_id }
        | TrieKey::ContractData { account_id, .. } => Some(account_id),
        _ => None,
    }
}

/// The token account's state by the time the receipt at position `upto`
/// runs (so before it): each of `keys` whose value then differs from `old`,
/// its value before the block. Changes come in execution order per key; a
/// change from a receipt at `upto` or later stops it, and changes with no
/// receipt apply where they fall.
fn state_at<'a>(
    keys: &[&'a RawStateChangesWithTrieKey],
    old: &'a [Option<Vec<u8>>],
    pos: &HashMap<CryptoHash, usize>,
    upto: usize,
) -> State<'a> {
    let mut state = vec![];
    for (i, (c, old)) in keys.iter().zip(old).enumerate() {
        let mut value = old.as_deref();
        let mut changed = false;
        for ch in &c.changes {
            if receipt_of(ch).and_then(|r| pos.get(&r)).is_some_and(|&p| p >= upto) {
                break;
            }
            value = ch.data.as_deref();
            changed = true;
        }
        if changed && value != old.as_deref() {
            state.push((i, value));
        }
    }
    state
}

fn receipt_of(ch: &RawStateChange) -> Option<CryptoHash> {
    match ch.cause {
        StateChangeCause::ReceiptProcessing { receipt_hash }
        | StateChangeCause::ActionReceiptProcessingStarted { receipt_hash }
        | StateChangeCause::ActionReceiptGasReward { receipt_hash }
        | StateChangeCause::PostponedReceipt { receipt_hash } => Some(receipt_hash),
        StateChangeCause::TransactionProcessing { tx_hash } => Some(tx_hash),
        _ => None,
    }
}

/// `token`'s view arguments: the holder's, if any, and a multi-token's
/// token id.
fn token_args(token: &Token, holder: Option<&AccountId>) -> Vec<u8> {
    let mut args = serde_json::Map::new();
    if let Some(h) = holder {
        args.insert(token.arg.clone(), h.to_string().into());
    }
    if let Some(id) = token.token_id() {
        args.insert("token_id".into(), id.clone().into());
    }
    serde_json::to_vec(&args).expect("json")
}

/// A view's result as an amount: a JSON string of digits (near-sdk's
/// `U128`) or a JSON number.
fn amount(v: &[u8]) -> Result<u128, String> {
    let parsed = match serde_json::from_slice::<serde_json::Value>(v) {
        Ok(serde_json::Value::String(s)) => s.parse().ok(),
        Ok(serde_json::Value::Number(n)) => n.to_string().parse().ok(),
        _ => None,
    };
    parsed.ok_or_else(|| format!("returned {}, not an amount", String::from_utf8_lossy(v)))
}
