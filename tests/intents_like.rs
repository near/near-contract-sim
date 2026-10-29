//! The custody specimen (`tests/contracts/custody`, built as
//! `tests/res/custody.wasm`, `custody_gas_bug.wasm`, `custody_log_bug.wasm`,
//! with `tests/res/receiver.wasm`), tested end to end and then audited the
//! way the skill's `audit.md` teaches.
//!
//! The custody holds one NEP-141 token and keeps an internal credit ledger,
//! one real token backing one unit of credit. A deposit is the token's
//! `ft_transfer_call`; with a `notify` action it calls the receiver's
//! `on_notify` and chains its own `resolve_deposit`, which refunds part of
//! the deposit. `withdraw` pays credit back out as tokens (vault-style,
//! gas-safe); `transfer` moves credit internally.
//!
//! Two bug builds each plant one input-dependent callback fault:
//! `custody_gas_bug.wasm`'s resolver has a flat gas budget while its work
//! grows with the position count, and `custody_log_bug.wasm`'s resolver emits
//! one refund log that can exceed the host's per-receipt limit. Either way a
//! failed resolver rolls back its burn while the token refunds the sender in
//! full, leaving credit unbacked.
//!
//! The audit does not aim at those inputs: it maps the flows, writes the
//! backing properties, and lets the default `search` (`mod plain`) try
//! position counts, id and memo lengths, interleavings, gas and faults. A
//! clean search is no proof the contract is correct.

mod common;
use common::res;
use near_contract_sim::explore::{
    Amount, Fail, Faults, Harness, Label, Op, OpDone, Plan, Skip, Status, plans,
};
use near_contract_sim::{Balance, Failure, Fixture, Gas, Reply, Sim, Stub, Token, TxOutcome};
use proptest::prelude::*;
use serde::{Deserialize, Serialize};
use serde_json::json;

const TOKEN: &str = "token.near";
const CUSTODY: &str = "custody.near";
const RECEIVER: &str = "receiver.near";

/// alice and bob with a million tokens each; the token, the custody and the
/// notify receiver deployed and registered. The backing token and the credit
/// ledger are both watched, with their supplies.
fn custody_world(custody_wasm: &str) -> Sim {
    Fixture::new()
        .account("alice.near", Balance::from_near(10))
        .account("bob.near", Balance::from_near(10))
        .contract(TOKEN, res("fungible_token.wasm"))
        .init("new_default_meta")
        .args_json(json!({ "owner_id": TOKEN, "total_supply": "10000000" }))
        .contract(CUSTODY, res(custody_wasm))
        .init("new")
        .args_json(json!({ "token_id": TOKEN }))
        .contract(RECEIVER, res("receiver.wasm"))
        .ft_register(TOKEN, [CUSTODY, RECEIVER, "alice.near", "bob.near"])
        .ft_transfer(TOKEN, TOKEN, "alice.near", 1_000_000)
        .ft_transfer(TOKEN, TOKEN, "bob.near", 1_000_000)
        .token(Token::nep141(TOKEN).holders([CUSTODY, RECEIVER, "alice.near", "bob.near"]))
        .token(
            Token::view(CUSTODY, "credit_of")
                .supply("total_credit")
                .holders([RECEIVER, "alice.near", "bob.near"])
                .pure(),
        )
        .build()
}

fn view(sim: &Sim, id: &str, method: &str, args: serde_json::Value) -> u128 {
    sim.view(id, method, args).u128()
}
fn ft(sim: &Sim, who: &str) -> u128 {
    view(sim, TOKEN, "ft_balance_of", json!({ "account_id": who }))
}
fn credit(sim: &Sim, who: &str) -> u128 {
    view(sim, CUSTODY, "credit_of", json!({ "account_id": who }))
}
fn total_credit(sim: &Sim) -> u128 {
    view(sim, CUSTODY, "total_credit", json!({}))
}

// ---- building deposits ----

/// The positions a deposit of `n_positions` ids of `id_len` bytes carries.
/// Each id stays within the contract's `MAX_ID_LEN`.
fn make_ids(n_positions: u8, id_len: u16) -> Vec<String> {
    (0..n_positions).map(|i| format!("p{i}:{}", "x".repeat(id_len as usize))).collect()
}

fn deposit_msg(receiver: &str, ids: &[String], memo_len: u16, notify: bool) -> String {
    json!({
        "receiver": receiver,
        "ids": ids,
        "memo": "m".repeat(memo_len as usize),
        "notify": notify,
    })
    .to_string()
}

// ---- everyday tests ----

fn world() -> Sim {
    custody_world("custody.wasm")
}

fn deposit(
    sim: &mut Sim,
    who: &str,
    receiver: &str,
    amount: u128,
    ids: &[String],
    memo_len: u16,
    notify: bool,
) -> TxOutcome {
    sim.call(who, TOKEN, "ft_transfer_call")
        .args_json(json!({
            "receiver_id": CUSTODY,
            "amount": amount.to_string(),
            "msg": deposit_msg(receiver, ids, memo_len, notify),
        }))
        .deposit(Balance::from_yoctonear(1))
        .gas(Gas::from_teragas(100))
        .transact()
}

#[test]
fn plain_deposit_credits_one_to_one() {
    let mut sim = world();
    let out = deposit(&mut sim, "alice.near", "alice.near", 1000, &make_ids(1, 4), 0, false);
    out.assert_success();
    assert_eq!(
        (ft(&sim, "alice.near"), credit(&sim, "alice.near"), ft(&sim, CUSTODY)),
        (999_000, 1000, 1000)
    );
    assert_eq!(total_credit(&sim), 1000);
    sim.findings().assert_none();
}

#[test]
fn notified_deposit_keeps_half() {
    // The receiver declines half; the custody keeps half and refunds the rest.
    let mut sim = world();
    let out = deposit(&mut sim, "alice.near", RECEIVER, 1000, &make_ids(2, 8), 0, true);
    out.assert_success();
    assert_eq!(credit(&sim, RECEIVER), 500, "{out}");
    assert_eq!(ft(&sim, CUSTODY), 500, "{out}");
    assert_eq!(ft(&sim, "alice.near"), 999_500, "the other half came back\n{out}");
    assert_eq!(total_credit(&sim), 500);
    sim.findings().assert_none();
}

#[test]
fn withdraw_pays_out_and_transfer_moves_credit() {
    let mut sim = world();
    deposit(&mut sim, "alice.near", "alice.near", 1000, &make_ids(1, 4), 0, false).assert_success();
    sim.call("alice.near", CUSTODY, "transfer")
        .args_json(json!({ "to": "bob.near", "amount": "300" }))
        .transact()
        .assert_success();
    assert_eq!((credit(&sim, "alice.near"), credit(&sim, "bob.near")), (700, 300));

    let w = sim
        .call("bob.near", CUSTODY, "withdraw")
        .args_json(json!({ "amount": "300" }))
        .deposit(Balance::from_yoctonear(1))
        .transact();
    w.assert_success();
    assert_eq!((credit(&sim, "bob.near"), ft(&sim, "bob.near")), (0, 1_000_300));
    assert_eq!(total_credit(&sim), 700);
    sim.findings().assert_none();
}

/// The fixed build refuses a deposit whose refund log would not fit, before
/// crediting: the token refunds everything and the books stay backed.
#[test]
fn fixed_rejects_an_overlong_refund_log() {
    let mut sim = world();
    let out = deposit(&mut sim, "alice.near", RECEIVER, 1000, &make_ids(1, 4), 18_000, true);
    assert!(out.is_success(), "the token handles the rejection\n{out}");
    assert_eq!(credit(&sim, RECEIVER), 0, "no credit was created\n{out}");
    assert_eq!(ft(&sim, CUSTODY), 0);
    assert_eq!(ft(&sim, "alice.near"), 1_000_000, "fully refunded");
    sim.findings().assert_none();
}

/// A receiver that answers with garbage: the resolver falls back to refunding
/// everything, so the books stay backed. Exercises `resolve_deposit`'s
/// malformed-reply branch, the one part of the notify flow the fixed half
/// refunder never reaches.
#[test]
fn a_garbage_receiver_reply_refunds_everything() {
    let mut sim = world();
    sim.deploy_stub(RECEIVER, Stub::new().on("on_notify", Reply::json("not a refund list")));
    let out = deposit(&mut sim, "alice.near", RECEIVER, 1000, &make_ids(2, 8), 0, true);
    out.assert_success();
    assert_eq!(credit(&sim, RECEIVER), 0, "a garbage reply refunds all\n{out}");
    assert_eq!(ft(&sim, CUSTODY), 0);
    assert_eq!(ft(&sim, "alice.near"), 1_000_000, "fully refunded\n{out}");
    sim.findings().assert_none();
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

/// The custody's value-moving operations. Gas is left unpinned so the search
/// explores it. A deposit's shape — how many positions, how long the ids and
/// memo, whether it notifies — is part of the op, drawn over natural ranges.
#[derive(Clone, Debug, Serialize, Deserialize)]
enum CustodyOp {
    Deposit { who: User, amount: Amount, positions: u8, id_len: u16, memo_len: u16, notify: bool },
    Withdraw { who: User, amount: Amount },
    Transfer { from: User, to: User, amount: Amount },
}

impl CustodyOp {
    /// Who a deposit credits: a notified deposit credits the receiver it
    /// notifies, a plain one credits the depositor.
    fn credited(who: User, notify: bool) -> &'static str {
        if notify { RECEIVER } else { who.id() }
    }
}

impl Op for CustodyOp {
    fn submit(&self, sim: &mut Sim) -> Result<(), Skip> {
        match *self {
            CustodyOp::Deposit { who, amount, positions, id_len, memo_len, notify } => {
                let n = amount.of(ft(sim, who.id()))?;
                if n == 0 {
                    return Err("nothing to deposit".into());
                }
                let receiver = CustodyOp::credited(who, notify);
                let ids = make_ids(positions, id_len);
                sim.call(who.id(), TOKEN, "ft_transfer_call")
                    .args_json(json!({
                        "receiver_id": CUSTODY,
                        "amount": n.to_string(),
                        "msg": deposit_msg(receiver, &ids, memo_len, notify),
                    }))
                    .deposit(Balance::from_yoctonear(1))
                    .submit();
            }
            CustodyOp::Withdraw { who, amount } => {
                let n = amount.of(credit(sim, who.id()))?;
                if n == 0 {
                    return Err("nothing to withdraw".into());
                }
                sim.call(who.id(), CUSTODY, "withdraw")
                    .args_json(json!({ "amount": n.to_string() }))
                    .deposit(Balance::from_yoctonear(1))
                    .submit();
            }
            CustodyOp::Transfer { from, to, amount } => {
                if from == to {
                    return Err("self-transfer".into());
                }
                let n = amount.of(credit(sim, from.id()))?;
                if n == 0 {
                    return Err("nothing to transfer".into());
                }
                sim.call(from.id(), CUSTODY, "transfer")
                    .args_json(json!({ "to": to.id(), "amount": n.to_string() }))
                    .submit();
            }
        }
        Ok(())
    }
}

fn user() -> impl Strategy<Value = User> {
    prop_oneof![Just(User::Alice), Just(User::Bob)]
}

// ---- the properties ----

/// always: credit and its total are written together, so the per-owner
/// credits always sum to the total, mid-flight included.
fn credit_sums_to_total(sim: &Sim) -> Result<(), String> {
    let total = total_credit(sim);
    let sum = credit(sim, "alice.near") + credit(sim, "bob.near") + credit(sim, RECEIVER);
    if total == sum { Ok(()) } else { Err(format!("total_credit {total} != sum of credits {sum}")) }
}

/// at_rest: once nothing is in flight, the custody's real token holdings back
/// its credit exactly. This is the invariant an unbacked credit breaks.
fn custody_is_backed(sim: &Sim) -> Result<(), String> {
    let (total, held) = (total_credit(sim), ft(sim, CUSTODY));
    if total == held { Ok(()) } else { Err(format!("credit {total} is backed by {held} tokens")) }
}

/// per-op: a deposit creates exactly as much credit as the tokens the custody
/// kept. A resolver that fails leaves the credit up while the token refunds
/// everything, which this catches.
fn deposit_is_backed(d: &OpDone<CustodyOp>) -> Result<(), Fail> {
    let CustodyOp::Deposit { who, notify, .. } = *d.op() else { unreachable!("deposits only") };
    let credited = d.effects().token_net(CUSTODY, CustodyOp::credited(who, notify))?;
    let backing = d.effects().token_net(TOKEN, CUSTODY)?;
    if credited != backing {
        return Err(format!(
            "created {credited} credit but the custody's tokens changed by {backing}"
        )
        .into());
    }
    Ok(())
}

/// per-op: a withdraw pays out exactly the credit it debited (or, if the
/// payout failed, debits nothing).
fn withdraw_delivers(d: &OpDone<CustodyOp>) -> Result<(), Fail> {
    let CustodyOp::Withdraw { who, .. } = *d.op() else { unreachable!("withdraws only") };
    let taken = -d.effects().token_net(CUSTODY, who.id())?;
    let paid = d.effects().token_net(TOKEN, who.id())?;
    if taken != paid {
        return Err(format!("{} gave up {taken} credit for {paid} tokens", who.id()).into());
    }
    Ok(())
}

/// per-op: an internal transfer moves credit between owners and no tokens.
fn transfer_conserves(d: &OpDone<CustodyOp>) -> Result<(), Fail> {
    let CustodyOp::Transfer { from, to, .. } = *d.op() else { unreachable!("transfers only") };
    let net =
        d.effects().token_net(CUSTODY, from.id())? + d.effects().token_net(CUSTODY, to.id())?;
    if net != 0 {
        return Err(format!("transfer changed total credit by {net}").into());
    }
    Ok(())
}

fn is_deposit(op: &CustodyOp) -> bool {
    matches!(op, CustodyOp::Deposit { .. })
}
fn is_withdraw(op: &CustodyOp) -> bool {
    matches!(op, CustodyOp::Withdraw { .. })
}
fn is_transfer(op: &CustodyOp) -> bool {
    matches!(op, CustodyOp::Transfer { .. })
}

fn custody_on(wasm: &str) -> Harness<CustodyOp> {
    Harness::new(custody_world(wasm))
        .always("credit sums to total", credit_sums_to_total)
        .at_rest("the custody is backed", custody_is_backed)
        .per_op_for(is_deposit, "a deposit's credit is backed", deposit_is_backed)
        .per_op_for(is_withdraw, "withdraw delivers what it debits", withdraw_delivers)
        .per_op_for(is_transfer, "transfer conserves credit", transfer_conserves)
}

fn custody() -> Harness<CustodyOp> {
    custody_on("custody.wasm")
}

// ---- what an op sent ----

/// A check reads what an op's transaction sent, here the deposit's `msg`,
/// from the outcome: the transaction's first receipt is its call.
#[test]
fn a_check_reads_what_its_op_sent() {
    let harness = custody().per_op_for(
        is_deposit,
        "a deposit names its receiver",
        |d: &OpDone<CustodyOp>| {
            let call = d.outcome().and_then(|o| o.first_call()).ok_or("one transaction")?;
            let args: serde_json::Value = call.args_json().map_err(|e| e.to_string())?;
            let msg: serde_json::Value = serde_json::from_str(args["msg"].as_str().unwrap_or("{}"))
                .map_err(|e| format!("msg isn't JSON: {e}"))?;
            match msg["receiver"].as_str() {
                Some(_) => Ok(()),
                None => Err(format!("no receiver in {msg}")),
            }
        },
    );
    let plan = Plan::new().op(CustodyOp::Deposit {
        who: User::Alice,
        amount: Amount::Exact(1000),
        positions: 1,
        id_len: 4,
        memo_len: 0,
        notify: true,
    });
    let run = harness.run(&plan);
    run.assert_clean();
    let judged = run.judged().iter().find(|j| j.check == "a deposit names its receiver");
    assert_eq!(judged.map(|j| j.ok), Some(1), "{run}");
}

// ---- positive controls: each check can fail ----

#[derive(borsh::BorshSerialize, borsh::BorshDeserialize)]
struct CustodyState {
    token_id: String,
    credit: Vec<u8>,
    total_credit: u128,
}

#[test]
fn the_backing_check_can_fail() {
    let mut sim = world();
    deposit(&mut sim, "alice.near", "alice.near", 1000, &make_ids(1, 4), 0, false).assert_success();
    assert!(custody_is_backed(&sim).is_ok());
    sim.update_contract_state::<CustodyState>(CUSTODY, |s| s.total_credit += 1);
    assert!(custody_is_backed(&sim).is_err(), "an unbacked credit must fail the check");
    assert!(credit_sums_to_total(&sim).is_err(), "and the sum no longer matches the total");
}

// ---- typed regressions (readable, build-pinned) ----

/// The gas bug as a typed plan: clean on the fixed build, found on the gas
/// build. A sixteen-position notified deposit starves the flat resolver.
#[test]
fn a_many_position_deposit_stays_backed_unless_gas_starved() {
    let plan = Plan::new().op(CustodyOp::Deposit {
        who: User::Alice,
        amount: Amount::Exact(1_000_000),
        positions: 16,
        id_len: 4,
        memo_len: 0,
        notify: true,
    });
    custody().run(&plan).assert_clean();
    custody_on("custody_gas_bug.wasm")
        .run(&plan)
        .assert_found("a deposit's credit is backed", "Deposit");
}

/// The log bug as a typed plan: a long memo overflows the refund log.
#[test]
fn a_long_memo_deposit_stays_backed_unless_log_overflows() {
    let plan = Plan::new().op(CustodyOp::Deposit {
        who: User::Alice,
        amount: Amount::Exact(1_000_000),
        positions: 1,
        id_len: 4,
        memo_len: 18_000,
        notify: true,
    });
    custody().run(&plan).assert_clean();
    custody_on("custody_log_bug.wasm")
        .run(&plan)
        .assert_found("a deposit's credit is backed", "Deposit");
}

// ---- the mechanism behind the symptom ----
// Both bugs show up as the same unbacked-credit finding; these pin the
// distinct cause on each build, so the finding isn't something incidental.

#[test]
fn gas_bug_resolver_runs_out_of_gas() {
    let mut sim = custody_world("custody_gas_bug.wasm");
    let out = deposit(&mut sim, "alice.near", RECEIVER, 1_000_000, &make_ids(16, 4), 0, true);
    assert_eq!(out.expect_receipt("resolve_deposit").failure(), Some(Failure::OutOfGas), "{out}");
    assert!(total_credit(&sim) > ft(&sim, CUSTODY), "credit is left unbacked\n{out}");
}

#[test]
fn log_bug_resolver_traps_on_the_log_limit() {
    let mut sim = custody_world("custody_log_bug.wasm");
    let out = deposit(&mut sim, "alice.near", RECEIVER, 1_000_000, &make_ids(1, 4), 18_000, true);
    match out.expect_receipt("resolve_deposit").failure() {
        Some(Failure::Host(m)) => assert!(m.contains("log"), "a log error, got {m}\n{out}"),
        f => panic!("expected a host log error, got {f:?}\n{out}"),
    }
    assert!(total_credit(&sim) > ft(&sim, CUSTODY), "credit is left unbacked\n{out}");
}

// ---- a scan of the deposit flow (every fault, one at a time) ----

/// A notified deposit (reaches resolve_deposit), a plain deposit and a
/// withdraw (reaches on_withdraw).
fn deposit_flow() -> Plan<CustodyOp> {
    Plan::new()
        .op(CustodyOp::Deposit {
            who: User::Alice,
            amount: Amount::Exact(1000),
            positions: 3,
            id_len: 8,
            memo_len: 0,
            notify: true,
        })
        .op(CustodyOp::Deposit {
            who: User::Bob,
            amount: Amount::Exact(1000),
            positions: 1,
            id_len: 4,
            memo_len: 0,
            notify: false,
        })
        .op(CustodyOp::Withdraw { who: User::Bob, amount: Amount::All })
}

/// The fixed build survives every tolerable fault at every receipt of the
/// flow: a failed notify or a late receipt never unbacks the credit.
#[test]
fn scan_fixed_deposit_flow_survives_tolerable_faults() {
    let report = custody().scan(&[deposit_flow()]).faults(Faults::tolerable()).run();
    report.assert_clean();
    // The callbacks it claims clean ran, and the scan forced faults.
    report.assert_reached("custody.near::resolve_deposit");
    report.assert_reached("custody.near::on_withdraw");
    assert!(report.cases() > 10, "{report}");
}

/// The default faults starve each receipt too, and on the fixed build a
/// starved resolve_deposit leaves the credit unbacked (the token refunds
/// the sender in full; the credit stays): a finding labeled forced.
#[test]
fn scan_fixed_deposit_flow_reports_a_starved_resolver() {
    let report = custody().scan(&[deposit_flow()]).faults(Faults::default()).run();
    let resolver = "OutOfGas at custody.near::resolve_deposit";
    let found = (report.found().iter())
        .find(|f| {
            f.finding.check == "a deposit's credit is backed"
                && matches!(&f.label, Label::Forced(how) if how.contains(resolver))
        })
        .unwrap_or_else(|| panic!("the starved resolver:\n{report}"));
    assert!(found.finding.injected, "{report}");
    // Every finding is a forced one: the flow is clean without faults.
    assert!(report.found().iter().all(|f| f.finding.injected), "{report}");
    let summary = report.to_string();
    assert!(summary.lines().count() <= 60, "{summary}");
}

// ---- a counterparty that acts: re-entry before it answers ----

/// The custody with a stub in the receiver's place, registered and funded
/// with the token, as a hostile receiver would be: what it does is bound
/// per deposit.
fn custody_with_stub_receiver() -> Sim {
    Fixture::new()
        .account("alice.near", Balance::from_near(10))
        .account("bob.near", Balance::from_near(10))
        .contract(TOKEN, res("fungible_token.wasm"))
        .init("new_default_meta")
        .args_json(json!({ "owner_id": TOKEN, "total_supply": "10000000" }))
        .contract(CUSTODY, res("custody.wasm"))
        .init("new")
        .args_json(json!({ "token_id": TOKEN }))
        .stub(RECEIVER, Stub::new().on("on_notify", Reply::json(["0"])))
        .ft_register(TOKEN, [CUSTODY, RECEIVER, "alice.near", "bob.near"])
        .ft_transfer(TOKEN, TOKEN, "alice.near", 1_000_000)
        .ft_transfer(TOKEN, TOKEN, RECEIVER, 1_000_000)
        .build()
}

/// The receiver's answer to a notified deposit of 1000: a full refund,
/// after moving the credit it was just given to bob if it re-enters.
fn receiver_answer(reenter: bool) -> Reply {
    let refund_all = Reply::json(["1000"]);
    match reenter {
        false => refund_all,
        true => Reply::call(CUSTODY, "transfer", json!({ "to": "bob.near", "amount": "1000" }))
            .then(refund_all),
    }
}

/// A receiver that calls back into the custody before it answers: it
/// moves its fresh credit to bob, then asks for all of it back. The
/// re-entrant transfer runs before `resolve_deposit`, which then reads
/// the receiver's credit, finds none, and fails: the token refunds alice
/// in full while bob keeps the credit, unbacked.
#[test]
fn a_receiver_that_reenters_moves_the_credit_before_the_resolver_reads_it() {
    let mut sim = custody_with_stub_receiver();
    let out = sim
        .call("alice.near", TOKEN, "ft_transfer_call")
        .args_json(json!({
            "receiver_id": CUSTODY,
            "amount": "1000",
            "msg": deposit_msg(RECEIVER, &make_ids(1, 4), 0, true),
        }))
        .deposit(Balance::from_yoctonear(1))
        .gas(Gas::from_teragas(100))
        .stub_reply(RECEIVER, "on_notify", receiver_answer(true))
        .transact();
    let at = |method: &str| {
        let r = out
            .receipts
            .iter()
            .find(|r| r.method_names.first().map(String::as_str) == Some(method));
        r.unwrap_or_else(|| panic!("no {method}\n{out}")).clone()
    };
    let (transfer, resolve) = (at("transfer"), at("resolve_deposit"));
    assert!(transfer.is_success(), "{out}");
    assert_eq!(transfer.predecessor_id.as_str(), RECEIVER, "the receiver called it");
    assert!(transfer.block_height <= resolve.block_height, "{out}");
    match resolve.failure() {
        Some(Failure::Panic(m)) => assert!(m.contains("not enough credit"), "{m}"),
        other => panic!("the resolver read the moved credit and failed, not {other:?}\n{out}"),
    }
    assert_eq!((credit(&sim, "bob.near"), credit(&sim, RECEIVER)), (1000, 0));
    assert_eq!((total_credit(&sim), ft(&sim, CUSTODY)), (1000, 0), "credit unbacked");
    assert_eq!(ft(&sim, "alice.near"), 1_000_000, "alice got everything back");

    // The same answer without the call: the resolver burns the refund.
    let mut sim = custody_with_stub_receiver();
    let out = sim
        .call("alice.near", TOKEN, "ft_transfer_call")
        .args_json(json!({
            "receiver_id": CUSTODY,
            "amount": "1000",
            "msg": deposit_msg(RECEIVER, &make_ids(1, 4), 0, true),
        }))
        .deposit(Balance::from_yoctonear(1))
        .gas(Gas::from_teragas(100))
        .stub_reply(RECEIVER, "on_notify", receiver_answer(false))
        .transact();
    out.assert_success();
    assert_eq!((total_credit(&sim), ft(&sim, CUSTODY)), (0, 0), "{out}");
}

/// A notified deposit of 1000 to the stub receiver, whose answer is
/// drawn: whether it re-enters is a field of the op, so a plan carries
/// it, shrinks it and replays it.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct Notified {
    reenter: bool,
}

impl Op for Notified {
    fn submit(&self, sim: &mut Sim) -> Result<(), Skip> {
        sim.call("alice.near", TOKEN, "ft_transfer_call")
            .args_json(json!({
                "receiver_id": CUSTODY,
                "amount": "1000",
                "msg": deposit_msg(RECEIVER, &make_ids(1, 4), 0, true),
            }))
            .deposit(Balance::from_yoctonear(1))
            .stub_reply(RECEIVER, "on_notify", receiver_answer(self.reenter))
            .submit();
        Ok(())
    }
}

/// The search finds the re-entry, natural, shrunk to the one deposit that
/// re-enters, and its replay reproduces.
#[test]
fn a_search_finds_a_reentering_receiver_and_replays_it() {
    let harness = Harness::new(custody_with_stub_receiver())
        .at_rest("the custody is backed", custody_is_backed)
        .must_not_fail("resolve_deposit");
    let ops = any::<bool>().prop_map(|reenter| Notified { reenter });
    let report =
        harness.search(plans(ops).len(1..=3).faults(Faults::none())).seed(3).cases(32).run();
    let found =
        report.found_at("the custody is backed", "world").unwrap_or_else(|| panic!("{report}"));
    assert!(!found.finding.injected, "{report}");
    let steps = &found.run.plan().steps;
    assert!(steps.iter().any(|s| s.act.op().is_some_and(|o| o.reenter)), "{report}");
    assert!(steps.len() <= 2, "{report}");
    let resolver = report.found_at(
        near_contract_sim::explore::builtin::MUST_NOT_FAIL,
        "custody.near::resolve_deposit",
    );
    assert!(resolver.is_some_and(|f| !f.finding.injected), "{report}");
    let path = found.replay.as_ref().expect("saved");
    let again = harness.clone().store(path.parent().unwrap()).regressions();
    let mine = again.iter().find(|r| &r.path == path).expect("its replay");
    assert!(matches!(mine.status, Status::Shows(_)), "{mine:#?}");
}

// ---- the default strategies: what an agent who doesn't know the bugs writes ----

/// The same custody audited the way the skill teaches a coding agent who
/// doesn't know where the bugs are: the op's fields are plain types
/// (`Vec<String>`, `String`, `Amount`), drawn with the documented default
/// strategies (`input::list(input::id())`, `input::text()`,
/// `amount::any()`, `input::answer()`), with no weights and no ranges taken
/// from the contract's constants. The notify receiver is a stub whose
/// reply each deposit draws and binds to its own transaction
/// (`Tx::stub_reply`): how much it asks back, and in what shape, or a
/// panic. The default search finds both bugs and nothing on the fixed
/// build.
mod plain {
    use super::*;
    use near_contract_sim::explore::input::{self, Answer};
    use near_contract_sim::explore::{Audit, Found, amount};

    /// [`custody_world`] with a stub in the receiver's place.
    fn stub_world(custody_wasm: &str) -> Sim {
        Fixture::new()
            .account("alice.near", Balance::from_near(10))
            .account("bob.near", Balance::from_near(10))
            .contract(TOKEN, res("fungible_token.wasm"))
            .init("new_default_meta")
            .args_json(json!({ "owner_id": TOKEN, "total_supply": "10000000" }))
            .contract(CUSTODY, res(custody_wasm))
            .init("new")
            .args_json(json!({ "token_id": TOKEN }))
            .stub(RECEIVER, Stub::new().on("on_notify", Reply::json(Vec::<String>::new())))
            .ft_register(TOKEN, [CUSTODY, RECEIVER, "alice.near", "bob.near"])
            .ft_transfer(TOKEN, TOKEN, "alice.near", 1_000_000)
            .ft_transfer(TOKEN, TOKEN, "bob.near", 1_000_000)
            .token(Token::nep141(TOKEN).holders([CUSTODY, RECEIVER, "alice.near", "bob.near"]))
            .token(
                Token::view(CUSTODY, "credit_of")
                    .supply("total_credit")
                    .holders([RECEIVER, "alice.near", "bob.near"])
                    .pure(),
            )
            .build()
    }

    /// The custody's value-moving operations. Every id, list and message
    /// field takes an `input` strategy, and the receiver's reply is part of
    /// the deposit.
    #[derive(Clone, Debug, Serialize, Deserialize)]
    enum CustodyOp {
        /// `ft_transfer_call` to the custody with a deposit message. With
        /// `notify`, the receiver's stub then answers this deposit: it asks
        /// back `refund` of the deposit for each position, in `answer`'s shape,
        /// or panics.
        Deposit {
            who: User,
            amount: Amount,
            ids: Vec<String>,
            memo: String,
            notify: bool,
            refund: Amount,
            answer: Answer,
        },
        Withdraw {
            who: User,
            amount: Amount,
        },
        Transfer {
            from: User,
            to: User,
            amount: Amount,
        },
    }

    impl Op for CustodyOp {
        fn submit(&self, sim: &mut Sim) -> Result<(), Skip> {
            match self {
                CustodyOp::Deposit { who, amount, ids, memo, notify, refund, answer } => {
                    let n = amount.of(ft(sim, who.id()))?;
                    if n == 0 {
                        return Err("nothing to deposit".into());
                    }
                    let receiver = if *notify { RECEIVER } else { who.id() };
                    let msg =
                        json!({ "receiver": receiver, "ids": ids, "memo": memo, "notify": notify });
                    let mut tx = sim
                        .call(who.id(), TOKEN, "ft_transfer_call")
                        .args_json(json!({
                            "receiver_id": CUSTODY,
                            "amount": n.to_string(),
                            "msg": msg.to_string(),
                        }))
                        .deposit(Balance::from_yoctonear(1));
                    if *notify {
                        // This deposit's own answer, even if another's notify
                        // runs in the same block.
                        let back = refund.of(n)?.to_string();
                        tx = tx.stub_reply(
                            RECEIVER,
                            "on_notify",
                            answer.reply(vec![back; ids.len()]),
                        );
                    }
                    tx.submit();
                }
                CustodyOp::Withdraw { who, amount } => {
                    let n = amount.of(credit(sim, who.id()))?;
                    if n == 0 {
                        return Err("nothing to withdraw".into());
                    }
                    sim.call(who.id(), CUSTODY, "withdraw")
                        .args_json(json!({ "amount": n.to_string() }))
                        .deposit(Balance::from_yoctonear(1))
                        .submit();
                }
                CustodyOp::Transfer { from, to, amount } => {
                    if from == to {
                        return Err("self-transfer".into());
                    }
                    let n = amount.of(credit(sim, from.id()))?;
                    if n == 0 {
                        return Err("nothing to transfer".into());
                    }
                    sim.call(from.id(), CUSTODY, "transfer")
                        .args_json(json!({ "to": to.id(), "amount": n.to_string() }))
                        .submit();
                }
            }
            Ok(())
        }
    }

    fn ops() -> impl Strategy<Value = CustodyOp> {
        let deposit = (
            user(),
            amount::any(),
            input::list(input::id()),
            input::text(),
            any::<bool>(),
            amount::any(),
            input::answer(),
        );
        prop_oneof![
            deposit.prop_map(|(who, amount, ids, memo, notify, refund, answer)| {
                CustodyOp::Deposit { who, amount, ids, memo, notify, refund, answer }
            }),
            (user(), amount::any()).prop_map(|(who, amount)| CustodyOp::Withdraw { who, amount }),
            (user(), user(), amount::any()).prop_map(|(from, to, amount)| CustodyOp::Transfer {
                from,
                to,
                amount
            }),
        ]
    }

    // ---- the per-op checks, on this op ----

    fn deposit_is_backed(d: &OpDone<CustodyOp>) -> Result<(), Fail> {
        let CustodyOp::Deposit { who, notify, .. } = d.op() else { unreachable!("deposits only") };
        let credited = if *notify { RECEIVER } else { who.id() };
        let credited = d.effects().token_net(CUSTODY, credited)?;
        let backing = d.effects().token_net(TOKEN, CUSTODY)?;
        if credited != backing {
            return Err(format!(
                "created {credited} credit but the custody's tokens changed by {backing}"
            )
            .into());
        }
        Ok(())
    }

    fn withdraw_delivers(d: &OpDone<CustodyOp>) -> Result<(), Fail> {
        let CustodyOp::Withdraw { who, .. } = d.op() else { unreachable!("withdraws only") };
        let taken = -d.effects().token_net(CUSTODY, who.id())?;
        let paid = d.effects().token_net(TOKEN, who.id())?;
        if taken != paid {
            return Err(format!("{} gave up {taken} credit for {paid} tokens", who.id()).into());
        }
        Ok(())
    }

    fn transfer_conserves(d: &OpDone<CustodyOp>) -> Result<(), Fail> {
        let CustodyOp::Transfer { from, to, .. } = d.op() else { unreachable!("transfers only") };
        let net =
            d.effects().token_net(CUSTODY, from.id())? + d.effects().token_net(CUSTODY, to.id())?;
        if net != 0 {
            return Err(format!("transfer changed total credit by {net}").into());
        }
        Ok(())
    }

    fn custody_on(wasm: &str) -> Harness<CustodyOp> {
        Harness::new(stub_world(wasm))
            .always("credit sums to total", credit_sums_to_total)
            .at_rest("the custody is backed", custody_is_backed)
            .per_op_for(
                |op| matches!(op, CustodyOp::Deposit { .. }),
                DEPOSIT_BACKED,
                deposit_is_backed,
            )
            .per_op_for(
                |op| matches!(op, CustodyOp::Withdraw { .. }),
                "withdraw delivers what it debits",
                withdraw_delivers,
            )
            .per_op_for(
                |op| matches!(op, CustodyOp::Transfer { .. }),
                "transfer conserves credit",
                transfer_conserves,
            )
    }

    const DEPOSIT_BACKED: &str = "a deposit's credit is backed";

    fn search(wasm: &str, seed: u64) -> Audit<CustodyOp> {
        custody_on(wasm).search(plans(ops())).seed(seed).run()
    }

    /// A natural break of the deposit's backing: what each bug build shows.
    fn bug_found(report: &Audit<CustodyOp>) -> Option<&Found<CustodyOp>> {
        let shown =
            |f: &&Found<CustodyOp>| f.finding.check == DEPOSIT_BACKED && !f.finding.injected;
        report.found().iter().find(shown)
    }

    /// How the resolver failed in `found`'s case.
    fn resolver_failure(found: &Found<CustodyOp>) -> Option<Failure> {
        let receipts = found.run.outcomes().iter().flat_map(|o| &o.receipts);
        receipts.filter(|r| r.method_names == ["resolve_deposit"]).find_map(|r| r.failure())
    }

    #[test]
    fn plain_search_fixed_is_clean() {
        let report = search("custody.wasm", 1);
        report.assert_reached("custody.near::resolve_deposit");
        report.assert_reached("custody.near::on_withdraw");
        let starved = |f: &Found<CustodyOp>| matches!(&f.label, Label::Forced(how) if how.contains("OutOfGas"));
        assert!(report.found().iter().all(starved) && report.errors().is_empty(), "{report}");
    }

    /// The gas bug is found, and its case shows why: the resolver ran out of
    /// its gas.
    #[test]
    fn plain_search_finds_the_gas_bug() {
        let report = search("custody_gas_bug.wasm", 1);
        let found = bug_found(&report).unwrap_or_else(|| panic!("{report}"));
        assert_eq!(resolver_failure(found), Some(Failure::OutOfGas), "{report}");
    }

    /// The log bug is found, and its case shows the resolver's refund log
    /// past the host's limit.
    #[test]
    fn plain_search_finds_the_log_bug() {
        let report = search("custody_log_bug.wasm", 1);
        let found = bug_found(&report).unwrap_or_else(|| panic!("{report}"));
        match resolver_failure(found) {
            Some(Failure::Host(m)) => assert!(m.contains("log"), "{m}"),
            other => panic!("{other:?}\n{report}"),
        }
    }
}
