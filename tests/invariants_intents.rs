//! Invariant testing showcase: near/intents (`intents.near`) with wNEAR.
//!
//! The generated search does not encode a bug. Three users hold wNEAR
//! inside intents; each op is "a random user signs a random intent
//! (transfer, native_withdraw or ft_withdraw) with a random amount to a
//! random receiver, and a relayer submits it", at the gas the plan
//! explores. Its checks are the built-ins and the actual wNEAR
//! backing/supply/ledger. Exact plans retain the June 2025 failure fixed by
//! near/intents#102 in v0.2.5 and v0.2.6; v0.2.8 runs generated plans.
//! `do_native_withdraw_headroom` shows the bug in `headroom()`.
//! Fixtures: `tests/res/intents/README.md`.

use base64::Engine;
use near_contract_sim::explore::{
    Amount, Faults, Gap, Harness, Op, Plan, Skip, Status, amount, plans,
};
use near_contract_sim::{
    AccountId, Balance, CryptoHash, Delta, Failure, Fault, Gas, KeyType, Receipts, SecretKey, Sim,
    Token, Tx, TxOutcome, builtin,
};
use proptest::prelude::*;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

const WNEAR: &str = "wrap.near";
const INTENTS: &str = "intents.near";
const RELAYER: &str = "relayer.near";

/// User 0 withdraws one yocto of wNEAR as NEAR to `to`, at `ggas`.
fn withdrawal(to: Receiver, ggas: u64) -> Plan<IntentOp> {
    let args = Args { user: 0, to, amount: Amount::One };
    Plan::new().op(IntentOp::NativeWithdraw(args)).gas(Gas::from_gigagas(ggas))
}

/// Known failures use exact plans, independent of RNG implementation.
#[test]
fn intents_v0_2_5_loses_funds() {
    let plan = withdrawal(Receiver::Near(0), 15_010);
    let run = intents("v0.2.5").run(&plan);
    let lost = run.findings().iter().find(|f| f.check == builtin::UNHANDLED_FAILURE);
    assert!(lost.is_some(), "{run}");
    // Saved and reproduced on a freshly built fixture: the signing nonce and
    // the runtime state reproduce alongside the plan.
    let dir = std::env::temp_dir().join(format!("ncs-intents-{}", std::process::id()));
    let report = intents("v0.2.5").store(&dir).scan(&[plan]).faults(Faults::none()).run();
    let found = report.found().iter().find(|f| f.finding.check == builtin::UNHANDLED_FAILURE);
    found.and_then(|f| f.replay.clone()).unwrap_or_else(|| panic!("{report}"));
    let again = intents("v0.2.5").store(&dir).regressions();
    let shows = again.iter().find(|r| r.check == builtin::UNHANDLED_FAILURE);
    match shows.map(|r| &r.status) {
        Some(Status::Shows(again)) => assert_eq!(again.findings(), run.findings()),
        _ => panic!("the replay doesn't show: {again:#?}"),
    }
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn intents_v0_2_6_loses_funds() {
    let run = intents("v0.2.6").run(&withdrawal(Receiver::Known(3), 15_020));
    assert!(run.findings().iter().any(|f| f.check == builtin::UNHANDLED_FAILURE), "{run}");
}

/// A real contract search resolves each amount from its current balance,
/// signs with the world's current nonce, and checks all wNEAR books at rest.
/// Each op settles before the next. No faults: what a failed `near_withdraw`
/// does is pinned apart, below.
#[test]
fn intents_v0_2_8_passes() {
    let ops = plans(intent_ops()).len(1..=8).gaps(Just(Gap::Settle)).faults(Faults::none());
    let report = intents("v0.2.8").search(ops).seed(0x5eed).cases(32).run();
    report.assert_clean();
    assert_eq!(report.cases(), 32, "{report}");
}

/// Overlapping signed intents, with the default block gaps.
#[test]
fn signed_intents_can_overlap() {
    let ops = plans(intent_ops()).len(2..=5).faults(Faults::none());
    intents("v0.2.8").search(ops).seed(0x5eed).cases(12).run().assert_clean();
}

/// What the default faults find on v0.2.8: if wrap.near's `near_withdraw`
/// fails, `do_native_withdraw` panics and the withdrawn wNEAR leaves the
/// ledger while wrap.near still holds it. Both findings are labelled
/// injected: a consequence of the forced failure, not evidence that
/// wrap.near fails on a network.
#[test]
fn a_failed_near_withdraw_unbalances_v0_2_8() {
    let args = Args { user: 0, to: Receiver::Eth(39), amount: Amount::Fraction(2, 4) };
    let plan = Plan::new()
        .op(IntentOp::NativeWithdraw(args))
        .inject(Receipts::callback("do_native_withdraw"), Fault::PromiseFailed);
    let run = intents("v0.2.8").run(&plan);
    let lost = run.assert_found(builtin::UNHANDLED_FAILURE, "intents.near::do_native_withdraw");
    assert!(lost.injected && lost.message.contains("near_withdraw failed"), "{run}");
    assert!(run.assert_found("wNEAR books balance", "world").injected, "{run}");
    let plain = Plan::new().op(IntentOp::NativeWithdraw(Args {
        user: 0,
        to: Receiver::Eth(39),
        amount: Amount::Fraction(2, 4),
    }));
    intents("v0.2.8").run(&plain).assert_clean();
}

/// Two signed payloads from the same user are queued before producing a block.
/// Check their effects, so submitting two rejected transactions cannot pass.
#[test]
fn two_pending_signed_transfers_both_execute() {
    let initial = Balance::from_near(1_000).as_yoctonear();
    let transfer =
        IntentOp::Transfer(Args { user: 0, to: Receiver::Known(4), amount: Amount::One });
    let run = intents("v0.2.8")
        .at_rest("both transfers credited", move |world: &Intents| {
            if world.nonce < 2 {
                return Ok(());
            }
            let alice = world.balance(&world.users[0].0);
            let bob = world.balance(&world.users[1].0);
            if alice == initial - 2 && bob == initial + 2 {
                Ok(())
            } else {
                Err(format!("after two transfers: alice {alice}, bob {bob}"))
            }
        })
        .run(&Plan::new().op(transfer.clone()).blocks(0).op(transfer));
    run.assert_clean();
}

/// A NEP-245 multi-token is watched one token id at a time: intents'
/// wNEAR, `Token::nep245(INTENTS, "nep141:wrap.near")`, measures a signed
/// transfer per holder, and `token_net` takes the contract and the token
/// id. Its users exist only inside intents, so they are declared holders.
/// `nep245_ids` watches several ids with the same holders, each apart.
#[test]
fn a_multi_token_is_watched_per_token_id() {
    const WNEAR_ID: &str = "nep141:wrap.near";
    let mut world = Intents::setup("v0.2.8");
    let users: Vec<AccountId> = world.users.iter().map(|(u, _)| u.clone()).collect();
    let ids = [WNEAR_ID, "nep141:usdc.near"];
    world.sim.watch(Token::nep245_ids(INTENTS, ids).holders(&users));
    let transfer =
        json!({"intent": "transfer", "receiver_id": users[1], "tokens": {WNEAR_ID: "5"}});
    let out = world.transaction(0, transfer).transact();
    out.assert_success();
    let e = out.effects();
    assert_eq!(e.token_net((INTENTS, WNEAR_ID), &users[0]), Ok(Delta::from(-5)), "{out}");
    assert_eq!(e.token_net((INTENTS, WNEAR_ID), &users[1]), Ok(Delta::from(5)), "{out}");
    assert_eq!(e.token_supply_net((INTENTS, WNEAR_ID)), Ok(Delta::ZERO), "{out}");
    assert_eq!(e.token_net((INTENTS, "nep141:usdc.near"), &users[0]), Ok(Delta::ZERO), "{out}");
    let other = e.token_net((INTENTS, "nep141:other.near"), &users[0]).unwrap_err();
    assert!(other.why.contains("intents.near[nep141:other.near] is not a watched"), "{other}");
    assert!(e.token_net(INTENTS, &users[0]).is_err(), "the contract alone is another token");
}

/// Positive control: transfer external wNEAR to intents without crediting
/// its internal ledger. The very same at-rest oracle must detect it.
#[test]
fn backing_property_detects_uncredited_external_tokens() {
    let mut world = Intents::setup("v0.2.8");
    check_books(&world).unwrap();
    world
        .sim
        .call(RELAYER, WNEAR, "ft_transfer")
        .args_json(json!({"receiver_id": INTENTS, "amount": "1"}))
        .deposit(Balance::from_yoctonear(1))
        .transact()
        .assert_success();
    assert!(check_books(&world).unwrap_err().contains("external"));
}

/// The #102 tree, at the gas the repro shrank to: v0.2.5's
/// `do_native_withdraw` has 4.00 Tgas attached and burns 3.26 Tgas, 2.37 of
/// it running, yet runs out of gas, since the transfer it makes to a new
/// implicit account needs more. Attached minus what it burnt running would
/// claim 1.6 Tgas of margin; `headroom()` is 0, as for any failed receipt.
/// v0.2.8 attaches enough and keeps some.
#[test]
fn do_native_withdraw_headroom() {
    let new_implicit: AccountId = "a".repeat(64).parse().unwrap();
    let callback = |version, gas| {
        let mut intents = Intents::setup(version);
        let intent = native_withdraw(&new_implicit, "1".into());
        let out = intents.execute(0, intent, gas);
        out.assert_success().receipt("do_native_withdraw").expect("the callback ran").clone()
    };
    let cb = callback("v0.2.5", Gas::from_gigagas(15_010));
    assert_eq!(cb.failure(), Some(Failure::OutOfGas));
    let ran = cb.gas_burnt.saturating_sub(cb.prepaid_exec_fees);
    assert!(cb.attached_gas.saturating_sub(ran) > Gas::from_gigagas(1_600), "{cb:?}");
    assert_eq!(cb.headroom(), Gas::ZERO);
    let cb = callback("v0.2.8", Gas::from_teragas(30));
    assert!(cb.is_success() && cb.headroom() > Gas::ZERO, "{cb:?}");
}

/// #102's gas window: a sweep to the 1 PGas transaction cap finds where
/// v0.2.5's `native_withdraw` to a new implicit account starts and stops
/// losing the withdrawal, each edge to 10 Ggas: 15.01 to 26.81 Tgas.
#[test]
fn native_withdraw_loses_funds_in_a_gas_window() {
    let base = Intents::setup("v0.2.5");
    let to: AccountId = "a".repeat(64).parse().unwrap();
    let lost = |ggas: u64| {
        let mut intents = base.clone(); // every sample starts from the same state
        let out = intents.execute(0, native_withdraw(&to, "1".into()), Gas::from_gigagas(ggas));
        out.receipts.iter().any(|r| r.unhandled)
    };
    let grid: Vec<u64> = (1..=100).map(|t| t * 10_000).collect(); // to the 1 PGas cap
    assert_eq!(edges(&grid, 10, lost), [(15_000, 15_009), (26_805, 26_815)]);
}

/// Where `bad` flips along `grid`, each flip bisected to `step`: the last
/// value on one side and the first on the other. A band narrower than the
/// grid's spacing, between two equal samples, goes unseen.
fn edges(grid: &[u64], step: u64, bad: impl Fn(u64) -> bool) -> Vec<(u64, u64)> {
    let samples: Vec<(u64, bool)> = grid.iter().map(|&g| (g, bad(g))).collect();
    let flips = samples.windows(2).filter(|w| w[0].1 != w[1].1);
    flips
        .map(|w| {
            let (mut lo, mut hi) = (w[0].0, w[1].0);
            while hi - lo > step {
                let mid = lo + (hi - lo) / 2;
                if bad(mid) == w[0].1 { lo = mid } else { hi = mid }
            }
            (lo, hi)
        })
        .collect()
}

/// Who signs, to whom, and how much of the signer's current balance.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct Args {
    user: usize,
    to: Receiver,
    amount: Amount,
}

/// One signed intent, which the relayer submits.
#[derive(Clone, Debug, Serialize, Deserialize)]
enum IntentOp {
    Transfer(Args),
    NativeWithdraw(Args),
    FtWithdraw(Args),
}

/// No mirror of Intents' accounting: the strategies produce recipes and the
/// op queries the real WASM's current balances before signing.
impl Op<Intents> for IntentOp {
    fn submit(&self, world: &mut Intents) -> Result<(), Skip> {
        let (IntentOp::Transfer(args)
        | IntentOp::NativeWithdraw(args)
        | IntentOp::FtWithdraw(args)) = self;
        let to = args.to.resolve(&world.known);
        if !world.known.contains(&to.to_string()) {
            world.known.push(to.to_string());
        }
        let amount = args.amount.of(world.balance(&world.users[args.user].0))?.to_string();
        let intent = match self {
            IntentOp::Transfer(_) => json!({"intent": "transfer", "receiver_id": to,
                                    "tokens": {"nep141:wrap.near": amount}}),
            IntentOp::NativeWithdraw(_) => native_withdraw(&to, amount),
            IntentOp::FtWithdraw(_) => json!({"intent": "ft_withdraw", "token": WNEAR,
                                       "receiver_id": to, "amount": amount}),
        };
        world.transaction(args.user, intent).submit();
        Ok(())
    }
}

fn intents(version: &str) -> Harness<IntentOp, Intents> {
    Harness::new(Intents::setup(version)).at_rest("wNEAR books balance", check_books)
}

fn intent_ops() -> impl Strategy<Value = IntentOp> {
    let receivers = prop_oneof![
        (0usize..6).prop_map(Receiver::Known),
        any::<u64>().prop_map(Receiver::Near),
        any::<u64>().prop_map(Receiver::Eth),
    ];
    let args = (0usize..3, receivers, amount::any())
        .prop_map(|(user, to, amount)| Args { user, to, amount })
        .boxed();
    prop_oneof![
        args.clone().prop_map(IntentOp::Transfer),
        args.clone().prop_map(IntentOp::NativeWithdraw),
        args.prop_map(IntentOp::FtWithdraw),
    ]
}

#[derive(Clone, Debug, Serialize, Deserialize)]
enum Receiver {
    Known(usize),
    Near(u64),
    Eth(u64),
}

impl Receiver {
    fn resolve(&self, known: &[String]) -> AccountId {
        let fresh = |n: u64| hex(&CryptoHash::hash_bytes(&n.to_le_bytes()).0);
        let id = match *self {
            Self::Known(index) => known[index].clone(),
            Self::Near(n) => fresh(n),
            Self::Eth(n) => format!("0x{}", &fresh(n)[..40]),
        };
        id.parse().unwrap()
    }
}

fn check_books(world: &Intents) -> Result<(), String> {
    let external: String =
        world.sim.view(WNEAR, "ft_balance_of", json!({"account_id": INTENTS})).json();
    let external: u128 = external.parse().unwrap();
    let supply: Option<String> =
        world.sim.view(INTENTS, "mt_supply", json!({"token_id": "nep141:wrap.near"})).json();
    let supply: u128 = supply.map_or(0, |n| n.parse().unwrap());
    let internal: u128 = world.known.iter().map(|who| world.balance(&who.parse().unwrap())).sum();
    if external == supply && supply == internal {
        Ok(())
    } else {
        Err(format!("wNEAR: external {external}, supply {supply}, sum of balances {internal}"))
    }
}

fn native_withdraw(to: &AccountId, amount: String) -> Value {
    json!({"intent": "native_withdraw", "receiver_id": to, "amount": amount})
}

#[derive(Clone)]
struct Intents {
    sim: Sim,
    users: Vec<(AccountId, SecretKey)>,
    /// The relayer, the contracts and the users, who exist only inside
    /// intents: a native withdrawal to one creates its account.
    known: Vec<String>,
    nonce: u64,
}

impl AsRef<Sim> for Intents {
    fn as_ref(&self) -> &Sim {
        &self.sim
    }
}

impl AsMut<Sim> for Intents {
    fn as_mut(&mut self) -> &mut Sim {
        &mut self.sim
    }
}

impl Intents {
    /// wrap.near + intents.near; three NEAR-implicit users with 1000 wNEAR
    /// each, deposited into intents by a relayer.
    fn setup(version: &str) -> Intents {
        let dir =
            std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR unset: run under cargo");
        let res = format!("{dir}/tests/res/intents");
        let mut sim = Sim::new();
        sim.create_account(RELAYER, Balance::from_near(1_000_000));
        sim.deploy(WNEAR, std::fs::read(format!("{res}/wnear.wasm")).unwrap());
        sim.call(WNEAR, WNEAR, "new").args_json(json!({})).transact().assert_success();
        sim.deploy(INTENTS, std::fs::read(format!("{res}/defuse-{version}.wasm")).unwrap());
        sim.call(INTENTS, INTENTS, "new")
            .args_json(json!({"config": {
                "wnear_id": WNEAR,
                "fees": {"fee": 0, "fee_collector": INTENTS},
                "roles": {"super_admins": [], "admins": {}, "grantees": {}},
            }}))
            .transact()
            .assert_success();
        sim.call(RELAYER, WNEAR, "near_deposit")
            .deposit(Balance::from_near(3_001))
            .transact()
            .assert_success();
        sim.call(RELAYER, WNEAR, "storage_deposit")
            .args_json(json!({"account_id": INTENTS}))
            .deposit(Balance::from_millinear(125))
            .transact()
            .assert_success();
        let users: Vec<(AccountId, SecretKey)> = (0..3)
            .map(|i| {
                let sk = SecretKey::from_seed(KeyType::ED25519, &format!("user{i}"));
                let pk = sk.public_key();
                (hex(pk.unwrap_as_ed25519().as_ref()).parse().unwrap(), sk)
            })
            .collect();
        for (user, _) in &users {
            sim.call(RELAYER, WNEAR, "ft_transfer_call")
                .args_json(json!({"receiver_id": INTENTS,
                    "amount": Balance::from_near(1_000).as_yoctonear().to_string(),
                    "msg": user}))
                .deposit(Balance::from_yoctonear(1))
                .transact()
                .assert_success();
        }
        let mut known: Vec<String> = vec![RELAYER.into(), INTENTS.into(), WNEAR.into()];
        known.extend(users.iter().map(|(u, _)| u.to_string()));
        Intents { sim, users, known, nonce: 0 }
    }

    fn balance(&self, account: &AccountId) -> u128 {
        let args = json!({"account_id": account, "token_id": "nep141:wrap.near"});
        let balance: String = self.sim.view(INTENTS, "mt_balance_of", args).json();
        balance.parse().unwrap()
    }

    /// User `i` signs `intent`; the relayer submits it, its gas unpinned.
    fn transaction(&mut self, i: usize, intent: Value) -> Tx<'_> {
        let (user, sk) = &self.users[i];
        self.nonce += 1;
        let nonce = [self.nonce.to_le_bytes(), [0; 8], [0; 8], [0; 8]].concat();
        let payload = json!({
            "signer_id": user,
            "verifying_contract": INTENTS,
            "deadline": "2100-01-01T00:00:00Z",
            "nonce": base64::engine::general_purpose::STANDARD.encode(nonce),
            "intents": [intent],
        })
        .to_string();
        let signed = json!({"standard": "raw_ed25519", "payload": payload,
            "public_key": sk.public_key().to_string(),
            "signature": sk.sign(payload.as_bytes()).to_string()});
        let args = json!({"signed": [signed]});
        self.sim.call(RELAYER, INTENTS, "execute_intents").args_json(args)
    }

    /// The standalone gas diagnostics deliberately drain the same transaction.
    fn execute(&mut self, i: usize, intent: Value, gas: Gas) -> TxOutcome {
        self.transaction(i, intent).gas(gas).transact()
    }
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}
