//! Transactions, all built by one `Tx`, and views.

use crate::edges::{self, Region, Samples, Why, bisect};
use crate::explore::gas;
use crate::inject::{Fault, Receipts};
use crate::outcome::{Failure, ReceiptOutcome, Refund, TxOutcome};
use crate::sim::{IntoAccountId, Sim};
use crate::stub::Reply;
use near_crypto::{InMemorySigner, PublicKey, SecretKey, Signer};
use near_primitives::account::{AccessKey, AccessKeyPermission, FunctionCallPermission};
use near_primitives::action::delegate::{DelegateAction, NonDelegateAction, SignedDelegateAction};
use near_primitives::action::{
    Action, AddKeyAction, CreateAccountAction, DeleteKeyAction, DeployContractAction,
    DeployGlobalContractAction, FunctionCallAction, GlobalContractDeployMode,
    GlobalContractIdentifier, TransferAction, TransferToGasKeyAction, UseGlobalContractAction,
    WithdrawFromGasKeyAction,
};
use near_primitives::hash::CryptoHash;
use near_primitives::transaction::{
    NonceMode, SignedTransaction, Transaction, TransactionNonce, TransactionV0, TransactionV1,
};
use near_primitives::types::{AccountId, Balance, Gas, Nonce, NonceIndex};
use near_primitives::views::FinalExecutionStatus;
use node_runtime::state_viewer::errors::CallFunctionError;
use serde::Serialize;
use serde::de::DeserializeOwned;
use std::collections::BTreeSet;
use std::ops::RangeInclusive;

impl Sim {
    /// A transaction from `signer` to `receiver` calling `method`:
    /// `tx(signer, receiver).call(method)`.
    #[track_caller]
    pub fn call(
        &mut self,
        signer: impl IntoAccountId,
        receiver: impl IntoAccountId,
        method: &str,
    ) -> Tx<'_> {
        self.tx(signer, receiver).call(method)
    }

    /// A transaction from `signer` to `receiver`, with no action yet.
    #[track_caller]
    pub fn tx(&mut self, signer: impl IntoAccountId, receiver: impl IntoAccountId) -> Tx<'_> {
        Tx {
            sim: self,
            signer_id: signer.into_account_id(),
            receiver_id: receiver.into_account_id(),
            actions: vec![],
            key: None,
            nonce: None,
            nonce_index: None,
            nonce_mode: None,
            promise_results: None,
            predecessor: None,
            injections: None,
            lenient: false,
            replies: vec![],
            unpinned: vec![],
        }
    }

    /// Deploys `wasm` to `id` with a signed `DeployContract` transaction,
    /// producing blocks; panics with the trace if it fails.
    /// `tx(id, id).deploy(wasm)` is the same transaction, to see it fail or
    /// `.call("new")` in it.
    ///
    /// A missing `id` is created first with 100 NEAR, as
    /// [`create_account`](Self::create_account) would, whatever the id: a
    /// misspelled one deploys fine, and calls to the intended id then fail
    /// with `AccountDoesNotExist`.
    #[track_caller]
    pub fn deploy(&mut self, id: impl IntoAccountId, wasm: impl Into<Vec<u8>>) {
        let id = id.into_account_id();
        if self.account(&id).is_none() {
            self.create_account(&id, Balance::from_near(100));
        }
        self.tx(&id, &id).deploy(wasm).transact().assert_success();
    }

    /// Creates `<name>.<parent>` as a parent does, with a real transaction
    /// `parent` signs: `CreateAccount`, a `Transfer` of `balance` and an
    /// `AddKey` of the full-access key [`create_account`](Self::create_account)
    /// would give it, which then signs its transactions. Produces blocks;
    /// panics with the trace if it fails. To see its rules refuse one, send
    /// [`Tx::create_account`] yourself.
    #[track_caller]
    pub fn create_subaccount(
        &mut self,
        parent: impl IntoAccountId,
        name: &str,
        balance: Balance,
    ) -> AccountId {
        let parent = parent.into_account_id();
        let id = format!("{name}.{parent}").into_account_id();
        let key = SecretKey::from_seed(near_crypto::KeyType::ED25519, id.as_str());
        (self.tx(&parent, &id).create_account().transfer(balance))
            .add_key(key.public_key(), AccessKey::full_access())
            .transact()
            .assert_success();
        self.set_key(&id, &key);
        id
    }

    /// Calls `method` of `id` without a transaction, as RPC's
    /// `call_function` does on the latest block. `args` go as JSON, but
    /// `null` (`()`) sends no input: all-optional parameters need `json!({})`.
    #[track_caller]
    pub fn view(&self, id: impl IntoAccountId, method: &str, args: impl Serialize) -> ViewResult {
        self.view_raw(id, method, json_input(args))
    }

    /// [`view`](Self::view) with `args` sent as they are.
    #[track_caller]
    pub fn view_raw(
        &self,
        id: impl IntoAccountId,
        method: &str,
        args: impl Into<Vec<u8>>,
    ) -> ViewResult {
        let at = self.chain.latest(&self.config);
        let id = id.into_account_id();
        let (result, logs) = self.chain.view(&self.config, at, &id, method, &args.into());
        if result.is_ok() {
            self.viewed.lock().map(|mut v| v.insert(format!("{id}::{method}"))).ok();
        }
        let result = result.map_err(|e| match e {
            CallFunctionError::VMError { error, .. } => Failure::call(&error),
            CallFunctionError::AccountDoesNotExist { requested_account_id } => {
                Failure::AccountDoesNotExist(requested_account_id)
            }
            e => panic!("view {id}.{method} at #{}: {e}", at.height),
        });
        let missing = result.as_ref().is_err_and(Failure::is_missing_method);
        ViewResult {
            exports: missing.then(|| self.exports(at, &id)).flatten(),
            result,
            logs,
            receiver_id: id,
            method_name: method.into(),
            block_height: at.height,
        }
    }
}

/// What a view returned, or why it failed, and its logs. It reads as a
/// [`TxOutcome`] does: `is_success`, `failure`, `assert_success`,
/// `assert_failure`, `assert_panic_contains`, `value`, `bytes`, `json`,
/// `u128` and `logs`.
#[non_exhaustive]
#[must_use = "a view's failure is in its result: read it, e.g. .json() or .assert_success()"]
#[derive(Debug, Clone)]
pub struct ViewResult {
    /// The bytes it returned, or why it failed, classified as a call's
    /// failure is: a panic is `Failure::Panic`.
    pub result: Result<Vec<u8>, Failure>,
    /// The account whose code the view ran.
    pub receiver_id: AccountId,
    /// The method the view called.
    pub method_name: String,
    /// The block height whose state and environment the view read.
    pub block_height: u64,
    pub(crate) logs: Vec<String>,
    /// For a method the contract doesn't export, the ones it does.
    pub(crate) exports: Option<Vec<String>>,
}

impl ViewResult {
    /// It returned a value.
    pub fn is_success(&self) -> bool {
        self.result.is_ok()
    }

    /// It failed.
    pub fn is_failure(&self) -> bool {
        self.result.is_err()
    }

    /// Why it failed, if it did.
    pub fn failure(&self) -> Option<Failure> {
        self.result.as_ref().err().cloned()
    }

    /// The value it returned, if it succeeded.
    pub fn value(&self) -> Option<&[u8]> {
        self.result.as_deref().ok()
    }

    /// The logs it emitted.
    pub fn logs(&self) -> Vec<&str> {
        self.logs.iter().map(String::as_str).collect()
    }

    /// Panics with the failure, and the logs, unless it succeeded.
    #[track_caller]
    pub fn assert_success(&self) -> &Self {
        if let Err(f) = &self.result {
            self.fail(format!("{} failed: {}", self.context(), self.failure_text(f)));
        }
        self
    }

    /// Why it failed, as [`TxOutcome::assert_failure`] says why a call did.
    /// Panics with the value it returned, and the logs, unless it failed.
    #[track_caller]
    pub fn assert_failure(&self) -> Failure {
        match &self.result {
            Err(f) => f.clone(),
            Ok(v) => self.fail(format!(
                "{} returned {:?}, not a failure",
                self.context(),
                String::from_utf8_lossy(v)
            )),
        }
    }

    /// The panic or abort message ([`Failure::message`]) it failed with,
    /// which must contain `fragment`, as
    /// [`TxOutcome::assert_panic_contains`] reads a call's. Panics
    /// otherwise.
    #[track_caller]
    pub fn assert_panic_contains(&self, fragment: &str) -> String {
        let got = match &self.result {
            Err(f) => match f.message() {
                Some(m) if m.contains(fragment) => return m.to_string(),
                _ => format!("it failed with: {}", self.failure_text(f)),
            },
            Ok(v) => format!("it returned {:?}", String::from_utf8_lossy(v)),
        };
        self.fail(format!("{}: expected a panic containing {fragment:?}; {got}", self.context()))
    }

    /// The bytes it returned. Panics with the failure, and the logs,
    /// unless it succeeded.
    #[track_caller]
    pub fn bytes(&self) -> &[u8] {
        self.assert_success().value().unwrap_or_default()
    }

    /// The result as JSON. Panics with the error, or the value that isn't a
    /// `T`, and the logs.
    #[track_caller]
    pub fn json<T: DeserializeOwned>(&self) -> T {
        match crate::outcome::decode(self.bytes()) {
            Ok(t) => t,
            Err(e) => self.fail(format!("{}: {e}", self.context())),
        }
    }

    /// The result as a `u128`: a near-sdk `U128` or a JSON number.
    #[track_caller]
    pub fn u128(&self) -> u128 {
        let v: serde_json::Value = self.json();
        match crate::outcome::u128_of(&v) {
            Ok(n) => n,
            Err(e) => self.fail(format!("{}: {v} isn't a u128: {e}", self.context())),
        }
    }

    /// `view <receiver>.<method> at #<height>`, for its panics.
    fn context(&self) -> String {
        format!("view {}.{} at #{}", self.receiver_id, self.method_name, self.block_height)
    }

    /// `f` on one line: a missing method with the methods the contract
    /// exports.
    fn failure_text(&self, f: &Failure) -> String {
        // Only a missing method has exports.
        match &self.exports {
            Some(e) => crate::render::missing_method(&self.receiver_id, &self.method_name, e),
            _ => crate::render::failure_line(f),
        }
    }

    /// Panics with `message`, then the logs if there are any.
    #[track_caller]
    fn fail(&self, message: String) -> ! {
        match &self.logs[..] {
            [] => panic!("{message}"),
            logs => panic!("{message}\nlogs: {logs:?}"),
        }
    }
}

/// A transaction being built by [`Sim::tx`] or [`Sim::call`]: by default a
/// `TransactionV0` signed with the signer's [`Sim::secret_key`] and next
/// nonce. Deploys, stakes, keys, global contracts and deletion need the
/// receiver to be the signer (or created in this transaction), else
/// `ActorNoPermission`.
#[must_use = "a Tx does nothing until submit(), transact(), relay() or sign_delegate()"]
pub struct Tx<'a> {
    sim: &'a mut Sim,
    signer_id: AccountId,
    receiver_id: AccountId,
    actions: Vec<Action>,
    key: Option<SecretKey>,
    nonce: Option<Nonce>,
    nonce_index: Option<NonceIndex>,
    nonce_mode: Option<NonceMode>,
    promise_results: Option<Vec<PromiseResult>>,
    predecessor: Option<AccountId>,
    /// `Some`, even empty, records the receipts it spawns.
    injections: Option<Vec<(Receipts, Fault)>>,
    /// Pass over a fault that can't apply, as an explored plan step's
    /// injection does.
    lenient: bool,
    /// Stub replies for this transaction's calls (`stub_reply`).
    replies: Vec<crate::chain::BoundReply>,
    /// The calls, by action index, whose gas no `gas()` set: what an
    /// explored plan step's gas replaces.
    unpinned: Vec<usize>,
}

/// A promise's result as a callback reads it (near-sdk's
/// `env::promise_result`), for [`Tx::promise_results`].
#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PromiseResult {
    /// It returned these bytes (empty for nothing).
    Successful(Vec<u8>),
    /// It failed: a panic, out of gas, or a missing receiver.
    Failed,
}

impl PromiseResult {
    /// `Successful` with `value` as JSON, as near-sdk returns it.
    #[track_caller]
    pub fn json(value: impl Serialize) -> Self {
        Self::Successful(serde_json::to_vec(&value).expect("JSON value"))
    }
}

impl<'a> Tx<'a> {
    /// Adds a call of `method`: no args, no deposit, 300 Tgas (what wallets
    /// attach, not the 1 PGas [`Sim::gas_ceiling`]). Its gas stays unpinned
    /// until [`gas`](Self::gas): an [`explore`](crate::explore) step that
    /// explores gas replaces it.
    pub fn call(self, method: &str) -> Self {
        let mut tx = self.action(FunctionCallAction {
            method_name: method.into(),
            args: vec![],
            gas: crate::chain::WALLET_GAS,
            deposit: Balance::ZERO,
        });
        tx.unpinned.push(tx.actions.len() - 1);
        tx
    }

    /// Sets the last call's args; panics if the last action isn't a call.
    #[track_caller]
    pub fn args(mut self, args: impl Into<Vec<u8>>) -> Self {
        self.last_call("args").args = args.into();
        self
    }

    /// [`args`](Self::args), serialized as JSON as [`Sim::view`] sends them
    /// (`()` sends none).
    #[track_caller]
    pub fn args_json(mut self, args: impl Serialize) -> Self {
        self.last_call("args_json").args = json_input(args);
        self
    }

    /// [`args`](Self::args), serialized with borsh, as near-sdk reads
    /// `#[serializer(borsh)]` arguments.
    #[track_caller]
    pub fn args_borsh(mut self, args: impl borsh::BorshSerialize) -> Self {
        self.last_call("args_borsh").args = borsh_input(args);
        self
    }

    /// Sets the last call's deposit; panics if the last action isn't a call.
    #[track_caller]
    pub fn deposit(mut self, deposit: Balance) -> Self {
        self.last_call("deposit").deposit = deposit;
        self
    }

    /// Sets and pins the last call's gas (a step exploring gas leaves it
    /// alone); panics if the last action isn't a call. All calls together
    /// attach at most 1 PGas, else `TotalPrepaidGasExceeded`. `None` sets
    /// nothing and never unpins, so `.gas(pin)` takes an `Option<Gas>`.
    #[track_caller]
    pub fn gas(mut self, gas: impl Into<Option<Gas>>) -> Self {
        let call = self.last_call("gas");
        let Some(gas) = gas.into() else { return self };
        call.gas = gas;
        let last = self.actions.len() - 1;
        self.unpinned.retain(|&i| i != last);
        self
    }

    #[track_caller]
    fn last_call(&mut self, edit: &str) -> &mut FunctionCallAction {
        match self.actions.last_mut() {
            Some(Action::FunctionCall(call)) => call,
            _ => panic!("Tx::{edit} edits the last action, which isn't a call"),
        }
    }

    /// Delivers the call as the receiver's callback to itself, reading these
    /// promise `results` (several as `promise_and` joins them): a callback
    /// on any result, one no counterparty returns included. It runs a block
    /// after conversion with `promise_then`'s receipt (predecessor the
    /// receiver, so `#[private]` passes), through nearcore's own callback
    /// path; the signer pays its gas.
    ///
    /// Panics at submit unless the transaction is one call to an account
    /// other than the signer, or if a result exceeds
    /// `max_length_returned_data`. The hash doesn't cover the results, so a
    /// resubmission must carry the same. No node makes this receipt: it is
    /// marked [`delivered`](crate::ReceiptOutcome::delivered), its failure
    /// is the transaction's (so [`Sim::findings`] doesn't flag it), and its
    /// deposit comes from the signer, not a contract.
    pub fn promise_results(mut self, results: impl IntoIterator<Item = PromiseResult>) -> Self {
        self.promise_results = Some(results.into_iter().collect());
        self
    }

    /// Delivers the transaction's receipt as one from `id`: its predecessor
    /// is `id` (`env::predecessor_account_id()`), as when `id`'s contract
    /// calls the receiver, while the signer stays the signer. A test of
    /// "only the token may call `ft_on_transfer`" sends it in one line, with
    /// no stub at `id`, which needn't exist. It is the `ActionV2` receipt a
    /// contract sends, except that a failed deposit is refunded to the
    /// signer, who paid it, not to `id`. It runs a block after conversion,
    /// its outcome is the transaction's, and the receipt tree marks it
    /// `[delivered from <id>]`.
    ///
    /// ```
    /// # use near_contract_sim::{Balance, Sim, Stub, Reply};
    /// # let mut sim = Sim::new();
    /// # sim.create_account("alice.near", Balance::from_near(10));
    /// # sim.deploy_stub("vault.near", Stub::new().on("ft_on_transfer", Reply::json("0")));
    /// let out = sim.call("alice.near", "vault.near", "ft_on_transfer")
    ///     .predecessor("token.near")
    ///     .transact();
    /// assert_eq!(out.receipts[0].predecessor_id, "token.near");
    /// ```
    ///
    /// Panics at submit if the signer is the receiver (its receipt runs in
    /// the conversion block, before it can be delivered), with
    /// [`promise_results`](Self::promise_results), and in a delegate. The
    /// hash doesn't cover the predecessor, so a resubmission must name the
    /// same. No node makes this receipt: it is marked
    /// [`delivered`](crate::ReceiptOutcome::delivered).
    #[track_caller]
    pub fn predecessor(mut self, id: impl IntoAccountId) -> Self {
        self.predecessor = Some(id.into_account_id());
        self
    }

    /// Forces `fault` on the spawned receipts `at` selects, as each leaves
    /// its block, through nearcore's own runtime ([`Fault`], [`Receipts`]).
    /// Chain it for more: each receipt takes the first injection per slot
    /// (what it runs with; `Hold` or `HoldUntil`; `HoldResult`). The outcome marks what
    /// landed ([`ReceiptOutcome::fault`](crate::ReceiptOutcome::fault),
    /// [`held`](crate::ReceiptOutcome::held),
    /// [`result_held`](crate::ReceiptOutcome::result_held)); a selector
    /// matching none changes nothing, so assert one landed.
    ///
    /// Panics here on `Hold(0)`/`HoldResult(0)`, at submit on an oversized
    /// `ReturnValue` or `Gas(0)`, and in the sending block if the fault
    /// doesn't fit the receipt (its "Needs"), unless the selector has an
    /// [`nth`](Receipts::nth), which counts only receipts it fits. A yield's
    /// callback never leaves a block, so nothing selects it. While a
    /// receipt is held the chain isn't idle. The hash doesn't cover
    /// injections, so a resubmission must carry the same. For a delegate's
    /// call, inject into `tx(relayer, signer).action(delegate)`.
    #[track_caller]
    pub fn inject(mut self, at: Receipts, fault: Fault) -> Self {
        if let Fault::Hold(0) | Fault::HoldResult(0) = fault {
            panic!("Tx::inject(.., {fault:?}) holds nothing: hold for at least 1 block");
        }
        self.injections.get_or_insert_default().push((at, fault));
        self
    }

    /// What `method` of the stub at `stub` answers calls descending from
    /// this transaction: `reply` over the stub-wide one
    /// ([`Sim::stub_reply`](crate::Sim::stub_reply)), whose unset fields it
    /// keeps. Overlapping ops each get their own answer. Binding again
    /// queues replies, taken in order, the last repeating. Panics if `stub`
    /// is no stub or `method` undeclared. The hash doesn't cover replies, so
    /// a resubmission must bind the same; bind a delegate's on the relayer's
    /// transaction (`relay` and `sign_delegate` panic with replies bound).
    ///
    /// ```
    /// # use near_contract_sim::{Reply, Sim, Stub};
    /// # let mut sim = Sim::new();
    /// # sim.deploy_stub("receiver.near", Stub::new().on("on_notify", Reply::json("0")));
    /// # sim.create_account("alice.near", near_contract_sim::Balance::from_near(10));
    /// sim.call("alice.near", "receiver.near", "on_notify")
    ///     .stub_reply("receiver.near", "on_notify", Reply::json("7"))
    ///     .submit();
    /// ```
    #[track_caller]
    pub fn stub_reply(mut self, stub: impl IntoAccountId, method: &str, reply: Reply) -> Self {
        let bound = self.sim.bound_reply(stub.into_account_id(), method, &reply);
        self.replies.push(bound);
        self
    }

    pub(crate) fn injections(&self) -> &[(Receipts, Fault)] {
        self.injections.as_deref().unwrap_or_default()
    }

    /// A transfer to a missing account fails (`AccountDoesNotExist`) unless
    /// it creates an implicit or deterministic account as the transaction's
    /// only action (universal `0u…` ones beside others too, from protocol 88,
    /// then uninitialized until a state init).
    pub fn transfer(self, deposit: Balance) -> Self {
        self.action(TransferAction { deposit })
    }

    /// The `DeployContract` action: `wasm` becomes the receiver's code.
    pub fn deploy(self, wasm: impl Into<Vec<u8>>) -> Self {
        self.action(DeployContractAction { code: wasm.into() })
    }

    /// The `CreateAccount` action, under the runtime's rules
    /// ([`Sim::create_account`] skips them).
    pub fn create_account(self) -> Self {
        self.action(CreateAccountAction {})
    }

    /// Adds `access_key` under `public_key`; a gas key is added with no
    /// balance (see [`transfer_to_gas_key`](Self::transfer_to_gas_key)).
    pub fn add_key(self, public_key: PublicKey, access_key: AccessKey) -> Self {
        self.action(AddKeyAction { public_key, access_key })
    }

    /// Adds a function-call access key under `public_key`: it signs calls
    /// to `receiver` alone, with no deposit, of `methods` (`&[]`: any
    /// method), paying their gas from `allowance` (`None`: no limit).
    #[track_caller]
    pub fn add_function_call_key(
        self,
        public_key: PublicKey,
        receiver: impl IntoAccountId,
        methods: &[&str],
        allowance: impl Into<Option<Balance>>,
    ) -> Self {
        let permission = FunctionCallPermission {
            allowance: allowance.into(),
            receiver_id: receiver.into_account_id().into(),
            method_names: methods.iter().map(|m| m.to_string()).collect(),
        };
        let access_key =
            AccessKey { nonce: 0, permission: AccessKeyPermission::FunctionCall(permission) };
        self.add_key(public_key, access_key)
    }

    /// Deletes a key; a gas key's balance burns, and over 1 NEAR is refused.
    pub fn delete_key(self, public_key: PublicKey) -> Self {
        self.action(DeleteKeyAction { public_key })
    }

    /// Publishes `wasm` as a global contract (by hash or account id), the
    /// receiver burning for its storage. Usable once its distribution
    /// receipt runs, a block later.
    pub fn deploy_global(self, wasm: impl Into<Vec<u8>>, mode: GlobalContractDeployMode) -> Self {
        self.action(DeployGlobalContractAction { code: wasm.into().into(), deploy_mode: mode })
    }

    /// Makes the global contract `identifier` the receiver's code.
    pub fn use_global(self, identifier: GlobalContractIdentifier) -> Self {
        self.action(Action::UseGlobalContract(Box::new(UseGlobalContractAction {
            contract_identifier: identifier,
        })))
    }

    /// Adds `deposit`, paid by the signer, to the receiver's gas key.
    pub fn transfer_to_gas_key(self, public_key: PublicKey, deposit: Balance) -> Self {
        self.action(TransferToGasKeyAction { public_key, deposit })
    }

    /// Moves `amount` from the receiver's gas key to its balance.
    pub fn withdraw_from_gas_key(self, public_key: PublicKey, amount: Balance) -> Self {
        self.action(WithdrawFromGasKeyAction { public_key, amount })
    }

    /// Adds any nearcore action, or a `SignedDelegateAction` to relay.
    pub fn action(mut self, action: impl Into<Action>) -> Self {
        self.actions.push(action.into());
        self
    }

    /// Signs with `key` instead of the signer's own (another key, a gas key,
    /// a wrong one). Read a `Sim`'s key before building: the `Tx` borrows it.
    pub fn sign_with(mut self, key: impl std::borrow::Borrow<SecretKey>) -> Self {
        self.key = Some(key.borrow().clone());
        self
    }

    /// Signs with this nonce instead of the next one (above the chain's,
    /// queued and pending-delegate nonces, so a refusal leaves no gap).
    pub fn nonce(mut self, nonce: Nonce) -> Self {
        self.nonce = Some(nonce);
        self
    }

    /// Signs a `TransactionV1` with gas-key nonce `index`: the gas key pays
    /// gas and gets refunds; deposits come from the account.
    pub fn nonce_index(mut self, index: NonceIndex) -> Self {
        self.nonce_index = Some(index);
        self
    }

    /// Signs a `TransactionV1` with this nonce mode (`Strict` accepts only
    /// the key's nonce + 1).
    pub fn nonce_mode(mut self, mode: NonceMode) -> Self {
        self.nonce_mode = Some(mode);
        self
    }

    /// Signs and queues it for the next block, without producing one: what
    /// is submitted before [`Sim::settle`] shares a block unless the chunk's
    /// budget pushes the tail on. Submitted twice, it is queued once.
    #[track_caller]
    pub fn submit(self) -> CryptoHash {
        self.queue().1
    }

    /// Submits, settles ([`Sim::settle`]) and returns the outcome; parked
    /// yields not yet due stay parked. It takes blocks: the transaction
    /// converts in the next, a call to another account runs in the one after,
    /// and callbacks and refunds later still, so the last block, and with it
    /// [`Sim::epoch_height`], can be past an epoch boundary the call itself
    /// ran before. Each receipt's `block_height` says where it ran.
    #[track_caller]
    pub fn transact(self) -> TxOutcome {
        let (sim, hash) = self.queue();
        sim.settle();
        sim.outcome(hash)
    }

    /// A meta transaction (NEP-366): the signer signs a delegate valid
    /// through `height() + 100`, and `relayer` sends and pays for it. The
    /// relayer's outcome succeeds even when an inner call fails: that shows
    /// in `receipts`, [unhandled](crate::ReceiptOutcome::unhandled).
    #[track_caller]
    pub fn relay(self, relayer: impl IntoAccountId) -> TxOutcome {
        let max_block_height = self.sim.height() + 100;
        let sender_id = self.signer_id.clone();
        let (sim, delegate) = self.delegate(max_block_height);
        sim.tx(relayer, sender_id).action(delegate).transact()
    }

    /// The delegate [`relay`](Self::relay) sends, valid until
    /// `max_block_height`, to tamper with or send yourself. Its nonce stays
    /// taken until it runs or expires.
    #[track_caller]
    pub fn sign_delegate(self, max_block_height: u64) -> SignedDelegateAction {
        self.delegate(max_block_height).1
    }

    /// The least gas each unpinned call can attach for the transaction to
    /// succeed ([`TxOutcome::is_success`]), by bisection on copies of the
    /// `Sim` (about 50 runs; nothing is submitted). `Err` with the outcome
    /// if it fails even at the most; panics if no call is unpinned. What it
    /// doesn't wait on may still starve there: what a relayer attaching
    /// just enough meets ([`explore::gas::mixed`](crate::explore::gas::mixed)
    /// draws it).
    #[track_caller]
    pub fn least_gas(self) -> Result<Gas, Box<TxOutcome>> {
        assert!(
            !self.unpinned.is_empty(),
            "Tx::least_gas varies the gas of the calls `gas()` didn't pin, and this transaction \
             has none"
        );
        self.least(self.unpinned_cap())
    }

    /// Every edge of `key` over the caller gas in `range`, as [`edges`]
    /// over gas with a radius of 64, plus solved points: each run attaches
    /// the gas to every unpinned call (pinned calls keep theirs, as with
    /// [`least_gas`](Self::least_gas)), on a copy of the `Sim` settled to
    /// the end ([`Sim::settle`]), so nothing is submitted.
    /// [`OutcomeShape::of`](crate::explore::OutcomeShape::of) is the key
    /// that leaks no gas; the regions where `key` is `true` are the windows
    /// where a predicate holds.
    ///
    /// The solved points: for each function call of the tree
    /// (`receiver::method#k`), a line through the gas it was attached at
    /// each two neighboring samples of a region (at least `step / 2`
    /// apart), solved for the caller gas at which it gets 0 or exactly a
    /// static gas (the fixture's gas constants, 30 Tgas and a wallet's
    /// default). Each is read ±`2k + 4`, at most 128, where `k` is the
    /// caller gas per gas the call is attached: a call attached a share
    /// floored from it holds one value for `k` gas, and the line's error
    /// grows with it. A line is solved out to the samples beside it, a
    /// region's first and last out across the neighboring regions. They
    /// find a window narrower than `step` between two samples that agree,
    /// such as the few gas where a child is attached 0 and nearcore
    /// refuses it (`FunctionCallZeroAttachedGas`). The radius probe and
    /// the solved points are heuristics, labeled by
    /// [`Why`](crate::explore::Why).
    ///
    /// It costs [`edges`]' runs, plus up to `4k + 9` a crossing a line
    /// solves in range: a static gas a call's line reaches, from up to
    /// three neighboring lines, and nothing caps them. Each run settles a
    /// copy of the `Sim`.
    ///
    /// What can still be missed: an island no read lands on, farther than
    /// 64 gas from every edge and outside the reads around each solved
    /// point, whether its key equals its neighbors' or not — including a
    /// branch on used gas that moves no fitted line, and a crossing whose
    /// solved point is off by more than its reads. Edges are exact for
    /// this world, build and the pinned protocol costs; trie-node gas
    /// depends on every key in the trie (fidelity.md), so on a network the
    /// window exists but its position may move.
    ///
    /// A panic in a run or in `key` (a run that hits `config.max_blocks`)
    /// propagates; the `Sim` is untouched either way. Panics if no call is
    /// unpinned, if `step` is 0, if `range` is empty or starts at 0
    /// (nearcore refuses a call with no gas), if it ends above what each
    /// unpinned call can attach (its share of what the pinned calls leave
    /// of [`Sim::gas_ceiling`]), and where [`edges`] panics: its guard
    /// (heuristic) panics when `key` changes more than 16 times within one
    /// step, as a key holding `gas_burnt` does, rather than return edges
    /// that are only gas moving.
    ///
    /// `tests/floor.rs` finds a band about 7 Tgas wide, read every 10, and
    /// `tests/gas_edges.rs` a 4-gas window found only by a solved point.
    ///
    /// [`edges`]: crate::explore::edges()
    #[track_caller]
    pub fn gas_edges<K: Eq + Clone>(
        self,
        range: RangeInclusive<Gas>,
        step: Gas,
        key: impl Fn(&TxOutcome) -> K,
    ) -> Vec<Region<K>> {
        assert!(
            !self.unpinned.is_empty(),
            "Tx::gas_edges varies the gas of the calls `gas()` didn't pin, and this transaction \
             has none"
        );
        let cap = self.unpinned_cap();
        let (lo, hi) = (range.start().as_gas(), range.end().as_gas());
        assert!(step > Gas::ZERO, "Tx::gas_edges: a step of 0 never moves");
        assert!(
            0 < lo && lo <= hi,
            "Tx::gas_edges: {range:?} is empty or starts at 0 (nearcore refuses a call with no \
             gas)"
        );
        assert!(
            hi <= cap.as_gas(),
            "Tx::gas_edges: {range:?} ends above {cap}, the most each unpinned call can attach"
        );
        let statics: Vec<u64> = std::iter::once(0)
            .chain(gas::with_fixture(self.sim, gas::thresholds).into_iter().map(|g| g.as_gas()))
            .collect();
        let run = |gas: u64| {
            let mut sim = self.sim.clone();
            let out = self.copy_on(&mut sim, Gas::from_gas(gas)).transact();
            (key(&out), attached(&out))
        };
        edges::scan(lo..=hi, step.as_gas(), 64, run, |samples| {
            solved(samples, &statics, step.as_gas())
        })
    }

    /// The most each unpinned call can attach: its share of what the
    /// pinned calls leave of the ceiling.
    fn unpinned_cap(&self) -> Gas {
        let max = self.sim.gas_ceiling();
        let pinned = pinned_gas(&self.actions, &self.unpinned);
        let room = max.as_gas().saturating_sub(pinned.as_gas()) / self.unpinned.len().max(1) as u64;
        Gas::from_gas(room)
    }

    /// [`least_gas`](Self::least_gas) up to `cap` a call.
    fn least(&self, cap: Gas) -> Result<Gas, Box<TxOutcome>> {
        if !self.succeeds_with(cap) {
            let mut sim = self.sim.clone();
            return Err(Box::new(self.copy_on(&mut sim, cap).transact()));
        }
        // 0 gas fails (it is no call), `cap` succeeds.
        Ok(Gas::from_gas(bisect(0, cap.as_gas(), |g| self.succeeds_with(Gas::from_gas(g)))))
    }

    /// Whether this transaction, with `gas` on each unpinned call,
    /// succeeds on a copy of the `Sim`: run until its result path ends
    /// (or the copy is idle), not until every receipt has run.
    fn succeeds_with(&self, gas: Gas) -> bool {
        let mut sim = self.sim.clone();
        let (sim, hash) = self.copy_on(&mut sim, gas).queue();
        let decided = |sim: &Sim| {
            !matches!(
                sim.tx_status(hash),
                FinalExecutionStatus::NotStarted | FinalExecutionStatus::Started
            )
        };
        sim.settle_until(decided);
        matches!(sim.tx_status(hash), FinalExecutionStatus::SuccessValue(_))
    }

    /// Where this transaction succeeds with `from` but a receipt of it
    /// fails once all have run: the least gas, up to `cap`, with which
    /// every receipt succeeds, and the first that fails at `from`. `None`
    /// if none fails at `from`, or one still fails at `cap`.
    fn band(&self, from: Gas, cap: Gas) -> Option<(Gas, String)> {
        let failing = |gas: u64| {
            let mut sim = self.sim.clone();
            let (sim, hash) = self.copy_on(&mut sim, Gas::from_gas(gas)).queue();
            sim.settle();
            let out = sim.outcome(hash);
            let failed = out.receipts.into_iter().find(|r| !r.is_success() && !r.is_refund());
            failed.map(|r| format!("{}::{}", r.receiver_id, r.method().unwrap_or_default()))
        };
        let site = failing(from.as_gas())?;
        if failing(cap.as_gas()).is_some() {
            return None;
        }
        Some((Gas::from_gas(bisect(from.as_gas(), cap.as_gas(), |g| failing(g).is_none())), site))
    }

    /// This transaction on `sim` (a copy of its own, outside any explored
    /// step), with `gas` on each unpinned call.
    fn copy_on<'b>(&self, sim: &'b mut Sim, gas: Gas) -> Tx<'b> {
        sim.explore = None;
        let mut tx = Tx {
            sim,
            signer_id: self.signer_id.clone(),
            receiver_id: self.receiver_id.clone(),
            actions: self.actions.clone(),
            key: self.key.clone(),
            nonce: self.nonce,
            nonce_index: self.nonce_index,
            nonce_mode: self.nonce_mode,
            promise_results: self.promise_results.clone(),
            predecessor: self.predecessor.clone(),
            injections: self.injections.clone(),
            lenient: self.lenient,
            replies: self.replies.clone(),
            unpinned: self.unpinned.clone(),
        };
        tx.set_unpinned(gas);
        tx
    }

    fn set_unpinned(&mut self, gas: Gas) {
        for &i in &self.unpinned {
            if let Some(Action::FunctionCall(fc)) = self.actions.get_mut(i) {
                fc.gas = gas;
            }
        }
    }

    /// As an explored plan step submits it: the step's gas on the unpinned
    /// calls, its stub answers over the transaction's own, and its
    /// injections ahead of the transaction's own, in the step's order.
    #[track_caller]
    fn explored(mut self) -> Self {
        let max = self.sim.gas_ceiling();
        let Some(hook) = self.sim.explore.as_deref_mut() else { return self };
        let injections = hook.prepare(&mut self.actions, &self.unpinned, max);
        let (least_plus, band, lenient) = (hook.least_plus(), hook.band, hook.lenient);
        let answers = hook.answers.clone();
        for a in answers {
            self.replies.retain(|b| !(b.stub == a.stub && b.method == a.method));
            let reply = crate::Reply::json(&a.json);
            self.replies.push(self.sim.bound_reply(a.stub, &a.method, &reply));
        }
        if !injections.is_empty() {
            // Ahead of the transaction's own, so each takes its receipt's
            // slot: what a scan forces.
            self.injections.get_or_insert_default().splice(0..0, injections);
            self.lenient = lenient;
        }
        // "Just enough": the least gas that succeeds here, plus the step's
        // offset. A transaction that fails at the most keeps its gas.
        if let Some(plus) = least_plus.filter(|_| !self.unpinned.is_empty()) {
            let cap = self.unpinned_cap();
            let least = self.least(cap);
            if let Ok(least) = least {
                // Just enough: the band above it where an inner receipt
                // still fails.
                let band = (band && plus == Gas::ZERO).then(|| self.band(least, cap)).flatten();
                if let (Some(hook), Some((all, site))) = (self.sim.explore.as_deref_mut(), band) {
                    hook.bands.push((least, all, site));
                }
                self.set_unpinned(Gas::from_gas(
                    least.as_gas().saturating_add(plus.as_gas()).min(cap.as_gas()),
                ));
            }
        }
        self
    }

    /// Panics unless `len` bytes of `what` fit what a receipt can return.
    #[track_caller]
    fn returnable(&self, what: &str, len: usize) {
        let limits = &self.sim.config.runtime_config().wasm_config.limit_config;
        let max = limits.max_length_returned_data;
        assert!(
            len as u64 <= max,
            "{what} of {len} bytes: no receipt returns more than max_length_returned_data, {max}"
        );
    }

    #[track_caller]
    fn queue(mut self) -> (&'a mut Sim, CryptoHash) {
        self = self.explored();
        let results = self.delivery();
        for (_, f) in self.injections() {
            match f {
                Fault::ReturnValue(v) => self.returnable("Fault::ReturnValue", v.len()),
                Fault::Gas(g) => assert!(
                    *g > Gas::ZERO,
                    "Fault::Gas(0): nearcore refuses a call with no gas \
                     (FunctionCallZeroAttachedGas); OutOfGas runs it with 1"
                ),
                _ => {}
            }
        }
        let (signer, nonce) = self.signer_and_nonce();
        let local = self.signer_id == self.receiver_id;
        let Tx {
            sim,
            signer_id,
            receiver_id,
            actions,
            nonce_index,
            nonce_mode,
            injections,
            lenient,
            replies,
            ..
        } = self;
        let public_key = signer.public_key();
        let block_hash = sim.chain.prev_block_hash(&sim.config);
        // A gas-key nonce or a nonce mode needs `TransactionV1`; everything
        // else is signed as `V0`, as wallets do.
        let tx = if nonce_index.is_none() && nonce_mode.is_none() {
            Transaction::V0(TransactionV0 {
                nonce,
                signer_id,
                public_key,
                receiver_id,
                block_hash,
                actions,
            })
        } else {
            let nonce = match nonce_index {
                Some(i) => TransactionNonce::from_nonce_and_index(nonce, i),
                None => TransactionNonce::from_nonce(nonce),
            };
            let nonce_mode = nonce_mode.unwrap_or_default();
            Transaction::V1(TransactionV1 {
                signer_id,
                public_key,
                nonce,
                receiver_id,
                block_hash,
                actions,
                nonce_mode,
            })
        };
        let signature = signer.sign(tx.get_hash_and_size().0.as_ref());
        let hash = sim.submit_pinned(local, |chain| {
            chain.submit(SignedTransaction::new(signature, tx), results, injections, replies)
        });
        if lenient {
            sim.chain.lenient(&hash);
        }
        (sim, hash)
    }

    /// How the simulator delivers the receipt, for
    /// [`promise_results`](Self::promise_results) and
    /// [`predecessor`](Self::predecessor), after checking that it can.
    #[track_caller]
    fn delivery(&self) -> Option<crate::chain::Delivery> {
        use crate::chain::Delivery;
        let what = match (&self.promise_results, &self.predecessor) {
            (None, None) => return None,
            (Some(_), None) => "promise_results",
            (None, Some(_)) => "predecessor",
            (Some(_), Some(_)) => panic!(
                "Tx::promise_results delivers the call from its receiver, so it takes no \
                 Tx::predecessor"
            ),
        };
        assert!(
            self.signer_id != self.receiver_id,
            "Tx::{what} needs a signer other than the receiver, {}: a receipt to its signer runs \
             in the conversion block, before the simulator can deliver it",
            self.receiver_id
        );
        let Some(results) = &self.promise_results else {
            return self.predecessor.clone().map(Delivery::From);
        };
        let one_call = matches!(&self.actions[..], [Action::FunctionCall(_)]);
        assert!(one_call, "Tx::promise_results needs the transaction to be one call");
        let data = |r: &PromiseResult| match r {
            PromiseResult::Successful(v) => {
                self.returnable("a promise result", v.len());
                Some(v.clone())
            }
            PromiseResult::Failed => None,
        };
        Some(Delivery::Callback(results.iter().map(data).collect()))
    }

    /// Signs the actions as a delegate (NEP-461 prefixed hash) and holds its
    /// nonce until it runs or expires.
    #[track_caller]
    fn delegate(self, max_block_height: u64) -> (&'a mut Sim, SignedDelegateAction) {
        assert!(
            self.nonce_index.is_none() && self.nonce_mode.is_none(),
            "a delegate has no nonce index or nonce mode"
        );
        assert!(
            self.promise_results.is_none() && self.predecessor.is_none(),
            "the simulator can't deliver a delegate as a callback or from a predecessor"
        );
        assert!(
            self.injections.is_none(),
            "a delegate carries no injections: send it in `tx(relayer, signer).action(delegate)`, \
             and inject into that"
        );
        assert!(
            self.replies.is_empty(),
            "a delegate carries no stub replies: send it in `tx(relayer, signer).action(delegate)`, \
             and bind replies on that"
        );
        let (signer, nonce) = self.signer_and_nonce();
        let Tx { sim, signer_id, receiver_id, actions, .. } = self;
        let actions = actions
            .into_iter()
            .map(NonDelegateAction::try_from)
            .collect::<Result<_, _>>()
            .expect("a delegate can't carry a delegate");
        let delegate = DelegateAction {
            sender_id: signer_id,
            receiver_id,
            actions,
            nonce,
            max_block_height,
            public_key: signer.public_key(),
        };
        // Its nonce is held until it runs or expires.
        let key = (delegate.sender_id.clone(), delegate.public_key.clone(), nonce);
        sim.delegates.insert(key, max_block_height);
        (sim, SignedDelegateAction::sign(&signer, delegate))
    }

    #[track_caller]
    fn signer_and_nonce(&self) -> (Signer, Nonce) {
        let key = match &self.key {
            Some(sk) => sk.clone(),
            None => self.sim.signing_key(&self.signer_id),
        };
        let signer = InMemorySigner::from_secret_key(self.signer_id.clone(), key);
        let nonce = match self.nonce {
            Some(n) => n,
            None => self.sim.next_nonce(&self.signer_id, &signer.public_key(), self.nonce_index),
        };
        (signer, nonce)
    }
}

/// The gas `actions` attach outside the calls at `unpinned`, as nearcore's
/// `total_prepaid_gas` counts it against `max_total_prepaid_gas`: each
/// other call's, and each call's inside a `Delegate` or `DelegateV2`,
/// whose gas a step never sets.
pub(crate) fn pinned_gas(actions: &[Action], unpinned: &[usize]) -> Gas {
    let inner = |calls: Vec<Action>| -> Gas {
        calls.iter().fold(Gas::ZERO, |sum, a| sum.saturating_add(a.get_prepaid_gas()))
    };
    (actions.iter().enumerate())
        .filter(|(i, _)| !unpinned.contains(i))
        .map(|(_, a)| match a {
            Action::Delegate(d) => inner(d.delegate_action.get_actions()),
            Action::DelegateV2(d) => inner(d.delegate_action.get_actions()),
            a => a.get_prepaid_gas(),
        })
        .fold(Gas::ZERO, Gas::saturating_add)
}

/// `args` as JSON, `null` (`()`) as no input.
#[track_caller]
pub(crate) fn json_input(args: impl Serialize) -> Vec<u8> {
    let json = serde_json::to_vec(&args).expect("JSON args");
    if json == b"null" { vec![] } else { json }
}

/// `args` in borsh.
#[track_caller]
pub(crate) fn borsh_input(args: impl borsh::BorshSerialize) -> Vec<u8> {
    borsh::to_vec(&args).expect("borsh args")
}

/// Each receipt of `out` that isn't a refund, in execution order, named
/// `receiver::method#k` (its actions for a receipt that calls nothing,
/// `receiver::Transfer#k`), `k` the count of that name before it.
fn numbered(out: &TxOutcome) -> Vec<(String, &ReceiptOutcome)> {
    let mut seen = std::collections::BTreeMap::<String, usize>::new();
    (out.receipts.iter().filter(|r| r.refund.is_none()))
        .map(|r| {
            let name = match r.method_names.is_empty() {
                true => format!("{}::{}", r.receiver_id, r.actions.join(",")),
                false => r.site(),
            };
            let k = seen.entry(name.clone()).or_default();
            *k += 1;
            (format!("{name}#{}", *k - 1), r)
        })
        .collect()
}

/// Each function call of `out`, [`numbered`], with the gas it was
/// attached.
type Calls = Vec<(String, u64)>;

/// The [`Calls`] of `out`.
fn attached(out: &TxOutcome) -> Calls {
    (numbered(out).into_iter())
        .filter(|(_, r)| !r.method_names.is_empty())
        .map(|(name, r)| (name, r.attached_gas.as_gas()))
        .collect()
}

/// The caller gas at which a call gets one of `statics`, each read
/// ±`2k + 4` (at most 128; `k` prepaid gas per gas attached), on the
/// lines through the gas it was attached at neighboring samples of a
/// region (read by the grid or a probe, at least `step / 2` apart). A line is
/// solved out to the samples either side of its pair, and the region's
/// first and last lines out across the neighboring regions: the child
/// that would get 0 usually doesn't exist where it would.
fn solved<K: Eq>(samples: &Samples<K, Calls>, statics: &[u64], step: u64) -> BTreeSet<u64> {
    /// The most read either side of a solved point.
    const READ: i128 = 128;
    /// A region's first and last gas, and the reads to fit through.
    type Fit<'s> = (u64, u64, Vec<(u64, &'s Calls)>);
    let mut regions: Vec<Fit> = vec![];
    let mut last: Option<&K> = None;
    for (&at, s) in samples {
        if last != Some(&s.key) {
            regions.push((at, at, vec![]));
        }
        last = Some(&s.key);
        let region = regions.last_mut().expect("pushed");
        region.1 = at;
        // Fitting through solved reads would move the lines they were
        // solved on, round after round.
        if s.why != Why::Solved {
            region.2.push((at, &s.extra));
        }
    }
    let gap = (step / 2).max(1);
    let mut out = BTreeSet::new();
    for (j, (_, _, fit)) in regions.iter().enumerate() {
        let from = regions[j.saturating_sub(1)].0;
        let to = regions.get(j + 1).unwrap_or(&regions[j]).1;
        let sites: BTreeSet<&String> =
            fit.iter().flat_map(|(_, c)| c.iter().map(|c| &c.0)).collect();
        for site in sites {
            let mut line: Vec<(i128, i128)> = vec![];
            for (at, calls) in fit {
                let Some((_, a)) = calls.iter().find(|c| &c.0 == site) else { continue };
                match line.last() {
                    Some(&(p, _)) if (*at as i128) < p + gap as i128 => {}
                    _ => line.push((*at as i128, *a as i128)),
                }
            }
            for (i, w) in line.windows(2).enumerate() {
                let [(p1, a1), (p2, a2)] = [w[0], w[1]];
                if a1 == a2 {
                    continue;
                }
                let lo = if i == 0 { from as i128 } else { line[i - 1].0 };
                let hi = line.get(i + 2).map_or(to as i128, |l| l.0);
                // Prepaid gas per gas attached, rounded up: an attached gas
                // floored from it holds for `k` prepaid values, and the
                // line's error grows with it.
                let k = ((p2 - p1) + (a2 - a1).abs() - 1) / (a2 - a1).abs();
                let r = (2 * k + 4).min(READ);
                for &t in statics {
                    let p = p1 + (t as i128 - a1) * (p2 - p1) / (a2 - a1);
                    if (lo..=hi).contains(&p) {
                        out.extend(((p - r).max(0)..=p + r).map(|p| p as u64));
                    }
                }
            }
        }
    }
    out
}

/// What [`Tx::gas_edges`] usually keys on: the shape of an outcome,
/// with nothing in it that moves with gas, so two outcomes agree unless
/// what ran, failed or refunded differs.
///
/// In: the transaction's status (ok, pending, or the [`Failure`] variant
/// it failed with); per receipt in execution order, its site
/// `receiver::method#k` (`k` its count before it; a receipt that calls
/// nothing is named by its actions, `receiver::Transfer#k`), and ok or
/// its `Failure` variant, and whether a fault forced it; each deposit or
/// deletion refund with its receiver; the count of receipts pending.
/// Out: gas burnt, used or attached, gas refunds, balances, logs,
/// heights, timestamps, return values, and any message text, so two
/// panics with different messages agree, and so do any two VM errors
/// that aren't a panic or out of gas (a trap, a host limit: `Host`). A
/// key of your own separates them. A failure's variants nest down to
/// the first field that isn't one: `Action::NewReceiptValidationError::
/// ActionsValidation::FunctionCallZeroAttachedGas`.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct OutcomeShape {
    status: String,
    receipts: Vec<String>,
    refunds: Vec<String>,
    pending: usize,
}

impl OutcomeShape {
    /// The shape of `outcome`.
    pub fn of(outcome: &TxOutcome) -> Self {
        let status = match &outcome.status {
            FinalExecutionStatus::SuccessValue(_) => "ok".to_owned(),
            FinalExecutionStatus::Failure(_) => {
                format!("failed: {}", outcome.failure().as_ref().map_or_else(String::new, kind))
            }
            FinalExecutionStatus::NotStarted | FinalExecutionStatus::Started => "pending".into(),
        };
        let receipts = (numbered(outcome).into_iter())
            .map(|(name, r)| {
                let ended = r.failure().map_or_else(|| "ok".to_owned(), |f| kind(&f));
                let forced = if r.fault.is_some() { ", forced" } else { "" };
                format!("{name} {ended}{forced}")
            })
            .collect();
        let refunds = (outcome.receipts.iter())
            .filter_map(|r| match r.refund {
                None | Some(Refund::Gas) => None,
                Some(refund) => Some(format!("refund {refund:?} to {}", r.receiver_id)),
            })
            .collect();
        OutcomeShape { status, receipts, refunds, pending: outcome.pending.len() }
    }
}

impl std::fmt::Display for OutcomeShape {
    /// The status, then a line a receipt, a line a refund, and what is
    /// pending.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.status)?;
        for r in self.receipts.iter().chain(&self.refunds) {
            write!(f, "\n  {r}")?;
        }
        if self.pending > 0 {
            write!(f, "\n  {} pending", self.pending)?;
        }
        Ok(())
    }
}

/// A failure's variant, with no value or message: `Panic`,
/// `Action::LackBalanceForState`, and nested variants down to the first
/// field that isn't one, `Action::FunctionCallError::MethodResolveError::
/// MethodNotFound`. A VM error that isn't a panic or out of gas is `Host`.
fn kind(failure: &Failure) -> String {
    let variants = |debug: String| {
        let mut out: Vec<&str> = vec![];
        let mut rest = debug.as_str();
        while rest.starts_with(|c: char| c.is_ascii_uppercase()) {
            let end = rest.find(|c: char| !c.is_ascii_alphanumeric()).unwrap_or(rest.len());
            out.push(&rest[..end]);
            match rest[end..].strip_prefix('(') {
                Some(inner) => rest = inner,
                None => break,
            }
        }
        out.join("::")
    };
    match failure {
        Failure::OutOfGas => "OutOfGas".into(),
        Failure::GasLimitExceeded => "GasLimitExceeded".into(),
        Failure::Panic(_) => "Panic".into(),
        Failure::AccountDoesNotExist(_) => "AccountDoesNotExist".into(),
        Failure::InvalidTx(e) => format!("InvalidTx::{}", variants(format!("{e:?}"))),
        Failure::Action(k) => format!("Action::{}", variants(format!("{k:?}"))),
        Failure::Host(_) => "Host".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::edges::Sample;

    /// Pinned gas: the calls not unpinned, and the calls inside either
    /// kind of delegate, as nearcore's `total_prepaid_gas` sums them.
    #[test]
    fn pinned_gas_counts_both_delegates_inner_calls() {
        use near_primitives::action::delegate::{
            DelegateActionV2, VersionedDelegateActionPayload, VersionedSignedDelegateAction,
        };
        let call = |tgas| {
            Action::FunctionCall(Box::new(FunctionCallAction {
                method_name: "m".into(),
                args: vec![],
                gas: Gas::from_teragas(tgas),
                deposit: Balance::ZERO,
            }))
        };
        let inner = || [call(7), call(4)].map(|a| NonDelegateAction::try_from(a).unwrap()).to_vec();
        let sk = SecretKey::from_seed(near_crypto::KeyType::ED25519, "a.near");
        let signer = InMemorySigner::from_secret_key("a.near".parse().unwrap(), sk.clone());
        let v1 = DelegateAction {
            sender_id: "a.near".parse().unwrap(),
            receiver_id: "b.near".parse().unwrap(),
            actions: inner(),
            nonce: 1,
            max_block_height: 1,
            public_key: sk.public_key(),
        };
        let v2 = VersionedDelegateActionPayload::V2(DelegateActionV2 {
            sender_id: v1.sender_id.clone(),
            receiver_id: v1.receiver_id.clone(),
            actions: inner(),
            nonce: TransactionNonce::from_nonce(1),
            max_block_height: 1,
            public_key: sk.public_key(),
        });
        let v1 = Action::Delegate(Box::new(SignedDelegateAction::sign(&signer, v1)));
        let v2 = Action::DelegateV2(Box::new(VersionedSignedDelegateAction::sign(&signer, v2)));
        let actions = [call(100), v1, call(30), v2];
        assert_eq!(pinned_gas(&actions, &[]), Gas::from_teragas(152));
        assert_eq!(pinned_gas(&actions, &[2]), Gas::from_teragas(122));
        assert_eq!(pinned_gas(&actions, &[0, 2]), Gas::from_teragas(22));
    }

    /// Samples by gas, key and why read, each of one call `c#0` attached
    /// the gas given, if any.
    fn samples(at: &[(u64, bool, Why, Option<u64>)]) -> Samples<bool, Calls> {
        (at.iter().enumerate())
            .map(|(n, &(g, key, why, a))| {
                let extra = a.map(|a| ("c#0".to_owned(), a)).into_iter().collect();
                (g, Sample { key, extra, why, n })
            })
            .collect()
    }

    /// `at - r..=at + r`.
    fn around(at: u64, r: u64) -> BTreeSet<u64> {
        (at - r..=at + r).collect()
    }

    /// Each line through neighboring samples is solved for each static
    /// gas, read ±`2k + 4`, within the samples either side of it, and the first and
    /// last out to the neighboring regions: a forward capped at 200
    /// still solves to 100 for 0, where one line through the farthest
    /// samples wouldn't. A solved read is not fitted through, nor a call
    /// whose gas doesn't move, nor a pair closer than half a step.
    #[test]
    fn solved_points_lie_on_the_lines_through_neighboring_samples() {
        let grid = |g, a| (g, true, Why::Grid, a);
        let capped =
            [grid(50, None), grid(200, Some(100)), grid(300, Some(200)), grid(400, Some(200))];
        assert_eq!(solved(&samples(&capped), &[0], 2), around(100, 6));
        // 150 crosses on the first line; 400 is past every sample.
        let line =
            [grid(50, None), grid(200, Some(100)), grid(250, Some(150)), grid(300, Some(200))];
        assert_eq!(solved(&samples(&line), &[50, 400], 2), around(150, 6));
        // A solved read past the grid moves nothing.
        let s = samples(&[
            grid(50, None),
            grid(200, Some(100)),
            grid(300, Some(200)),
            (1_000, true, Why::Solved, Some(7)),
        ]);
        assert_eq!(solved(&s, &[0], 2), around(100, 6));
        // Two regions: each fitted on its own, out across the other.
        let s = samples(&[
            grid(50, None),
            grid(200, Some(1)),
            (300, false, Why::Probe, Some(200)),
            (400, false, Why::Grid, Some(300)),
        ]);
        assert_eq!(solved(&s, &[0], 2), around(100, 6));
        // The last line solves out to the next region's last sample.
        let s =
            samples(&[grid(100, Some(100)), grid(200, Some(200)), (300, false, Why::Grid, None)]);
        assert_eq!(solved(&s, &[300], 2), around(300, 6));
        // A later line solves out to the sample before its pair (the
        // first, flat, solves nothing).
        let later = [grid(100, Some(100)), grid(200, Some(100)), grid(300, Some(150))];
        assert_eq!(solved(&samples(&later), &[75], 2), around(150, 8));
        // Within half a step of the last kept, a read is not fitted
        // through; at half a step, it is.
        let near = [grid(200, Some(100)), grid(201, Some(500)), grid(300, Some(200))];
        assert_eq!(solved(&samples(&near), &[150], 200), around(250, 6));
        // A quarter share: 4 gas of prepaid a gas attached, read ±12; a
        // thousandth, ±128 at most.
        let quarter = [grid(50, None), grid(200, Some(25)), grid(600, Some(125))];
        assert_eq!(solved(&samples(&quarter), &[0], 2), around(100, 12));
        // 1.5 gas of prepaid a gas attached rounds up to 2: ±8.
        let half = [grid(50, None), grid(200, Some(100)), grid(350, Some(200))];
        assert_eq!(solved(&samples(&half), &[50], 2), around(125, 8));
        let thin =
            [grid(50, None), grid(1_000, Some(10)), grid(2_000, Some(11)), grid(3_000, Some(12))];
        assert_eq!(solved(&samples(&thin), &[12], 2), around(3_000, 128));
        let flat = [grid(200, Some(7)), grid(300, Some(7))];
        assert_eq!(solved(&samples(&flat), &[0], 2), BTreeSet::new());
    }

    #[test]
    fn view_json_panics_with_the_error_and_the_logs() {
        let view = |result, logs: &[&str]| ViewResult {
            result,
            logs: logs.iter().map(|l| l.to_string()).collect(),
            receiver_id: "status.near".parse().unwrap(),
            method_name: "get_status".into(),
            block_height: 7,
            exports: None,
        };
        assert_eq!(view(Ok(b"\"hi\"".to_vec()), &[]).json::<String>(), "hi");
        let panic = |v: ViewResult| {
            let p = std::panic::catch_unwind(|| v.json::<u64>()).unwrap_err();
            *p.downcast::<String>().unwrap()
        };
        assert_eq!(
            panic(view(Err(Failure::Panic("boom".into())), &[])),
            "view status.near.get_status at #7 failed: panicked: boom"
        );
        let serde = serde_json::from_slice::<u64>(b"\"hi\"").unwrap_err();
        assert_eq!(
            panic(view(Ok(b"\"hi\"".to_vec()), &["a"])),
            format!("view status.near.get_status at #7: {serde}: \"\\\"hi\\\"\"\nlogs: [\"a\"]")
        );
    }
}
