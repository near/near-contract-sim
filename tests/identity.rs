//! A finding's identity: the check, the site, the failure class on its
//! value path and the faults forced, not the check and site alone. A
//! finding prints no cause label; its facts are the breaking step and the
//! row. These gates run against actual simulator behavior: an
//! interleaving with nothing failed on its value path is one row whose
//! breaking step is the second deposit, never an unrelated failed
//! receipt of the same case.
//!
//! The specimen (`tests/res/pool_sync.wat`) books as a reward whatever its
//! balance holds beyond its books: two deposits in one block book the
//! first twice, a liquid staking pool's in-flight double count. A `ping`
//! fails a value-neutral receipt (a detached self-call that panics, no deposit).

mod common;
mod explore_vault;
use common::manifest_dir;
use near_contract_sim::explore::{Faults, Harness, Op, Plan, Skip, Verdict};
use near_contract_sim::{Balance, Fixture, Sim};
use serde::{Deserialize, Serialize};

const POOL: &str = "pool.near";
const CHECK: &str = "the books never exceed the balance";

fn world() -> Sim {
    let wat = std::fs::read_to_string(manifest_dir().join("tests/res/pool_sync.wat")).unwrap();
    Fixture::new()
        .account("alice.near", Balance::from_near(100))
        .account("bob.near", Balance::from_near(100))
        .contract(POOL, wat::parse_str(&wat).unwrap())
        .setup("book the pool's balance", |sim| {
            sim.call("alice.near", POOL, "init").transact().assert_success();
        })
        .build()
}

#[derive(Clone, Debug, Serialize, Deserialize)]
enum PoolOp {
    /// `who` deposits 1 NEAR.
    Deposit(String),
    /// Fails a value-neutral receipt.
    Ping,
    /// Calls the helper with 1 NEAR: it fails, and the deposit comes
    /// back. A failure on the value path that breaks nothing.
    Bounced,
    /// Calls the public `on_deposit` directly: 1 NEAR booked that never
    /// arrived. Breaks the books alone, with nothing failed.
    Forge,
}

impl Op for PoolOp {
    fn submit(&self, sim: &mut Sim) -> Result<(), Skip> {
        match self {
            PoolOp::Deposit(who) => {
                sim.call(who, POOL, "deposit").deposit(Balance::from_near(1)).submit()
            }
            PoolOp::Ping => sim.call("alice.near", POOL, "ping").submit(),
            PoolOp::Bounced => {
                sim.call("alice.near", POOL, "helper").deposit(Balance::from_near(1)).submit()
            }
            PoolOp::Forge => {
                let args = [&Balance::from_near(1).as_yoctonear().to_le_bytes()[..], b"alice.near"];
                sim.call("alice.near", POOL, "on_deposit").args(args.concat()).submit()
            }
        };
        Ok(())
    }
}

fn books(sim: &Sim) -> u128 {
    let bytes = sim.view_raw(POOL, "booked", vec![]).result.unwrap();
    u128::from_le_bytes(bytes.try_into().unwrap())
}

fn harness() -> Harness<PoolOp> {
    Harness::new(world()).at_rest(CHECK, |sim: &Sim| {
        let (booked, balance) = (books(sim), sim.account(POOL).unwrap().amount.as_yoctonear());
        match booked <= balance {
            true => Ok(()),
            false => Err(format!("the books hold {booked}, the balance {balance}")),
        }
    })
}

fn deposit(who: &str) -> PoolOp {
    PoolOp::Deposit(who.into())
}

/// Gate (c): a ping (its helper fails, moving nothing) and two deposits,
/// all in one block, double-book the first deposit. One row, shown; its
/// breaking step is the second deposit, not the ping. Settled apart, or
/// alone, the deposits keep the books.
#[test]
fn an_interleaving_is_one_finding_not_blamed_on_an_unrelated_failure() {
    let harness = harness();
    let plan = Plan::new()
        .op(PoolOp::Ping)
        .blocks(0)
        .op(deposit("alice.near"))
        .blocks(0)
        .op(deposit("bob.near"));
    let run = harness.run(&plan);
    assert_eq!(run.verdict(), Verdict::Findings, "{run}");
    assert!(run.trees().contains("helper failed"), "the case holds the unrelated failure: {run}");
    let audit = harness.scan(&[plan]).faults(Faults::none()).run();
    let rows: Vec<_> = audit.found().iter().filter(|f| f.finding.check == CHECK).collect();
    assert_eq!(rows.len(), 1, "{audit}");
    assert!(!rows[0].finding.injected, "{audit}");
    assert_eq!(rows[0].broke.as_deref(), Some("breaking step 3 of 3: Deposit"), "{audit}");

    for clean in [
        Plan::new().op(deposit("alice.near")).op(deposit("bob.near")),
        Plan::new().op(PoolOp::Ping).blocks(0).op(deposit("alice.near")),
    ] {
        let run = harness.run(&clean);
        assert!(run.findings().iter().all(|f| f.check != CHECK), "{run}");
    }
}

/// A step that fails on the value path and breaks nothing (a deposit the
/// helper bounces), then, a block later, two deposits that
/// double-book. The breaking step is the last deposit, never step 0; the
/// search's row keeps the plan's real steps and names its breaking op.
///
/// Verification boundary: the breaking step, over an actual search case.
#[test]
fn an_earlier_steps_failure_is_never_the_breaking_step() {
    let plan = Plan::new()
        .op(PoolOp::Bounced)
        .blocks(1)
        .op(deposit("alice.near"))
        .blocks(0)
        .op(deposit("bob.near"));
    let harness = harness();
    let run = harness.run(&plan);
    assert!(run.trees().contains("helper failed"), "the bounce failed: {run}");
    let audit = harness.search(proptest::strategy::Just(plan.clone())).cases(1).seed(1).run();
    let row = audit.found().iter().find(|f| f.finding.check == CHECK);
    let row = row.unwrap_or_else(|| panic!("{audit}"));
    assert_eq!(row.original.plan().steps.len(), 3, "{audit}");
    let steps = &row.original.plan().steps;
    assert!(matches!(steps[0].act.op(), Some(PoolOp::Bounced)), "{audit}");
    assert!(matches!(steps[2].act.op(), Some(PoolOp::Deposit(_))), "{audit}");
    // The failed bounce is on the value path, so part of the finding's
    // identity: the shrink keeps it, and names the breaking op.
    assert_eq!(row.run.plan().steps.len(), 3, "{audit}");
    assert_eq!(row.broke.as_deref(), Some("breaking step 3 of 3: Deposit"));
}

/// Two causes at one at-rest check and one site stay two findings: the
/// op it broke at is part of a finding's identity. The vault starts with
/// alice holding 900 tokens and 100 shares; "alice holds 900 tokens"
/// breaks after a deposit (fewer) and after a withdraw (more), both at
/// `world`, with nothing failed on either path.
/// In one campaign they are two rows; saved by two campaigns into one
/// store, they are two replays, each still showing.
///
/// Verification boundary: finding identity over actual simulator runs.
#[test]
fn two_causes_at_one_check_and_site_are_two_findings() {
    use explore_vault::{TOKEN, User, VAULT, VaultOp, ft, vault_world};
    use near_contract_sim::explore::{Amount, Status};
    const HOLDS: &str = "alice holds 900 tokens";
    let mut sim = vault_world("vault.wasm");
    let args = serde_json::json!({ "receiver_id": VAULT, "amount": "100", "msg": "" });
    let tx = sim.call("alice.near", TOKEN, "ft_transfer_call").args_json(args);
    tx.deposit(Balance::from_yoctonear(1)).transact().assert_success();
    let dir = std::env::temp_dir().join(format!("ncs-two-causes-{}", std::process::id()));
    let harness = Harness::new(sim)
        .at_rest(HOLDS, |s: &Sim| match ft(s, "alice.near") {
            900 => Ok(()),
            n => Err(format!("alice holds {n}")),
        })
        .store(&dir);
    let deposit = Plan::new().op(VaultOp::Deposit { who: User::Alice, amount: Amount::Exact(10) });
    let withdraw =
        Plan::new().op(VaultOp::Withdraw { who: User::Alice, amount: Amount::Exact(40) });

    let both = harness.scan(&[deposit.clone(), withdraw.clone()]).faults(Faults::none()).run();
    let rows: Vec<_> = both.found().iter().filter(|f| f.finding.check == HOLDS).collect();
    assert_eq!(rows.len(), 2, "{both}");
    assert!(rows.iter().all(|r| r.finding.site.to_string() == "world"), "{both}");

    let _ = std::fs::remove_dir_all(&dir);
    for plan in [deposit, withdraw] {
        let _ = harness.scan(&[plan]).faults(Faults::none()).run();
    }
    let replays = harness.clone().store(&dir).regressions();
    let shows = replays.iter().filter(|r| r.check == HOLDS && matches!(r.status, Status::Shows(_)));
    assert_eq!(shows.count(), 2, "{:?}", replays.iter().map(|r| &r.path).collect::<Vec<_>>());
    std::fs::remove_dir_all(&dir).unwrap();
}

/// Two causes with nothing failed on the value path, at one check and
/// site: a forged booking alone, and two deposits in one block. Keyed by
/// the last op alone, a race whose last op is a deposit and a deposit
/// alone would be one finding, and a forged booking followed by an
/// unrelated op would take that op's name. With nothing forced and
/// nothing failed, the kinds of op submitted last before the check broke
/// are the cause: each is its own finding, in one scan, in two scans
/// saved into one store, and in one search; and a race of one kind is
/// that kind alone.
///
/// Verification boundary: finding identity, over actual scans and a
/// search of the pool_sync specimen.
#[test]
fn two_causes_with_nothing_failed_are_two_findings() {
    use near_contract_sim::explore::{Audit, Status};
    let forged = Plan::new().op(PoolOp::Forge);
    let race = Plan::new().op(deposit("alice.near")).blocks(0).op(deposit("bob.near"));
    let rows = |a: &Audit<PoolOp>| -> Vec<String> {
        let shown = a.found().iter().filter(|f| f.finding.check == CHECK && !f.finding.injected);
        let mut ops: Vec<String> = shown
            .map(|f| match f.run.plan().steps.last().and_then(|s| s.act.op()) {
                Some(PoolOp::Forge) => "Forge".into(),
                _ => "Deposit".into(),
            })
            .collect();
        ops.sort();
        ops
    };
    let one = harness().scan(&[forged.clone(), race.clone()]).faults(Faults::none()).run();
    assert_eq!(rows(&one), ["Deposit", "Forge"], "{one}");

    let dir = std::env::temp_dir().join(format!("ncs-nothing-failed-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let stored = harness().store(&dir);
    for plan in [&forged, &race] {
        let _ = stored.scan(std::slice::from_ref(plan)).faults(Faults::none()).run();
    }
    let replays = stored.clone().store(&dir).regressions();
    let shows = replays.iter().filter(|r| r.check == CHECK && matches!(r.status, Status::Shows(_)));
    assert_eq!(shows.count(), 2, "{:?}", replays.iter().map(|r| &r.path).collect::<Vec<_>>());
    std::fs::remove_dir_all(&dir).unwrap();

    let h = harness();
    let search = h.search(proptest::sample::select(vec![race, forged])).cases(16).seed(1).run();
    assert_eq!(rows(&search), ["Deposit", "Forge"], "{search}");
}

/// A store keeps each plan of a finding: a replay a
/// report cites keeps reproducing what it printed. A forged deposit
/// breaks the books alone; a scan's replay of it is one file, and curves
/// along repeats of it after a deposit, the same finding with another
/// plan, add files of their own and never rewrite the scan's. Every file reproduces; a
/// plan run again writes no new one.
///
/// Verification boundary: what the replay store keeps, over actual runs.
#[test]
fn a_store_keeps_each_plan_of_a_finding() {
    use near_contract_sim::explore::Status;
    let dir = std::env::temp_dir().join(format!("ncs-each-plan-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let h = harness().store(&dir);
    let forge = Plan::new().op(PoolOp::Forge);
    let plan_of = |p: &std::path::Path| {
        let v: serde_json::Value = serde_json::from_slice(&std::fs::read(p).unwrap()).unwrap();
        v["plan"].clone()
    };
    let scan = h.scan(std::slice::from_ref(&forge)).faults(Faults::none()).run();
    let cited = scan.assert_found(CHECK, "world");
    let path = cited.replay.clone().unwrap_or_else(|| panic!("no replay: {scan}"));
    let printed = serde_json::to_value(cited.run.plan()).unwrap();
    assert_eq!(plan_of(&path), printed);
    // Curves of another witness of the same finding: a deposit first.
    let deposited = Plan::new().op(deposit("bob.near")).op(PoolOp::Forge);
    let curves = h.curve(&deposited, 1, "repeat Forge (step 1)").run();
    let rows: Vec<_> = curves.found().iter().filter(|f| f.finding.check == CHECK).collect();
    assert!(rows.iter().any(|f| f.replay.as_ref() != Some(&path)), "{curves}");
    for f in &rows {
        let file = f.replay.as_ref().unwrap_or_else(|| panic!("no replay: {curves}"));
        assert_eq!(plan_of(file), serde_json::to_value(f.run.plan()).unwrap(), "{curves}");
    }
    assert_eq!(plan_of(&path), printed, "the scan's replay was rewritten");
    let replays = h.clone().store(&dir).regressions();
    assert!(replays.len() >= 2, "{:?}", replays.iter().map(|r| &r.path).collect::<Vec<_>>());
    for r in &replays {
        assert!(matches!(r.status, Status::Shows(_)), "{} doesn't reproduce", r.path.display());
    }
    let files = std::fs::read_dir(&dir).unwrap().count();
    h.run(&forge).assert_found(CHECK, "world");
    assert_eq!(std::fs::read_dir(&dir).unwrap().count(), files, "a plan run again is one file");
    let _ = std::fs::remove_dir_all(&dir);
}

/// A search's shrink drops the steps that don't change its finding.
/// Nine steps, of which only two deposits in one block double-book: the
/// row's plan is those two, its first case all nine.
///
/// Verification boundary: the search's shrink, over actual runs.
#[test]
fn a_search_finding_shrinks_to_its_causal_steps() {
    let plan = Plan::new()
        .op(PoolOp::Bounced)
        .op(PoolOp::Ping)
        .op(deposit("bob.near"))
        .op(PoolOp::Ping)
        .op(deposit("alice.near"))
        .op(PoolOp::Bounced)
        .op(deposit("bob.near"))
        .op(deposit("alice.near"))
        .blocks(0)
        .op(deposit("bob.near"));
    let audit = harness().search(proptest::strategy::Just(plan)).cases(1).seed(1).run();
    let row = audit.found().iter().find(|f| f.finding.check == CHECK);
    let row = row.unwrap_or_else(|| panic!("{audit}"));
    assert_eq!(row.original.plan().steps.len(), 9, "{audit}");
    let steps = &row.run.plan().steps;
    assert_eq!(steps.len(), 2, "{audit}");
    assert!(steps.iter().all(|s| matches!(s.act.op(), Some(PoolOp::Deposit(_)))), "{audit}");
}

/// One cause, one row: two checks that one forged booking breaks are one
/// row, the other check listed under it; `found_at` still finds the
/// folded check, and the store keeps a replay for each check, each still
/// showing: another check's fold keeps its own. A race of two deposits,
/// a different cause breaking the same two checks, stays its own row, in
/// a scan and in a search.
///
/// Verification boundary: report rows over actual scans and a search of
/// the pool_sync specimen; finding identity and replays are unchanged.
#[test]
fn one_cause_breaking_two_checks_is_one_row() {
    use near_contract_sim::explore::{Audit, Status};
    const SOLVENT: &str = "the pool is solvent";
    let dir = std::env::temp_dir().join(format!("ncs-one-cause-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let two = || {
        harness().store(&dir).at_rest(SOLVENT, |sim: &Sim| {
            let balance = sim.account(POOL).unwrap().amount.as_yoctonear();
            match books(sim) <= balance {
                true => Ok(()),
                false => Err("insolvent".to_owned()),
            }
        })
    };
    let rows = |a: &Audit<PoolOp>| -> Vec<(String, usize)> {
        let shown = a.found().iter().filter(|f| !f.finding.injected);
        shown.map(|f| (f.finding.check.clone(), f.also.len())).collect()
    };
    let forged = Plan::new().op(PoolOp::Forge);
    let race = Plan::new().op(deposit("alice.near")).blocks(0).op(deposit("bob.near"));

    let one = two().scan(std::slice::from_ref(&forged)).faults(Faults::none()).run();
    assert_eq!(rows(&one).len(), 1, "{one}");
    let row = &one.found()[0];
    let other = if row.finding.check == CHECK { SOLVENT } else { CHECK };
    // Its case shows the other check's finding: one replay holds both.
    assert!(row.also.iter().any(|a| *a == format!("{other} at world")), "{one}");
    assert!(one.found_at(other, "world").is_some(), "{one}");
    assert!(one.report().findings[0].also.iter().any(|a| a.starts_with(other)), "{one}");
    let replays = two().store(&dir).regressions();
    let shows = replays.iter().filter(|r| matches!(r.status, Status::Shows(_)));
    assert_eq!(shows.count(), 1, "{:?}", replays.iter().map(|r| &r.path).collect::<Vec<_>>());
    std::fs::remove_dir_all(&dir).unwrap();

    let both = two().scan(&[forged.clone(), race.clone()]).faults(Faults::none()).run();
    assert_eq!(rows(&both).iter().map(|r| r.1).collect::<Vec<_>>(), [1, 1], "{both}");
    let search = two().search(proptest::sample::select(vec![race, forged])).cases(16).seed(1).run();
    assert_eq!(rows(&search).iter().map(|r| r.1).collect::<Vec<_>>(), [1, 1], "{search}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// One cause, one row, within one check: a forged booking alone, with a
/// deposit in its block, and with a bounced call (a failure on the value
/// path) in its block, are three identities (the ops of the last block,
/// or the failure on the path, differ) that broke at the same op, which
/// failed nowhere. They are one row, the others listed under it, with one
/// replay that shows. A race of two deposits breaks at a deposit: its
/// own row.
///
/// Verification boundary: report rows over actual scans of the pool_sync
/// specimen; finding identity and replays unchanged.
#[test]
fn one_check_broken_at_one_op_is_one_row() {
    use near_contract_sim::explore::Status;
    let dir = std::env::temp_dir().join(format!("ncs-one-op-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let h = harness().store(&dir);
    let forged = Plan::new().op(PoolOp::Forge);
    let with = Plan::new().op(deposit("bob.near")).blocks(0).op(PoolOp::Forge);
    let bounced = Plan::new().op(PoolOp::Bounced).blocks(0).op(PoolOp::Forge);
    let race = Plan::new().op(deposit("alice.near")).blocks(0).op(deposit("bob.near"));
    let audit = h.scan(&[forged, with, bounced, race]).faults(Faults::none()).run();
    let rows: Vec<_> = audit.found().iter().filter(|f| f.finding.check == CHECK).collect();
    assert_eq!(rows.len(), 2, "{audit}");
    let also: Vec<usize> = rows.iter().map(|r| r.also.len()).collect();
    assert_eq!(also, [2, 0], "the forged three are one row: {audit}");
    let replays = h.clone().store(&dir).regressions();
    let shows = replays.iter().filter(|r| r.check == CHECK && matches!(r.status, Status::Shows(_)));
    assert_eq!(shows.count(), 2, "{:?}", replays.iter().map(|r| &r.path).collect::<Vec<_>>());
    std::fs::remove_dir_all(&dir).unwrap();
}
