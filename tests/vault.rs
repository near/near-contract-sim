//! The token vault, tested end to end: the runnable example the guides
//! quote (skills/near-contract-sim).
//!
//! The first half is everyday tests, what near-sdk unit tests would be,
//! except that the cross-contract calls, callbacks and refunds are real.
//! The second half is an audit harness: the vault's value-moving
//! operations as an enum, the checks they must keep, and a search and a
//! scan that try overlapping calls, block gaps, explored gas and faults.
//!
//! The vault (`tests/contracts/vault`, built as `tests/res/vault.wasm`)
//! holds one NEP-141 token. A deposit is the token's `ft_transfer_call`,
//! and credits shares 1:1. `withdraw` refuses more than credited deposits
//! back, debits shares, pays the tokens with `ft_transfer` and, in the
//! callback `on_withdraw`, gives the shares back if the payout failed.
//! `vault_forgetful.wasm` is the vault before its audit: its `on_withdraw`
//! forgets to give them back, and it lacks the backing guard.
//! `claim_rewards` credits 10 unbacked reward shares once a day and sends
//! a detached notification to `rewards.near`, which doesn't exist.
//!
//! A clean search or scan here means no check failed in the cases tried.
//! It is no proof that the vault is correct.

mod common;
use borsh::{BorshDeserialize, BorshSerialize};
use common::res;
use near_contract_sim::explore::{
    Amount, Audit, Fail, Faults, Found, Gap, Harness, Label, Op, OpDone, Plan, Skip, Status,
    amount, gas, plans,
};
use near_contract_sim::{
    Balance, Delta, Failure, Fault, Finding, Fixture, Gas, Receipts, Reply, Sim, Stub, Token, Tx,
    builtin,
};
use proptest::prelude::*;
use serde::{Deserialize, Serialize};
use serde_json::json;

const TOKEN: &str = "token.near";
const VAULT: &str = "vault.near";
const HOUR: u64 = 3_600_000_000_000;

/// Accounts, both contracts, storage registration and 1000 tokens each.
/// Built once per test binary and wasm; each call returns a fresh clone.
fn vault_world(vault_wasm: &str) -> Sim {
    Fixture::new()
        .account("alice.near", Balance::from_near(10))
        .account("bob.near", Balance::from_near(10))
        .contract(TOKEN, res("fungible_token.wasm"))
        .init("new_default_meta")
        .args_json(json!({ "owner_id": TOKEN, "total_supply": "2000" }))
        .contract(VAULT, res(vault_wasm))
        .init("new")
        .args_json(json!({ "token_id": TOKEN }))
        .ft_register(TOKEN, [VAULT, "alice.near", "bob.near"])
        .ft_transfer(TOKEN, TOKEN, "alice.near", 1000)
        .ft_transfer(TOKEN, TOKEN, "bob.near", 1000)
        .token(Token::nep141(TOKEN))
        .token(Token::view(VAULT, "shares_of").supply("total_shares"))
        .build()
}

fn world() -> Sim {
    vault_world("vault.wasm")
}

/// A near-sdk `U128` view of one account, as in the quickstart.
fn u128_view(sim: &Sim, id: &str, method: &str, account_id: &str) -> u128 {
    sim.view(id, method, json!({ "account_id": account_id })).u128()
}

fn ft(sim: &Sim, who: &str) -> u128 {
    u128_view(sim, TOKEN, "ft_balance_of", who)
}

fn shares(sim: &Sim, who: &str) -> u128 {
    u128_view(sim, VAULT, "shares_of", who)
}

fn deposit<'a>(sim: &'a mut Sim, who: &str, amount: u128) -> Tx<'a> {
    sim.call(who, TOKEN, "ft_transfer_call")
        .args_json(json!({ "receiver_id": VAULT, "amount": amount.to_string(), "msg": "" }))
        .deposit(Balance::from_yoctonear(1))
}

fn withdraw<'a>(sim: &'a mut Sim, who: &str, amount: u128) -> Tx<'a> {
    sim.call(who, VAULT, "withdraw")
        .args_json(json!({ "amount": amount.to_string() }))
        .deposit(Balance::from_yoctonear(1))
}

// ---- everyday tests ----

#[test]
fn deposit_100_then_withdraw_40() {
    let mut sim = world();
    let d = deposit(&mut sim, "alice.near", 100).transact();
    d.assert_success();
    assert_eq!(d.effects().token_net(TOKEN, "alice.near"), Ok(Delta::from(-100)));
    assert_eq!(d.effects().token_net(VAULT, "alice.near"), Ok(Delta::from(100))); // shares

    let w = withdraw(&mut sim, "alice.near", 40).transact();
    w.assert_success();
    assert_eq!(w.json::<String>(), "40");
    assert_eq!(w.effects().token_net(TOKEN, "alice.near"), Ok(Delta::from(40)));
    assert_eq!(w.effects().token_net(VAULT, "alice.near"), Ok(Delta::from(-40)));
    assert_eq!(
        (ft(&sim, "alice.near"), shares(&sim, "alice.near"), ft(&sim, VAULT)),
        (940, 60, 60)
    );
    sim.findings().assert_none();
}

/// The branch a unit test can't reach: the payout fails, and on_withdraw
/// must give the shares back.
#[test]
fn a_failed_payout_gives_the_shares_back() {
    let mut sim = world();
    deposit(&mut sim, "alice.near", 100).transact().assert_success();
    let w = withdraw(&mut sim, "alice.near", 40)
        .inject(Receipts::callback("on_withdraw"), Fault::PromiseFailed)
        .transact();
    assert_eq!(w.u128(), 0);
    assert_eq!(w.effects().token_net(TOKEN, "alice.near"), Ok(Delta::ZERO));
    assert_eq!(w.effects().token_net(VAULT, "alice.near"), Ok(Delta::ZERO), "{w}");
    // PromiseFailed at the callback starves the call it waits on: that call shows OutOfGas.
    assert_eq!(w.expect_receipt("ft_transfer").fault, Some(Fault::OutOfGas));
    sim.findings().assert_none();
}

/// The same branch without a fault: a stub replaces the token's code, and
/// its `ft_transfer` panics as a paused token's would.
#[test]
fn a_token_that_refuses_the_payout() {
    let mut sim = world();
    deposit(&mut sim, "alice.near", 100).transact().assert_success();
    sim.deploy_stub(TOKEN, Stub::new().on("ft_transfer", Reply::panic("the token is paused")));
    let w = withdraw(&mut sim, "alice.near", 40).transact();
    assert_eq!(w.u128(), 0);
    assert_eq!(shares(&sim, "alice.near"), 100);
    let calls = sim.calls_to(TOKEN);
    let payout = calls.last().unwrap();
    assert_eq!((payout.method_name.as_str(), payout.succeeded), ("ft_transfer", false));
    assert_eq!(payout.args_json::<serde_json::Value>().unwrap()["amount"], "40");
}

/// A flow, block by block: the books' readings after each block (run
/// with `--nocapture`). The shares are debited before the tokens leave,
/// so mid-flow the vault holds more than it owes.
#[test]
fn a_withdraw_block_by_block() {
    let mut sim = world();
    deposit(&mut sim, "alice.near", 100).transact().assert_success();
    let tx = withdraw(&mut sim, "alice.near", 40).submit();
    let mut mid_flow = false;
    while !sim.is_idle() {
        sim.step();
        let (owed, held) = (sim.view(VAULT, "total_shares", ()).u128(), ft(&sim, VAULT));
        println!("#{}: the vault owes {owed} shares, holds {held} tokens", sim.height());
        mid_flow |= owed < held;
    }
    println!("{}", sim.outcome(tx));
    assert!(mid_flow);
}

/// The vault's own guards: only its token deposits, and a withdraw needs
/// the shares and exactly one yoctoNEAR.
#[test]
fn only_the_token_can_deposit() {
    let mut sim = world();
    let out = sim
        .call("alice.near", VAULT, "ft_on_transfer")
        .args_json(json!({ "sender_id": "alice.near", "amount": "100", "msg": "" }))
        .transact();
    let Some(Failure::Panic(m)) = out.failure() else { panic!("expected a panic\n{out}") };
    assert!(m.contains("only the vault's token deposits"), "{m}");
    assert_eq!(shares(&sim, "alice.near"), 0);
}

#[test]
fn withdraw_rejects_overdraw_and_a_missing_yocto() {
    let mut sim = world();
    deposit(&mut sim, "alice.near", 100).transact().assert_success();
    let Some(Failure::Panic(m)) = withdraw(&mut sim, "alice.near", 101).transact().failure() else {
        panic!("overdraw must fail")
    };
    assert!(m.contains("not enough shares"), "{m}");
    let out =
        sim.call("alice.near", VAULT, "withdraw").args_json(json!({ "amount": "10" })).transact();
    let Some(Failure::Panic(m)) = out.failure() else { panic!("expected a panic\n{out}") };
    assert!(m.contains("Requires attached deposit of exactly 1 yoctoNEAR"), "{m}");
    assert_eq!((shares(&sim, "alice.near"), ft(&sim, VAULT)), (100, 100));
}

/// The forgetful bug needs no fault: a claim's reward shares are not
/// backed by tokens, and the forgetful build lacks the backing guard, so
/// withdrawing them from an empty vault fails the payout naturally. The
/// fixed build refuses the withdraw before it debits anything.
#[test]
fn forgetful_loses_unbacked_reward_shares_without_a_fault() {
    const REWARD: u128 = 10;
    let claim = |sim: &mut Sim| sim.call("alice.near", VAULT, "claim_rewards").transact();
    let mut sim = vault_world("vault_forgetful.wasm");
    claim(&mut sim).assert_success();
    assert_eq!(shares(&sim, "alice.near"), REWARD);
    let w = withdraw(&mut sim, "alice.near", REWARD).transact();
    assert!(!w.receipt("ft_transfer").unwrap().is_success(), "the vault holds no tokens\n{w}");
    assert_eq!(shares(&sim, "alice.near"), 0, "{w}");

    let mut sim = world();
    claim(&mut sim).assert_success();
    let w = withdraw(&mut sim, "alice.near", REWARD).transact();
    let Some(Failure::Panic(m)) = w.failure() else { panic!("the fixed build refuses\n{w}") };
    assert!(m.contains("the vault holds too few credited tokens"), "{m}");
    assert!(w.receipt("ft_transfer").is_none(), "{w}");
    assert_eq!(shares(&sim, "alice.near"), REWARD, "{w}");
}

#[test]
fn rewards_once_per_day() {
    let mut sim = world();
    let t0 = sim.timestamp_ns();
    let claim = |sim: &mut Sim| sim.call("alice.near", VAULT, "claim_rewards").transact();
    sim.set_timestamp_ns(t0);
    claim(&mut sim).assert_success();
    sim.set_timestamp_ns(t0 + HOUR);
    assert!(claim(&mut sim).is_failure());
    sim.set_timestamp_ns(t0 + 24 * HOUR + 1_000_000_000);
    claim(&mut sim).assert_success();
    assert_eq!(shares(&sim, "alice.near"), 20);
    // Each claim's detached notification to rewards.near fails, by design;
    // nothing else may.
    let findings = sim.findings();
    assert_eq!(findings.len(), 2, "{findings}");
    assert!(findings.iter().all(|f| f.check == builtin::UNHANDLED_FAILURE));
    assert!(findings.iter().all(|f| f.site.to_string() == "rewards.near::on_claim"));
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(16))]
    /// Plain proptest on a Sim: no harness needed.
    #[test]
    fn shares_follow_deposits(amounts in prop::collection::vec(1u128..=200, 1..=5)) {
        let mut sim = world();
        for &a in &amounts { deposit(&mut sim, "alice.near", a).transact().assert_success(); }
        prop_assert_eq!(shares(&sim, "alice.near"), amounts.iter().sum::<u128>());
    }
}

// ---- the audit harness ----

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
enum User {
    Alice,
    Bob,
}

impl User {
    fn id(self) -> &'static str {
        match self {
            User::Alice => "alice.near",
            User::Bob => "bob.near",
        }
    }
}

/// The vault's value-moving actions. Gas is left unpinned, so the search
/// explores it.
#[derive(Clone, Debug, Serialize, Deserialize)]
enum VaultOp {
    Deposit { who: User, amount: Amount },
    Withdraw { who: User, amount: Amount },
    Claim { who: User },
}

impl Op for VaultOp {
    fn submit(&self, sim: &mut Sim) -> Result<(), Skip> {
        match *self {
            VaultOp::Deposit { who, amount } => {
                let n = amount.of(ft(sim, who.id()))?;
                sim.call(who.id(), TOKEN, "ft_transfer_call")
                    .args_json(json!({ "receiver_id": VAULT, "amount": n.to_string(), "msg": "" }))
                    .deposit(Balance::from_yoctonear(1))
                    .submit();
            }
            VaultOp::Withdraw { who, amount } => {
                let n = amount.of(shares(sim, who.id()))?;
                withdraw(sim, who.id(), n).submit();
            }
            VaultOp::Claim { who } => {
                sim.call(who.id(), VAULT, "claim_rewards").submit();
            }
        }
        Ok(())
    }
}

fn ops() -> impl Strategy<Value = VaultOp> {
    let who = || prop_oneof![Just(User::Alice), Just(User::Bob)];
    prop_oneof![
        3 => (who(), amount::any()).prop_map(|(who, amount)| VaultOp::Deposit { who, amount }),
        3 => (who(), amount::any()).prop_map(|(who, amount)| VaultOp::Withdraw { who, amount }),
        1 => who().prop_map(|who| VaultOp::Claim { who }),
    ]
}

/// always: every receipt is atomic, so this holds after every block.
fn shares_add_up(sim: &Sim) -> Result<(), String> {
    let total = sim.view(VAULT, "total_shares", ()).u128();
    let sum = shares(sim, "alice.near") + shares(sim, "bob.near");
    if total == sum { Ok(()) } else { Err(format!("total_shares {total} != sum of shares {sum}")) }
}

/// at_rest: tokens in flight make this false mid-flow. Reward shares are
/// unbacked by design, so the vault holds exactly the rest.
fn vault_holds_what_it_owes(sim: &Sim) -> Result<(), String> {
    let total = sim.view(VAULT, "total_shares", ()).u128();
    let rewards = sim.view(VAULT, "total_rewards", ()).u128();
    let held = ft(sim, VAULT);
    match total.checked_sub(rewards) {
        Some(owed) if owed == held => Ok(()),
        owed => Err(format!("vault holds {held} tokens, owes {owed:?} (rewards {rewards})")),
    }
}

/// at_rest: shares are what the vault's receipts, replayed in order
/// through a model, say. Any world `AsRef<Sim>` serves.
fn shares_follow_receipts<W: AsRef<Sim>>(w: &W) -> Result<(), Fail> {
    let sim = w.as_ref();
    let mut model = std::collections::BTreeMap::<String, i128>::new();
    for r in sim.receipts_in(..).iter().filter(|r| r.receiver_id == VAULT && r.is_success()) {
        let a: serde_json::Value = r.args_json().unwrap_or_default();
        let n = |k: &str| a[k].as_str().map_or(0, |n| n.parse::<i128>().unwrap());
        let s = |k: &str| a[k].as_str().unwrap_or_default().to_string();
        let caller = r.predecessor_id.to_string();
        let (who, delta) = match r.method() {
            Some("ft_on_transfer") => (s("sender_id"), n("amount")),
            Some("withdraw") => (caller, -n("amount")),
            Some("on_withdraw") if r.value() == Some(b"\"0\"") => (s("account_id"), n("amount")),
            Some("claim_rewards") => (caller, 10),
            _ => continue, // refunds, transfers, views
        };
        *model.entry(who).or_default() += delta;
    }
    for (who, want) in model {
        let got = shares(sim, &who) as i128;
        if got != want {
            return Err(format!("{who} holds {got} shares; its receipts say {want}").into());
        }
    }
    Ok(())
}

/// per-op: what this withdraw's own receipts did, whatever else is in
/// flight.
fn withdraw_delivers(d: &OpDone<VaultOp>) -> Result<(), Fail> {
    let VaultOp::Withdraw { who, .. } = *d.op() else { unreachable!("withdraws only") };
    let taken = -d.effects().token_net(VAULT, who.id())?;
    let paid = d.effects().token_net(TOKEN, who.id())?;
    if taken != paid {
        return Err(
            format!("{} gave up {taken} shares and received {paid} tokens", who.id()).into()
        );
    }
    Ok(())
}

/// per-op: a deposit credits as many shares as it took tokens.
fn deposit_credits(d: &OpDone<VaultOp>) -> Result<(), Fail> {
    let VaultOp::Deposit { who, .. } = *d.op() else { unreachable!("deposits only") };
    let paid = -d.effects().token_net(TOKEN, who.id())?;
    let credited = d.effects().token_net(VAULT, who.id())?;
    if paid != credited {
        return Err(format!("{} paid {paid} tokens for {credited} shares", who.id()).into());
    }
    Ok(())
}

fn is_withdraw(op: &VaultOp) -> bool {
    matches!(op, VaultOp::Withdraw { .. })
}

fn is_deposit(op: &VaultOp) -> bool {
    matches!(op, VaultOp::Deposit { .. })
}

/// Forced faults the fixed vault is known to break under, every one a
/// starved fixed budget (written up, no input grows them): on_withdraw's
/// 10 Tgas, under 1 Tgas used, burns shares with nothing paid out; the
/// token's 5 Tgas `ft_resolve_transfer`, starved, can't refund a refused
/// deposit. A search or scan here asks that nothing else shows: every
/// finding is forced, but a claim's notice, which fails by design.
fn only_forced(audit: &Audit<VaultOp>) {
    let notice = |f: &Found<VaultOp>| {
        f.finding.check == builtin::UNHANDLED_FAILURE
            && f.finding.site.to_string().starts_with("rewards.near::on_claim")
    };
    let forced = |f: &Found<VaultOp>| {
        (matches!(f.label, Label::Forced(_)) && f.finding.injected) || notice(f)
    };
    assert!(audit.found().iter().all(forced), "{audit}");
    assert!(audit.errors().is_empty(), "{audit}");
}

fn vault_on(wasm: &str) -> Harness<VaultOp> {
    audit(vault_world(wasm))
}

fn audit(world: Sim) -> Harness<VaultOp> {
    Harness::new(world)
        .always("total_shares is the sum of shares", shares_add_up)
        .at_rest("the vault holds what it owes", vault_holds_what_it_owes)
        .per_op_for(is_withdraw, "withdraw delivers what it debits", withdraw_delivers)
        .per_op_for(is_deposit, "deposit credits what it takes", deposit_credits)
}

fn vault() -> Harness<VaultOp> {
    vault_on("vault.wasm")
}

/// Overlapping calls, block gaps, explored gas, and a fault in a quarter
/// of cases. A fresh seed each run; the report prints it, and
/// `NEAR_SIM_SEED` repeats it.
#[test]
fn audit_search() {
    let report = vault().search(plans(ops()).len(1..=6)).run();
    report.assert_reached("vault.near::on_withdraw");
    only_forced(&report);
}

/// The same search finds the forgetful build's bug. It may find it through
/// an injected failure or naturally (the forgetful build lacks the backing
/// guard too, so a withdraw of unbacked reward shares fails its payout),
/// so the test asks only that it is found.
#[test]
fn audit_search_finds_the_forgetful_payout() {
    let report = vault_on("vault_forgetful.wasm").search(plans(ops())).seed(7).run();
    let found = report.found_at("withdraw delivers what it debits", "Withdraw");
    let found = found.unwrap_or_else(|| panic!("the forgetful payout:\n{report}"));
    assert!(found.run.plan().steps.len() <= 3, "{report}");
    assert!(found.replay.is_some(), "{report}");
}

/// A deposit, then two overlapping withdraws: every fault at every
/// receipt it fits, one variant at a time. The second withdraw is refused
/// (the first took its shares), so the witness says so.
#[test]
fn audit_every_fault_of_two_overlapping_withdraws() {
    let flow = Plan::new()
        .op(VaultOp::Deposit { who: User::Alice, amount: Amount::Exact(100) })
        .op(VaultOp::Withdraw { who: User::Alice, amount: Amount::Exact(40) })
        .blocks(0)
        .op(VaultOp::Withdraw { who: User::Alice, amount: Amount::All })
        .refused();
    let report = vault().scan(&[flow]).faults(Faults::default()).run();
    only_forced(&report);
    report.assert_reached("vault.near::on_withdraw");
}

/// Alice deposited everything, then left the token (`storage_unregister`
/// at a zero balance): a payout to her fails with no fault forced.
fn alice_left_the_token() -> Sim {
    let mut sim = world();
    deposit(&mut sim, "alice.near", 1000).transact().assert_success();
    sim.call("alice.near", TOKEN, "storage_unregister")
        .args_json(json!({}))
        .deposit(Balance::from_yoctonear(1))
        .transact()
        .assert_success();
    sim
}

/// The default faults starve on_withdraw too: no prepaid gas can, since
/// the vault reserves its gas, but a fixed budget is what input and state
/// shape exhaust. It breaks even the fixed build, a finding labeled
/// forced, with the fault it forced.
#[test]
fn starving_on_withdraw_breaks_even_the_fixed_vault() {
    // The payout to an account the token no longer knows fails, and
    // on_withdraw must give the shares back.
    let vault = audit(alice_left_the_token());
    let flow = Plan::new().op(VaultOp::Withdraw { who: User::Alice, amount: Amount::Exact(40) });
    vault.run(&flow).assert_clean();
    let report = vault.scan(&[flow]).faults(Faults::default()).run();
    let found = report.assert_found("withdraw delivers what it debits", "Withdraw");
    assert!(
        matches!(&found.label, Label::Forced(how) if how.contains("OutOfGas at vault.near::on_withdraw")),
        "{report}"
    );
    only_forced(&report);
}

/// Every prepaid gas a caller can attach keeps the books: below 30 Tgas
/// the withdraw refuses before it debits anything.
#[test]
fn every_gas_keeps_the_books() {
    let flow = |tgas| {
        Plan::new()
            .op(VaultOp::Deposit { who: User::Alice, amount: Amount::Exact(100) })
            .op(VaultOp::Withdraw { who: User::Alice, amount: Amount::Exact(40) })
            .gas(Gas::from_teragas(tgas))
    };
    for tgas in [1, 5, 10, 20, 29, 30, 31, 40, 100, 300] {
        // A refusal below 30 Tgas is meant: not a failed witness.
        let plan = if tgas < 30 { flow(tgas).refused() } else { flow(tgas) };
        vault().run(&plan).assert_clean();
    }
    let report = vault().search(plans(ops()).gas(gas::wide())).cases(64).run();
    only_forced(&report);
}

/// Positive control: the backing check can fail.
#[derive(BorshSerialize, BorshDeserialize)]
struct VaultState {
    token_id: String,
    shares: Vec<u8>,
    total_shares: u128,
    total_rewards: u128,
    last_claim: Vec<u8>,
}

#[test]
fn the_backing_check_can_fail() {
    let mut sim = world();
    deposit(&mut sim, "alice.near", 100).transact().assert_success();
    assert!(vault_holds_what_it_owes(&sim).is_ok());
    sim.update_contract_state::<VaultState>(VAULT, |s| s.total_shares += 1);
    assert!(vault_holds_what_it_owes(&sim).is_err());
}

// ---- the regression from a report ----
// A tolerable scan of a deposit then a withdraw, on vault_forgetful.wasm,
// finds "withdraw delivers what it debits" at Withdraw and "the vault
// holds what it owes" at world, both forced (PromiseFailed at
// vault.near::on_withdraw), in one row; its one replay holds both
// identities. Copied into tests/found/, it is a regression of every
// harness: the store reproduces it, and says if the fixture changed since.

fn found_dir() -> std::path::PathBuf {
    common::manifest_dir().join("tests/found")
}

#[test]
fn forgetful_payout_reproduces() {
    let replays = vault_on("vault_forgetful.wasm").store(found_dir()).regressions();
    assert_eq!(replays.len(), 1, "{replays:#?}");
    for r in &replays {
        assert!(!r.fixture_changed, "regenerate: {r:#?}");
        assert!(matches!(r.status, Status::Shows(_)), "{r:#?}");
    }
}

#[test]
fn forgetful_payout_stays_fixed() {
    let replays = vault().store(found_dir()).regressions();
    assert_eq!(replays.len(), 1, "{replays:#?}");
    for r in &replays {
        assert!(r.fixture_changed, "{r:#?}");
        match &r.status {
            Status::Gone(run) => run.assert_clean(),
            other => panic!("{other:#?}"),
        }
    }
}

/// The same case as a typed plan, when a readable regression is wanted.
#[test]
fn a_withdraw_whose_payout_fails_keeps_the_shares() {
    let plan = Plan::new()
        .op(VaultOp::Deposit { who: User::Alice, amount: Amount::Exact(100) })
        .op(VaultOp::Withdraw { who: User::Alice, amount: Amount::Exact(40) })
        .inject(Receipts::callback("on_withdraw"), Fault::PromiseFailed);
    vault().run(&plan).assert_clean();
    vault_on("vault_forgetful.wasm")
        .run(&plan)
        .assert_found("withdraw delivers what it debits", "Withdraw");
}

/// The model check finds the forgetful payout the same plan shows.
#[test]
fn shares_follow_receipts_finds_the_forgetful_payout() {
    let plan = Plan::new()
        .op(VaultOp::Deposit { who: User::Alice, amount: Amount::Exact(100) })
        .op(VaultOp::Withdraw { who: User::Alice, amount: Amount::Exact(40) })
        .inject(Receipts::callback("on_withdraw"), Fault::PromiseFailed);
    let on = |wasm| Harness::new(vault_world(wasm)).at_rest("shares", shares_follow_receipts);
    on("vault.wasm").run(&plan).assert_clean();
    on("vault_forgetful.wasm").run(&plan).assert_found("shares", "world");
    let claim = Plan::new().op(VaultOp::Claim { who: User::Bob });
    let claim = claim.op(VaultOp::Withdraw { who: User::Bob, amount: Amount::Exact(4) });
    let claimed = on("vault.wasm").run(&claim);
    assert!(claimed.findings().iter().all(|f| f.check == builtin::UNHANDLED_FAILURE), "{claimed}");
}

/// Writes the replay file `forgetful_payout_*` read: run it once with
/// `--ignored` after the fixture changes, deleting the old one.
#[test]
#[ignore = "regenerates tests/found/"]
fn regenerate_the_forgetful_replay() {
    let plan = Plan::new()
        .op(VaultOp::Deposit { who: User::Alice, amount: Amount::Exact(100) })
        .op(VaultOp::Withdraw { who: User::Alice, amount: Amount::Exact(40) });
    let dir = common::manifest_dir().join("tests/found");
    let h = vault_on("vault_forgetful.wasm").store(&dir);
    let report = h.scan(&[plan]).faults(Faults::tolerable()).run();
    println!("{report}");
    let found = report.found_at("withdraw delivers what it debits", "Withdraw").unwrap();
    // The vault's books break in the same case: its row folds into this
    // one, and this replay holds its identity too.
    h.run(found.run.plan()).assert_found("the vault holds what it owes", "world");
}

/// Found by `audit_search` (seed `0xc2ecc4c0e7607b80`): Bob withdraws his
/// reward shares in the block where Alice's deposit is in flight, and her
/// `ft_on_transfer` fails. Paying Bob from her uncredited tokens would
/// leave the token's refund capped by the vault's balance, so Alice would
/// pay 10 tokens for nothing. The vault pays only what credited deposits
/// back, so the refund is whole.
#[test]
fn a_withdraw_never_spends_a_deposit_in_flight() {
    let plan = Plan::new()
        .op(VaultOp::Deposit { who: User::Bob, amount: Amount::All })
        .op(VaultOp::Claim { who: User::Bob })
        .op(VaultOp::Claim { who: User::Alice })
        .op(VaultOp::Deposit { who: User::Alice, amount: Amount::All })
        .blocks(0)
        .inject(Receipts::call(TOKEN, "ft_resolve_transfer"), Fault::PromiseFailed)
        .op(VaultOp::Withdraw { who: User::Bob, amount: Amount::All });
    let run = vault().run(&plan);
    // Only the claims' notices to the absent rewards.near fail, by design.
    let notice = |f: &&Finding| f.site.to_string().starts_with("rewards.near::on_claim");
    assert!(run.findings().iter().all(|f| notice(&f)), "{run}");
    // The same flow by hand: the fault lands, Bob's withdraw refuses, and
    // Alice's refund is whole.
    let mut sim = world();
    deposit(&mut sim, "bob.near", 1000).transact().assert_success();
    sim.call("bob.near", VAULT, "claim_rewards").transact().assert_success();
    let d = sim
        .call("alice.near", TOKEN, "ft_transfer_call")
        .args_json(json!({ "receiver_id": VAULT, "amount": "1000", "msg": "" }))
        .deposit(Balance::from_yoctonear(1))
        .inject(Receipts::call(TOKEN, "ft_resolve_transfer"), Fault::PromiseFailed)
        .submit();
    let w = withdraw(&mut sim, "bob.near", 1010).submit();
    sim.settle();
    let (d, w) = (sim.outcome(d), sim.outcome(w));
    assert_eq!(d.expect_receipt("ft_on_transfer").fault, Some(Fault::OutOfGas), "{d}");
    assert!(w.is_failure(), "1000 credited tokens back 1010 shares only in part\n{w}");
    assert_eq!(d.effects().token_net(TOKEN, "alice.near"), Ok(Delta::ZERO), "{d}");
    assert_eq!((ft(&sim, VAULT), shares(&sim, "bob.near")), (1000, 1010));
}

/// A union of a search and curves run `with` it counts the search's
/// judgments and sites reached once: `with` closes the curves' coverage
/// by the search's, and copies no count. Its own withdraw check never
/// runs on a deposit-only witness, so the union's count is the search's.
#[test]
fn a_union_with_curves_that_cover_with_a_search_counts_it_once() {
    const CHECK: &str = "withdraw delivers what it debits";
    let h = vault();
    let search = h.search(plans(ops()).len(1..=3)).seed(1).cases(12).run();
    let deposit = Plan::new().op(VaultOp::Deposit { who: User::Alice, amount: Amount::Exact(10) });
    let curves = h.curve(&deposit, 0, "repeat Deposit (step 0)").rungs(2).with(&search).run();
    let ok = |r: &near_contract_sim::explore::Report| {
        let v: serde_json::Value = serde_json::from_str(&r.json()).unwrap();
        v["judged"][CHECK]["ok"].as_u64().unwrap_or(0)
    };
    let searched = ok(&search.report());
    assert!(searched > 0, "the search judged a withdraw: {search}");
    assert_eq!(ok(&h.union(&[search.report(), curves.report()])), searched, "{curves}");
    assert!(curves.todo().iter().all(|t| t.id != format!("unjudged:{CHECK}")), "{curves}");
    // So with the sites reached: the curves' own count is theirs, the
    // search's site covers them, and a union counts each case once.
    const SITE: &str = "vault.near::withdraw";
    let reached = |r: &near_contract_sim::explore::Report| {
        let v: serde_json::Value = serde_json::from_str(&r.json()).unwrap();
        v["reached"][SITE].as_u64().unwrap_or(0)
    };
    assert!(search.reached(SITE) > 0, "{search}");
    assert_eq!(curves.reached(SITE), 0, "a deposit-only witness never withdraws");
    curves.assert_reached(SITE);
    assert!(curves.todo().iter().all(|t| t.id != format!("unreached:{SITE}")), "{curves}");
    let u = h.union(&[search.report(), curves.report()]);
    assert_eq!(reached(&u), u64::from(search.reached(SITE)), "{u}");
}

/// Why `per_op_for` stays: a per-op check that answers `Fail::idle`
/// for other ops (`let .. else`) finds the same and leaves the same items
/// open, but its summary differs: its other ops count in the not-judged
/// line's total. `per_op_for`'s check never sees the other ops, so its
/// line counts only its own. (Both name why it went unjudged, not the
/// idle reason: the two stay apart.)
#[test]
fn per_op_for_reports_what_an_idle_answer_for_other_ops_blurs() {
    const PAYOUT: &str = "withdraw delivers what it debits";
    const WORLD: &str = "a withdraw read the world";
    const CLAIM: &str = "a claim";
    fn withdraw_or_idle(d: &OpDone<VaultOp>) -> Result<(), Fail> {
        let VaultOp::Withdraw { .. } = d.op() else { return Err(Fail::idle("not a withdraw")) };
        withdraw_delivers(d)
    }
    fn reads_world(d: &OpDone<VaultOp>) -> Result<(), Fail> {
        let VaultOp::Withdraw { .. } = d.op() else { return Err(Fail::idle("not a withdraw")) };
        let _ = shares(d.world(), "alice.near");
        Ok(())
    }
    fn claim_or_idle(d: &OpDone<VaultOp>) -> Result<(), Fail> {
        let VaultOp::Claim { .. } = d.op() else { return Err(Fail::idle("not a claim")) };
        Ok(())
    }
    let is_claim = |op: &VaultOp| matches!(op, VaultOp::Claim { .. });
    let dir = |n: &str| std::env::temp_dir().join(format!("ncs-idle-{n}-{}", std::process::id()));
    let base = || {
        Harness::new(vault_world("vault_forgetful.wasm"))
            .always("total_shares is the sum of shares", shares_add_up)
            .at_rest("the vault holds what it owes", vault_holds_what_it_owes)
    };
    let only = base()
        .per_op_for(is_withdraw, PAYOUT, withdraw_delivers)
        .per_op_for(is_withdraw, WORLD, reads_world)
        .per_op_for(is_claim, CLAIM, claim_or_idle)
        .store(dir("only"));
    let idle = (base().per_op(PAYOUT, withdraw_or_idle))
        .per_op(WORLD, reads_world)
        .per_op(CLAIM, claim_or_idle)
        .store(dir("idle"));
    let alice = || Just(User::Alice);
    let no_claims = prop_oneof![
        (alice(), amount::any()).prop_map(|(who, amount)| VaultOp::Deposit { who, amount }),
        (alice(), amount::any()).prop_map(|(who, amount)| VaultOp::Withdraw { who, amount }),
    ];
    let overlapping = || plans(no_claims.clone()).len(2..=4).gaps(Just(Gap::Blocks(0)));
    let search = |h: &Harness<VaultOp>| h.search(overlapping()).seed(7).cases(24).run();
    let (a, b) = (search(&only), search(&idle));
    let found =
        |x: &Audit<VaultOp>| x.found().iter().map(|f| f.finding.to_string()).collect::<Vec<_>>();
    assert_eq!(found(&a), found(&b));
    let todo = |x: &Audit<VaultOp>| x.todo().into_iter().map(|t| t.id).collect::<Vec<_>>();
    assert_eq!(todo(&a), todo(&b));
    assert!(todo(&b).contains(&format!("unjudged:{CLAIM}")), "{b}");
    let lines = |x: &Audit<VaultOp>| {
        let text = x.to_string();
        let lines = text.lines().filter(|l| !l.starts_with("TODO unjudged:"));
        lines.map(str::to_owned).collect::<Vec<_>>()
    };
    let not_judged = |x: &Audit<VaultOp>| {
        let text = x.to_string();
        text.lines().find(|l| l.starts_with("not judged: ")).unwrap_or_default().to_owned()
    };
    let (own, blurred) = (not_judged(&a), not_judged(&b));
    assert!(own.contains(" 48 of 48 time(s) (overlapped: it read world()"), "{a}");
    assert!(blurred.contains(" 48 of 85 time(s) (overlapped: it read world()"), "{b}");
    let same = |l: &&String| !l.starts_with("not judged: ");
    let (a, b) = (lines(&a), lines(&b));
    assert_eq!(
        a.iter().filter(same).collect::<Vec<_>>(),
        b.iter().filter(same).collect::<Vec<_>>()
    );
    let _ = (std::fs::remove_dir_all(dir("only")), std::fs::remove_dir_all(dir("idle")));
}
