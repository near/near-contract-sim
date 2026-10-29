//! `Fixture`: setup as data, built once and cached.
//!
//! A [`Fixture`] records setup steps (accounts, contracts, init calls,
//! token registrations and transfers, stubs) and [`Fixture::build`] runs
//! them once: the result is memoized in the process and saved to disk with
//! [`Sim::cached`], keyed by every step, the wasm bytes included, so a
//! rebuilt contract never loads a stale setup.

use crate::cache::hash_of;
use crate::chain::Config;
use crate::effects::Token;
use crate::findings::panic_text;
use crate::outcome::TxOutcome;
use crate::sim::{IntoAccountId, Sim};
use crate::stub::Stub;
use borsh::BorshSerialize;
use near_primitives::types::{AccountId, Balance, Gas};
use serde::Serialize;
use std::collections::HashMap;
use std::fmt;
use std::hash::{Hash, Hasher};
use std::panic::{AssertUnwindSafe, Location, catch_unwind, resume_unwind};
use std::sync::{Arc, LazyLock, Mutex, PoisonError};

/// Setup as data: accounts, contracts and the calls that prepare them, run
/// in order by [`build`](Self::build), which returns a fresh clone of the
/// result each time.
///
/// The first `build` of a step list in a test process runs it, or loads it
/// from disk if an earlier run of the same test binary saved it
/// ([`Sim::cached`]); later builds clone it from memory. The key is every
/// step, the wasm bytes and arguments included: a rebuilt contract, read
/// from disk when the test runs, gets a fresh setup. Each call step must
/// succeed; a failing step panics, naming the step and showing its
/// outcome. `build` ends by settling the chain.
///
/// ```no_run
/// # use near_contract_sim::{Balance, Fixture};
/// # use serde_json::json;
/// # let res = |name: &str| std::fs::read(format!("tests/res/{name}")).unwrap();
/// let sim = Fixture::new()
///     .account("alice.near", Balance::from_near(10))
///     .contract("token.near", res("fungible_token.wasm"))
///     .init("new_default_meta")
///     .args_json(json!({ "owner_id": "token.near", "total_supply": "2000" }))
///     .ft_register("token.near", ["alice.near"])
///     .ft_transfer("token.near", "token.near", "alice.near", 1000)
///     .build();
/// ```
#[must_use]
#[derive(Clone, Debug, Default)]
pub struct Fixture {
    /// The `Sim`'s config, before the first step.
    config: Config,
    steps: Vec<Step>,
}

#[derive(Clone, Debug, Hash)]
enum Step {
    Account {
        id: AccountId,
        balance: Balance,
    },
    Contract {
        id: AccountId,
        wasm: Vec<u8>,
    },
    /// `what` names it in a panic: `init`, `call` or `ft_transfer`.
    Call {
        what: String,
        signer: AccountId,
        receiver: AccountId,
        method: String,
        args: Vec<u8>,
        deposit: Balance,
        /// The gas it attaches; `None`: the transaction's default.
        gas: Option<Gas>,
    },
    FtRegister {
        token: AccountId,
        accounts: Vec<AccountId>,
    },
    Stub {
        id: AccountId,
        stub: StubStep,
    },
    /// The key holds its name and where it was written, not what it
    /// captures.
    Setup {
        name: String,
        at: &'static Location<'static>,
        step: SetupFn,
    },
    Token(Token),
}

/// The fields of a call step that its editors set.
struct CallStep<'a> {
    args: &'a mut Vec<u8>,
    deposit: &'a mut Balance,
    gas: &'a mut Option<Gas>,
}

/// A `Stub` in the key: its `Debug` form holds every method and reply.
#[derive(Clone, Debug)]
struct StubStep(Stub);

impl Hash for StubStep {
    fn hash<H: Hasher>(&self, state: &mut H) {
        format!("{:?}", self.0).hash(state);
    }
}

/// A setup closure; the step's name and place stand for it in the key.
#[derive(Clone)]
struct SetupFn(Arc<dyn Fn(&mut Sim) + Send + Sync>);

impl Hash for SetupFn {
    fn hash<H: Hasher>(&self, _: &mut H) {}
}

// A panicking step fails the build, which never reuses a half-built `Sim`,
// so a fixture is as unwind-safe as one of plain data.
impl std::panic::RefUnwindSafe for SetupFn {}
impl std::panic::UnwindSafe for SetupFn {}

impl fmt::Debug for SetupFn {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SetupFn")
    }
}

impl fmt::Display for Step {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Step::Account { id, balance } => write!(f, "account({id}, {balance})"),
            Step::Contract { id, wasm } => {
                write!(f, "contract({id}, {} bytes of wasm)", wasm.len())
            }
            Step::Call { what, .. } => f.write_str(what),
            Step::FtRegister { token, accounts } => {
                let accounts: Vec<&str> = accounts.iter().map(|a| a.as_ref()).collect();
                write!(f, "ft_register({token}, [{}])", accounts.join(", "))
            }
            Step::Stub { id, .. } => write!(f, "stub({id})"),
            Step::Setup { name, .. } => write!(f, "setup({name:?})"),
            Step::Token(t) => write!(f, "token({})", t.id),
        }
    }
}

impl Fixture {
    /// No steps: `build` gives `Sim::new()`.
    pub fn new() -> Self {
        Self::default()
    }

    /// Edits the `Sim`'s [`Config`] before the first step, wherever it is
    /// written: `.config(|c| c.epoch_length = 100)`. The key holds the
    /// config it makes.
    pub fn config(mut self, edit: impl FnOnce(&mut Config)) -> Self {
        edit(&mut self.config);
        self
    }

    /// Creates `id` with `balance` ([`Sim::create_account`]). Panics in
    /// `build` if it exists.
    #[track_caller]
    pub fn account(self, id: impl IntoAccountId, balance: Balance) -> Self {
        self.with(Step::Account { id: id.into_account_id(), balance })
    }

    /// Deploys `wasm` to `id` ([`Sim::deploy`]: it creates a missing `id`
    /// with 100 NEAR, a misspelled one too). Later [`init`](Self::init)s
    /// call this contract.
    #[track_caller]
    pub fn contract(self, id: impl IntoAccountId, wasm: impl Into<Vec<u8>>) -> Self {
        self.with(Step::Contract { id: id.into_account_id(), wasm: wasm.into() })
    }

    /// Calls `method` of the last [`contract`](Self::contract), signed by
    /// itself, with no args and no deposit unless [`args_json`](Self::args_json),
    /// [`args_borsh`](Self::args_borsh), [`args`](Self::args) or
    /// [`deposit`](Self::deposit) follow; it must succeed. Panics here if no
    /// contract comes before it.
    ///
    /// Self-signed, as `#[private] #[init]` needs, the contract pays the
    /// gas, and nearcore refunds what `method` left unused in a later
    /// receipt: `env::account_balance()` inside `method` is below the
    /// balance after `build`. To keep that refund off the contract's own
    /// accounting, init with [`call`](Self::call) from another account,
    /// if `method` isn't `#[private]`.
    #[track_caller]
    pub fn init(self, method: &str) -> Self {
        let contract = self.steps.iter().rev().find_map(|s| match s {
            Step::Contract { id, .. } => Some(id.clone()),
            _ => None,
        });
        let Some(id) = contract else {
            panic!("Fixture::init({method:?}) needs a .contract(..) before it");
        };
        let what = format!("init {id}.{method}");
        self.call_step(what, id.clone(), id, method, vec![], Balance::ZERO)
    }

    /// Calls `method` of `receiver`, signed by `signer`, with no args and no
    /// deposit unless [`args_json`](Self::args_json),
    /// [`args_borsh`](Self::args_borsh), [`args`](Self::args) or
    /// [`deposit`](Self::deposit) follow; it must succeed.
    #[track_caller]
    pub fn call(
        self,
        signer: impl IntoAccountId,
        receiver: impl IntoAccountId,
        method: &str,
    ) -> Self {
        let (signer, receiver) = (signer.into_account_id(), receiver.into_account_id());
        let what = format!("call {receiver}.{method}, signed by {signer}");
        self.call_step(what, signer, receiver, method, vec![], Balance::ZERO)
    }

    fn call_step(
        self,
        what: String,
        signer: AccountId,
        receiver: AccountId,
        method: &str,
        args: Vec<u8>,
        deposit: Balance,
    ) -> Self {
        let method = method.to_string();
        self.with(Step::Call { what, signer, receiver, method, args, deposit, gas: None })
    }

    /// The last step, an [`init`](Self::init) or a [`call`](Self::call)
    /// (or an [`ft_transfer`](Self::ft_transfer)), sends `args` as they are, as [`Tx::args`](crate::Tx::args) does.
    /// Panics after any other step.
    #[track_caller]
    pub fn args(mut self, args: impl Into<Vec<u8>>) -> Self {
        *self.last_call("args").args = args.into();
        self
    }

    /// [`args`](Self::args), serialized as JSON as
    /// [`Tx::args_json`](crate::Tx::args_json) sends them.
    #[track_caller]
    pub fn args_json(mut self, args: impl Serialize) -> Self {
        *self.last_call("args_json").args = crate::tx::json_input(args);
        self
    }

    /// [`args`](Self::args), serialized with borsh as
    /// [`Tx::args_borsh`](crate::Tx::args_borsh) sends them.
    #[track_caller]
    pub fn args_borsh(mut self, args: impl BorshSerialize) -> Self {
        *self.last_call("args_borsh").args = crate::tx::borsh_input(args);
        self
    }

    /// The last step, an [`init`](Self::init) or a [`call`](Self::call),
    /// attaches `deposit`. Panics after any other step.
    #[track_caller]
    pub fn deposit(mut self, deposit: Balance) -> Self {
        *self.last_call("deposit").deposit = deposit;
        self
    }

    /// The last step, an [`init`](Self::init) or a [`call`](Self::call),
    /// attaches `gas` instead of the transaction's 300 Tgas: what a real
    /// deployment script attaches, where the gas a call is given changes
    /// what it does (a share forwarded to its callees, a fee booked from
    /// what it burns). Panics after any other step.
    #[track_caller]
    pub fn gas(mut self, gas: Gas) -> Self {
        *self.last_call("gas").gas = Some(gas);
        self
    }

    /// The last step, which `what` edits.
    #[track_caller]
    fn last_call(&mut self, what: &str) -> CallStep<'_> {
        match self.steps.last_mut() {
            Some(Step::Call { args, deposit, gas, .. }) => CallStep { args, deposit, gas },
            _ => panic!(
                "Fixture::{what} edits the last step, an init or a call, and the last step is \
                 neither"
            ),
        }
    }

    /// Registers each of `accounts` with the NEP-145 storage of `token`:
    /// reads `storage_balance_bounds` and calls `storage_deposit` with its
    /// `min` for each, signed and paid by `token`, so an account needn't
    /// exist yet. Each deposit must succeed. For another payer, call
    /// `storage_deposit` with [`call`](Self::call).
    #[track_caller]
    pub fn ft_register(
        self,
        token: impl IntoAccountId,
        accounts: impl IntoIterator<Item = impl IntoAccountId>,
    ) -> Self {
        let accounts = accounts.into_iter().map(IntoAccountId::into_account_id).collect();
        self.with(Step::FtRegister { token: token.into_account_id(), accounts })
    }

    /// `from` calls NEP-141 `ft_transfer` of `token` to send `amount` to
    /// `to`, with the 1 yoctoNEAR it requires; it must succeed.
    #[track_caller]
    pub fn ft_transfer(
        self,
        token: impl IntoAccountId,
        from: impl IntoAccountId,
        to: impl IntoAccountId,
        amount: u128,
    ) -> Self {
        let (token, from, to) =
            (token.into_account_id(), from.into_account_id(), to.into_account_id());
        let what = format!("ft_transfer({token}: {from} -> {to}, {amount})");
        let args = serde_json::json!({ "receiver_id": to, "amount": amount.to_string() });
        let args = crate::tx::json_input(args);
        self.call_step(what, from, token, "ft_transfer", args, Balance::from_yoctonear(1))
    }

    /// Deploys `stub` to `id` ([`Sim::deploy_stub`]), creating `id` with
    /// 100 NEAR if it doesn't exist.
    #[track_caller]
    pub fn stub(self, id: impl IntoAccountId, stub: Stub) -> Self {
        self.with(Step::Stub { id: id.into_account_id(), stub: StubStep(stub) })
    }

    /// Watches `token` ([`Sim::watch`]) from this step on: the built `Sim`
    /// and its clones measure its moves per receipt.
    pub fn token(self, token: Token) -> Self {
        self.with(Step::Token(token))
    }

    /// Runs `step` on the `Sim` built so far: the escape hatch for setup
    /// the other steps don't cover. It may capture what it needs (a wasm,
    /// an amount).
    ///
    /// The key holds `name` and where this call is written, not what
    /// `step` captures or reads: a closure that captures a value that
    /// differs between two fixtures built from the same line (in a loop,
    /// say, or a helper's parameter) must say so in `name`, or the second
    /// gets the first one's cached setup. Anything else that changes what
    /// it does, such as a file it reads, goes in `name` too, or in another
    /// step (a [`contract`](Self::contract)'s bytes).
    #[track_caller]
    pub fn setup(self, name: &str, step: impl Fn(&mut Sim) + Send + Sync + 'static) -> Self {
        let (name, at) = (name.to_string(), Location::caller());
        self.with(Step::Setup { name, at, step: SetupFn(Arc::new(step)) })
    }

    /// The `Sim` the steps make: a fresh clone of the one built, or loaded,
    /// first in this process. The first build of a step list runs each step
    /// in order and then settles; a step that fails, or panics, panics
    /// naming its number and itself.
    pub fn build(&self) -> Sim {
        self.built(hash_of((&self.config, &self.steps)))
    }

    /// The `Sim` under `key`: memoized in the process, cached on disk,
    /// else built by running the steps.
    fn built(&self, key: u64) -> Sim {
        static MEMO: LazyLock<Mutex<HashMap<u64, Sim>>> = LazyLock::new(Default::default);
        // Not held while building, so a step's panic poisons nothing.
        let memo = || MEMO.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(hit) = memo().get(&key) {
            return hit.clone();
        }
        let built = Sim::cached(key, || self.run());
        memo().insert(key, built.clone());
        built
    }

    fn with(mut self, step: Step) -> Self {
        self.steps.push(step);
        self
    }

    fn run(&self) -> Sim {
        let mut sim = Sim::new();
        sim.config = self.config.clone();
        for (i, step) in self.steps.iter().enumerate() {
            let ran = catch_unwind(AssertUnwindSafe(|| run_step(&mut sim, step)));
            let failure = match ran {
                Ok(Ok(())) => continue,
                Ok(Err(out)) => format!("failed:\n{out}"),
                Err(payload) => match panic_text(&*payload) {
                    Some(msg) => format!("panicked: {msg}"),
                    None => resume_unwind(payload),
                },
            };
            panic!("Fixture step {} of {}, {step}, {failure}", i + 1, self.steps.len());
        }
        sim.settle();
        sim
    }
}

/// Runs one step; a call that fails gives its outcome, rendered.
fn run_step(sim: &mut Sim, step: &Step) -> Result<(), String> {
    let must_succeed =
        |out: TxOutcome| if out.is_success() { Ok(()) } else { Err(out.to_string()) };
    match step {
        Step::Account { id, balance } => {
            sim.create_account(id, *balance);
        }
        Step::Contract { id, wasm } => sim.deploy(id, wasm.clone()),
        Step::Call { signer, receiver, method, args, deposit, gas, .. } => {
            let out = sim.call(signer, receiver, method).args(args.clone()).deposit(*deposit);
            let out = out.gas(*gas);
            must_succeed(out.transact())?;
        }
        Step::FtRegister { token, accounts } => {
            let bounds = sim.view(token, "storage_balance_bounds", ()).json::<serde_json::Value>();
            let min = bounds["min"].as_str().and_then(|m| m.parse().ok());
            let min = min.unwrap_or_else(|| panic!("storage_balance_bounds has no min: {bounds}"));
            for account in accounts {
                let out = sim
                    .call(token, token, "storage_deposit")
                    .args_json(serde_json::json!({ "account_id": account }))
                    .deposit(Balance::from_yoctonear(min))
                    .transact();
                must_succeed(out)?;
            }
        }
        Step::Stub { id, stub } => sim.deploy_stub(id, stub.0.clone()),
        Step::Setup { step, .. } => (step.0)(sim),
        Step::Token(t) => sim.watch(t.clone()),
    }
    Ok(())
}
