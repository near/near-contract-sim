//! The outsider sweep: a scan sends each call the witnesses made
//! into an audited contract directly from `axis.near`, moves a raw call's
//! deposit and the account ids its args name, says what it sent, and
//! judges a self-only method that runs for the outsider
//! (`builtin::SELF_ONLY`).
//!
//! Verification boundary: the scan's direct witnesses, edges, report line
//! and the built-in's verdict, over actual runs of the demos
//! (`tests/res/self_only.wat`, `tests/res/ticket_desk.wat`) on the pinned
//! runtime.

mod common;

use common::manifest_dir;
use near_contract_sim::explore::{
    Audit, Call, Faults, Harness, Label, Op, Plan, Report, Skip, builtin,
};
use near_contract_sim::{Balance, Fixture, Sim};
use serde::{Deserialize, Serialize};

const JOBS: &str = "jobs.near";
const DESK: &str = "desk.near";
const ALICE: &str = "alice.near";
const BOB: &str = "bob.near";
const NEAR: u128 = 10u128.pow(24);

fn wat(name: &str) -> Vec<u8> {
    wat::parse_file(manifest_dir().join("tests/res").join(name)).unwrap()
}

fn world(id: &str, file: &str) -> Sim {
    Fixture::new()
        .account(ALICE, Balance::from_near(100))
        .account(BOB, Balance::from_near(100))
        .contract(id, wat(file))
        .build()
}

#[derive(Clone, Debug, Serialize, Deserialize)]
enum DeskOp {
    /// Alice calls `jobs.near::start`, which calls its self-only methods.
    Start,
    /// Alice calls `jobs.near::on_done` herself.
    Done,
    /// Alice buys a ticket at its 1 NEAR price.
    Buy,
    /// Alice claims a credit for herself.
    Claim,
    /// Alice buys two tickets' worth, 2 NEAR.
    BuyTwo,
}

impl Op for DeskOp {
    fn submit(&self, sim: &mut Sim) -> Result<(), Skip> {
        match self {
            DeskOp::Start => sim.call(ALICE, JOBS, "start").submit(),
            DeskOp::Done => sim.call(ALICE, JOBS, "on_done").submit(),
            DeskOp::BuyTwo => sim.call(ALICE, DESK, "buy").deposit(Balance::from_near(2)).submit(),
            DeskOp::Buy => sim.call(ALICE, DESK, "buy").deposit(Balance::from_near(1)).submit(),
            DeskOp::Claim => sim
                .call(ALICE, DESK, "claim")
                .args_json(serde_json::json!({ "beneficiary": ALICE }))
                .submit(),
        };
        Ok(())
    }
}

fn count(sim: &Sim) -> u64 {
    let bytes = sim.view_raw(JOBS, "count", vec![]).result.unwrap();
    u64::from_le_bytes(bytes.try_into().unwrap())
}

/// The self-only demo, with a check that reads the `count` view, so
/// `count` runs (as a view) and is never sent.
fn jobs() -> Harness<DeskOp> {
    Harness::new(world(JOBS, "self_only.wat")).at_rest("the counter stays small", |sim: &Sim| {
        match count(sim) {
            n if n < 1000 => Ok(()),
            n => Err(format!("count {n}")),
        }
    })
}

fn start() -> Plan<DeskOp> {
    Plan::new().op(DeskOp::Start)
}

const ON_DONE: &str = "jobs.near::on_done";
const GUARDED: &str = "jobs.near::on_done_guarded";

fn self_only(audit: &Audit<DeskOp>, site: &str) -> bool {
    audit.found_at(builtin::SELF_ONLY, site).is_some()
}

/// `on_done`, which only jobs.near ever called, runs for `axis.near` and
/// bumps the counter: a `SELF_ONLY` finding naming the method and the
/// change, shown at the direct call. `on_done_guarded` refuses the
/// outsider and `on_ping` changes nothing: none there.
#[test]
fn a_self_only_method_that_runs_for_an_outsider_fails_the_builtin() {
    let audit = jobs().scan(&[start()]).run();
    let found = audit.assert_found(builtin::SELF_ONLY, ON_DONE);
    let why = &found.finding.message;
    assert!(why.contains(ON_DONE) && why.contains("axis.near"), "{why}");
    assert!(why.ends_with(r#"changed jobs.near's state: storage "n""#), "{why}");
    assert_eq!(found.label, Label::ShownAt(format!("{ON_DONE} sent directly from axis.near")));
    assert!(!self_only(&audit, GUARDED), "{audit}");
    // `relay` writes nothing itself, but the guarded method it calls on
    // its own contract passes its guard and does.
    let relay = audit.assert_found(builtin::SELF_ONLY, "jobs.near::relay");
    let why = &relay.finding.message;
    assert!(why.ends_with(r#"state: storage "g" (by on_done_guarded)"#), "{why}");
    // `on_ping` runs for the outsider, but changes nothing.
    assert!(!self_only(&audit, "jobs.near::on_ping"), "{audit}");
    let report = audit.report();
    let kinds = report.findings.iter().filter(|r| r.check == builtin::SELF_ONLY);
    assert!(kinds.map(|r| r.kind.as_str()).eq(["self-only"; 2]), "{audit}");
    // The witness itself calls both from jobs.near: nothing there.
    assert!(audit.found().iter().all(|f| f.label != Label::Shown), "{audit}");
    // A guarded method declared must-not-fail refusing the outsider is
    // its guard working, not a failure it guards against.
    let guarded = jobs().must_not_fail(GUARDED).scan(&[start()]).run();
    // Only the faults the scan forces on the witness's call fail it.
    let mnf = |f: &&near_contract_sim::Finding| f.check == builtin::MUST_NOT_FAIL;
    let all = guarded.found().iter().flat_map(|x| std::iter::once(&x.finding).chain(&x.folded));
    let failed: Vec<_> = all.filter(mnf).collect();
    assert!(!failed.is_empty() && failed.iter().all(|f| f.injected), "{guarded}");
}

/// `forward`, which only fwd.near ever called, runs for `axis.near`, but
/// what changes is peer.near's state, not fwd.near's: no `SELF_ONLY`
/// finding. `picky` panics for a call that axis.near signed but
/// fwd.near made, which `must_not_fail` still reports.
#[test]
fn a_self_only_method_that_changes_only_another_contract_passes() {
    let world = Fixture::new()
        .account(ALICE, Balance::from_near(100))
        .contract("fwd.near", wat("self_only_peer.wat"))
        .contract("peer.near", wat("self_only_peer.wat"))
        .build();
    let start = Plan::new().call(Call::new(ALICE, "fwd.near", "start"));
    let picky = "peer.near::picky";
    let audit = Harness::<DeskOp>::new(world).must_not_fail(picky).scan(&[start]).run();
    let never = audit.report().paths.never_sent.unwrap_or_default();
    assert!(!never.contains(&"fwd.near::forward".to_owned()), "{audit}");
    assert!(!self_only(&audit, "fwd.near::forward"), "{audit}");
    let mut all = audit.found().iter().flat_map(|x| std::iter::once(&x.finding).chain(&x.folded));
    let refused = |f: &near_contract_sim::Finding| {
        f.check == builtin::MUST_NOT_FAIL && f.site.to_string().starts_with(picky) && !f.injected
    };
    assert!(all.any(refused), "{audit}");
}

const PEND: &str = "pend.near";

fn pending() -> Harness<DeskOp> {
    Harness::new(world(PEND, "pending.wat"))
}

/// Alice requests, then a later step processes: the request and the
/// processing.
fn request_then_process() -> Plan<DeskOp> {
    Plan::new().call(Call::new(ALICE, PEND, "request")).call(Call::new(ALICE, PEND, "process"))
}

/// `on_withdraw` panics with nothing pending. Sent from `axis.near` after
/// the witness's request, as the witness's processing found it, it pays
/// out: a `SELF_ONLY` finding.
#[test]
fn a_self_only_callback_is_sent_after_the_witness_s_earlier_steps() {
    let audit = pending().scan(&[request_then_process()]).run();
    let found = audit.assert_found(builtin::SELF_ONLY, "pend.near::on_withdraw");
    assert!(found.finding.message.contains(r#"storage "d""#), "{audit}");
    let at = "pend.near::on_withdraw sent directly from axis.near";
    assert_eq!(found.label, Label::ShownAt(at.to_owned()), "{audit}");
    // After a first processing that found nothing pending, the call is
    // sent where the one that succeeded was: the earlier failure, the
    // contract's own, isn't the outsider's.
    let process = Call::new(ALICE, PEND, "process");
    let twice = Plan::new().call(process.clone()).call(Call::new(ALICE, PEND, "request"));
    let audit = pending().scan(&[twice.call(process)]).run();
    assert!(self_only(&audit, "pend.near::on_withdraw"), "{audit}");
}

/// `on_result` reads a promise result: sent from `axis.near` with none,
/// it fails in the host, which is it refusing the outsider, not a failure
/// `must_not_fail` guards against.
#[test]
fn a_callback_without_its_promise_result_refuses_the_outsider() {
    let site = "pend.near::on_result";
    let audit = pending().must_not_fail(site).scan(&[request_then_process()]).run();
    let mut all = audit.found().iter().flat_map(|x| std::iter::once(&x.finding).chain(&x.folded));
    let unforced = |f: &near_contract_sim::Finding| {
        f.check == builtin::MUST_NOT_FAIL && f.site.to_string().starts_with(site) && !f.injected
    };
    assert!(!all.any(unforced), "{audit}");
    let never = audit.report().paths.never_sent.unwrap_or_default();
    assert!(!never.contains(&site.to_owned()), "{audit}");
    assert!(!self_only(&audit, site), "{audit}");
}

/// `migrate` arrives batched after a 1 yocto transfer: it is sent alone,
/// with no deposit (its edge is 1 yocto), and changes the contract's
/// state for the outsider.
#[test]
fn a_call_batched_with_other_actions_is_sent_alone() {
    let audit = pending().scan(&[Plan::new().call(Call::new(ALICE, PEND, "batch"))]).run();
    let found = audit.assert_found(builtin::SELF_ONLY, "pend.near::migrate");
    assert!(found.finding.message.ends_with(r#"state: storage "m""#), "{audit}");
    let names: Vec<String> = audit.report().timings.into_iter().map(|t| t.0).collect();
    let edge = "pend.near::migrate sent directly from axis.near: step 0, deposit = 1";
    assert!(names.contains(&edge.to_owned()), "{names:#?}");
}

/// `left` and `right` arrive as two calls in one receipt: neither is sent,
/// and the report names both as ran but never sent.
#[test]
fn two_calls_in_one_receipt_are_ran_but_never_sent() {
    let audit = pending().scan(&[Plan::new().call(Call::new(ALICE, PEND, "pair"))]).run();
    let never = audit.report().paths.never_sent.unwrap_or_default();
    assert_eq!(never, ["pend.near::left", "pend.near::right"], "{audit}");
}

/// `pay_in` arrives batched after a new key, with 1 yocto: sent alone, it
/// keeps its deposit (its edges are 0 and 2 yocto).
#[test]
fn a_call_batched_without_a_transfer_keeps_its_deposit() {
    let audit = pending().scan(&[Plan::new().call(Call::new(ALICE, PEND, "keyed"))]).run();
    let names: Vec<String> = audit.report().timings.into_iter().map(|t| t.0).collect();
    let at =
        |d: u8| format!("pend.near::pay_in sent directly from axis.near: step 0, deposit = {d}");
    assert!(names.contains(&at(0)) && names.contains(&at(2)), "{names:#?}");
}

/// A transaction jobs.near signs itself calling `on_done` is its first
/// receipt, not a call to itself: the method is public, not self-only.
#[test]
fn a_transaction_the_contract_signs_is_not_a_self_call() {
    let signed = Plan::new().call(Call::new(JOBS, JOBS, "on_done"));
    let audit = jobs().scan(&[signed]).run();
    assert!(audit.errors().is_empty(), "{audit}");
    let names: Vec<String> = audit.report().timings.into_iter().map(|t| t.0).collect();
    let edge = format!("{ON_DONE} sent directly from axis.near: step 0, deposit = 1");
    assert!(names.contains(&edge), "{names:#?}");
    assert!(!self_only(&audit, ON_DONE), "{audit}");
}

/// Declaring the method public turns the finding off, there or for every
/// site; the guarded one stays clean.
#[test]
fn anyone_may_call_turns_the_builtin_off() {
    for site in [ON_DONE, "on_done", "*"] {
        let audit = jobs().anyone_may_call(site).scan(&[start()]).run();
        assert!(!self_only(&audit, ON_DONE), "{site}: {audit}");
        assert!(!self_only(&audit, GUARDED), "{site}: {audit}");
    }
    let other = jobs().anyone_may_call("jobs.near::count").scan(&[start()]).run();
    assert!(self_only(&other, ON_DONE), "{other}");
    // A method some witness's account called itself isn't self-only.
    let called = jobs().scan(&[start(), Plan::new().op(DeskOp::Done)]).run();
    assert!(!self_only(&called, ON_DONE), "{called}");
    assert!(self_only(&called, "jobs.near::relay"), "{called}");
    // Nor is a contract left out of the audit swept.
    let left_out = jobs().audits(["alice.near"]).scan(&[start()]).run();
    assert!(left_out.found().is_empty(), "{left_out}");
}

/// The report names the methods that ran but were never sent from
/// `axis.near`: `admin_reset` and `count`, which ran only in the fixture.
/// Not a view, nor a method that never ran, an `unreached:` item.
#[test]
fn the_report_says_what_ran_but_was_never_sent_from_the_outsider() {
    let reset = Fixture::new()
        .account(ALICE, Balance::from_near(100))
        .contract(JOBS, wat("self_only.wat"))
        .call(ALICE, JOBS, "admin_reset")
        .call(ALICE, JOBS, "count")
        .build();
    let harness = || Harness::<DeskOp>::new(reset.clone());
    let never = |r: &Report| {
        let mut n = r.paths.never_sent.clone().unwrap_or_else(|| vec!["None".into()]);
        n.sort();
        n
    };
    let audit = harness().scan(&[start()]).run();
    let report = audit.report();
    assert_eq!(never(&report), ["jobs.near::admin_reset", "jobs.near::count"], "{audit}");
    assert!(report.paths.unreached.is_empty(), "{audit}");
    let line = "ran but never sent from axis.near: jobs.near::count, jobs.near::admin_reset";
    assert!(audit.to_string().contains(line), "{audit}");
    // Without them in the fixture, `count` is only viewed and
    // `admin_reset` never ran: an `unreached:` item only.
    let jobs = jobs().scan(&[start()]).run().report();
    assert_eq!(jobs.paths.never_sent.as_deref(), Some(&[][..]), "{jobs}");
    assert_eq!(jobs.paths.unreached, ["jobs.near::admin_reset"], "{jobs}");
    assert!(jobs.to_string().contains("ran but never sent from axis.near: none"), "{jobs}");
    // A scan with no edges sent nothing and says nothing.
    let plain: Report = harness().scan(&[start()]).faults(Faults::none()).run().report();
    assert_eq!(plain.paths.never_sent, None, "{plain}");
    assert!(!plain.to_string().contains("never sent from axis.near"), "{plain}");
    // A union keeps what a sweeping report listed and none sent: a report
    // that never ran a method doesn't drop it; one that ran it in a case
    // and didn't list it sent it.
    let both = harness().union(&[plain, jobs, report.clone()]);
    assert_eq!(never(&both), ["jobs.near::admin_reset", "jobs.near::count"], "{both}");
    let reset_call = Plan::new().call(Call::new(ALICE, JOBS, "admin_reset"));
    let sent = harness().scan(&[start(), reset_call]).run().report();
    assert_eq!(never(&sent), ["jobs.near::count"], "{sent}");
    let both = harness().union(&[report, sent]);
    assert_eq!(never(&both), ["jobs.near::count"], "{both}");
}

const PAID: &str = "every ticket is paid for";
const MEMBERS: &str = "credits go to members only";

fn read(sim: &Sim, key: &[u8]) -> u128 {
    let mut b = sim.state(DESK, key).unwrap_or_default();
    b.resize(16, 0);
    u128::from_le_bytes(b.try_into().unwrap())
}

fn desk() -> Harness<DeskOp> {
    Harness::new(world(DESK, "ticket_desk.wat"))
        .at_rest(PAID, |sim: &Sim| {
            let (sold, paid) = (read(sim, b"s"), read(sim, b"p"));
            if paid >= sold * NEAR { Ok(()) } else { Err(format!("{sold} sold for {paid}")) }
        })
        .at_rest(MEMBERS, |sim: &Sim| {
            let credited = sim.view_state(DESK, b"c");
            let ids = credited.iter().map(|(k, _)| String::from_utf8_lossy(&k[1..]).into_owned());
            match ids.filter(|id| id != ALICE && id != BOB).collect::<Vec<_>>() {
                out if out.is_empty() => Ok(()),
                out => Err(format!("credited non-members: {out:?}")),
            }
        })
}

/// `buy` takes 1 yocto for a 1 NEAR ticket: the direct call's deposit
/// edges break the user's check, which the witness as written (and its
/// op, with no deposit to move) never does.
#[test]
fn a_one_yocto_deposit_is_found_through_the_users_check() {
    let audit = desk().scan(&[Plan::new().op(DeskOp::Buy)]).run();
    let found = audit.found().iter().find(|f| f.finding.check == PAID);
    let found = found.unwrap_or_else(|| panic!("{audit}"));
    assert!(found.finding.message.contains("sold for"), "{audit}");
    let Label::ShownAt(at) = &found.label else { panic!("{audit}") };
    assert!(at.contains("deposit = 1"), "{at}");
    let direct = &found.run.plan().steps[0];
    assert!(format!("{direct:?}").contains("axis.near"), "{direct:?}");
    assert!(!audit.found().iter().any(|f| f.finding.check == MEMBERS), "{audit}");
    // `buy` refusing the outsider's zero deposit isn't a failure
    // `must_not_fail` guards against.
    let declared = desk().must_not_fail("desk.near::buy");
    let declared = declared.scan(&[Plan::new().op(DeskOp::Buy)]).run();
    assert!(declared.found_at(builtin::MUST_NOT_FAIL, "desk.near::buy").is_none(), "{declared}");
}

/// The swept call carries the first witness's deposit, 1 NEAR, so its
/// edges sit around that, not the later witness's 2 NEAR.
#[test]
fn a_swept_call_carries_the_first_witness_s_args() {
    let plans = [Plan::new().op(DeskOp::Buy), Plan::new().op(DeskOp::BuyTwo)];
    let audit = desk().scan(&plans).run();
    let names: Vec<String> = audit.report().timings.into_iter().map(|t| t.0).collect();
    let at =
        |d: u128| format!("desk.near::buy sent directly from axis.near: step 0, deposit = {d}");
    assert!(names.contains(&at(NEAR - 1)), "{names:#?}");
    assert!(!names.contains(&at(2 * NEAR - 1)), "{names:#?}");
    // A witness that is alice's own raw call isn't one from axis.near:
    // buy is still sent from there.
    let raw = Plan::new().call(Call::new(ALICE, DESK, "buy").deposit(Balance::from_near(1)));
    let names: Vec<String> =
        desk().scan(&[raw]).run().report().timings.into_iter().map(|t| t.0).collect();
    assert!(names.contains(&at(NEAR - 1)), "{names:#?}");
    // A deposit alice can't hold whole isn't her call's edge: 10 × 10
    // NEAR is her 100. (axis.near, made with what it sends, has no cap.)
    let ten = Plan::new().call(Call::new(ALICE, DESK, "buy").deposit(Balance::from_near(10)));
    let names: Vec<String> =
        desk().scan(&[ten]).run().report().timings.into_iter().map(|t| t.0).collect();
    let all = format!("step 0, deposit = {}", 100 * NEAR);
    assert!(!names.contains(&all), "{names:#?}");
    assert!(
        names.iter().any(|n| n.ends_with(&format!("deposit = {}", 10 * NEAR + 1))),
        "{names:#?}"
    );
    // One axis.near already sends is not sent again.
    let own = Plan::new().call(Call::new("axis.near", DESK, "buy").deposit(Balance::from_near(1)));
    let names: Vec<String> =
        desk().scan(&[own]).run().report().timings.into_iter().map(|t| t.0).collect();
    assert!(names.iter().all(|n| !n.contains("sent directly")), "{names:#?}");
}

/// `claim` trusts its `beneficiary`: sent from `axis.near` naming the
/// outsider, a non-member is credited. The witness, alice for herself,
/// passes.
#[test]
fn a_trusted_beneficiary_is_found_with_the_outsiders_id() {
    let audit = desk().scan(&[Plan::new().op(DeskOp::Claim)]).run();
    let found = audit.found().iter().find(|f| f.finding.check == MEMBERS);
    let found = found.unwrap_or_else(|| panic!("{audit}"));
    assert!(found.finding.message.contains("axis.near"), "{audit}");
    let Label::ShownAt(at) = &found.label else { panic!("{audit}") };
    assert!(at.contains("/args/json/beneficiary = axis.near (the outsider)"), "{at}");
}
