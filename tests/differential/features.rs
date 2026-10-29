//! Hand-written differential scenarios for the protocol features the random
//! generator doesn't reach: gas keys, global contracts, NEP-616 state inits,
//! meta transactions (`Delegate`, and `DelegateV2`'s rejection at protocol 87)
//! and secp256k1 / ML-DSA-65 keys.
//!
//! They run after the random scenarios on the same chain (and the same
//! `Sim`, so both tries keep the same key set). The sandbox side builds and
//! signs everything with near-kit, the simulator side with nearcore's types
//! through the simulator's `Tx`, from one shared description.
//!
//! Compared, per scenario:
//! - account views after setup and at the end, incl. the global contract
//!   fields;
//! - per tx: an invalid tx must be invalid on both sides with the same
//!   `InvalidTxError` kind (the simulator records it as the tx's failure,
//!   the sandbox's RPC refuses it); otherwise the receipt tree, as for the
//!   random scenarios;
//! - access key lists (permission, allowance, gas-key balance; nonces
//!   relative to their value after setup, or to the height-derived start
//!   value of a key added later), gas-key nonces (relative) and contract
//!   state.

use crate::Mismatch;
use crate::fuzz_common::*;
use crate::sandbox::Sandbox;
use near_contract_sim::nearcore::near_crypto::{self as nc, InMemorySigner};
use near_contract_sim::nearcore::near_primitives as np;
use near_contract_sim::{Balance, Gas, KeyType, NonceMode, Sim};
use near_kit::protocol as kp;
use near_kit::signer as ks;
use near_kit::{AccountId, CryptoHash, NearToken};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::str::FromStr;

/// New access keys start at `(height - 1) * 1e6`.
const NONCE_RANGE: u64 = 1_000_000;
const DROPPED: &str = "dropped: accepted by the RPC but never included";
const CALL_GAS: u64 = 30 * TGAS;
/// Blocks after which a sent tx with no status counts as dropped (a valid
/// one is in within 2 or 3).
const DROP_BLOCKS: u64 = 12;

// ---------------------------------------------------------------------------
// Scenario description
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Kind {
    Ed25519,
    Secp256k1,
    MlDsa65,
}

/// A deterministic key: near-crypto's `SecretKey::from_seed(kind, seed)`.
#[derive(Clone, Debug)]
pub struct KeySpec {
    pub kind: Kind,
    pub seed: String,
}

impl KeySpec {
    pub fn sim(&self) -> nc::SecretKey {
        let t = match self.kind {
            Kind::Ed25519 => KeyType::ED25519,
            Kind::Secp256k1 => KeyType::SECP256K1,
            Kind::MlDsa65 => KeyType::MLDSA65,
        };
        nc::SecretKey::from_seed(t, &self.seed)
    }

    /// The same key for near-kit. near-crypto prints an ML-DSA-65 secret key
    /// as the 4032-byte expanded key, which near-kit can't parse (it only
    /// takes the 32-byte FIPS 204 seed), so the ML-DSA key is rebuilt from
    /// the seed bytes near-crypto's `from_seed` uses: the seed string's
    /// first 32 bytes, space-padded. Checked by comparing the public keys.
    pub fn kit(&self) -> ks::SecretKey {
        let k = match self.kind {
            Kind::MlDsa65 => {
                let mut seed = [b' '; 32];
                let b = self.seed.as_bytes();
                let n = b.len().min(32);
                seed[..n].copy_from_slice(&b[..n]);
                ks::SecretKey::ml_dsa65_from_seed(seed)
            }
            _ => ks::SecretKey::from_str(&self.sim().to_string()).expect("near-kit secret key"),
        };
        assert_eq!(
            k.public_key().to_string(),
            self.sim().public_key().to_string(),
            "near-kit and near-crypto derive different {:?} keys from seed {:?}",
            self.kind,
            self.seed
        );
        k
    }
}

#[derive(Clone, Copy, Debug)]
pub enum Code {
    Fuzz,
    Status,
    Probe,
}

impl Code {
    fn wasm(self) -> Vec<u8> {
        let name = match self {
            Code::Fuzz => return fuzz_wasm(),
            Code::Status => "status_message.wasm",
            Code::Probe => "probe.wasm",
        };
        let dir = std::env::var_os("CARGO_MANIFEST_DIR")
            .expect("CARGO_MANIFEST_DIR unset: run under cargo");
        let p = std::path::Path::new(&dir).join("tests/res").join(name);
        std::fs::read(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display()))
    }
}

/// Nonce relative to the key's current one (or, for a gas key, to the
/// current nonce of the chosen index).
#[derive(Clone, Copy, Debug)]
pub enum N {
    Next,
    Same,
    Plus(u64),
    /// Beyond the `height * 1e6` upper bound.
    TooLarge,
}

fn pick(n: N, cur: u64, height: u64) -> u64 {
    match n {
        N::Next => cur + 1,
        N::Same => cur,
        N::Plus(k) => cur + k,
        N::TooLarge => (height + 10_000) * NONCE_RANGE,
    }
}

/// Account ids in actions are roles, resolved by `Feature::id`.
#[derive(Clone, Debug)]
pub enum FAct {
    /// The fuzz contract's `run` with a program, 30 Tgas.
    Run {
        program: Vec<u8>,
        deposit: u128,
    },
    Call {
        method: String,
        args: Vec<u8>,
    },
    Transfer(u128),
    CreateAccount,
    AddFullKey(KeySpec),
    AddFcKey {
        key: KeySpec,
        receiver: String,
        methods: Vec<String>,
        allowance: Option<u128>,
    },
    /// `fc`: function-call gas key restricted to (receiver, methods).
    AddGasKey {
        key: KeySpec,
        num_nonces: u16,
        fc: Option<(String, Vec<String>)>,
    },
    TransferToGasKey(KeySpec, u128),
    WithdrawFromGasKey(KeySpec, u128),
    DeleteKey(KeySpec),
    DeployGlobal {
        code: Code,
        by_account: bool,
    },
    UseGlobalHash(Code),
    UseGlobalAccount(String),
    Delegate(Box<FDelegate>),
    /// A nearcore action, alone in its transaction, which nearcore's types
    /// sign on both sides: near-kit has no `UniversalStateInit`.
    Raw(np::action::Action),
}

#[derive(Clone, Debug)]
pub struct FDelegate {
    pub sender: String,
    pub receiver: String,
    /// `None`: the sender's account key.
    pub key: Option<KeySpec>,
    pub nonce: N,
    /// `Some(i)`: a `DelegateV2` using gas-key nonce index `i`.
    pub v2_index: Option<u16>,
    /// `max_block_height = height + ttl`.
    pub ttl: i64,
    /// Change the action after signing it.
    pub tamper: bool,
    pub actions: Vec<FAct>,
}

impl FDelegate {
    fn key(mut self, k: &KeySpec) -> Self {
        self.key = Some(k.clone());
        self
    }
    fn nonce(mut self, n: N) -> Self {
        self.nonce = n;
        self
    }
    fn ttl(mut self, t: i64) -> Self {
        self.ttl = t;
        self
    }
    fn tamper(mut self) -> Self {
        self.tamper = true;
        self
    }
    fn v2(mut self, index: u16) -> Self {
        self.v2_index = Some(index);
        self
    }
    fn act(self) -> FAct {
        FAct::Delegate(Box::new(self))
    }
}

fn dl(sender: &str, receiver: &str, actions: Vec<FAct>) -> FDelegate {
    FDelegate {
        sender: sender.into(),
        receiver: receiver.into(),
        key: None,
        nonce: N::Next,
        v2_index: None,
        ttl: 100,
        tamper: false,
        actions,
    }
}

#[derive(Clone, Debug)]
pub struct FTx {
    pub signer: String,
    pub receiver: String,
    /// `None`: the signer's account key.
    pub key: Option<KeySpec>,
    pub nonce_index: Option<u16>,
    pub nonce: N,
    pub strict: bool,
    pub actions: Vec<FAct>,
}

impl FTx {
    fn key(mut self, k: &KeySpec) -> Self {
        self.key = Some(k.clone());
        self
    }
    fn idx(mut self, i: u16) -> Self {
        self.nonce_index = Some(i);
        self
    }
    fn nonce(mut self, n: N) -> Self {
        self.nonce = n;
        self
    }
    fn strict(mut self) -> Self {
        self.strict = true;
        self
    }
    fn label(&self) -> String {
        let names: Vec<&str> = self.actions.iter().map(act_name).collect();
        let key = self.key.as_ref().map_or(String::new(), |k| format!(" key {}", k.seed));
        let idx = self.nonce_index.map_or(String::new(), |i| format!("#{i}"));
        format!("{}->{}{key}{idx} {}", self.signer, self.receiver, names.join("+"))
    }
}

fn act_name(a: &FAct) -> &'static str {
    match a {
        FAct::Run { .. } => "Run",
        FAct::Call { .. } => "Call",
        FAct::Transfer(_) => "Transfer",
        FAct::CreateAccount => "CreateAccount",
        FAct::AddFullKey(_) => "AddFullKey",
        FAct::AddFcKey { .. } => "AddFcKey",
        FAct::AddGasKey { .. } => "AddGasKey",
        FAct::TransferToGasKey(..) => "TransferToGasKey",
        FAct::WithdrawFromGasKey(..) => "WithdrawFromGasKey",
        FAct::DeleteKey(_) => "DeleteKey",
        FAct::DeployGlobal { .. } => "DeployGlobal",
        FAct::UseGlobalHash(_) => "UseGlobalHash",
        FAct::UseGlobalAccount(_) => "UseGlobalAccount",
        FAct::Delegate(d) if d.v2_index.is_some() => "DelegateV2",
        FAct::Delegate(_) => "Delegate",
        FAct::Raw(_) => "Raw",
    }
}

fn tx(signer: &str, receiver: &str, actions: Vec<FAct>) -> FTx {
    FTx {
        signer: signer.into(),
        receiver: receiver.into(),
        key: None,
        nonce_index: None,
        nonce: N::Next,
        strict: false,
        actions,
    }
}

pub struct Feature {
    pub name: &'static str,
    pub prefix: String,
    pub root: String,
    /// Created before the steps (by the sandbox root; directly in the sim):
    /// (role, full-access key, code).
    pub accounts: Vec<(String, KeySpec, Option<Code>)>,
    /// Roles the steps create (compared at the end).
    pub created: Vec<String>,
    /// Gas keys whose nonces are compared at the end.
    pub gas_keys: Vec<(String, KeySpec)>,
    /// Roles whose contract state is compared at the end.
    pub state_of: Vec<String>,
    pub steps: Vec<FTx>,
}

impl Feature {
    fn new(name: &'static str, base: &str, root: &str) -> Self {
        Feature {
            name,
            prefix: format!("{base}{name}"),
            root: root.into(),
            accounts: vec![],
            created: vec![],
            gas_keys: vec![],
            state_of: vec![],
            steps: vec![],
        }
    }

    /// `u` -> `<prefix>u.<root>`; `s0.p` -> `s0.<id of p>`; a NEP-616 id,
    /// `0s…` or `0u…`, is its own role.
    pub fn id(&self, role: &str) -> String {
        if role.starts_with('0') {
            return role.into();
        }
        match role.split_once('.') {
            Some((sub, parent)) => format!("{sub}.{}", self.id(parent)),
            None => format!("{}{role}.{}", self.prefix, self.root),
        }
    }

    fn k(&self, kind: Kind, name: &str) -> KeySpec {
        // ML-DSA seeds are cut at 32 bytes: keep them short and distinct.
        KeySpec { kind, seed: format!("{}{name}", self.prefix) }
    }

    fn account(&mut self, role: &str, kind: Kind, code: Option<Code>) {
        let key = self.k(kind, role);
        self.accounts.push((role.into(), key, code));
    }

    fn account_key(&self, role: &str) -> KeySpec {
        self.accounts
            .iter()
            .find(|(r, ..)| r == role)
            .map(|(_, k, _)| k.clone())
            .unwrap_or_else(|| panic!("{}: no key for role {role}", self.name))
    }

    fn roles(&self) -> Vec<String> {
        self.accounts.iter().map(|(r, ..)| r.clone()).chain(self.created.iter().cloned()).collect()
    }
}

fn run(program: Vec<u8>) -> FAct {
    FAct::Run { program, deposit: 0 }
}
/// Logs predecessor, signer and attached deposit.
fn who() -> Vec<u8> {
    encode(&[Op::Env(4), Op::Env(5), Op::Env(7)])
}
fn write() -> Vec<u8> {
    encode(&[Op::Write { key: b"k".to_vec(), value: b"v".to_vec() }, Op::Env(4)])
}
fn fail() -> Vec<u8> {
    encode(&[Op::Log("boom".into()), Op::Panic])
}

/// Every feature scenario; account ids start with `base`.
pub fn scenarios(base: &str, root: &str) -> Vec<Feature> {
    vec![
        gas_keys(base, root),
        global_contracts(base, root),
        state_init(base, root),
        delegate(base, root),
        key_types(base, root),
    ]
}

fn gas_keys(base: &str, root: &str) -> Feature {
    use FAct::*;
    let mut f = Feature::new("gk", base, root);
    f.account("u", Kind::Ed25519, None);
    f.account("v", Kind::Ed25519, None);
    f.account("c", Kind::Ed25519, Some(Code::Fuzz));
    let (gk, fk, ek) =
        (f.k(Kind::Ed25519, "gk"), f.k(Kind::Ed25519, "fk"), f.k(Kind::Ed25519, "ek"));
    f.gas_keys = vec![("u".into(), gk.clone()), ("u".into(), fk.clone()), ("u".into(), ek.clone())];
    f.state_of = vec!["c".into()];
    f.steps = vec![
        tx(
            "u",
            "u",
            vec![
                AddGasKey { key: gk.clone(), num_nonces: 4, fc: None },
                TransferToGasKey(gk.clone(), 5 * NEAR),
            ],
        ),
        // Gas from the key, deposits from the account.
        tx("u", "c", vec![run(who())]).key(&gk).idx(0),
        tx("u", "c", vec![Run { program: write(), deposit: MILLI }]).key(&gk).idx(2),
        tx("u", "v", vec![Transfer(3 * NEAR)]).key(&gk).idx(1),
        tx("u", "c", vec![run(who())]).key(&gk).idx(3).strict(),
        // Invalid: strict mode with a gap, index out of range, gas key
        // without an index, plain key with an index, replayed nonce.
        tx("u", "c", vec![run(who())]).key(&gk).idx(3).strict().nonce(N::Plus(2)),
        tx("u", "c", vec![run(who())]).key(&gk).idx(4),
        tx("u", "c", vec![run(who())]).key(&gk),
        tx("u", "c", vec![run(who())]).idx(0),
        tx("u", "c", vec![run(who())]).key(&gk).idx(0).nonce(N::Same),
        // Funding and withdrawing.
        tx("u", "u", vec![TransferToGasKey(gk.clone(), 2 * NEAR)]),
        tx("u", "u", vec![WithdrawFromGasKey(gk.clone(), 4 * NEAR)]).key(&gk).idx(1),
        tx("u", "u", vec![WithdrawFromGasKey(gk.clone(), 10 * NEAR)]),
        // Over the 1 NEAR burn cap: refused.
        tx("u", "u", vec![DeleteKey(gk.clone())]),
        tx("u", "u", vec![WithdrawFromGasKey(gk.clone(), 2_500 * MILLI)]),
        // Function-call gas key.
        tx(
            "u",
            "u",
            vec![
                AddGasKey {
                    key: fk.clone(),
                    num_nonces: 2,
                    fc: Some(("c".into(), vec!["run".into()])),
                },
                TransferToGasKey(fk.clone(), NEAR),
            ],
        ),
        tx("u", "c", vec![run(who())]).key(&fk).idx(0),
        tx("u", "v", vec![run(who())]).key(&fk).idx(1),
        tx("u", "c", vec![Call { method: "digest".into(), args: vec![] }]).key(&fk).idx(1),
        tx("u", "c", vec![Transfer(NEAR)]).key(&fk).idx(1),
        // An empty gas key can't pay.
        tx("u", "u", vec![AddGasKey { key: ek.clone(), num_nonces: 1, fc: None }]),
        tx("u", "c", vec![run(who())]).key(&ek).idx(0),
        // Below the cap now: deleted, the rest burnt.
        tx("u", "u", vec![DeleteKey(gk.clone())]),
    ];
    f
}

fn global_contracts(base: &str, root: &str) -> Feature {
    use FAct::*;
    let mut f = Feature::new("gc", base, root);
    for r in ["p", "q", "a1", "a2"] {
        f.account(r, Kind::Ed25519, None);
    }
    f.account("c", Kind::Ed25519, Some(Code::Fuzz));
    f.created = vec!["s0.p".into()];
    f.state_of = vec!["a1".into(), "a2".into(), "c".into(), "s0.p".into()];
    let set_status = || Call { method: "set_status".into(), args: br#"{"message":"v1"}"#.to_vec() };
    let s0_key = f.k(Kind::Ed25519, "s0");
    f.steps = vec![
        // By code hash.
        tx("p", "p", vec![DeployGlobal { code: Code::Fuzz, by_account: false }]),
        tx("a1", "a1", vec![UseGlobalHash(Code::Fuzz)]),
        tx("q", "a1", vec![run(write())]),
        // By account id, then upgraded under the same id.
        tx("p", "p", vec![DeployGlobal { code: Code::Status, by_account: true }]),
        tx("a2", "a2", vec![UseGlobalAccount("p".into())]),
        tx("q", "a2", vec![set_status()]),
        tx("p", "p", vec![DeployGlobal { code: Code::Fuzz, by_account: true }]),
        tx("q", "a2", vec![run(who())]),
        tx("q", "a2", vec![set_status()]),
        // Never published.
        tx("a1", "a1", vec![UseGlobalHash(Code::Probe)]),
        // Local code -> global code.
        tx("c", "c", vec![UseGlobalAccount("p".into())]),
        tx("q", "c", vec![run(who())]),
        // Create an account and point it at a global contract in one batch.
        tx(
            "p",
            "s0.p",
            vec![CreateAccount, Transfer(5 * NEAR), AddFullKey(s0_key), UseGlobalHash(Code::Fuzz)],
        ),
        tx("q", "s0.p", vec![run(write())]),
    ];
    f
}

/// A deterministic (`0s`) account running a global contract, and a
/// universal (`0u`) one holding a key, which then signs.
fn state_init(base: &str, root: &str) -> Feature {
    use FAct::*;
    use np::action::{Action, DeterministicStateInitAction, UniversalStateInitAction};
    use np::deterministic_account_id::*;
    use np::global_contract::GlobalContractIdentifier;
    use np::universal_state_init::{UniversalStateInit, UniversalStateInitV1};
    use np::utils::{derive_near_deterministic_account_id, derive_universal_account_id};
    let mut f = Feature::new("si", base, root);
    f.account("p", Kind::Ed25519, None);
    f.account("u", Kind::Ed25519, None);
    let code = GlobalContractIdentifier::CodeHash(np::hash::hash(&Code::Fuzz.wasm()));
    let det = |value: &[u8], deposit| {
        let data = [(b"k".to_vec(), [f.prefix.as_bytes(), value].concat())].into();
        let state_init = DeterministicAccountStateInit::V1(DeterministicAccountStateInitV1 {
            code: code.clone(),
            data,
        });
        let id = derive_near_deterministic_account_id(&state_init).to_string();
        let init =
            DeterministicStateInitAction { state_init, deposit: Balance::from_yoctonear(deposit) };
        (id, Raw(Action::DeterministicStateInit(Box::new(init))))
    };
    let key = f.k(Kind::Ed25519, "uk");
    let state_init = UniversalStateInit::V1(UniversalStateInitV1 {
        code: None,
        data: Default::default(),
        access_keys: [(&key.sim().public_key()).into()].into(),
    })
    .to_raw();
    let uid = derive_universal_account_id(&state_init).to_string();
    let init = UniversalStateInitAction { state_init, deposit: Balance::from_yoctonear(NEAR) };
    let (d, init_d) = det(b"", NEAR);
    let (small, init_small) = det(&[0; 1_000], 1);
    f.created = vec![d.clone(), small.clone(), uid.clone()];
    f.state_of = vec![d.clone()];
    f.steps = vec![
        tx("p", "p", vec![DeployGlobal { code: Code::Fuzz, by_account: false }]),
        tx("u", &d, vec![init_d.clone()]),
        tx("u", &d, vec![run(write())]),
        // Again: the account exists, and the deposit comes back.
        tx("u", &d, vec![init_d]),
        // Over the 770 bytes an account stores free, with no deposit to
        // stake for them.
        tx("u", &small, vec![init_small]),
        tx("u", &uid, vec![Raw(Action::UniversalStateInit(Box::new(init)))]),
        tx("u", &uid, vec![Transfer(NEAR)]),
        tx(&uid, "u", vec![Transfer(MILLI)]).key(&key),
    ];
    f
}

fn delegate(base: &str, root: &str) -> Feature {
    use FAct::*;
    let mut f = Feature::new("dl", base, root);
    for r in ["s", "r", "t"] {
        f.account(r, Kind::Ed25519, None);
    }
    f.account("c", Kind::Ed25519, Some(Code::Fuzz));
    let (fck, wrong, gk) =
        (f.k(Kind::Ed25519, "fck"), f.k(Kind::Ed25519, "wrong"), f.k(Kind::Ed25519, "gk"));
    f.gas_keys = vec![("s".into(), gk.clone())];
    f.state_of = vec!["c".into()];
    f.steps = vec![
        tx("r", "s", vec![dl("s", "c", vec![run(who())]).act()]),
        // The inner call fails: the deposit goes back to the sender.
        tx("r", "s", vec![dl("s", "c", vec![Run { program: fail(), deposit: NEAR }]).act()]),
        tx("r", "s", vec![dl("s", "t", vec![Transfer(2 * NEAR)]).act()]),
        // Expired, replayed nonce, nonce too large, key the sender doesn't
        // have, tampered after signing, relayed to the wrong receiver.
        tx("r", "s", vec![dl("s", "c", vec![run(who())]).ttl(-1).act()]),
        tx("r", "s", vec![dl("s", "c", vec![run(who())]).nonce(N::Same).act()]),
        tx("r", "s", vec![dl("s", "c", vec![run(who())]).nonce(N::TooLarge).act()]),
        tx("r", "s", vec![dl("s", "c", vec![run(who())]).key(&wrong).act()]),
        tx("r", "s", vec![dl("s", "c", vec![run(who())]).tamper().act()]),
        tx("r", "c", vec![dl("s", "c", vec![run(who())]).act()]),
        // A function-call key signs delegates for its receiver only.
        tx(
            "s",
            "s",
            vec![AddFcKey {
                key: fck.clone(),
                receiver: "c".into(),
                methods: vec!["run".into()],
                allowance: None,
            }],
        ),
        tx("r", "s", vec![dl("s", "c", vec![run(who())]).key(&fck).act()]),
        tx("r", "s", vec![dl("s", "t", vec![run(who())]).key(&fck).act()]),
        // DelegateV2 (gas-key nonces) is rejected at protocol 87, and so is
        // a gas-key withdrawal inside a delegate.
        tx(
            "s",
            "s",
            vec![
                AddGasKey { key: gk.clone(), num_nonces: 2, fc: None },
                TransferToGasKey(gk.clone(), NEAR),
            ],
        ),
        tx("r", "s", vec![dl("s", "c", vec![run(who())]).key(&gk).v2(1).act()]),
        tx("r", "s", vec![dl("s", "s", vec![WithdrawFromGasKey(gk.clone(), MILLI)]).act()]),
    ];
    f
}

fn key_types(base: &str, root: &str) -> Feature {
    use FAct::*;
    let mut f = Feature::new("kt", base, root);
    f.account("e", Kind::Secp256k1, None);
    f.account("m", Kind::MlDsa65, None);
    f.account("r", Kind::Ed25519, None);
    f.account("c", Kind::Ed25519, Some(Code::Fuzz));
    let m2 = f.k(Kind::MlDsa65, "m2");
    let s2 = f.k(Kind::Secp256k1, "s2");
    let e2 = f.k(Kind::Ed25519, "e2");
    let mx = f.k(Kind::MlDsa65, "mx");
    let mg = f.k(Kind::MlDsa65, "mg");
    let sf = f.k(Kind::Secp256k1, "sf");
    f.gas_keys = vec![("m".into(), mg.clone())];
    f.state_of = vec!["c".into()];
    f.steps = vec![
        tx("e", "c", vec![run(who())]),
        tx("m", "c", vec![run(who())]),
        tx("m", "m", vec![AddFullKey(e2), AddFullKey(s2.clone()), AddFullKey(m2.clone())]),
        tx("m", "c", vec![run(who())]).key(&m2),
        tx("m", "c", vec![run(write())]).key(&s2),
        // Not a key of the account.
        tx("m", "c", vec![run(who())]).key(&mx),
        tx("e", "e", vec![AddFullKey(m2.clone())]),
        tx("e", "c", vec![run(who())]).key(&m2),
        // Meta transactions from ML-DSA and secp256k1 senders.
        tx("r", "m", vec![dl("m", "c", vec![run(who())]).act()]),
        tx("r", "e", vec![dl("e", "c", vec![run(who())]).act()]),
        tx("m", "m", vec![DeleteKey(s2.clone())]),
        tx("m", "c", vec![run(who())]).key(&s2),
        // An ML-DSA gas key, a secp256k1 function-call key with an allowance.
        tx(
            "m",
            "m",
            vec![
                AddGasKey { key: mg.clone(), num_nonces: 1, fc: None },
                TransferToGasKey(mg.clone(), NEAR),
            ],
        ),
        tx("m", "c", vec![run(who())]).key(&mg).idx(0),
        tx(
            "m",
            "m",
            vec![AddFcKey {
                key: sf.clone(),
                receiver: "c".into(),
                methods: vec![],
                allowance: Some(500 * MILLI),
            }],
        ),
        tx("m", "c", vec![run(who())]).key(&sf),
    ];
    f
}

// ---------------------------------------------------------------------------
// Simulator side
// ---------------------------------------------------------------------------

fn sim_nonce(sim: &Sim, account: &str, pk: &nc::PublicKey, index: Option<u16>) -> u64 {
    if let Some(n) =
        index.and_then(|i| sim.gas_key_nonces(account, pk).and_then(|v| v.get(i as usize).copied()))
    {
        return n;
    }
    sim.access_key(account, pk).map_or(0, |k| k.nonce)
}

fn sim_action(sim: &mut Sim, f: &Feature, a: &FAct) -> np::action::Action {
    use np::account::{AccessKey, AccessKeyPermission, FunctionCallPermission};
    use np::action::*;
    let y = Balance::from_yoctonear;
    let add_key =
        |public_key, access_key| Action::AddKey(Box::new(AddKeyAction { public_key, access_key }));
    match a {
        FAct::Run { program, deposit } => Action::FunctionCall(Box::new(FunctionCallAction {
            method_name: "run".into(),
            args: program.clone(),
            gas: Gas::from_gas(CALL_GAS),
            deposit: y(*deposit),
        })),
        FAct::Call { method, args } => Action::FunctionCall(Box::new(FunctionCallAction {
            method_name: method.clone(),
            args: args.clone(),
            gas: Gas::from_gas(CALL_GAS),
            deposit: Balance::ZERO,
        })),
        FAct::Transfer(d) => Action::Transfer(TransferAction { deposit: y(*d) }),
        FAct::CreateAccount => Action::CreateAccount(CreateAccountAction {}),
        FAct::AddFullKey(k) => add_key(k.sim().public_key(), AccessKey::full_access()),
        FAct::AddFcKey { key, receiver, methods, allowance } => add_key(
            key.sim().public_key(),
            AccessKey {
                nonce: 0,
                permission: AccessKeyPermission::FunctionCall(FunctionCallPermission {
                    allowance: allowance.map(y),
                    receiver_id: f.id(receiver),
                    method_names: methods.clone(),
                }),
            },
        ),
        FAct::AddGasKey { key, num_nonces, fc } => add_key(
            key.sim().public_key(),
            match fc {
                None => AccessKey::gas_key_full_access(*num_nonces),
                Some((r, m)) => AccessKey::gas_key_function_call(
                    *num_nonces,
                    FunctionCallPermission {
                        allowance: None,
                        receiver_id: f.id(r),
                        method_names: m.clone(),
                    },
                ),
            },
        ),
        FAct::TransferToGasKey(k, d) => {
            Action::TransferToGasKey(Box::new(TransferToGasKeyAction {
                public_key: k.sim().public_key(),
                deposit: y(*d),
            }))
        }
        FAct::WithdrawFromGasKey(k, a) => {
            Action::WithdrawFromGasKey(Box::new(WithdrawFromGasKeyAction {
                public_key: k.sim().public_key(),
                amount: y(*a),
            }))
        }
        FAct::DeleteKey(k) => {
            Action::DeleteKey(Box::new(DeleteKeyAction { public_key: k.sim().public_key() }))
        }
        FAct::DeployGlobal { code, by_account } => {
            Action::DeployGlobalContract(DeployGlobalContractAction {
                code: code.wasm().into(),
                deploy_mode: if *by_account {
                    GlobalContractDeployMode::AccountId
                } else {
                    GlobalContractDeployMode::CodeHash
                },
            })
        }
        FAct::UseGlobalHash(c) => Action::UseGlobalContract(Box::new(UseGlobalContractAction {
            contract_identifier: GlobalContractIdentifier::CodeHash(np::hash::hash(&c.wasm())),
        })),
        FAct::UseGlobalAccount(r) => Action::UseGlobalContract(Box::new(UseGlobalContractAction {
            contract_identifier: GlobalContractIdentifier::AccountId(f.id(r).parse().unwrap()),
        })),
        FAct::Delegate(d) => sim_delegate(sim, f, d),
        FAct::Raw(a) => a.clone(),
    }
}

fn sim_delegate(sim: &mut Sim, f: &Feature, d: &FDelegate) -> np::action::Action {
    use np::action::Action;
    use np::action::delegate::{
        DelegateActionV2, NonDelegateAction, VersionedDelegateActionPayload,
        VersionedSignedDelegateAction,
    };
    let sender = f.id(&d.sender);
    let receiver = f.id(&d.receiver);
    let sk = d.key.clone().unwrap_or_else(|| f.account_key(&d.sender)).sim();
    let cur = sim_nonce(sim, &sender, &sk.public_key(), d.v2_index);
    let nonce = pick(d.nonce, cur, sim.height());
    let max_block_height = (sim.height() as i64 + d.ttl) as u64;
    let inner: Vec<Action> = d.actions.iter().map(|a| sim_action(sim, f, a)).collect();
    if let Some(i) = d.v2_index {
        let signer = InMemorySigner::from_secret_key(sender.parse().unwrap(), sk.clone());
        let payload = VersionedDelegateActionPayload::V2(DelegateActionV2 {
            sender_id: sender.parse().unwrap(),
            receiver_id: receiver.parse().unwrap(),
            actions: inner.into_iter().map(|a| NonDelegateAction::try_from(a).unwrap()).collect(),
            nonce: np::transaction::TransactionNonce::from_nonce_and_index(nonce, i),
            max_block_height,
            public_key: sk.public_key(),
        });
        return Action::DelegateV2(Box::new(VersionedSignedDelegateAction::sign(&signer, payload)));
    }
    let mut tx = sim.tx(sender, receiver).sign_with(&sk).nonce(nonce);
    for a in inner {
        tx = tx.action(a);
    }
    let mut signed = tx.sign_delegate(max_block_height);
    if d.tamper {
        signed.delegate_action.max_block_height += 1;
    }
    Action::Delegate(Box::new(signed))
}

fn sim_submit(sim: &mut Sim, f: &Feature, t: &FTx) -> near_contract_sim::CryptoHash {
    let signer = f.id(&t.signer);
    let sk = t.key.clone().unwrap_or_else(|| f.account_key(&t.signer)).sim();
    let cur = sim_nonce(sim, &signer, &sk.public_key(), t.nonce_index);
    let nonce = pick(t.nonce, cur, sim.height());
    let actions: Vec<_> = t.actions.iter().map(|a| sim_action(sim, f, a)).collect();
    let mut tx = sim.tx(&signer, f.id(&t.receiver)).sign_with(&sk).nonce(nonce);
    if let Some(i) = t.nonce_index {
        tx = tx.nonce_index(i);
    }
    if t.strict {
        tx = tx.nonce_mode(NonceMode::Strict);
    }
    for a in actions {
        tx = tx.action(a);
    }
    tx.submit()
}

fn sim_account(sim: &Sim, id: &str) -> Value {
    match sim.account(id) {
        None => Value::Null,
        Some(a) => json!({
            "amount": a.amount.as_yoctonear().to_string(),
            "locked": a.locked.as_yoctonear().to_string(),
            "storage_usage": a.storage_usage,
            "code_hash": a.code_hash.to_string(),
            "global_contract_hash": a.global_contract_hash.map(|h| h.to_string()),
            "global_contract_account_id": a.global_contract_account_id.map(|x| x.to_string()),
        }),
    }
}

/// `view_access_key_list` shape: {public key: access key view}, empty for a
/// missing account, as a node answers.
fn sim_keys(sim: &Sim, id: &str) -> Value {
    let m: serde_json::Map<String, Value> = sim
        .access_keys(id)
        .into_iter()
        .map(|(pk, ak)| {
            (pk.to_string(), serde_json::to_value(np::views::AccessKeyView::from(ak)).unwrap())
        })
        .collect();
    Value::Object(m)
}

pub fn run_sim(mut sim: Sim, f: &Feature) -> (Sim, Value) {
    let start = Balance::from_yoctonear(START_BALANCE);
    for (role, key, code) in &f.accounts {
        let id = f.id(role);
        sim.create_account_with_key(&id, start, &key.sim());
        if let Some(c) = code {
            // In the sandbox the root pays for the deploy (see SimRunner).
            sim.deploy(&id, c.wasm());
            sim.set_balance(&id, start);
        }
    }
    let setup: BTreeMap<String, Value> =
        f.accounts.iter().map(|(r, ..)| (r.clone(), sim_account(&sim, &f.id(r)))).collect();
    let setup_keys: BTreeMap<String, Value> =
        f.accounts.iter().map(|(r, ..)| (r.clone(), sim_keys(&sim, &f.id(r)))).collect();
    let mut txs = vec![];
    for t in &f.steps {
        let h = sim_submit(&mut sim, f, t);
        sim.settle();
        txs.push(sim_tx_json(&sim.outcome(h)));
    }
    let v = json!({
        "setup_accounts": setup,
        "setup_keys": setup_keys,
        "txs": txs,
        "accounts": f.roles().iter().map(|r| (r.clone(), sim_account(&sim, &f.id(r)))).collect::<BTreeMap<_, _>>(),
        "keys": f.roles().iter().map(|r| (r.clone(), sim_keys(&sim, &f.id(r)))).collect::<BTreeMap<_, _>>(),
        "gas_key_nonces": f.gas_keys.iter().map(|(r, k)| {
            json!(sim.gas_key_nonces(f.id(r), &k.sim().public_key()))
        }).collect::<Vec<_>>(),
        "state": f.state_of.iter().map(|r| {
            let id = f.id(r);
            let kv: Vec<Value> = if sim.account(&id).is_some() {
                sim.view_state(&id, b"").iter().map(|(k, v)| json!([b64(k), b64(v)])).collect()
            } else {
                vec![]
            };
            (r.clone(), Value::Array(kv))
        }).collect::<BTreeMap<_, _>>(),
    });
    (sim, v)
}

// ---------------------------------------------------------------------------
// Sandbox side (near-kit types and signing)
// ---------------------------------------------------------------------------

fn aid(s: &str) -> AccountId {
    s.parse().unwrap_or_else(|e| panic!("account id {s}: {e:?}"))
}

async fn sb_nonce(sb: &Sandbox, account: &str, pk: &ks::PublicKey, index: Option<u16>) -> u64 {
    let q = |request_type: &str| {
        json!({"request_type": request_type, "finality": "optimistic",
               "account_id": account, "public_key": pk.to_string()})
    };
    if let Some(i) = index
        && let Ok(v) = sb.call("query", q("view_gas_key_nonces")).await
        && let Some(n) = v["nonces"].get(i as usize).and_then(Value::as_u64)
    {
        return n;
    }
    sb.call("query", q("view_access_key")).await.ok().and_then(|v| v["nonce"].as_u64()).unwrap_or(0)
}

fn kit_plain(f: &Feature, a: &FAct) -> kp::Action {
    let y = NearToken::from_yoctonear;
    let gas = near_kit::Gas::from_gas(CALL_GAS);
    let add_key = |public_key, permission| {
        kp::Action::AddKey(kp::AddKeyAction {
            public_key,
            access_key: kp::AccessKey { nonce: 0, permission },
        })
    };
    match a {
        FAct::Run { program, deposit } => {
            kp::Action::function_call("run", program.clone(), gas, y(*deposit))
        }
        FAct::Call { method, args } => {
            kp::Action::function_call(method.clone(), args.clone(), gas, y(0))
        }
        FAct::Transfer(d) => kp::Action::transfer(y(*d)),
        FAct::CreateAccount => kp::Action::create_account(),
        FAct::AddFullKey(k) => kp::Action::add_full_access_key(k.kit().public_key()),
        FAct::AddFcKey { key, receiver, methods, allowance } => kp::Action::add_function_call_key(
            key.kit().public_key(),
            aid(&f.id(receiver)),
            methods.clone(),
            allowance.map(y),
        ),
        FAct::AddGasKey { key, num_nonces, fc } => {
            let info = kp::GasKeyInfo { balance: y(0), num_nonces: *num_nonces };
            add_key(
                key.kit().public_key(),
                match fc {
                    None => kp::AccessKeyPermission::GasKeyFullAccess(info),
                    Some((r, m)) => kp::AccessKeyPermission::GasKeyFunctionCall(
                        info,
                        kp::FunctionCallPermission {
                            allowance: None,
                            receiver_id: aid(&f.id(r)),
                            method_names: m.clone(),
                        },
                    ),
                },
            )
        }
        FAct::TransferToGasKey(k, d) => {
            kp::Action::transfer_to_gas_key(k.kit().public_key(), y(*d))
        }
        FAct::WithdrawFromGasKey(k, a) => {
            kp::Action::withdraw_from_gas_key(k.kit().public_key(), y(*a))
        }
        FAct::DeleteKey(k) => kp::Action::delete_key(k.kit().public_key()),
        FAct::DeployGlobal { code, by_account } => kp::Action::publish(
            code.wasm(),
            if *by_account { kp::PublishMode::Updatable } else { kp::PublishMode::Immutable },
        ),
        FAct::UseGlobalHash(c) => kp::Action::deploy_from_hash(CryptoHash::hash(&c.wasm())),
        FAct::UseGlobalAccount(r) => kp::Action::deploy_from_account(aid(&f.id(r))),
        FAct::Delegate(_) => panic!("nested delegate action"),
        FAct::Raw(_) => panic!("a raw action signs alone"),
    }
}

async fn kit_delegate(sb: &Sandbox, f: &Feature, d: &FDelegate) -> kp::Action {
    let sender = f.id(&d.sender);
    let key = d.key.clone().unwrap_or_else(|| f.account_key(&d.sender)).kit();
    let pk = key.public_key();
    let cur = sb_nonce(sb, &sender, &pk, d.v2_index).await;
    let height = sb.height().await;
    let nonce = pick(d.nonce, cur, height);
    let max_block_height = (height as i64 + d.ttl) as u64;
    let actions: Vec<kp::NonDelegateAction> = d
        .actions
        .iter()
        .map(|a| kp::NonDelegateAction::from_action(kit_plain(f, a)).unwrap())
        .collect();
    let (sender_id, receiver_id) = (aid(&sender), aid(&f.id(&d.receiver)));
    if let Some(i) = d.v2_index {
        let payload = kp::VersionedDelegateActionPayload::V2(kp::DelegateActionV2 {
            sender_id,
            receiver_id,
            actions,
            nonce: kp::TransactionNonce::from_nonce_and_index(nonce, i),
            max_block_height,
            public_key: pk,
        });
        let signature = key.sign(payload.get_hash().as_bytes());
        return kp::Action::delegate_v2(payload.sign(signature));
    }
    let da = kp::DelegateAction {
        sender_id,
        receiver_id,
        actions,
        nonce,
        max_block_height,
        public_key: pk,
    };
    let signature = key.sign(da.get_hash().as_bytes());
    let mut signed = da.sign(signature);
    if d.tamper {
        signed.delegate_action.max_block_height += 1;
    }
    kp::Action::delegate(signed)
}

/// Sign and submit; the tx hash, or the RPC's error for an invalid tx.
async fn sb_submit(sb: &Sandbox, f: &Feature, t: &FTx) -> Result<String, String> {
    if let [FAct::Raw(a)] = &t.actions[..] {
        return sb_submit_raw(sb, f, t, a.clone()).await;
    }
    let signer = f.id(&t.signer);
    let key = t.key.clone().unwrap_or_else(|| f.account_key(&t.signer)).kit();
    let pk = key.public_key();
    let mut actions = vec![];
    for a in &t.actions {
        actions.push(match a {
            FAct::Delegate(d) => kit_delegate(sb, f, d).await,
            a => kit_plain(f, a),
        });
    }
    let cur = sb_nonce(sb, &signer, &pk, t.nonce_index).await;
    let nonce = pick(t.nonce, cur, sb.height().await);
    let block = sb.call_ok("block", json!({"finality": "optimistic"})).await;
    let block_hash = CryptoHash::from_str(block["header"]["hash"].as_str().unwrap()).unwrap();
    let (signer_id, receiver_id) = (aid(&signer), aid(&f.id(&t.receiver)));
    // Same choice as the simulator: V1 only for a gas-key nonce or a nonce mode.
    let tx = if t.nonce_index.is_none() && !t.strict {
        kp::VersionedTransaction::V0(kp::Transaction::new(
            signer_id,
            pk,
            nonce,
            receiver_id,
            block_hash,
            actions,
        ))
    } else {
        kp::VersionedTransaction::V1(kp::TransactionV1 {
            signer_id,
            public_key: pk,
            nonce: match t.nonce_index {
                Some(i) => kp::TransactionNonce::from_nonce_and_index(nonce, i),
                None => kp::TransactionNonce::from_nonce(nonce),
            },
            receiver_id,
            block_hash,
            actions,
            nonce_mode: if t.strict { kp::NonceMode::Strict } else { kp::NonceMode::Monotonic },
        })
    };
    let hash = tx.get_hash();
    let signed = kp::SignedTransactionV1 { signature: key.sign(hash.as_bytes()), transaction: tx };
    sb.send(&signed.to_base64()).await.map(|()| hash.to_string())
}

async fn sb_submit_raw(
    sb: &Sandbox,
    f: &Feature,
    t: &FTx,
    action: np::action::Action,
) -> Result<String, String> {
    use np::transaction::{SignedTransaction, Transaction, TransactionV0};
    let signer = f.id(&t.signer);
    let key = t.key.clone().unwrap_or_else(|| f.account_key(&t.signer));
    let cur = sb_nonce(sb, &signer, &key.kit().public_key(), None).await;
    let block = sb.call_ok("block", json!({"finality": "optimistic"})).await;
    let tx = Transaction::V0(TransactionV0 {
        signer_id: signer.parse().unwrap(),
        public_key: key.sim().public_key(),
        nonce: pick(t.nonce, cur, sb.height().await),
        receiver_id: f.id(&t.receiver).parse().unwrap(),
        block_hash: block["header"]["hash"].as_str().unwrap().parse().unwrap(),
        actions: vec![action],
    });
    let hash = tx.get_hash_and_size().0;
    let signed = SignedTransaction::new(key.sim().sign(hash.as_ref()), tx);
    sb.send(&b64(&borsh::to_vec(&signed).unwrap())).await.map(|()| hash.to_string())
}

async fn sb_account(sb: &Sandbox, id: &str) -> Value {
    let q = json!({"request_type": "view_account", "finality": "optimistic", "account_id": id});
    match sb.call("query", q).await {
        Ok(a) => json!({
            "amount": a["amount"], "locked": a["locked"],
            "storage_usage": a["storage_usage"], "code_hash": a["code_hash"],
            "global_contract_hash": a.get("global_contract_hash").cloned().unwrap_or(Value::Null),
            "global_contract_account_id": a.get("global_contract_account_id").cloned().unwrap_or(Value::Null),
        }),
        Err(_) => Value::Null,
    }
}

async fn sb_keys(sb: &Sandbox, id: &str) -> Value {
    let q =
        json!({"request_type": "view_access_key_list", "finality": "optimistic", "account_id": id});
    match sb.call("query", q).await {
        Ok(r) => Value::Object(
            r["keys"]
                .as_array()
                .unwrap()
                .iter()
                .map(|k| (k["public_key"].as_str().unwrap().to_string(), k["access_key"].clone()))
                .collect(),
        ),
        Err(_) => Value::Null,
    }
}

pub async fn run_sandbox(sb: &mut Sandbox, f: &Feature) -> Value {
    let (root, root_key) = (sb.root.clone(), sb.root_key.clone());
    let balance = NearToken::from_yoctonear(START_BALANCE);
    for (role, key, code) in &f.accounts {
        let id = f.id(role);
        let mut actions = vec![
            kp::Action::create_account(),
            kp::Action::transfer(balance),
            kp::Action::add_full_access_key(key.kit().public_key()),
        ];
        if let Some(c) = code {
            actions.push(kp::Action::deploy_contract(c.wasm()));
        }
        let h = sb.submit(&root, &root_key, &id, actions).await.expect("setup tx");
        sb.quiesce(&[(h, root.clone())]).await;
    }
    let mut setup = BTreeMap::new();
    let mut setup_keys = BTreeMap::new();
    for (r, ..) in &f.accounts {
        setup.insert(r.clone(), sb_account(sb, &f.id(r)).await);
        setup_keys.insert(r.clone(), sb_keys(sb, &f.id(r)).await);
    }
    let mut sent: Vec<Result<String, String>> = vec![];
    let mut hashes: Vec<(String, String)> = vec![];
    for t in &f.steps {
        let mut r = sb_submit(sb, f, t).await;
        if let Ok(h) = &r {
            hashes.push((h.clone(), f.id(&t.signer)));
            if !sb.quiesce_within(&hashes, Some(DROP_BLOCKS)).await {
                // Accepted by the RPC, then never included.
                hashes.pop();
                r = Err(DROPPED.into());
            }
        }
        sent.push(r);
    }
    sb.refresh(&hashes).await;
    let txs: Vec<Value> = sent
        .iter()
        .map(|r| match r {
            Ok(h) => sb.snap[h].clone(),
            Err(e) => json!({"rpc_error": e}),
        })
        .collect();
    let mut accounts = BTreeMap::new();
    let mut keys = BTreeMap::new();
    for r in f.roles() {
        accounts.insert(r.clone(), sb_account(sb, &f.id(&r)).await);
        keys.insert(r.clone(), sb_keys(sb, &f.id(&r)).await);
    }
    let mut gas_key_nonces = vec![];
    for (r, k) in &f.gas_keys {
        let q = json!({"request_type": "view_gas_key_nonces", "finality": "optimistic",
                       "account_id": f.id(r), "public_key": k.kit().public_key().to_string()});
        gas_key_nonces.push(sb.call("query", q).await.map_or(Value::Null, |v| v["nonces"].clone()));
    }
    let mut state = BTreeMap::new();
    for r in &f.state_of {
        let q = json!({"request_type": "view_state", "finality": "optimistic",
                       "account_id": f.id(r), "prefix_base64": ""});
        let kv: Vec<Value> = match sb.call("query", q).await {
            Ok(v) => v["values"]
                .as_array()
                .unwrap()
                .iter()
                .map(|x| json!([x["key"], x["value"]]))
                .collect(),
            Err(_) => vec![],
        };
        state.insert(r.clone(), Value::Array(kv));
    }
    json!({"setup_accounts": setup, "setup_keys": setup_keys, "txs": txs, "accounts": accounts,
           "keys": keys, "gas_key_nonces": gas_key_nonces, "state": state})
}

// ---------------------------------------------------------------------------
// Comparison
// ---------------------------------------------------------------------------

/// `InvalidTxError` variant path of an invalid tx in the simulator (e.g.
/// `["InvalidAccessKeyError", "ReceiverMismatch"]`), `None` for a valid one.
fn invalid_kinds(status: &Value) -> Option<Vec<String>> {
    let mut cur = status.get("Failure")?.get("InvalidTxError")?;
    let mut kinds = vec![];
    loop {
        match cur {
            Value::String(s) => {
                kinds.push(s.clone());
                break;
            }
            Value::Object(m) if m.len() == 1 => {
                let (k, v) = m.iter().next().unwrap();
                if !k.starts_with(|c: char| c.is_ascii_uppercase()) {
                    break;
                }
                kinds.push(k.clone());
                cur = v;
            }
            _ => break,
        }
    }
    Some(kinds)
}

/// Nonces made comparable: relative to the key's nonce after setup, or for
/// a key added later, to the `(height - 1) * 1e6` it started at.
fn norm_keys(keys: &Value, setup: &Value) -> Value {
    let Some(m) = keys.as_object() else { return keys.clone() };
    let out: serde_json::Map<String, Value> = m
        .iter()
        .map(|(pk, ak)| {
            let n = ak["nonce"].as_u64().unwrap();
            let base = setup[pk]["nonce"].as_u64().unwrap_or(n - n % NONCE_RANGE);
            let mut ak = ak.clone();
            ak["nonce"] = json!(n - base);
            (pk.clone(), ak)
        })
        .collect();
    Value::Object(out)
}

/// Nonces in errors (`DelegateActionInvalidNonce`, `...NonceTooLarge`) are
/// chain-specific: keys added on chain start at `(height - 1) * 1e6`, the
/// simulator's setup keys at 0.
fn mask_nonces(mut t: NormTx) -> NormTx {
    fn mask(v: &mut Value) {
        match v {
            Value::Object(m) => {
                for (k, x) in m.iter_mut() {
                    if ["ak_nonce", "delegate_nonce", "tx_nonce", "upper_bound"]
                        .contains(&k.as_str())
                    {
                        *x = json!("*");
                    } else {
                        mask(x);
                    }
                }
            }
            Value::Array(a) => a.iter_mut().for_each(mask),
            _ => {}
        }
    }
    mask(&mut t.status);
    for n in &mut t.nodes {
        mask(&mut n.status);
    }
    t
}

fn norm_nonces(v: &Value) -> Value {
    match v.as_array() {
        Some(a) => json!(a.iter().map(|n| n.as_u64().unwrap() % NONCE_RANGE).collect::<Vec<_>>()),
        None => Value::Null,
    }
}

pub fn compare(f: &Feature, sim: &Value, sb: &Value, timing: &mut Vec<String>) -> Vec<Mismatch> {
    let p = format!("{} ({})", f.prefix, f.name);
    let mut out = vec![];
    let mut mm = |what: String, a: &Value, b: &Value| {
        out.push(Mismatch { scenario: p.clone(), what, sim: a.clone(), sandbox: b.clone() })
    };
    for (r, ..) in &f.accounts {
        let (a, b) = (&sim["setup_accounts"][r], &sb["setup_accounts"][r]);
        if a != b {
            mm(format!("setup account {r}"), a, b);
        }
    }
    let sim_txs = sim["txs"].as_array().unwrap();
    let sb_txs = sb["txs"].as_array().unwrap();
    for (side, txs) in [("sim", sim_txs), ("sandbox", sb_txs)] {
        if txs.len() != f.steps.len() {
            mm(format!("{side} tx count"), &json!(f.steps.len()), &json!(txs.len()));
        }
    }
    for (i, (t, (a, b))) in f.steps.iter().zip(sim_txs.iter().zip(sb_txs)).enumerate() {
        let label = format!("step {i} [{}]", t.label());
        if a.is_null() || b.is_null() {
            mm(format!("{label} missing result"), a, b);
            continue;
        }
        // Where the sandbox says a tx is invalid: refused by the RPC (with
        // the kind), dropped (the 2.14 sandbox's RPC accepts invalid txs and
        // the chunk producer leaves them out: no kind to compare), or
        // included and failed as invalid. Chain-specific fields (nonces,
        // balances) differ, so only the error kinds are compared.
        let sb_invalid = b.get("rpc_error").cloned().or_else(|| {
            invalid_kinds(&b["status"]).map(|k| json!(format!("in chunk: {}", k.join(" "))))
        });
        match (invalid_kinds(&a["status"]), sb_invalid.as_ref()) {
            (Some(_), Some(e)) if e == DROPPED => {}
            (Some(kinds), Some(e)) => {
                let e = e.as_str().unwrap_or_default();
                if kinds.iter().any(|k| !e.contains(k.as_str())) {
                    mm(format!("{label} invalid-tx kind"), &a["status"], &json!(e));
                }
            }
            (Some(_), None) => mm(
                format!("{label}: invalid in the sim, accepted by the sandbox"),
                &a["status"],
                &b["status"],
            ),
            (None, Some(e)) => mm(format!("{label}: not executed by the sandbox"), &a["status"], e),
            (None, None) => {
                for (w, x, y) in crate::compare_tx(
                    &p,
                    &format!("step {i}"),
                    &mask_nonces(normalize_tx(a)),
                    &mask_nonces(normalize_tx(b)),
                    false,
                    timing,
                ) {
                    mm(format!("{label} {w}"), &x, &y);
                }
            }
        }
    }
    for r in f.roles() {
        let (a, b) = (&sim["accounts"][&r], &sb["accounts"][&r]);
        if a != b {
            mm(format!("final account {r}"), a, b);
        }
        let sk = |v: &Value| norm_keys(&v["keys"][&r], &v["setup_keys"][&r]);
        let (a, b) = (sk(sim), sk(sb));
        if a != b {
            mm(format!("final access keys {r}"), &a, &b);
        }
    }
    for (i, (r, k)) in f.gas_keys.iter().enumerate() {
        let (a, b) =
            (norm_nonces(&sim["gas_key_nonces"][i]), norm_nonces(&sb["gas_key_nonces"][i]));
        if a != b {
            mm(format!("gas key nonces {r} {}", k.seed), &a, &b);
        }
    }
    for r in &f.state_of {
        let (a, b) = (crate::norm_state(&sim["state"][r]), crate::norm_state(&sb["state"][r]));
        if a != b {
            mm(format!("final state {r}"), &a, &b);
        }
    }
    out
}

#[test]
fn comparison_rejects_missing_feature_results() {
    let mut feature = Feature::new("compare", "", "near");
    feature.steps.push(tx("u", "u", vec![]));
    let complete = json!({"txs": [{
        "transaction_outcome": null, "receipts_outcome": [], "status": {"SuccessValue": ""},
    }]});
    assert!(compare(&feature, &complete, &complete, &mut vec![]).is_empty());
    for (txs, expected) in [(json!([]), "sandbox tx count"), (json!([null]), "missing result")] {
        let incomplete = json!({"txs": txs});
        let mismatches = compare(&feature, &complete, &incomplete, &mut vec![]);
        assert!(mismatches.iter().any(|m| m.what.contains(expected)), "{mismatches:?}");
    }
    let empty = json!({"txs": []});
    assert_eq!(compare(&feature, &empty, &empty, &mut vec![]).len(), 2);
}
