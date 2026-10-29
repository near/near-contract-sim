//! The simulator: the `Sim` handle over the private chain core.

use crate::chain::{Chain, Config, VIEWER, action_receipt, calls, method_names, yield_timeout};
use near_crypto::{KeyType, PublicKey, PublicKeyHandle, SecretKey};
use near_primitives::account::{AccessKey, Account as NearAccount, AccountContract};
use near_primitives::action::{Action, GlobalContractIdentifier};
use near_primitives::hash::CryptoHash;
use near_primitives::state_record::{StateRecord, state_record_to_account_id};
use near_primitives::trie_key::{TrieKey, col};
use near_primitives::types::{
    AccountId, Balance, BlockHeight, Gas, Nonce, NonceIndex, PromiseYieldStatus,
};
use near_primitives::version::ProtocolFeature;
use near_primitives::views::AccountView;
use near_store::TrieAccess;
use near_store::trie::AccessOptions;
use near_vm_runner::ContractCode;
use node_runtime::state_viewer::errors::ViewStateError;
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::time::Duration;

/// 2020-09-13T12:26:40Z: deterministic, never the wall clock.
pub(crate) const GENESIS_TIMESTAMP_NS: u64 = 1_600_000_000_000_000_000;
/// How many pending receipts the `settle` panic lists.
const LISTED: usize = 20;

/// Anything that names an account: any `AsRef<str>`, near-sdk's
/// `AccountId` included. An invalid account id panics.
pub trait IntoAccountId {
    /// The account id; panics if it isn't a valid one.
    fn into_account_id(self) -> AccountId;
}
impl<T: AsRef<str>> IntoAccountId for T {
    #[track_caller]
    fn into_account_id(self) -> AccountId {
        let s = self.as_ref();
        match s.parse() {
            Ok(id) => id,
            Err(e) => panic!("invalid account id {s:?}: {e}"),
        }
    }
}

/// A yielded promise still waiting for `promise_yield_resume` or timeout.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingYield {
    /// The account that yielded, whose callback waits to run on it.
    pub receiver_id: AccountId,
    /// What `promise_yield_resume` takes.
    pub data_id: CryptoHash,
    /// The block that fires the timeout, 200 after the yield
    /// (`yield_timeout_length_in_blocks`); `settle()` then runs the callback
    /// with `PromiseResult::Failed` in the next block. A full chunk can
    /// defer it, and after a `set_height` rewind it can queue behind a later
    /// expiry. A `promise_yield_resume` that runs before the timeout's resume
    /// receipt still wins (nearcore drops the timeout): a resume submitted
    /// while the yield is listed wins, one after the timeout fired loses.
    pub expires_at: u64,
    /// The methods the callback receipt calls.
    pub callback_methods: Vec<String>,
    /// Each call's arguments, as `promise_yield_create` was given them.
    pub callback_args: Vec<Vec<u8>>,
    /// The parked callback receipt, its id in `sim.outcome(y.tx)` once it
    /// runs.
    pub receipt_id: CryptoHash,
    /// The transaction whose receipts created the yield.
    pub tx: CryptoHash,
}

/// In-process NEAR chain (single shard) running nearcore's own runtime.
///
/// `Clone` is a cheap snapshot: the clone shares the insert-only trie store
/// and copies the rest, so the two evolve independently.
///
/// ```
/// use near_contract_sim::prelude::*;
/// use serde_json::json;
///
/// let mut sim = Sim::new();
/// sim.create_account("alice.near", Balance::from_near(10));
/// // near-sdk's fungible-token example; a test reads its own contract's wasm.
/// sim.deploy("token.near", std::fs::read("tests/res/fungible_token.wasm").unwrap());
/// let init = json!({ "owner_id": "alice.near", "total_supply": "1000" });
/// let out = sim.call("token.near", "token.near", "new_default_meta").args_json(init).transact();
/// out.assert_success();
/// let balance = sim.view("token.near", "ft_balance_of", json!({ "account_id": "alice.near" }));
/// assert_eq!(balance.u128(), 1000);
/// sim.findings().assert_none();
/// ```
///
/// # Methods by task
///
/// - **Calls, views and deploys:** [`call`](Self::call), [`view`](Self::view),
///   [`deploy`](Self::deploy), [`tx`](Self::tx) for any actions,
///   [`view_raw`](Self::view_raw), [`outcome`](Self::outcome) of a submitted
///   transaction, [`txs`](Self::txs).
/// - **Accounts and keys:** [`create_account`](Self::create_account),
///   [`create_subaccount`](Self::create_subaccount),
///   [`create_account_with_key`](Self::create_account_with_key),
///   [`account`](Self::account), [`balance`](Self::balance),
///   [`set_balance`](Self::set_balance), [`rewarded`](Self::rewarded),
///   [`secret_key`](Self::secret_key), [`set_key`](Self::set_key),
///   [`access_key`](Self::access_key), [`access_keys`](Self::access_keys),
///   [`gas_key_nonces`](Self::gas_key_nonces).
/// - **Contract state:** [`contract_state`](Self::contract_state),
///   [`update_contract_state`](Self::update_contract_state),
///   [`state`](Self::state), [`view_state`](Self::view_state),
///   [`set_state`](Self::set_state), [`state_records`](Self::state_records),
///   [`apply_state_records`](Self::apply_state_records),
///   [`global_contract_code`](Self::global_contract_code),
///   [`state_root`](Self::state_root).
/// - **Blocks and the clock:** [`step`](Self::step), [`settle`](Self::settle),
///   [`run_until`](Self::run_until), [`is_idle`](Self::is_idle),
///   [`advance_to`](Self::advance_to), [`advance_by`](Self::advance_by),
///   [`advance_epochs`](Self::advance_epochs),
///   [`set_timestamp_ns`](Self::set_timestamp_ns),
///   [`set_height`](Self::set_height), [`pause_clock_at`](Self::pause_clock_at),
///   [`resume_clock`](Self::resume_clock), [`height`](Self::height),
///   [`timestamp_ns`](Self::timestamp_ns), [`epoch_height`](Self::epoch_height),
///   [`epoch_of`](Self::epoch_of), [`epoch_start`](Self::epoch_start).
/// - **Yields and pending work:** [`yields`](Self::yields),
///   [`expire`](Self::expire), [`pending_receipts`](Self::pending_receipts),
///   [`receipts_in`](Self::receipts_in).
/// - **Checks and tokens:** [`findings`](Self::findings),
///   [`watch`](Self::watch).
/// - **Stubs:** [`deploy_stub`](Self::deploy_stub),
///   [`stub_reply`](Self::stub_reply), [`calls_to`](Self::calls_to).
/// - **Costs:** [`storage_cost`](Self::storage_cost),
///   [`global_storage_cost`](Self::global_storage_cost),
///   [`gas_ceiling`](Self::gas_ceiling).
/// - **Setup:** [`new`](Self::new), [`cached`](Self::cached), and
///   [`Fixture`](crate::Fixture) for a setup built once.
///
/// Reads of a missing account, and of a missing item on one that exists:
///
/// | Read | Missing account | Missing item |
/// |---|---|---|
/// | [`account`](Self::account) | `None` | |
/// | [`balance`](Self::balance) | panics | |
/// | [`view_state`](Self::view_state) | panics | empty |
/// | [`state`](Self::state) | panics | `None` |
/// | [`secret_key`](Self::secret_key) | panics | panics |
/// | [`access_key`](Self::access_key), [`gas_key_nonces`](Self::gas_key_nonces) | `None` | `None` |
/// | [`access_keys`](Self::access_keys) | empty | empty |
/// | [`outcome`](Self::outcome) | | panics on an unknown hash |
#[derive(Clone)]
pub struct Sim {
    /// Its [`runtime_config`](Config::runtime_config): the fees and limits.
    pub config: Config,
    pub(crate) chain: Chain,
    pub(crate) keys: HashMap<AccountId, SecretKey>,
    /// Each delegate's nonce, held until it runs or expires.
    pub(crate) delegates: HashMap<(AccountId, PublicKey, Nonce), BlockHeight>,
    /// The `block_time` a pause saved; `None` while the clock runs.
    pub(crate) paused: Option<Duration>,
    /// The height and time the next transaction's first receipt runs at.
    pub(crate) pinned: Pinned,
    /// While an `explore::Op` submits: its plan step's gas and injections.
    pub(crate) explore: Option<Box<crate::explore::Hook>>,
    /// `account::method` of each view that succeeded, shared by clones.
    pub(crate) viewed: Viewed,
}

/// Methods that ran as views ([`Sim::viewed`]).
pub(crate) type Viewed = std::sync::Arc<std::sync::Mutex<std::collections::BTreeSet<String>>>;

impl AsRef<Sim> for Sim {
    fn as_ref(&self) -> &Sim {
        self
    }
}

impl AsMut<Sim> for Sim {
    fn as_mut(&mut self) -> &mut Sim {
        self
    }
}

/// The clock, the state root and the queue length.
impl fmt::Debug for Sim {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Sim")
            .field("height", &self.height())
            .field("timestamp_ns", &self.timestamp_ns())
            .field("state_root", &self.state_root())
            .field("queued", &self.chain.queued().count())
            .finish_non_exhaustive()
    }
}

impl Default for Sim {
    fn default() -> Self {
        Self::new()
    }
}

impl Sim {
    /// A chain at genesis (height 0, 2020-09-13T12:26:40Z), default
    /// `Config`, no accounts.
    pub fn new() -> Self {
        Sim {
            config: Config::default(),
            chain: Chain::new(0, GENESIS_TIMESTAMP_NS),
            keys: HashMap::new(),
            delegates: HashMap::new(),
            paused: None,
            pinned: Pinned::default(),
            explore: None,
            viewed: Default::default(),
        }
    }

    /// The last block's height, what a view sees. A transaction submitted
    /// now converts in `height() + 1`; its first receipt runs there if the
    /// signer is the receiver, else in `height() + 2`. After
    /// [`set_height`](Self::set_height), it runs at `height()` itself.
    pub fn height(&self) -> u64 {
        self.chain.height
    }

    /// The last block's epoch, 1 at genesis: `env::epoch_height()` in a
    /// view, `epoch_of(height())`. Each receipt runs in its own block's
    /// epoch, so a call submitted in an epoch's last two blocks runs in the
    /// next. Panics if `config.epoch_length` is under 3, or isn't the first
    /// block's.
    #[track_caller]
    pub fn epoch_height(&self) -> u64 {
        self.epoch_of(self.height())
    }

    /// The epoch of block `h`, what `env::epoch_height()` returns in a
    /// receipt that runs there (a [`ReceiptOutcome`](crate::ReceiptOutcome)'s
    /// `block_height`). Panics where [`epoch_height`](Self::epoch_height)
    /// does.
    #[track_caller]
    pub fn epoch_of(&self, h: u64) -> u64 {
        self.chain.epoch_height(&self.config, h)
    }

    /// The first block of epoch `e`, `(e - 1) * epoch_length + 1`. Panics
    /// on epoch 0, an overflow, and where `epoch_height` does.
    #[track_caller]
    pub fn epoch_start(&self, e: u64) -> u64 {
        self.chain.epoch_start(&self.config, e)
    }

    /// What one transaction may attach: `max_total_prepaid_gas` with
    /// `config.gas_limits` applied, 1 PGas by default. The chunk gas limit
    /// counts conversion gas, so doesn't lower it.
    pub fn gas_ceiling(&self) -> Gas {
        self.config.gas_ceiling()
    }

    /// What `bytes` of account storage lock (1e19 yocto a byte by default).
    pub fn storage_cost(&self, bytes: u64) -> Balance {
        let per_byte = self.config.runtime_config().storage_amount_per_byte();
        per_byte.saturating_mul(bytes.into())
    }

    /// What publishing `bytes` of global contract code burns, on top of
    /// the deploy's gas.
    pub fn global_storage_cost(&self, bytes: u64) -> Balance {
        let fees = &self.config.runtime_config().fees.storage_usage_config;
        fees.global_contract_storage_amount_per_byte.saturating_mul(bytes.into())
    }

    /// The last block's timestamp, what a view sees; each block is one
    /// `config.block_time` later. Height and time are independent.
    pub fn timestamp_ns(&self) -> u64 {
        self.chain.timestamp_ns
    }

    /// Pins the height: the next transaction's first receipt runs in block
    /// `h`, and a view sees `h` until then. No block is produced; submitting
    /// the transaction moves the last block back to `h - 1` (one to its own
    /// signer runs in the block it converts in) or `h - 2` (one to another
    /// account converts a block before). A block produced first spends the
    /// pin and follows `h`. Later receipts, a callback's say, run after `h`.
    /// The submit panics if the last block would be below genesis.
    ///
    /// In a relayed call the pin lands on the delegate's receipt, and the
    /// inner call runs a block later.
    ///
    /// Any `h` goes, rewinds included: ids stay unique (each descends from a
    /// transaction hash covering the last block's), but height rules follow
    /// `h`: a key added in a block above the one a later transaction
    /// converts in is past that block's nonce bound, its height times 1e6,
    /// so the transaction fails with `NonceTooLarge`. No
    /// epoch boundary runs twice: after a rewind a stake change counts in the
    /// furthest epoch reached, so a contract's cooldown can unlock before the
    /// stake frees. `sim.clone()` tries both sides of a boundary.
    ///
    /// Panics on a rewind while a receipt is held
    /// ([`Fault::Hold`](crate::Fault::Hold), `HoldUntil`): it would run below the
    /// receipts that went ahead of it. Call `settle()` first.
    #[track_caller]
    pub fn set_height(&mut self, h: u64) {
        assert!(
            h >= self.height() || !self.chain.is_holding(),
            "set_height({h}) rewinds from {} while a receipt is held (Fault::Hold): settle() first",
            self.height()
        );
        self.chain.height = h;
        self.pinned.height = true;
    }

    /// Pins the clock: the next transaction's first receipt runs in a block
    /// at `ns`, and a view sees `ns` until then. As
    /// [`set_height`](Self::set_height) moves the last block back, submitting
    /// it moves the last block's time back one or two `block_time`s; a block
    /// produced first spends the pin, one `block_time` after `ns`. The clock
    /// runs on from there; [`pause_clock_at`](Self::pause_clock_at) stops it.
    /// Any value goes, the past included: the runtime doesn't check
    /// monotonicity (the chain layer does). The submit panics if the last
    /// block's time would be below 0.
    pub fn set_timestamp_ns(&mut self, ns: u64) {
        self.chain.timestamp_ns = ns;
        self.pinned.time = true;
    }

    /// Queues a transaction with `submit`, then moves the last block back
    /// so that its first receipt runs at the pinned height and time: one
    /// block on if it runs where it converts (`local`), else two. Panics,
    /// before anything is queued or moves, if that is below genesis or time
    /// 0, or rewinds below a held receipt. A transaction already queued
    /// (`submit` queues nothing) leaves the pin.
    #[track_caller]
    pub(crate) fn submit_pinned(
        &mut self,
        local: bool,
        submit: impl FnOnce(&mut Chain) -> CryptoHash,
    ) -> CryptoHash {
        let Pinned { height, time } = self.pinned;
        let k: u64 = if local { 1 } else { 2 };
        let room = |what: &str, at: u64, need: u64, unit: &str| {
            at.checked_sub(need).unwrap_or_else(|| {
                let hint = match local {
                    true => String::new(),
                    false => ", or submit to the signer itself, which runs in the block it \
                              converts in"
                        .into(),
                };
                panic!(
                    "{what}({at}): the next transaction's first receipt runs {k} block(s) after \
                     the last, which would be below {unit}; pin {need} or more{hint}"
                )
            })
        };
        let h = match height {
            true => room("set_height", self.chain.height, k, "genesis"),
            false => self.chain.height,
        };
        let bt = self.config.block_time_ns();
        let t = match time {
            true => {
                room("set_timestamp_ns", self.chain.timestamp_ns, k.saturating_mul(bt), "time 0")
            }
            false => self.chain.timestamp_ns,
        };
        let produced = self.chain.log().blocks.last().map_or(0, |b| b.height);
        assert!(
            h >= produced || !self.chain.is_holding(),
            "set_height({}) rewinds below block {produced} while a receipt is held \
             (Fault::Hold): settle() first",
            self.chain.height
        );
        let queued = self.chain.queued().count();
        let hash = submit(&mut self.chain);
        if self.chain.queued().count() > queued {
            (self.chain.height, self.chain.timestamp_ns) = (h, t);
            self.pinned = Pinned::default();
        }
        hash
    }

    /// Applies block `height() + 1` with the transactions a chunk producer
    /// would take and every incoming receipt. Panics if the runtime drops a
    /// transaction (only when the chunk's tokens burnt overflow u128), or
    /// the height or timestamp overflows u64.
    pub fn step(&mut self) {
        self.pinned = Pinned::default();
        self.chain.produce_block(&self.config);
        self.release_delegates();
    }

    /// Produces blocks until [`is_idle`](Self::is_idle): an overdue yield
    /// timeout fires, one still ahead stays parked. It is
    /// [`run_until`](Self::run_until)`(config.max_blocks, Sim::is_idle)`,
    /// and panics where that returns [`Busy`], listing what is pending.
    #[track_caller]
    pub fn settle(&mut self) {
        self.settle_until(|_| false);
    }

    /// Produces blocks one at a time until `done` holds (checked before
    /// each block and after the last): `Ok` with how many it produced, else
    /// [`Busy`] after `max_blocks`. A test can stop on the exact block a
    /// predicate turns true; the chain stays usable either way.
    pub fn run_until(
        &mut self,
        max_blocks: u64,
        mut done: impl FnMut(&Sim) -> bool,
    ) -> Result<u64, Busy> {
        for n in 0..max_blocks {
            if done(self) {
                return Ok(n);
            }
            self.step();
        }
        if done(self) {
            return Ok(max_blocks);
        }
        Err(Busy { blocks: max_blocks, idle: self.is_idle() })
    }

    /// [`settle`](Self::settle), stopping early once `done`.
    #[track_caller]
    pub(crate) fn settle_until(&mut self, mut done: impl FnMut(&Sim) -> bool) {
        let max = self.config.max_blocks;
        if let Err(busy) = self.run_until(max, |sim| done(sim) || sim.is_idle()) {
            panic!("{}", self.busy_message(busy));
        }
    }

    /// What `settle` panics with: the budget, then what is still pending.
    fn busy_message(&self, busy: Busy) -> String {
        use std::fmt::Write;
        let mut m = format!(
            "not idle after {} blocks (config.max_blocks; a receipt loop?), at block {}",
            busy.blocks,
            self.height()
        );
        let queued = self.chain.queued().count();
        if queued > 0 {
            let _ = write!(m, "\n{queued} transaction(s) queued");
        }
        let pending = self.pending_receipts();
        if !pending.is_empty() {
            let _ = write!(m, "\n{} receipt(s) pending:", pending.len());
        }
        for p in pending.iter().take(LISTED) {
            let (id, tx) = (p.receipt_id, p.tx);
            let _ = write!(
                m,
                "\n  {} from {} (receipt {id}, tx {tx}): {}",
                p.label(),
                p.predecessor_id,
                p.state
            );
        }
        if pending.len() > LISTED {
            let _ = write!(m, "\n  ... and {} more", pending.len() - LISTED);
        }
        if self.chain.timeout_due().is_some_and(|e| e <= self.height()) {
            m.push_str("\nand a yield timeout overdue");
        }
        m
    }

    /// Produces blocks until block `h` is the last. An idle chain skips the
    /// heights where nothing can happen, producing only the block where a
    /// yield timeout is due and block `h`. Time moves by `block_time` a
    /// height, skipped or not, and the random seed derives from the height:
    /// only `state_root`, block hashes and later ids notice a skip. Panics if
    /// `h < height()`, after `config.max_blocks` blocks, and where
    /// [`epoch_height`](Self::epoch_height) does.
    #[track_caller]
    pub fn advance_to(&mut self, h: u64) {
        assert!(h >= self.height(), "advance_to({h}): block {} is already the last", self.height());
        if h == self.height() {
            return;
        }
        self.epoch_height();
        let ns = (h - self.height()).checked_mul(self.config.block_time_ns());
        ns.and_then(|t| t.checked_add(self.timestamp_ns()))
            .expect("advance_to: timestamp overflow");
        let max = self.config.max_blocks;
        for _ in 0..max {
            if self.height() == h {
                return;
            }
            if self.is_idle() {
                let land = self.chain.timeout_due().map_or(h, |e| e.min(h));
                let skipped = land - 1 - self.height();
                self.chain.timestamp_ns += skipped * self.config.block_time_ns();
                self.chain.height = land - 1;
            }
            self.step();
        }
        assert_eq!(
            self.height(),
            h,
            "not at block {h} after {max} blocks (config.max_blocks; a receipt loop?)"
        );
    }

    /// [`advance_to`](Self::advance_to) the first height at least `d` later
    /// (`d` rounded up to whole block times). Panics if the clock is paused
    /// or `block_time` is zero (no block moves it), on overflow, and where
    /// `advance_to` does.
    #[track_caller]
    pub fn advance_by(&mut self, d: Duration) {
        if d.is_zero() {
            return;
        }
        assert!(
            self.paused.is_none(),
            "advance_by({d:?}): the clock is paused (pause_clock_at); use advance_to, or \
             resume_clock() first"
        );
        let bt = self.config.block_time_ns();
        assert!(bt > 0, "advance_by({d:?}): config.block_time is zero, the clock is paused");
        let n = u64::try_from(d.as_nanos().div_ceil(bt.into())).ok();
        let h = n.and_then(|n| self.height().checked_add(n));
        self.advance_to(h.expect("advance_by: height overflow"));
    }

    /// [`advance_to`](Self::advance_to) the first block of epoch `e + n`, `e`
    /// the last block's: `n` boundaries, each returning the stake due and
    /// paying `config.epoch_reward` (an idle chain applies them all in the
    /// block it lands on). Panics where `advance_to` does.
    #[track_caller]
    pub fn advance_epochs(&mut self, n: u64) {
        if n == 0 {
            return;
        }
        let e = self.epoch_height().checked_add(n);
        self.advance_to(self.epoch_start(e.expect("advance_epochs: epoch overflow")));
    }

    /// Stops the clock at `ns`: every block has time `ns` until
    /// [`resume_clock`](Self::resume_clock); calling it again moves the
    /// stopped clock. Heights go on ([`advance_by`](Self::advance_by)
    /// panics). The first pause saves `config.block_time` and zeroes it.
    pub fn pause_clock_at(&mut self, ns: u64) {
        if self.paused.is_none() {
            self.paused = Some(self.config.block_time);
        }
        self.config.block_time = Duration::ZERO;
        self.chain.timestamp_ns = ns;
    }

    /// Starts the clock again from where it stands, restoring the saved
    /// `block_time`; a no-op unless paused.
    pub fn resume_clock(&mut self) {
        if let Some(bt) = self.paused.take() {
            self.config.block_time = bt;
        }
    }

    /// Times out yield `y` now: `advance_to(y.expires_at)`, then `settle()`,
    /// and `outcome(y.tx)`, its callback run with `PromiseResult::Failed`.
    /// Yields due earlier fire on the way.
    ///
    /// Panics if `y` isn't in [`yields()`](Self::yields) (resumed or timed
    /// out), if it is still listed after settling (a rewind queued it behind
    /// a later expiry), and where [`settle`](Self::settle) does.
    #[track_caller]
    pub fn expire(&mut self, y: &PendingYield) -> crate::TxOutcome {
        let listed = |sim: &Sim| sim.yields().iter().any(|p| p.data_id == y.data_id);
        assert!(
            listed(self),
            "expire: the yield of {} on {} (data_id {}) is not parked: resumed or timed out already",
            y.callback_methods.join(","),
            y.receiver_id,
            y.data_id
        );
        if y.expires_at > self.height() {
            self.advance_to(y.expires_at);
        }
        self.settle();
        assert!(
            !listed(self),
            "expire: the yield of {} on {} is still parked after its expiry, block {}: it queues \
             behind a later expiry (a set_height rewind?)",
            y.callback_methods.join(","),
            y.receiver_id,
            y.expires_at
        );
        self.outcome(y.tx)
    }

    /// No transaction queued, no receipt in flight, delayed or buffered, and
    /// no yield timeout overdue; a yield whose block is still ahead waits.
    pub fn is_idle(&self) -> bool {
        self.chain.is_settled() && self.chain.timeout_due().is_none_or(|e| e > self.height())
    }

    /// Yields waiting for resume or timeout, oldest first; resumed ones
    /// (even with the resume in flight) are left out.
    pub fn yields(&self) -> Vec<PendingYield> {
        let tu = self.chain.trie_update();
        let idx = near_store::get_promise_yield_indices(&tu).expect("storage");
        let mut out = vec![];
        for i in idx.first_index..idx.next_available_index {
            let Some(t) = yield_timeout(&tu, i) else { continue };
            let Some(r) = near_store::get_promise_yield_receipt(&tu, &t.account_id, t.data_id)
                .expect("storage")
            else {
                continue;
            };
            if near_store::get_promise_yield_status(&tu, &t.account_id, t.data_id).expect("storage")
                == Some(PromiseYieldStatus::ResumeInitiated)
            {
                continue;
            }
            let receipt = action_receipt(&r);
            let actions = receipt.as_ref().map_or(&[][..], |a| a.actions());
            out.push(PendingYield {
                receiver_id: t.account_id,
                data_id: t.data_id,
                expires_at: t.expires_at,
                callback_methods: method_names(actions),
                callback_args: calls(actions).map(|c| c.args.clone()).collect(),
                receipt_id: *r.receipt_id(),
                tx: self.chain.log().origin[r.receipt_id()],
            });
        }
        out
    }

    /// Every transaction submitted, setup included, in order.
    pub fn txs(&self) -> &[CryptoHash] {
        &self.chain.log().roots
    }

    /// Creates an account with a full-access ed25519 key seeded from its id,
    /// written into the trie like a genesis record (no parent needed; the
    /// real action is [`Tx::create_account`](crate::Tx::create_account)).
    /// Panics if the account exists.
    #[track_caller]
    pub fn create_account(&mut self, id: impl IntoAccountId, balance: Balance) -> AccountId {
        let id = id.into_account_id();
        let sk = SecretKey::from_seed(KeyType::ED25519, id.as_str());
        self.create_account_with_key(id, balance, &sk)
    }

    /// `create_account` with a given key. Panics where `set_key` does, and
    /// on `Balance::MAX`, which an account record can't hold.
    #[track_caller]
    pub fn create_account_with_key(
        &mut self,
        id: impl IntoAccountId,
        balance: Balance,
        secret_key: &SecretKey,
    ) -> AccountId {
        let id = id.into_account_id();
        assert!(self.stored(&id).is_none(), "account {id} already exists");
        assert_storable(balance);
        self.assert_key_supported(&secret_key.public_key());
        let bytes = self.config.runtime_config().fees.storage_usage_config.num_bytes_account;
        let account = NearAccount::new(balance, Balance::ZERO, AccountContract::None, bytes);
        self.chain.write(&self.config, |tu| near_store::set_account(tu, id.clone(), &account));
        self.set_key(&id, secret_key);
        id
    }

    /// Makes `secret_key` the key `id` signs with, adding it as a
    /// full-access key in the trie if missing. Panics if the account doesn't
    /// exist, or the protocol version lacks the key type (ML-DSA needs
    /// `PostQuantumSignatures`).
    #[track_caller]
    pub fn set_key(&mut self, id: impl IntoAccountId, secret_key: &SecretKey) {
        let id = id.into_account_id();
        let (pk, ak) = (secret_key.public_key(), AccessKey::full_access());
        self.assert_key_supported(&pk);
        if self.access_key(&id, &pk).is_none() {
            let mut acc = self.existing(&id);
            let suc = &self.config.runtime_config().fees.storage_usage_config;
            let key = pk.trie_id_len() + borsh::object_length(&ak).unwrap();
            acc.set_storage_usage(acc.storage_usage() + suc.num_extra_bytes_record + key as u64);
            self.chain.write(&self.config, |tu| {
                near_store::set_account(tu, id.clone(), &acc);
                near_store::set_access_key(tu, id.clone(), pk, &ak);
            });
        }
        self.keys.insert(id, secret_key.clone());
    }

    /// The account, as `view_account` shows it (`code_hash` is the default
    /// for a global contract's user).
    #[track_caller]
    pub fn account(&self, id: impl IntoAccountId) -> Option<AccountView> {
        self.stored(&id.into_account_id()).map(AccountView::from)
    }

    /// The epoch rewards `id`'s stake was paid since genesis.
    pub fn rewarded(&self, id: impl IntoAccountId) -> Balance {
        let r = self.chain.log().rewarded.get(&id.into_account_id()).copied();
        Balance::from_yoctonear(r.unwrap_or(0))
    }

    /// The account's liquid balance. Panics if it doesn't exist.
    #[track_caller]
    pub fn balance(&self, id: impl IntoAccountId) -> Balance {
        self.existing(&id.into_account_id()).amount()
    }

    /// Overwrites an account's liquid balance. Panics if it doesn't exist,
    /// or on `Balance::MAX`.
    #[track_caller]
    pub fn set_balance(&mut self, id: impl IntoAccountId, amount: Balance) {
        let id = id.into_account_id();
        assert_storable(amount);
        let mut acc = self.existing(&id);
        acc.set_amount(amount);
        self.chain.write(&self.config, |tu| near_store::set_account(tu, id, &acc));
    }

    /// `id`'s contract-storage entries under `prefix`, sorted (`view_state`
    /// without the 50 KB limit). Panics if the account doesn't exist.
    #[track_caller]
    pub fn view_state(&self, id: impl IntoAccountId, prefix: &[u8]) -> Vec<(Vec<u8>, Vec<u8>)> {
        let id = id.into_account_id();
        let tu = self.chain.trie_update();
        let r = match VIEWER.view_state(&tu, &id, prefix, None, None, false) {
            Ok(r) => r,
            Err(ViewStateError::AccountDoesNotExist { .. }) => {
                panic!("account {id} does not exist")
            }
            Err(e) => panic!("view_state({id}): {e}"),
        };
        r.values.into_iter().map(|i| (i.key.to_vec(), i.value.to_vec())).collect()
    }

    /// `id`'s contract-storage value at `key`. Panics if the account
    /// doesn't exist.
    #[track_caller]
    pub fn state(&self, id: impl IntoAccountId, key: &[u8]) -> Option<Vec<u8>> {
        let id = id.into_account_id();
        self.existing(&id);
        let tk = TrieKey::ContractData { account_id: id, key: key.to_vec() };
        self.chain.trie_update().get(&tk, AccessOptions::NO_SIDE_EFFECTS).expect("storage")
    }

    /// Writes (`None`: deletes) one contract-storage entry, keeping
    /// `storage_usage` consistent. Panics if the account doesn't exist.
    #[track_caller]
    pub fn set_state(&mut self, id: impl IntoAccountId, key: &[u8], value: Option<&[u8]>) {
        let id = id.into_account_id();
        let mut acc = self.existing(&id);
        let extra = self.config.runtime_config().fees.storage_usage_config.num_extra_bytes_record;
        let size = |v: &[u8]| extra + key.len() as u64 + v.len() as u64;
        self.chain.write(&self.config, |tu| {
            let tk = TrieKey::ContractData { account_id: id.clone(), key: key.to_vec() };
            let old = tu.get(&tk, AccessOptions::NO_SIDE_EFFECTS).expect("storage");
            // `apply_state_records`' entries aren't in the usage: floor at 0.
            let (old, new) = (old.as_deref().map_or(0, size), value.map_or(0, size));
            let usage = acc.storage_usage();
            acc.set_storage_usage(if new >= old {
                usage + (new - old)
            } else {
                usage.saturating_sub(old - new)
            });
            match value {
                Some(v) => tu.set(tk, v.to_vec()),
                None => tu.remove(tk),
            }
            near_store::set_account(tu, id, &acc);
        });
    }

    /// Writes raw state records into the trie in one commit, as a genesis
    /// file or `sandbox_patch_state` gives them: accounts, access keys,
    /// gas-key nonces (missing ones start at 0), code and data. Values are
    /// written as given, nothing recomputed; the balances count as minted.
    /// A `locked` balance the account lacked stakes it, as a forked chain's
    /// validator, from the current epoch (or the height's, if nothing is
    /// staked): it earns `config.epoch_reward`, and an unstake waits the
    /// delay. Changing a nonzero `locked` leaves the stake: the next
    /// boundary returns the excess, and panics on less (nearcore's staking
    /// check). Records carry no epoch state: a mid-unstake copy restakes.
    pub fn apply_state_records(&mut self, records: impl IntoIterator<Item = StateRecord>) {
        self.chain.write(&self.config, |tu| {
            let mut gas_keys = vec![];
            for r in records {
                match r {
                    StateRecord::Account { account_id, account } => {
                        near_store::set_account(tu, account_id, &account)
                    }
                    StateRecord::AccessKey { account_id, public_key, access_key } => {
                        if let Some(g) = access_key.gas_key_info() {
                            gas_keys.push((account_id.clone(), public_key.clone(), g.num_nonces));
                        }
                        near_store::set_access_key_by_handle(
                            tu,
                            account_id,
                            public_key,
                            &access_key,
                        )
                    }
                    StateRecord::GasKeyNonce { account_id, public_key, index, nonce } => {
                        near_store::set_gas_key_nonce_by_handle(
                            tu, account_id, public_key, index, nonce,
                        )
                    }
                    StateRecord::Contract { account_id, code } => {
                        tu.set_code(account_id, &ContractCode::new(code, None))
                    }
                    StateRecord::Data { account_id, data_key, value } => tu.set(
                        TrieKey::ContractData { account_id, key: data_key.into() },
                        value.into(),
                    ),
                    other => panic!("apply_state_records: unsupported record {other:?}"),
                }
            }
            for (id, pk, n) in gas_keys {
                for i in 0..n {
                    let tk = TrieKey::gas_key_nonce(id.clone(), pk.clone(), i);
                    if near_store::get::<Nonce>(tu, &tk).expect("storage").is_none() {
                        near_store::set::<Nonce>(tu, tk, &0);
                    }
                }
            }
        });
    }

    /// The records of accounts `ids`, the inverse of `apply_state_records`,
    /// in trie order (accounts first, as `sandbox_patch_state` needs),
    /// without gas-key nonces, which it refuses.
    pub fn state_records<I: IntoAccountId>(
        &self,
        ids: impl IntoIterator<Item = I>,
    ) -> Vec<StateRecord> {
        let ids: HashSet<AccountId> = ids.into_iter().map(I::into_account_id).collect();
        let columns = [col::ACCOUNT, col::CONTRACT_CODE, col::ACCESS_KEY, col::CONTRACT_DATA];
        let records = self.chain.entries().into_iter().filter(|(k, _)| columns.contains(&k[0]));
        records
            .filter_map(|(k, v)| StateRecord::from_raw_key_value(&k, v))
            .filter(|r| !matches!(r, StateRecord::GasKeyNonce { .. }))
            .filter(|r| ids.contains(state_record_to_account_id(r)))
            .collect()
    }

    /// Every account with a contract of its own (not a global one), and
    /// its code, in trie order.
    pub(crate) fn contracts(&self) -> Vec<(String, Vec<u8>)> {
        let entries = self.chain.entries().into_iter();
        let code = entries.filter(|(k, _)| k[0] == col::CONTRACT_CODE);
        code.map(|(k, v)| (String::from_utf8_lossy(&k[1..]).into_owned(), v)).collect()
    }

    /// Swaps `id`'s code for one that logs the function a trap is in
    /// (`trap::instrument`); the original, or `None`.
    pub(crate) fn name_traps(&mut self, id: &AccountId) -> Option<Vec<u8>> {
        let code = self.contracts().into_iter().find(|(a, _)| a == id.as_str())?.1;
        let copy = crate::trap::instrument(&code)?;
        let mut acc = self.stored(id)?;
        acc.set_contract(AccountContract::Local(CryptoHash::hash_bytes(&copy))).ok()?;
        acc.set_storage_usage(acc.storage_usage() + copy.len() as u64 - code.len() as u64);
        self.chain.write(&self.config, |tu| {
            near_store::set_account(tu, id.clone(), &acc);
            tu.set_code(id.clone(), &ContractCode::new(copy, None));
        });
        Some(code)
    }

    /// The key `id`'s transactions sign with. Panics for an account a
    /// transaction or `apply_state_records` made (use `set_key` or
    /// `Tx::sign_with`), and for one not on chain.
    #[track_caller]
    pub fn secret_key(&self, id: impl IntoAccountId) -> SecretKey {
        let id = id.into_account_id();
        match self.keys.get(&id) {
            Some(sk) => sk.clone(),
            None if self.stored(&id).is_none() => panic!(
                "no key for {id}: no such account exists on chain (a name a contract keeps in its \
                 own ledger isn't one); create it with Sim::create_account, which gives it a key, \
                 or send the call from an account that has one"
            ),
            None => panic!(
                "no key for {id}: the account has no access key the simulator holds (a \
                 transaction or apply_state_records made it); use Sim::set_key or Tx::sign_with"
            ),
        }
    }

    /// The key a transaction of `id` signs with unless told otherwise:
    /// [`secret_key`](Self::secret_key), which must still be on chain.
    #[track_caller]
    pub(crate) fn signing_key(&self, id: &AccountId) -> SecretKey {
        let sk = self.secret_key(id);
        let pk = sk.public_key();
        // A deleted account's transaction is the runtime's to refuse.
        assert!(
            self.stored(id).is_none() || self.access_key(id, &pk).is_some(),
            "no key for {id}: the key the simulator holds, {pk}, is no longer on chain (a \
             transaction deleted it); use Sim::set_key with a key it has, or Tx::sign_with"
        );
        sk
    }

    /// `id`'s access key `pk` (`view_access_key`); a gas key's nonces are in
    /// `gas_key_nonces`.
    #[track_caller]
    pub fn access_key(&self, id: impl IntoAccountId, pk: &PublicKey) -> Option<AccessKey> {
        let tu = self.chain.trie_update();
        near_store::get_access_key(&tu, &id.into_account_id(), pk).expect("storage")
    }

    /// Every access key of `id`, in trie order (ML-DSA-65 as hash handles).
    #[track_caller]
    pub fn access_keys(&self, id: impl IntoAccountId) -> Vec<(PublicKeyHandle, AccessKey)> {
        let id = id.into_account_id();
        let trie = self.chain.trie();
        let (mut keys, mut after) = (vec![], None);
        loop {
            let (page, next) = VIEWER
                .view_access_keys(&trie, &id, after.as_ref(), Some(std::num::NonZeroU32::MAX))
                .unwrap_or_else(|e| panic!("view_access_keys({id}): {e:?}"));
            keys.extend(page);
            let Some(next) = next else { return keys };
            after = Some(next);
        }
    }

    /// The nonces of gas key `pk`, if `id` has it.
    #[track_caller]
    pub fn gas_key_nonces(&self, id: impl IntoAccountId, pk: &PublicKey) -> Option<Vec<Nonce>> {
        let tu = self.chain.trie_update();
        VIEWER.view_gas_key_nonces(&tu, &id.into_account_id(), pk).ok()
    }

    /// The code a [`Tx::deploy_global`](crate::Tx::deploy_global) published.
    pub fn global_contract_code(&self, identifier: GlobalContractIdentifier) -> Option<Vec<u8>> {
        let tu = self.chain.trie_update();
        VIEWER.view_global_contract_code(&tu, identifier).ok().map(|c| c.code().to_vec())
    }

    /// The current state root.
    pub fn state_root(&self) -> CryptoHash {
        self.chain.state_root()
    }

    fn stored(&self, id: &AccountId) -> Option<NearAccount> {
        near_store::get_account(&self.chain.trie_update(), id).expect("storage")
    }

    #[track_caller]
    fn existing(&self, id: &AccountId) -> NearAccount {
        let Some(account) = self.stored(id) else { panic!("account {id} does not exist") };
        account
    }

    #[track_caller]
    fn assert_key_supported(&self, pk: &PublicKey) {
        let v = self.config.protocol_version;
        let pq = ProtocolFeature::PostQuantumSignatures;
        assert!(
            !matches!(pk.key_type(), KeyType::MLDSA65) || pq.enabled(v),
            "{} keys need protocol {} or later; config.protocol_version is {v}",
            pk.key_type(),
            pq.protocol_version()
        );
    }

    /// One above the on-chain, queued and delegated nonces of
    /// `(id, pk, index)`: a refused transaction leaves no gap.
    pub(crate) fn next_nonce(
        &self,
        id: &AccountId,
        pk: &PublicKey,
        index: Option<NonceIndex>,
    ) -> Nonce {
        let key = (id, pk, index);
        let queued = self.chain.queued().map(|t| &t.transaction);
        let queued = queued
            .filter(|t| (t.signer_id(), t.public_key(), t.nonce().nonce_index()) == key)
            .map(|t| t.nonce().nonce());
        let delegated = self.delegates.iter().filter_map(|((a, k, n), max_block_height)| {
            ((a, k, None) == key && *max_block_height > self.height()).then_some(*n)
        });
        let tu = self.chain.trie_update();
        let on_chain = match index {
            Some(i) => near_store::get_gas_key_nonce(&tu, id, pk, i).expect("storage"),
            None => near_store::get_access_key(&tu, id, pk).expect("storage").map(|k| k.nonce),
        };
        queued.chain(delegated).fold(on_chain.unwrap_or(0), Nonce::max) + 1
    }

    /// Frees the nonces of delegates the last block ran or that expired; a
    /// relay refused at conversion leaves its delegate valid.
    fn release_delegates(&mut self) {
        let log = self.chain.log();
        let block = log.blocks.last().expect("a block");
        for tx in block.outcomes.iter().filter_map(|o| log.converted_from(&o.id)) {
            for a in tx.actions() {
                if let Action::Delegate(d) = a {
                    let d = &d.delegate_action;
                    self.delegates.remove(&(d.sender_id.clone(), d.public_key.clone(), d.nonce));
                }
            }
        }
        let height = self.height();
        self.delegates.retain(|_, max_block_height| *max_block_height > height);
    }
}

/// Which of the last block's height and time a test set since the last
/// block ([`Sim::set_height`], [`Sim::set_timestamp_ns`]).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Pinned {
    pub height: bool,
    pub time: bool,
}

/// [`Sim::run_until`] ran out of blocks before the predicate held.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Busy {
    /// How many blocks were produced: the budget.
    pub blocks: u64,
    /// The chain was [idle](Sim::is_idle) at the end.
    pub idle: bool,
}

impl fmt::Display for Busy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let state = if self.idle { "idle" } else { "busy" };
        write!(f, "still waiting after {} blocks (the chain is {state})", self.blocks)
    }
}

impl std::error::Error for Busy {}

/// `Balance::MAX` marks a versioned record in nearcore's account encoding.
#[track_caller]
fn assert_storable(balance: Balance) {
    assert!(balance != Balance::MAX, "an account can't hold Balance::MAX (u128::MAX yocto)");
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::Balance;

    /// The checkout, read when the test runs rather than baked in by
    /// `env!`, so the test binary is the same in every checkout.
    pub(crate) fn manifest_dir() -> std::path::PathBuf {
        std::env::var_os("CARGO_MANIFEST_DIR")
            .expect("CARGO_MANIFEST_DIR unset: run under cargo")
            .into()
    }

    /// alice.near, and probe.near running `tests/res/probe.wasm`.
    pub(crate) fn probe_sim() -> Sim {
        let mut sim = Sim::new();
        sim.create_account("alice.near", Balance::from_near(100));
        let probe = manifest_dir().join("tests/res/probe.wasm");
        sim.deploy("probe.near", std::fs::read(probe).unwrap());
        sim
    }

    /// Parks a yield on probe.near; returns its `expires_at`.
    pub(crate) fn park(sim: &mut Sim) -> u64 {
        sim.call("alice.near", "probe.near", "start_yield").transact().assert_success();
        sim.yields().last().unwrap().expires_at
    }

    /// Parks a yield and resumes it, which leaves its timeout-queue entry
    /// behind, stale, until it is due; returns that `expires_at`.
    pub(crate) fn stale(sim: &mut Sim) -> u64 {
        let expires_at = park(sim);
        sim.call("alice.near", "probe.near", "resume").transact().assert_success();
        expires_at
    }

    fn blocks(sim: &Sim) -> usize {
        sim.chain.log().blocks.len()
    }

    /// The heights of the blocks after the first `n`.
    fn heights_after(sim: &Sim, n: usize) -> Vec<u64> {
        sim.chain.log().blocks[n..].iter().map(|b| b.height).collect()
    }

    /// A long idle jump produces the block where each timeout-queue head is
    /// due (a stale one's pops it; a parked one's fires the timeout, and the
    /// callback and its refund keep the chain busy for two more), and block
    /// `h`.
    #[test]
    fn an_idle_jump_produces_each_due_block_the_busy_ones_after_it_and_h() {
        let mut sim = probe_sim();
        let first = stale(&mut sim);
        sim.advance_to(sim.height() + 50);
        let second = park(&mut sim);
        let (n, h) = (blocks(&sim), second + 1_000);
        sim.advance_to(h);
        assert_eq!(heights_after(&sim, n), [first, second, second + 1, second + 2, h]);
    }

    #[test]
    fn debug_shows_the_clock_the_root_and_the_queue() {
        let mut sim = Sim::new();
        sim.create_account("alice.near", crate::Balance::from_near(1));
        sim.call("alice.near", "alice.near", "m").submit();
        let root = sim.state_root();
        let want = format!(
            "Sim {{ height: 0, timestamp_ns: {GENESIS_TIMESTAMP_NS}, state_root: {root:?}, queued: 1, .. }}"
        );
        assert_eq!(format!("{sim:?}"), want);
    }

    /// With no yield, a jump of a million heights from block 1 is one block,
    /// well within the cap. Time moves by `block_time` per height all the
    /// same.
    #[test]
    fn an_idle_jump_with_no_yield_produces_block_h_alone() {
        let mut sim = Sim::new();
        sim.config.block_time = std::time::Duration::from_millis(600);
        sim.step();
        let t = sim.timestamp_ns();
        sim.advance_to(1_000_001);
        assert_eq!(heights_after(&sim, 1), [1_000_001]);
        assert_eq!(sim.timestamp_ns(), t + 1_000_000 * 600_000_000);
    }

    /// `advance_to(height())` produces nothing, even with a transaction
    /// queued, and `advance_to(height() + 1)` is `step()`, idle or
    /// busy.
    #[test]
    fn advance_to_the_next_height_is_step() {
        let mut sim = probe_sim();
        let chain = |s: &Sim| (s.height(), s.timestamp_ns(), s.state_root(), blocks(s));
        for queued in [false, true] {
            if queued {
                sim.call("alice.near", "probe.near", "height").submit();
            }
            let before = chain(&sim);
            sim.advance_to(sim.height());
            assert_eq!(chain(&sim), before);
            let mut stepped = sim.clone();
            stepped.step();
            sim.advance_to(sim.height() + 1);
            assert_eq!(chain(&sim), chain(&stepped));
        }
    }

    /// Each stale entry in the timeout queue costs a jump one block, the one
    /// that pops it.
    #[test]
    fn a_stale_timeout_costs_a_jump_one_block() {
        let mut sim = probe_sim();
        let stale: Vec<u64> = (0..3).map(|_| stale(&mut sim)).collect();
        let (n, h) = (blocks(&sim), sim.height() + 1_000);
        sim.advance_to(h);
        assert_eq!(heights_after(&sim, n), [stale[0], stale[1], stale[2], h]);
    }

    /// After a rewind, a yield can queue behind one that expires later.
    /// nearcore pops only the queue's head, so it fires with the head: a jump
    /// to its own expiry produces that block, where nothing fires, and leaves
    /// it listed and the chain idle.
    #[test]
    fn a_yield_queued_behind_a_later_expiry_fires_with_it() {
        let mut sim = probe_sim();
        sim.set_height(100);
        let later = park(&mut sim);
        sim.set_height(50);
        let sooner = park(&mut sim);
        let n = blocks(&sim);
        sim.advance_to(sooner);
        assert_eq!(heights_after(&sim, n), [sooner]);
        let listed: Vec<u64> = sim.yields().iter().map(|y| y.expires_at).collect();
        assert_eq!(listed, [later, sooner]);
        assert!(sim.is_idle());
        sim.advance_to(later);
        sim.settle();
        assert_eq!(heights_after(&sim, n)[..2], [sooner, later]);
        assert!(sim.yields().is_empty());
        let outcomes = sim.txs().iter().map(|h| sim.outcome(h));
        let callbacks: Vec<u64> =
            outcomes.filter_map(|o| Some(o.receipt("on_resume")?.block_height)).collect();
        assert_eq!(callbacks, [later + 1, later + 1]);
    }

    /// Each of `n` ids minted on `tests/res/mtoken_events.wasm`, given to
    /// its own holder that isn't an account, then one more write per id:
    /// the views the last write cost.
    fn views_per_write(n: usize) -> u64 {
        use crate::{Delta, Fixture, Token};
        use serde_json::json;
        let (mt, issuer) = ("mt.near", "issuer.near");
        let wasm = std::fs::read(manifest_dir().join("tests/res/mtoken_events.wasm")).unwrap();
        let mut sim = Fixture::new()
            .account(issuer, Balance::from_near(100))
            .account("alice.near", Balance::from_near(100))
            .contract(mt, wasm)
            .init("new")
            .args_json(json!({ "owner": issuer, "ids": [] }))
            .token(Token::nep245_ids(mt, Vec::<String>::new()).discover())
            .build();
        for i in 0..n {
            let id = format!("id{i}");
            sim.call(issuer, mt, "create")
                .args_json(json!({ "id": id }))
                .transact()
                .assert_success();
            let give = json!({ "to": format!("ghost{i}.near"), "id": id, "amount": "5" });
            sim.call(issuer, mt, "give").args_json(give).transact().assert_success();
        }
        let before = sim.chain.log().watch_views();
        let give = json!({ "to": "alice.near", "id": "id0", "amount": "1" });
        let out = sim.call(issuer, mt, "give").args_json(give).transact();
        out.assert_success();
        assert_eq!(out.effects().token_net((mt, "id0"), "alice.near"), Ok(Delta::from(1)));
        sim.chain.log().watch_views() - before
    }

    /// A holder an event names joins only the ids it names: with 16 ids each
    /// held by its own ghost, a write costs about 16 times what one id's does,
    /// not 16 times as many holders per id as well (ids x holders).
    ///
    /// Verification boundary: the watch's cost, over actual runs of the
    /// specimen.
    #[test]
    fn a_holder_named_by_an_event_is_measured_for_its_ids_only() {
        let (one, many) = (views_per_write(1), views_per_write(16));
        // Unscoped, 16 ids cost 400 views here (each id measuring all 16
        // ghosts); scoped, 160.
        assert!(many <= 16 * one + one, "16 ids cost {many} views, 1 id {one}");
    }
}
