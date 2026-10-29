//! Shared machinery for the tests that run the fuzz contract: the property
//! tests (`tests/prop_*.rs`), the differential test against a real sandbox
//! (`tests/differential.rs`), and `chain`, `congestion`, `delegate`,
//! `invariants`, `order` and `trie_gas`:
//!
//! - `Op`: mirror of the fuzz contract's program encoding
//!   (`tests/contracts/fuzz-contract/src/lib.rs`, same borsh layout);
//! - proptest strategies for programs and scenarios;
//! - `Scenario` / `Step`: a serializable scenario (the sandbox runner
//!   `tests/differential/sandbox.rs` executes the same steps);
//! - `SimRunner`: executes a scenario in the simulator;
//! - `normalize_tx`: turns an RPC-shaped tx status (from either side) into a
//!   comparable tree with chain-specific values removed.
#![allow(dead_code)]

use base64::Engine;
use borsh::BorshSerialize;
use near_contract_sim::nearcore::near_primitives::views::ExecutionStatusView;
use near_contract_sim::{Balance, Gas, KeyType, SecretKey, Sim, TxOutcome, ViewResult};
use proptest::prelude::*;
use proptest::test_runner::{Config, TestRunner};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

pub const TGAS: u64 = 1_000_000_000_000;
pub const MILLI: u128 = 1_000_000_000_000_000_000_000;
pub const NEAR: u128 = 1_000 * MILLI;
/// Starting balance of every scenario account (contracts and users).
pub const START_BALANCE: u128 = 50 * NEAR;

/// Built from `tests/contracts/fuzz-contract` by its `build.sh` (cargo-near
/// 0.22, near-sdk 5.29.1, Rust 1.95.0, home path remapped).
pub fn fuzz_wasm() -> Vec<u8> {
    let dir =
        std::env::var_os("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR unset: run under cargo");
    let p = std::path::Path::new(&dir).join("tests/res/fuzz_contract.wasm");
    std::fs::read(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display()))
}

// ---------------------------------------------------------------------------
// Program encoding (mirror of the contract's `Op`)
// ---------------------------------------------------------------------------

#[derive(BorshSerialize, Serialize, Deserialize, Clone, Debug, PartialEq)]
pub enum Op {
    Write { key: Vec<u8>, value: Vec<u8> },
    Read { key: Vec<u8> },
    Remove { key: Vec<u8> },
    Log(String),
    Env(u8),
    Return(Vec<u8>),
    Panic,
    Call { to: String, args: Vec<u8>, deposit: u128, gas: u64, weight: u64 },
    Transfer { to: String, amount: u128 },
    Then { args: Vec<u8>, gas: u64, weight: u64 },
    And,
    PromiseReturn,
    Results,
    YieldCreate { slot: u8, args: Vec<u8>, gas: u64, weight: u64 },
    YieldResume { slot: u8, payload: Vec<u8> },
    YieldCreateWithId { id: u8, args: Vec<u8>, deposit: u128, gas: u64, weight: u64 },
    YieldResumeWithId { id: u8, payload: Vec<u8> },
    CreateAccount { name: String, amount: u128 },
    BurnGas(u32),
}

pub fn encode(p: &[Op]) -> Vec<u8> {
    borsh::to_vec(&p.to_vec()).unwrap()
}

pub fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}
pub fn unhex(s: &str) -> Vec<u8> {
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
}
pub fn b64(b: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(b)
}
pub fn unb64(s: &str) -> Vec<u8> {
    base64::engine::general_purpose::STANDARD.decode(s).unwrap()
}

// ---------------------------------------------------------------------------
// Scenario
// ---------------------------------------------------------------------------

/// Account names of one scenario. All ids are `<prefix><role>.<root>` so the
/// same ids work in the sandbox (sub-accounts of its root account) and in the
/// simulator (which accepts any id).
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Names {
    pub prefix: String,
    pub contracts: Vec<String>,
    pub users: Vec<String>,
    /// Never created: target of failing calls/transfers.
    pub missing: String,
}

impl Names {
    pub fn new(prefix: &str, n_contracts: usize, n_users: usize) -> Self {
        Self::under(prefix, "test.near", n_contracts, n_users)
    }
    pub fn under(prefix: &str, root: &str, n_contracts: usize, n_users: usize) -> Self {
        Names {
            prefix: prefix.into(),
            contracts: (0..n_contracts).map(|i| format!("{prefix}c{i}.{root}")).collect(),
            users: (0..n_users).map(|i| format!("{prefix}u{i}.{root}")).collect(),
            missing: format!("{prefix}nx.{root}"),
        }
    }
    /// Every account id a scenario can possibly touch (incl. sub-accounts
    /// that `Op::CreateAccount` may create).
    pub fn all_accounts(&self) -> Vec<String> {
        let mut v: Vec<String> = self.contracts.iter().chain(&self.users).cloned().collect();
        v.push(self.missing.clone());
        for c in &self.contracts {
            for s in SUB_NAMES {
                v.push(format!("{s}.{c}"));
            }
        }
        v
    }
    pub fn signers(&self) -> Vec<String> {
        self.contracts.iter().chain(&self.users).cloned().collect()
    }
}

pub const SUB_NAMES: [&str; 2] = ["s0", "s1"];

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(tag = "kind")]
pub enum TxAction {
    /// FunctionCall `run(program)`.
    Call {
        program_hex: String,
        gas: u64,
        deposit: String,
    },
    Transfer {
        amount: String,
    },
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(tag = "kind")]
pub enum Step {
    /// Signed transaction (both sides).
    Tx {
        signer: String,
        receiver: String,
        actions: Vec<TxAction>,
    },
    /// Several signed transactions submitted into the same block (sim only).
    TxGroup {
        txs: Vec<(String, String, Vec<TxAction>)>,
    },
    /// View call of `run(program)` (both sides).
    View {
        account: String,
        program_hex: String,
    },
    /// View call of `digest` (both sides).
    Digest {
        account: String,
    },
    /// Contract-storage patch; `value_hex: None` deletes (sim only).
    Patch {
        account: String,
        key_hex: String,
        value_hex: Option<String>,
    },
    /// Let blocks pass (sandbox: approximate; nothing in flight then).
    Wait {
        blocks: u64,
    },
    /// Clock controls (sim only).
    SetTimestamp {
        ns: u64,
    },
    AdvanceTimeSecs {
        secs: u64,
    },
    SetHeight {
        height: u64,
    },
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Scenario {
    pub names: Names,
    /// Secret keys (ed25519, `SecretKey::from_seed(account_id)`, i.e. what
    /// `Sim::create_account` uses) so the sandbox signs with the same keys.
    pub keys: BTreeMap<String, String>,
    pub start_balance: String,
    pub steps: Vec<Step>,
    /// Built to overfill chunks (see `congested_scenario_strategy_under`):
    /// the differential compare then treats block placement, trie-node
    /// accounting shared within a chunk and gas-price drift as timing.
    #[serde(default)]
    pub congested: bool,
}

impl Scenario {
    pub fn new(names: Names, steps: Vec<Step>) -> Self {
        let keys = names
            .signers()
            .into_iter()
            .map(|a| {
                let sk = SecretKey::from_seed(KeyType::ED25519, &a);
                (a, sk.to_string())
            })
            .collect();
        Scenario { names, keys, start_balance: START_BALANCE.to_string(), steps, congested: false }
    }
}

// ---------------------------------------------------------------------------
// Strategies
// ---------------------------------------------------------------------------

fn key_strategy() -> impl Strategy<Value = Vec<u8>> {
    (0u8..8).prop_map(|i| vec![b'k', b'0' + i])
}
fn bytes(max: usize) -> impl Strategy<Value = Vec<u8>> {
    prop::collection::vec(any::<u8>(), 0..max)
}
fn deposit_strategy() -> impl Strategy<Value = u128> {
    prop_oneof![4 => Just(0u128), 1 => Just(1), 1 => Just(MILLI), 1 => Just(NEAR), 1 => Just(40 * NEAR)]
}
fn fc_gas_strategy() -> impl Strategy<Value = u64> {
    prop_oneof![3 => Just(0u64), 1 => Just(2 * TGAS), 2 => Just(10 * TGAS), 1 => Just(40 * TGAS)]
}
fn weight_strategy() -> impl Strategy<Value = u64> {
    prop_oneof![1 => Just(0u64), 3 => Just(1), 1 => Just(3)]
}

fn leaf_op(view_safe: bool) -> BoxedStrategy<Op> {
    let base = prop_oneof![
        4 => (key_strategy(), bytes(6)).prop_map(|(key, value)| Op::Write { key, value }),
        3 => key_strategy().prop_map(|key| Op::Read { key }),
        2 => key_strategy().prop_map(|key| Op::Remove { key }),
        1 => prop::sample::select(vec!["hi", "", "λ-log"]).prop_map(|s| Op::Log(s.to_string())),
        4 => (0u8..13).prop_map(Op::Env),
        2 => bytes(4).prop_map(Op::Return),
        1 => (0u32..30).prop_map(Op::BurnGas),
    ];
    if view_safe {
        return base.boxed();
    }
    prop_oneof![
        12 => base,
        1 => Just(Op::Panic),
        2 => Just(Op::Results),
        1 => Just(Op::And),
        1 => Just(Op::PromiseReturn),
        2 => (0u8..2, bytes(4)).prop_map(|(slot, payload)| Op::YieldResume { slot, payload }),
        2 => (0u8..3, bytes(4)).prop_map(|(id, payload)| Op::YieldResumeWithId { id, payload }),
    ]
    .boxed()
}

/// A program of `depth` levels of nested promises. `targets` are the
/// accounts `Call`/`Transfer` may address.
pub fn program(targets: Arc<Vec<String>>, depth: u32) -> BoxedStrategy<Vec<Op>> {
    program_with(targets, depth, || leaf_op(false))
}

/// Leaf ops of congested scenarios: as `leaf_op(false)` minus the env reads
/// that legitimately differ between two chains packing the same load into
/// different chunks: `used_gas` (trie-node accounting is shared within a
/// chunk) and `account_balance` (the sandbox's gas price rises with full
/// chunks).
pub fn congested_leaf_op() -> BoxedStrategy<Op> {
    let envs: Vec<u8> = (0u8..13).filter(|k| *k != 9 && *k != 10).collect();
    prop_oneof![
        4 => (key_strategy(), bytes(6)).prop_map(|(key, value)| Op::Write { key, value }),
        3 => key_strategy().prop_map(|key| Op::Read { key }),
        2 => key_strategy().prop_map(|key| Op::Remove { key }),
        1 => prop::sample::select(vec!["hi", "", "λ-log"]).prop_map(|s| Op::Log(s.to_string())),
        4 => prop::sample::select(envs).prop_map(Op::Env),
        2 => bytes(4).prop_map(Op::Return),
        1 => (0u32..30).prop_map(Op::BurnGas),
        1 => Just(Op::Panic),
        2 => Just(Op::Results),
        1 => Just(Op::And),
        1 => Just(Op::PromiseReturn),
        2 => (0u8..2, bytes(4)).prop_map(|(slot, payload)| Op::YieldResume { slot, payload }),
        2 => (0u8..3, bytes(4)).prop_map(|(id, payload)| Op::YieldResumeWithId { id, payload }),
    ]
    .boxed()
}

pub fn program_with(
    targets: Arc<Vec<String>>,
    depth: u32,
    leaf_fn: fn() -> BoxedStrategy<Op>,
) -> BoxedStrategy<Vec<Op>> {
    let leaf = leaf_fn();
    if depth == 0 {
        return prop::collection::vec(leaf, 0..6).boxed();
    }
    let inner = program_with(targets.clone(), depth - 1, leaf_fn).prop_map(|p| encode(&p));
    let t = targets.clone();
    let composite = prop_oneof![
        4 => (prop::sample::select((*t).clone()), inner.clone(), deposit_strategy(), fc_gas_strategy(), weight_strategy())
            .prop_map(|(to, args, deposit, gas, weight)| Op::Call { to, args, deposit, gas, weight }),
        3 => (inner.clone(), fc_gas_strategy(), weight_strategy()).prop_map(|(args, gas, weight)| Op::Then { args, gas, weight }),
        1 => (prop::sample::select((*t).clone()), deposit_strategy()).prop_map(|(to, amount)| Op::Transfer { to, amount }),
        2 => (0u8..2, inner.clone(), prop_oneof![Just(5 * TGAS), Just(15 * TGAS)], weight_strategy())
            .prop_map(|(slot, args, gas, weight)| Op::YieldCreate { slot, args, gas, weight }),
        2 => (0u8..3, inner.clone(), prop_oneof![3 => Just(0u128), 1 => Just(1), 1 => Just(MILLI)], prop_oneof![Just(5 * TGAS), Just(15 * TGAS)], weight_strategy())
            .prop_map(|(id, args, deposit, gas, weight)| Op::YieldCreateWithId { id, args, deposit, gas, weight }),
        1 => (prop::sample::select(SUB_NAMES.to_vec()), prop_oneof![Just(MILLI), Just(10 * MILLI), Just(NEAR)])
            .prop_map(|(name, amount)| Op::CreateAccount { name: name.to_string(), amount }),
    ];
    prop::collection::vec(prop_oneof![3 => leaf, 2 => composite], 1..8).boxed()
}

pub fn view_program() -> BoxedStrategy<Vec<Op>> {
    // Mostly view-safe ops; occasionally a prohibited one (write/promise) to
    // compare the error path.
    prop::collection::vec(prop_oneof![8 => leaf_op(true), 1 => leaf_op(false)], 0..5).boxed()
}

fn tx_actions(targets: Arc<Vec<String>>, depth: u32) -> BoxedStrategy<Vec<TxAction>> {
    let call = |gas: u64| {
        (
            program(targets.clone(), depth),
            prop_oneof![5 => Just(0u128), 1 => Just(1), 1 => Just(NEAR)],
        )
            .prop_map(move |(p, d)| TxAction::Call {
                program_hex: hex(&encode(&p)),
                gas,
                deposit: d.to_string(),
            })
    };
    let transfer = prop_oneof![Just(1u128), Just(NEAR)]
        .prop_map(|a| TxAction::Transfer { amount: a.to_string() });
    prop_oneof![
        6 => call(300 * TGAS).prop_map(|a| vec![a]),
        1 => Just(vec![]).prop_map(|_: Vec<()>| vec![TxAction::Call { program_hex: hex(&encode(&[])), gas: 3 * TGAS / 2, deposit: "0".into() }]),
        2 => prop::collection::vec(prop_oneof![3 => call(100 * TGAS), 1 => transfer], 1..4),
    ]
    .boxed()
}

pub struct StepConfig {
    /// Include steps that only the simulator supports.
    pub sim_only: bool,
    pub depth: u32,
}

pub fn step_strategy(names: &Names, cfg: &StepConfig) -> BoxedStrategy<Step> {
    // Mostly contracts; sometimes a code-less user or a missing account.
    let mut targets: Vec<String> =
        [names.contracts.clone(), names.contracts.clone(), names.contracts.clone()].concat();
    targets.push(names.users[0].clone());
    targets.push(names.missing.clone());
    let targets = Arc::new(targets);
    let signers = names.signers();
    let contracts = names.contracts.clone();
    let mut receivers = [contracts.clone(), contracts.clone(), contracts.clone()].concat();
    receivers.push(names.users[0].clone());
    let depth = cfg.depth;

    let tx = (
        prop::sample::select(signers.clone()),
        prop::sample::select(receivers.clone()),
        tx_actions(targets.clone(), depth),
    )
        .prop_map(|(signer, receiver, actions)| Step::Tx { signer, receiver, actions });
    let view = (prop::sample::select(contracts.clone()), view_program())
        .prop_map(|(account, p)| Step::View { account, program_hex: hex(&encode(&p)) });
    let digest =
        prop::sample::select(contracts.clone()).prop_map(|account| Step::Digest { account });
    let patch = (prop::sample::select(contracts.clone()), key_strategy(), bytes(5)).prop_map(
        |(account, k, v)| Step::Patch { account, key_hex: hex(&k), value_hex: Some(hex(&v)) },
    );
    let wait = (1u64..4).prop_map(|blocks| Step::Wait { blocks });
    let common = prop_oneof![
        10 => tx,
        2 => view,
        1 => digest,
        1 => patch,
        1 => wait,
    ];
    if !cfg.sim_only {
        return common.boxed();
    }
    let group = prop::collection::vec(
        (
            prop::sample::select(signers.clone()),
            prop::sample::select(receivers.clone()),
            tx_actions(targets.clone(), depth),
        ),
        2..4,
    )
    .prop_map(|txs| Step::TxGroup { txs });
    let patch_del = (prop::sample::select(contracts.clone()), key_strategy())
        .prop_map(|(account, k)| Step::Patch { account, key_hex: hex(&k), value_hex: None });
    let clock = prop_oneof![
        // Includes past values, which `Sim::set_timestamp_ns` accepts, down
        // to the 2 s a call's first receipt needs before it.
        prop_oneof![
            Just(1_500_000_000_000_000_000u64),
            Just(1_700_000_000_000_000_000),
            Just(2_000_000_000)
        ]
        .prop_map(|ns| Step::SetTimestamp { ns }),
        (1u64..100_000).prop_map(|secs| Step::AdvanceTimeSecs { secs }),
        (2u64..5_000_000).prop_map(|height| Step::SetHeight { height }),
    ];
    // Waits on either side of a parked yield's 200-block timeout: short of
    // it (1, 100) and past it (201, 250).
    let long_wait = prop_oneof![Just(1u64), Just(100), Just(201), Just(250)]
        .prop_map(|blocks| Step::Wait { blocks });
    prop_oneof![
        12 => common,
        2 => group,
        1 => patch_del,
        2 => clock,
        1 => long_wait,
    ]
    .boxed()
}

pub fn scenario_strategy(
    prefix: String,
    cfg: StepConfig,
    max_steps: usize,
) -> BoxedStrategy<Scenario> {
    scenario_strategy_under(Names::new(&prefix, 3, 2), cfg, max_steps)
}

pub fn scenario_strategy_under(
    names: Names,
    cfg: StepConfig,
    max_steps: usize,
) -> BoxedStrategy<Scenario> {
    let steps = prop::collection::vec(step_strategy(&names, &cfg), 1..max_steps);
    steps.prop_map(move |steps| Scenario::new(names.clone(), steps)).boxed()
}

/// A proptest runner for `NCS_INV_CASES` cases (default 16).
pub fn runner() -> TestRunner {
    let cases = std::env::var("NCS_INV_CASES").ok().and_then(|s| s.parse().ok()).unwrap_or(16);
    TestRunner::new(Config { cases, failure_persistence: None, ..Config::default() })
}

/// A burst: 6-15 signed transactions submitted into the same block, each a
/// `run` call that first burns `BurnGas(burn)` (one of `burns`) and then
/// runs a random program (nested calls, callbacks, yields, storage).
///
/// One signer per burst: a chunk producer visits signers in an order of its
/// own (the pool hashes them with a per-node seed) but one signer's
/// transactions always go in nonce order, so the simulator's FIFO and a
/// real node execute the burst in the same order.
pub fn burst_step(names: &Names, burns: &'static [u32]) -> BoxedStrategy<Step> {
    let mut targets: Vec<String> = [names.contracts.clone(), names.contracts.clone()].concat();
    targets.push(names.users[0].clone());
    targets.push(names.missing.clone());
    let targets = Arc::new(targets);
    let call = (program_with(targets, 1, congested_leaf_op), prop::sample::select(burns.to_vec()))
        .prop_map(|(p, burn)| {
            let mut ops = vec![Op::BurnGas(burn)];
            ops.extend(p);
            vec![TxAction::Call {
                program_hex: hex(&encode(&ops)),
                gas: 300 * TGAS,
                deposit: "0".into(),
            }]
        });
    let group = prop::collection::vec((prop::sample::select(names.contracts.clone()), call), 6..16);
    (prop::sample::select(names.users.clone()), group)
        .prop_map(|(signer, txs)| Step::TxGroup {
            txs: txs.into_iter().map(|(r, a)| (signer.clone(), r, a)).collect(),
        })
        .boxed()
}

/// Scenarios that overfill chunks: mostly bursts, plus single calls of the
/// same kind, `digest` views and short waits. Nothing that reads a balance
/// or the used gas (see `congested_leaf_op`).
pub fn congested_scenario_strategy_under(
    names: Names,
    burns: &'static [u32],
    max_steps: usize,
) -> BoxedStrategy<Scenario> {
    let single = burst_step(&names, burns).prop_map(|s| match s {
        Step::TxGroup { mut txs } => {
            let (signer, receiver, actions) = txs.remove(0);
            Step::Tx { signer, receiver, actions }
        }
        s => s,
    });
    let digest =
        prop::sample::select(names.contracts.clone()).prop_map(|account| Step::Digest { account });
    let step = prop_oneof![
        4 => burst_step(&names, burns),
        1 => single,
        1 => digest,
        1 => (1u64..4).prop_map(|blocks| Step::Wait { blocks }),
    ];
    prop::collection::vec(step, 1..max_steps)
        .prop_map(move |steps| Scenario { congested: true, ..Scenario::new(names.clone(), steps) })
        .boxed()
}

// ---------------------------------------------------------------------------
// Simulator runner
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub enum StepOut {
    Tx(Vec<near_contract_sim::CryptoHash>),
    View(ViewResult),
    None,
}

pub struct SimRunner {
    pub sim: Sim,
    /// How many transactions the setup submitted.
    setup: usize,
}

pub fn to_actions(actions: &[TxAction]) -> Vec<near_contract_sim::Action> {
    use near_contract_sim::Action;
    use near_contract_sim::nearcore::near_primitives::action::{
        FunctionCallAction, TransferAction,
    };
    actions
        .iter()
        .map(|a| match a {
            TxAction::Call { program_hex, gas, deposit } => {
                Action::FunctionCall(Box::new(FunctionCallAction {
                    method_name: "run".into(),
                    args: unhex(program_hex),
                    gas: Gas::from_gas(*gas),
                    deposit: Balance::from_yoctonear(deposit.parse().unwrap()),
                }))
            }
            TxAction::Transfer { amount } => Action::Transfer(TransferAction {
                deposit: Balance::from_yoctonear(amount.parse().unwrap()),
            }),
        })
        .collect()
}

impl SimRunner {
    pub fn new(sc: &Scenario) -> Self {
        Self::with_sim(Sim::new(), sc)
    }

    /// Create accounts: contracts get the fuzz contract via a real deploy
    /// tx and then their balance is reset to `START_BALANCE` (in the
    /// sandbox the root account pays for the deploy, so both sides start
    /// from identical balances).
    pub fn with_sim(mut sim: Sim, sc: &Scenario) -> Self {
        let b = Balance::from_yoctonear(sc.start_balance.parse().unwrap());
        let wasm = fuzz_wasm();
        for c in &sc.names.contracts {
            sim.create_account(c, b);
            sim.deploy(c, wasm.clone());
            sim.set_balance(c, b);
        }
        for u in &sc.names.users {
            sim.create_account(u, b);
        }
        SimRunner { setup: sim.txs().len(), sim }
    }

    /// The scenario's transactions, in submission order.
    pub fn roots(&self) -> &[near_contract_sim::CryptoHash] {
        &self.sim.txs()[self.setup..]
    }

    pub fn submit(
        &mut self,
        signer: &str,
        receiver: &str,
        actions: &[TxAction],
    ) -> near_contract_sim::CryptoHash {
        let mut tx = self.sim.tx(signer, receiver);
        for a in to_actions(actions) {
            tx = tx.action(a);
        }
        tx.submit()
    }

    pub fn step(&mut self, s: &Step) -> StepOut {
        match s {
            Step::Tx { signer, receiver, actions } => {
                let h = self.submit(signer, receiver, actions);
                self.sim.settle();
                StepOut::Tx(vec![h])
            }
            Step::TxGroup { txs } => {
                let hs = txs.iter().map(|(s, r, a)| self.submit(s, r, a)).collect();
                self.sim.settle();
                StepOut::Tx(hs)
            }
            Step::View { account, program_hex } => {
                StepOut::View(self.sim.view_raw(account, "run", unhex(program_hex)))
            }
            Step::Digest { account } => StepOut::View(self.sim.view(account, "digest", ())),
            Step::Patch { account, key_hex, value_hex } => {
                let v = value_hex.as_ref().map(|v| unhex(v));
                self.sim.set_state(account, &unhex(key_hex), v.as_deref());
                StepOut::None
            }
            Step::Wait { blocks } => {
                // Blocks may fire yield timeouts; settle what they start.
                self.sim.advance_to(self.sim.height() + blocks);
                self.sim.settle();
                StepOut::None
            }
            Step::SetTimestamp { ns } => {
                self.sim.set_timestamp_ns(*ns);
                StepOut::None
            }
            Step::AdvanceTimeSecs { secs } => {
                self.sim.set_timestamp_ns(self.sim.timestamp_ns() + secs * 1_000_000_000);
                StepOut::None
            }
            Step::SetHeight { height } => {
                // Past a timeout's expiry, the next block fires it.
                self.sim.set_height(*height);
                self.sim.settle();
                StepOut::None
            }
        }
    }
}

// ---------------------------------------------------------------------------
// RPC-shaped outcome JSON + normalization
// ---------------------------------------------------------------------------

fn outcome_json(r: &near_contract_sim::ReceiptOutcome) -> Value {
    json!({
        "id": r.receipt_id.to_string(),
        "block_height": r.block_height,
        "outcome": {
            "executor_id": r.receiver_id.as_str(),
            "gas_burnt": r.gas_burnt.as_gas(),
            "tokens_burnt": r.tokens_burnt.as_yoctonear().to_string(),
            "logs": r.logs,
            "receipt_ids": r.receipt_ids.iter().map(|x| x.to_string()).collect::<Vec<_>>(),
            "status": serde_json::to_value(ExecutionStatusView::from(r.status.clone())).unwrap(),
            "gas_profile": r.gas_profile,
        }
    })
}

/// The simulator's `TxOutcome` in the shape of RPC `EXPERIMENTAL_tx_status`
/// (only the fields the normalizer reads). `block_height` is attached to
/// each outcome (the sandbox runner resolves block hashes to heights).
pub fn sim_tx_json(o: &TxOutcome) -> Value {
    json!({
        "transaction_outcome": o.transaction.as_ref().map(outcome_json),
        "receipts_outcome": o.receipts.iter().map(outcome_json).collect::<Vec<_>>(),
        "status": o.status,
    })
}

/// Log lines whose value legitimately differs between chains.
pub const CHAIN_SPECIFIC_ENV: [&str; 4] = ["e0:", "e1:", "e2:", "e3:"];

pub fn normalize_log(l: &str) -> String {
    for p in CHAIN_SPECIFIC_ENV {
        if l.starts_with(p) {
            return format!("{p}*");
        }
    }
    l.to_string()
}

/// Comparable form of one tx: nodes in DFS preorder from the transaction
/// outcome following `receipt_ids`; ids replaced by node indices.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct NormNode {
    pub executor: String,
    pub gas_burnt: u64,
    pub tokens_burnt: String,
    pub logs: Vec<String>,
    pub status: Value,
    /// Child node indices (`None` = listed but never executed).
    pub children: Vec<Option<usize>>,
    /// Block height delta to the parent node (`None` for the root).
    pub dh: Option<i64>,
    pub gas_profile: BTreeMap<String, u64>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct NormTx {
    pub status: Value,
    pub nodes: Vec<NormNode>,
}

fn as_u64(v: &Value) -> u64 {
    v.as_u64()
        .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
        .unwrap_or_else(|| panic!("not a u64: {v}"))
}

fn norm_status(s: &Value, idx: &HashMap<String, usize>) -> Value {
    if let Some(r) = s.get("SuccessReceiptId").and_then(Value::as_str) {
        return json!({"SuccessReceiptId": idx.get(r).map_or(json!("?"), |i| json!(i))});
    }
    s.clone()
}

pub fn norm_final_status(s: &Value) -> Value {
    match s {
        Value::String(x) if x == "NotStarted" || x == "Started" => json!("Pending"),
        s => s.clone(),
    }
}

pub fn normalize_tx(v: &Value) -> NormTx {
    let mut by_id: HashMap<String, &Value> = HashMap::new();
    for r in v["receipts_outcome"].as_array().unwrap() {
        by_id.insert(r["id"].as_str().unwrap().to_string(), r);
    }
    let root = if v["transaction_outcome"].is_null() {
        v["receipts_outcome"].as_array().unwrap().first().cloned()
    } else {
        Some(v["transaction_outcome"].clone())
    };
    let Some(root) = root else {
        return NormTx { status: norm_final_status(&v["status"]), nodes: vec![] };
    };
    // DFS preorder.
    let mut order: Vec<(Value, Option<(usize, u64)>)> = vec![];
    let mut idx: HashMap<String, usize> = HashMap::new();
    let mut stack: Vec<(Value, Option<(usize, u64)>)> = vec![(root, None)];
    while let Some((node, parent)) = stack.pop() {
        let id = node["id"].as_str().unwrap().to_string();
        if idx.contains_key(&id) {
            continue;
        }
        idx.insert(id, order.len());
        let me = order.len();
        let h = as_u64(&node["block_height"]);
        let kids: Vec<Value> = node["outcome"]["receipt_ids"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|c| by_id.get(c.as_str().unwrap()).map(|x| (*x).clone()))
            .collect();
        order.push((node, parent));
        for k in kids.into_iter().rev() {
            stack.push((k, Some((me, h))));
        }
    }
    let nodes = order
        .iter()
        .map(|(n, parent)| {
            let o = &n["outcome"];
            let h = as_u64(&n["block_height"]) as i64;
            let gas_profile = match &o["gas_profile"] {
                Value::Object(m) => m.iter().map(|(k, v)| (k.clone(), as_u64(v))).collect(),
                Value::Array(a) => a
                    .iter()
                    .filter(|c| as_u64(&c["gas_used"]) > 0)
                    .map(|c| (c["cost"].as_str().unwrap().to_string(), as_u64(&c["gas_used"])))
                    .collect(),
                _ => BTreeMap::new(),
            };
            NormNode {
                executor: o["executor_id"].as_str().unwrap().to_string(),
                gas_burnt: as_u64(&o["gas_burnt"]),
                tokens_burnt: o["tokens_burnt"].as_str().unwrap().to_string(),
                logs: o["logs"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|l| normalize_log(l.as_str().unwrap()))
                    .collect(),
                status: norm_status(&o["status"], &idx),
                children: o["receipt_ids"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|c| idx.get(c.as_str().unwrap()).copied())
                    .collect(),
                dh: parent.map(|(_, ph)| h - ph as i64),
                gas_profile,
            }
        })
        .collect();
    NormTx { status: norm_final_status(&v["status"]), nodes }
}

// ---------------------------------------------------------------------------
// Mainnet-shaped state (trie-node gas depends on every key in the trie)
// ---------------------------------------------------------------------------

use near_contract_sim::nearcore::near_primitives::account::{
    Account as NearAccount, AccountContract,
};
use near_contract_sim::{AccessKey, StateRecord};

/// Deterministic pseudo-random bytes: sha256(tag || i).
fn prng(tag: &str, i: u64) -> [u8; 32] {
    let mut b = tag.as_bytes().to_vec();
    b.extend(i.to_le_bytes());
    near_contract_sim::nearcore::near_primitives::hash::hash(&b).0
}

/// Account ids with roughly mainnet's mix: 64-hex implicit accounts, named
/// `.near` accounts and sub-accounts, and app namespaces (`.tg`, `.sweat`,
/// `.kaiching`). `tag` keeps different sets disjoint.
pub fn mainnet_like_ids(n: usize, tag: &str) -> Vec<String> {
    const WORDS: [&str; 12] = [
        "alice", "bob", "wallet", "degen", "nft", "stake", "pool", "dao", "trade", "moon", "sun",
        "user",
    ];
    (0..n as u64)
        .map(|i| {
            let r = prng(tag, i);
            let w = WORDS[r[1] as usize % WORDS.len()];
            let k = u32::from_le_bytes([r[2], r[3], r[4], 0]);
            match r[0] % 20 {
                0..=7 => hex(&r),
                8..=13 => format!("{w}{k}{}.near", &tag[..tag.len().min(4)]),
                14..=15 => format!("{w}{k}.{}.near", WORDS[r[5] as usize % WORDS.len()]),
                16..=17 => format!("{}{k}.tg", &hex(&r[6..10])),
                18 => format!("{}.u.sweat", &hex(&r[6..12])),
                _ => format!("{w}{k}.users.kaiching"),
            }
        })
        .collect()
}

/// Account + full-access ed25519 key records for `ids`, with the
/// `storage_usage` `Sim::create_account` would compute.
pub fn account_records(ids: &[String]) -> Vec<StateRecord> {
    ids.iter()
        .flat_map(|id| {
            let account_id: near_contract_sim::AccountId = id.parse().unwrap();
            let pk = SecretKey::from_seed(KeyType::ED25519, id).public_key();
            let ak = AccessKey::full_access();
            let usage =
                100 + 40 + pk.trie_id_len() as u64 + borsh::object_length(&ak).unwrap() as u64;
            let acc = NearAccount::new(
                Balance::from_near(5),
                Balance::ZERO,
                AccountContract::None,
                usage,
            );
            [
                StateRecord::Account { account_id: account_id.clone(), account: acc },
                StateRecord::access_key(account_id, pk, ak),
            ]
        })
        .collect()
}

/// Contract state for every 10th of `ids` (as if a contract lived there): a
/// `STATE` entry and one `LookupMap` entry. The trie keeps all contract data
/// under one column, so the path to a contract's storage depends on how many
/// other accounts hold contract data, not on the number of accounts.
pub fn contract_state_records(ids: &[String]) -> Vec<StateRecord> {
    ids.iter()
        .step_by(10)
        .enumerate()
        .flat_map(|(i, id)| {
            let account_id: near_contract_sim::AccountId = id.parse().unwrap();
            let mut k = b"a".to_vec();
            k.extend(borsh::to_vec(&ids[(i * 7 + 3) % ids.len()]).unwrap());
            [(b"STATE".to_vec(), prng(id, 0).to_vec()), (k, (i as u128).to_le_bytes().to_vec())]
                .map(|(k, v)| StateRecord::Data {
                    account_id: account_id.clone(),
                    data_key: k.into(),
                    value: v.into(),
                })
        })
        .collect()
}

/// A contract account holding `wasm` and `n` storage entries shaped like a
/// near-sdk token contract: half `LookupMap<AccountId, u128>` entries (`a` +
/// borsh account id -> borsh u128), half `Vector` entries (`v` + u64 index ->
/// 64 bytes). `storage_usage` matches the records.
pub fn big_contract_records(id: &str, wasm: &[u8], n: usize) -> Vec<StateRecord> {
    let account_id: near_contract_sim::AccountId = id.parse().unwrap();
    let holders = mainnet_like_ids(n / 2, &format!("holder-{id}"));
    let mut data: BTreeMap<Vec<u8>, Vec<u8>> = holders
        .iter()
        .enumerate()
        .map(|(i, h)| {
            let mut k = b"a".to_vec();
            k.extend(borsh::to_vec(h).unwrap());
            (k, (1_000_000u128 * (i as u128 + 1)).to_le_bytes().to_vec())
        })
        .collect();
    for i in 0..(n - n / 2) as u64 {
        let mut k = b"v".to_vec();
        k.extend(i.to_le_bytes());
        data.insert(k, [prng(id, i), prng(id, !i)].concat());
    }
    let data_bytes: u64 = data.iter().map(|(k, v)| 40 + (k.len() + v.len()) as u64).sum();
    let code_hash = near_contract_sim::nearcore::near_primitives::hash::hash(wasm);
    let usage = 100 + wasm.len() as u64 + data_bytes;
    let acc = NearAccount::new(
        Balance::from_near(10_000),
        Balance::ZERO,
        AccountContract::Local(code_hash),
        usage,
    );
    let mut out = vec![
        StateRecord::Account { account_id: account_id.clone(), account: acc },
        StateRecord::Contract { account_id: account_id.clone(), code: wasm.to_vec() },
    ];
    out.extend(data.into_iter().map(|(k, v)| StateRecord::Data {
        account_id: account_id.clone(),
        data_key: k.into(),
        value: v.into(),
    }));
    out
}

/// Key of the `i`th `LookupMap` entry of `big_contract_records(id, _, _)`.
pub fn big_contract_holder_key(id: &str, n: usize, i: usize) -> Vec<u8> {
    let h = &mainnet_like_ids(n / 2, &format!("holder-{id}"))[i];
    let mut k = b"a".to_vec();
    k.extend(borsh::to_vec(h).unwrap());
    k
}
