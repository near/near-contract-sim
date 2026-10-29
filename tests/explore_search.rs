//! `explore::Harness::search` on actual WASM: it keeps going after a
//! finding, gives every distinct finding its own row, and shrinks each
//! through cases that still show that same finding, a forced fault
//! kept at the receipt it landed on; gas shrinks to `None` or to the top
//! edge of a window; a world whose clones share state is caught; the
//! token vault's planted bug and refund_ledger.wat's hostile reply are
//! found from defaults. The contracts are the fuzz contract, the vault
//! specimen and refund_ledger.wat; nothing here models the search.

mod common;
mod explore_vault;
mod fuzz_common;

use fuzz_common::{Op as Fz, TGAS, encode, fuzz_wasm};
use near_contract_sim::explore::input;
use near_contract_sim::explore::{
    Act, Amount, Audit, Fail, Faults, Gap, Harness, Label, Op, OpDone, Plan, Shrink, Skip, StepGas,
    Verdict, amount, gas, plans,
};
use near_contract_sim::{
    Balance, Fault, Finding, Fixture, Gas, Receipts, Reply, Sim, Stub, builtin,
};
use proptest::prelude::*;
use serde::{Deserialize, Serialize};
use std::cell::Cell;
use std::rc::Rc;

/// alice, and the fuzz contract on contract.near and other.near.
fn fixture() -> Sim {
    Fixture::new()
        .account("alice.near", Balance::from_near(100))
        // No code: a call to either fails, at a site that names it, as the
        // fixture has it (a plan's own name for an account the fixture
        // lacks reads `<new account>`).
        .account("missing-a.near", Balance::from_near(1))
        .account("missing-b.near", Balance::from_near(1))
        .contract("contract.near", fuzz_wasm())
        .contract("other.near", fuzz_wasm())
        .build()
}

/// alice runs a program on contract.near.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
enum Prog {
    /// Writes `v`, then makes a call that always fails, unhandled.
    Noisy(u8),
    /// A call that fails unhandled: at `missing-a.near` for 128 and up, at
    /// `missing-b.near` for 1 to 127, none for 0.
    Byte(u8),
    /// This program.
    Run(Vec<Fz>),
    /// A call that fails unhandled at `missing-a.near`, for any value.
    Wide(u128),
}

impl Op for Prog {
    fn submit(&self, sim: &mut Sim) -> Result<(), Skip> {
        let program = match self {
            Prog::Noisy(v) => vec![
                Fz::Write { key: b"v".to_vec(), value: vec![*v] },
                detached_call("missing-a.near"),
            ],
            Prog::Byte(v @ 128..) => vec![detached_call("missing-a.near"), Fz::Log(v.to_string())],
            Prog::Byte(1..=127) => vec![detached_call("missing-b.near")],
            Prog::Byte(0) => vec![],
            Prog::Run(program) => program.clone(),
            Prog::Wide(_) => vec![detached_call("missing-a.near")],
        };
        sim.call("alice.near", "contract.near", "run").args(encode(&program)).submit();
        Ok(())
    }
}

/// A call whose failure nobody learns of: `missing` doesn't exist.
fn detached_call(missing: &str) -> Fz {
    Fz::Call { to: missing.into(), args: encode(&[]), deposit: 0, gas: 10 * TGAS, weight: 0 }
}

fn values(plan: &Plan<Prog>) -> Vec<u8> {
    let value = |s: &near_contract_sim::explore::Step<Prog>| match s.act {
        Act::Op(Prog::Noisy(v) | Prog::Byte(v)) => Some(v),
        _ => None,
    };
    plan.steps.iter().filter_map(value).collect()
}

/// The value written stays below 200 (a business property).
fn noisy() -> Harness<Prog> {
    Harness::new(fixture()).at_rest("value below 200", |sim: &Sim| {
        match sim.state("contract.near", b"v") {
            Some(v) if v[0] >= 200 => Err(format!("value {} stored", v[0])),
            _ => Ok(()),
        }
    })
}

fn noisy_plans() -> impl Strategy<Value = Plan<Prog>> {
    plans(any::<u8>().prop_map(Prog::Noisy)).len(1..=4).faults(Faults::none()).repeats(false)
}

// ---- One finding neither hides nor replaces another ----

/// Every op also makes a call that always fails, and a rare
/// value breaks the business property: both are found, each shrunk to
/// its own minimal case.
#[test]
fn a_harmless_failure_on_every_case_does_not_hide_a_business_finding() {
    let audit = noisy().search(noisy_plans()).seed(11).cases(16).run();
    let business = audit.assert_found("value below 200", "world");
    assert_eq!(values(business.run.plan()), [200], "its own minimal case: {audit}");
    assert_eq!(business.run.plan().steps[0].gas, None, "gas didn't matter: {audit}");
    assert!(matches!(business.shrink, Shrink::Done(_)), "{audit}");
    let noise = audit.assert_found(builtin::UNHANDLED_FAILURE, "missing-a.near::run (Noisy)");
    assert_eq!(noise.run.plan().steps.len(), 1, "{audit}");
    assert_eq!(noise.label, Label::Shown, "{audit}");
    assert_eq!(audit.seeds(), [11]);
    assert_eq!(audit.cases(), 16);
}

/// Always starts at 255, then shrinks by proptest's binary search toward 0.
#[derive(Debug)]
struct LargeByte;

impl Strategy for LargeByte {
    type Tree = proptest::num::u8::BinarySearch;
    type Value = u8;

    fn new_tree(
        &self,
        _: &mut proptest::test_runner::TestRunner,
    ) -> proptest::strategy::NewTree<Self> {
        Ok(proptest::num::u8::BinarySearch::new(255))
    }
}

/// One check at two sites (`missing-a.near` from 128, `missing-b.near`
/// below): shrinking site a's finding stops at 128 instead of sliding to
/// site b, which turns up while shrinking as a finding of its own.
#[test]
fn shrinking_cannot_swap_one_finding_for_another() {
    let strategy = plans(LargeByte.prop_map(Prog::Byte))
        .len(1..=1)
        .repeats(false)
        .gas(gas::off())
        .faults(Faults::none());
    let audit = Harness::new(fixture()).search(strategy).cases(1).run();
    let a = audit.assert_found(builtin::UNHANDLED_FAILURE, "missing-a.near::run");
    assert_eq!(values(a.original.plan()), [255], "the original case is kept: {audit}");
    assert_eq!(values(a.run.plan()), [128], "{audit}");
    assert!(!a.incidental);
    let b = audit.assert_found(builtin::UNHANDLED_FAILURE, "missing-b.near::run");
    assert!(b.incidental, "{audit}");
    // The candidate that showed it is its case: no row of 0 cases.
    assert_eq!(b.cases, 1, "{audit}");
    assert_eq!(audit.found()[0].finding.site, a.finding.site, "the searched one is first");
}

/// A search shrinks every finding while its shrink time lasts (at least
/// `SHRINKS`, else as long as the cases took): cheap findings all shrink,
/// none is cut.
#[test]
fn the_findings_all_shrink_within_the_time_budget() {
    let keyed = Harness::new(fixture()).per_op("keyed", |d: &OpDone<Prog>| match d.op() {
        Prog::Run(p) => Err(Fail::new("always").key(format!("{p:?}"))),
        _ => Ok(()),
    });
    let ops = (0u8..12).prop_map(|k| Prog::Run(vec![Fz::Log(k.to_string())]));
    let strategy = plans(ops).len(1..=1).gas(gas::off()).faults(Faults::none());
    let audit = keyed.search(strategy).seed(3).cases(64).run();
    assert_eq!(audit.found().len(), 12, "{audit}");
    let cut = |f: &&near_contract_sim::explore::Found<Prog>| {
        matches!(f.shrink, Shrink::No | Shrink::Cut(..))
    };
    assert_eq!(audit.found().iter().filter(cut).count(), 0, "{audit}");
}

/// A finding shrinks through a u128 to its least value. A shrink may run
/// 128 times per value in its plan (a u128's binary search), so a long
/// shrink ends at its time share, not at a few runs per value.
///
/// Verification boundary: the search's shrinking, over actual runs.
#[test]
fn a_wide_number_shrinks_all_the_way() {
    let strategy = plans((1u128..).prop_map(Prog::Wide))
        .len(1..=1)
        .repeats(false)
        .gas(gas::off())
        .faults(Faults::none());
    let audit = Harness::new(fixture()).search(strategy).seed(5).cases(1).run();
    let f = audit.assert_found(builtin::UNHANDLED_FAILURE, "missing-a.near::run");
    assert!(matches!(f.shrink, Shrink::Done(_)), "{audit}");
    assert_eq!(f.run.plan().steps[0].act.op(), Some(&Prog::Wide(1)), "{audit}");
}

// ---- the injection while shrinking ----

/// A finding first seen in a case with an injected hold, which happens
/// without one too: shrinking drops the hold, and it is labelled shown.
#[test]
fn an_injected_finding_shrinks_to_a_natural_one() {
    let harness = Harness::new(fixture());
    let strategy = plans(Just(Prog::Run(vec![detached_call("missing-a.near")])))
        .len(1..=1)
        .gas(gas::off())
        .faults(Faults::new(Receipts::all(), [Fault::Hold(1)]).per_case(0.99));
    let mut relabelled = 0;
    for seed in 0..24 {
        let audit = harness.search(strategy.clone()).seed(seed).cases(1).run();
        let found = audit.assert_found(builtin::UNHANDLED_FAILURE, "missing-a.near::run");
        if found.original.plan().steps[0].inject.is_empty() {
            continue;
        }
        let original = harness.run(found.original.plan());
        let first = original.assert_found(builtin::UNHANDLED_FAILURE, "missing-a.near::run");
        assert!(first.injected, "the original was forced: {original}");
        assert!(found.run.plan().steps.iter().all(|s| s.inject.is_empty()), "{audit}");
        assert!(!found.finding.injected, "now shown: {audit}");
        assert_eq!(found.label, Label::Shown, "{audit}");
        assert_eq!(audit.found().len(), 1, "one row, not a forced and a shown one: {audit}");
        relabelled += 1;
    }
    assert!(relabelled >= 20, "{relabelled} of 24 seeds landed the hold");
}

/// Two detached calls, and a per-op check that the op's receipts ran
/// within two blocks: holding either breaks it, at the same site. A case
/// whose hold landed on `missing-b.near` keeps it there while shrinking.
#[test]
fn shrinking_keeps_the_receipt_the_injection_landed_on() {
    let harness = Harness::new(fixture()).per_op("done within two blocks", |d: &OpDone<Prog>| {
        let receipts = d.outcomes().iter().flat_map(|o| &o.receipts);
        let heights: Vec<u64> = receipts.map(|r| r.block_height).collect();
        let span = heights.iter().max().unwrap() - heights.iter().min().unwrap();
        if span <= 2 { Ok(()) } else { Err(format!("took {span} blocks")) }
    });
    let op = Prog::Run(vec![detached_call("missing-a.near"), detached_call("missing-b.near")]);
    let strategy = plans(Just(op))
        .len(1..=1)
        .gas(gas::off())
        .faults(Faults::new(Receipts::all(), [Fault::Hold(1), Fault::Hold(2)]).per_case(0.99));
    let mut pinned = 0;
    for seed in 0..40 {
        let audit = harness.search(strategy.clone()).seed(seed).cases(1).run();
        for found in audit.found().iter().filter(|f| f.finding.check == "done within two blocks") {
            let Label::Forced(at) = &found.label else { continue };
            if !at.ends_with("at missing-b.near::run") {
                continue;
            }
            assert!(at.starts_with("Hold("), "the same kind of fault: {audit}");
            let inject = &found.run.plan().steps[0].inject;
            assert_eq!(inject.len(), 1, "{audit}");
            assert_eq!(
                inject[0].at.to_string(),
                "Receipts::occurrence(\"missing-b.near\", \"run\", 0)",
                "{audit}"
            );
            pinned += 1;
        }
    }
    assert!(pinned >= 8, "{pinned} of 40 seeds landed the hold on missing-b first");
}

// ---- gas ----

/// Writes `pending`, calls other.near, and removes `pending` in a
/// callback that gets only the gas left over (weight 1, no static gas):
/// with too little prepaid gas the callback runs out and `pending` stays,
/// with much too little the whole call fails and nothing is written. A
/// gas window.
fn leftover_callback() -> Prog {
    Prog::Run(vec![
        Fz::Write { key: b"pending".to_vec(), value: b"1".to_vec() },
        Fz::Call {
            to: "other.near".into(),
            args: encode(&[]),
            deposit: 0,
            gas: 5 * TGAS,
            weight: 0,
        },
        Fz::Then { args: encode(&[Fz::Remove { key: b"pending".to_vec() }]), gas: 0, weight: 1 },
        Fz::PromiseReturn,
    ])
}

fn no_pending(sim: &Sim) -> Result<(), String> {
    match sim.state("contract.near", b"pending") {
        Some(_) => Err("pending was left behind".into()),
        None => Ok(()),
    }
}

/// The finding of a gas window shrinks to the window's top edge: gas only
/// moves up while shrinking, and 0.1 Tgas more is clean.
#[test]
fn gas_in_a_window_shrinks_to_its_top_edge_and_never_below() {
    let harness = Harness::new(fixture()).at_rest("no pending", no_pending);
    let strategy = plans(Just(leftover_callback()))
        .len(1..=1)
        .gaps(Just(Gap::Settle))
        .gas(gas::wide())
        .faults(Faults::none())
        .repeats(false);
    let audit = harness.search(strategy).seed(1).cases(48).run();
    let found = audit.assert_found("no pending", "world");
    assert!(matches!(found.shrink, Shrink::Done(_)), "{audit}");
    let (Some(StepGas::Exactly(from)), Some(StepGas::Exactly(to))) =
        (found.original.plan().steps[0].gas, found.run.plan().steps[0].gas)
    else {
        panic!("gas matters here, so it stays set: {audit}");
    };
    assert!(to >= from, "shrinking never lowers gas: {from} -> {to}");
    let at = |g: Gas| harness.run(&Plan::new().op(leftover_callback()).gas(g));
    at(to).assert_found("no pending", "world");
    let above = at(Gas::from_gas(to.as_gas() + TGAS / 10));
    assert!(above.findings().iter().all(|f| f.check != "no pending"), "{above}");
    harness.run(&Plan::new().op(leftover_callback())).assert_clean();
}

/// A finding gas has nothing to do with shrinks to unset gas.
#[test]
fn gas_that_doesnt_matter_shrinks_to_none() {
    let strategy = plans(Just(Prog::Run(vec![detached_call("missing-a.near")])))
        .len(1..=3)
        .gas(gas::wide())
        .faults(Faults::none());
    let audit = Harness::new(fixture()).search(strategy).seed(3).cases(8).run();
    let found = audit.assert_found(builtin::UNHANDLED_FAILURE, "missing-a.near::run");
    assert!(found.original.plan().steps.iter().any(|s| s.gas.is_some()), "{audit}");
    assert!(found.run.plan().steps.iter().all(|s| s.gas.is_none()), "{audit}");
    assert_eq!(found.run.plan().steps.len(), 1, "{audit}");
}

// ---- determinism ----

/// A world whose clones share a counter: the op writes it, so no two runs
/// of one plan agree.
#[derive(Clone)]
struct Leaky {
    sim: Sim,
    counter: Rc<Cell<u8>>,
}

impl AsRef<Sim> for Leaky {
    fn as_ref(&self) -> &Sim {
        &self.sim
    }
}

impl AsMut<Sim> for Leaky {
    fn as_mut(&mut self) -> &mut Sim {
        &mut self.sim
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Count;

impl Op<Leaky> for Count {
    fn submit(&self, w: &mut Leaky) -> Result<(), Skip> {
        let n = w.counter.get();
        w.counter.set(n.wrapping_add(1));
        let program = [Fz::Write { key: b"n".to_vec(), value: vec![n] }];
        w.sim.call("alice.near", "contract.near", "run").args(encode(&program)).submit();
        Ok(())
    }
}

#[test]
fn a_nondeterministic_world_is_a_harness_error() {
    let world = Leaky { sim: fixture(), counter: Rc::new(Cell::new(0)) };
    let audit = Harness::new(world).search(plans(Just(Count))).seed(1).cases(4).run();
    assert!(!audit.is_clean());
    assert!(audit.errors()[0].contains("case 0 ran twice with different results"), "{audit}");
    let world = Leaky { sim: fixture(), counter: Rc::new(Cell::new(0)) };
    assert_eq!(Harness::new(world).run(&Plan::new().op(Count)).verdict(), Verdict::Clean);
}

/// Writes `once` on contract.near, insisting it isn't there yet: a case
/// that saw another case's write would panic, a harness error.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct Once;

impl Op for Once {
    fn submit(&self, sim: &mut Sim) -> Result<(), Skip> {
        assert!(sim.state("contract.near", b"once").is_none(), "a previous case leaked");
        let program = [Fz::Write { key: b"once".to_vec(), value: vec![1] }];
        sim.call("alice.near", "contract.near", "run").args(encode(&program)).submit();
        Ok(())
    }
}

/// Each case starts from its own clone of the fixture, contract state
/// included, and the fixture itself is left as it was.
#[test]
fn each_case_starts_from_a_fresh_clone_of_the_fixture() {
    let world = fixture();
    let harness = Harness::new(world.clone()).at_rest("once at most", |sim: &Sim| {
        match sim.state("contract.near", b"once").as_deref() {
            None | Some([1]) => Ok(()),
            other => Err(format!("once is {other:?}")),
        }
    });
    let audit = harness.search(plans(Just(Once)).len(1..=1)).seed(3).cases(8).run();
    audit.assert_clean();
    assert_eq!(audit.cases(), 8, "{audit}");
    assert_eq!(world.state("contract.near", b"once"), None);
}

/// A search of no cases tried nothing, so it can't be clean.
#[test]
fn a_zero_case_search_is_not_clean() {
    let audit = Harness::new(fixture()).search(plans(Just(Once))).seed(3).cases(0).run();
    assert_eq!(audit.errors(), ["a search of 0 cases tries nothing"]);
    assert!(!audit.is_clean());
}

/// An op that always skips itself.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct Idle;

impl Op for Idle {
    fn submit(&self, _: &mut Sim) -> Result<(), Skip> {
        Err("nothing to do".into())
    }
}

/// A search in which every op skipped itself in every case judged
/// nothing: never clean.
#[test]
fn a_search_where_every_op_skips_is_not_clean() {
    let audit = Harness::new(fixture())
        .per_op("ran", |_: &OpDone<Idle>| Ok::<_, Fail>(()))
        .search(plans(Just(Idle)).len(1..=2))
        .seed(1)
        .cases(4)
        .run();
    assert!(!audit.is_clean(), "{audit}");
}

/// The seed is recorded and repeats the search; `seeds(n)` runs n, the
/// first as given, each its cases.
#[test]
fn a_seed_repeats_a_search_and_seeds_run_their_cases_each() {
    let harness = Harness::new(fixture()).at_rest("no pending", no_pending);
    let strategy = || plans(Just(leftover_callback())).len(1..=2);
    let rows = |a: &Audit<Prog>| -> Vec<String> {
        let f = a.found().iter();
        f.map(|f| format!("{:?} {:?} {:?} {}", f.finding, f.run.plan(), f.shrink, f.cases))
            .collect()
    };
    let a = harness.search(strategy()).seed(5).cases(12).run();
    let b = harness.search(strategy()).seed(5).cases(12).run();
    assert_eq!(rows(&a), rows(&b));
    let fresh = harness.search(strategy()).cases(2).run();
    assert_eq!(fresh.seeds().len(), 1, "a fresh seed is recorded");
    let two = harness.search(strategy()).seed(5).seeds(2).cases(12).run();
    assert_eq!(two.seeds()[0], 5);
    assert_eq!(two.seeds().len(), 2);
    assert_eq!(two.cases(), 24);
}

// ---- the token vault ----

/// The planted bug's findings: what the fixed build must never show
/// unforced.
fn bug_found(audit: &Audit<explore_vault::VaultOp>) -> bool {
    audit.found().iter().any(|f| {
        !matches!(f.label, Label::Forced(_))
            && (f.finding.check == "withdraw delivers what it debits"
                || f.finding.check == "the vault holds what it owes")
    })
}

/// Two seeds of 256 default cases: the forgetful build's bug is found,
/// and the fixed build shows nothing a forced fault didn't cause.
#[test]
fn vault_search_finds_the_forgetful_payout_and_not_the_fix() {
    use explore_vault::{vault_on, vault_ops};
    for wasm in ["vault_forgetful.wasm", "vault.wasm"] {
        let audit =
            vault_on(wasm).search(plans(vault_ops()).len(1..=6)).seed(0x5EED_0000).seeds(2).run();
        match wasm {
            "vault.wasm" => {
                assert!(!bug_found(&audit), "{audit}");
                // A claim's notice to the absent rewards.near fails by
                // design; anything else must be forced.
                let notice = |f: &Finding| f.site.to_string().starts_with("rewards.near::on_claim");
                assert!(
                    audit
                        .found()
                        .iter()
                        .all(|f| matches!(f.label, Label::Forced(_)) || notice(&f.finding)),
                    "{audit}"
                );
                assert!(audit.errors().is_empty(), "{audit}");
            }
            _ => assert!(bug_found(&audit), "{audit}"),
        }
    }
}

// ---- sites don't embed drawn identities ----

/// alice runs a call to an account whose name the plan draws, which
/// doesn't exist.
#[derive(Clone, Debug, Serialize, Deserialize)]
enum Drawn {
    Call { name: String },
}

impl Op for Drawn {
    fn submit(&self, sim: &mut Sim) -> Result<(), Skip> {
        let Drawn::Call { name } = self;
        let to = format!("{name}.near");
        to.parse::<near_contract_sim::AccountId>().map_err(|e| e.to_string())?;
        let program = vec![detached_call(&to)];
        sim.call("alice.near", "contract.near", "run").args(encode(&program)).submit();
        Ok(())
    }
}

/// Every drawn name fails the same way: one finding, at `<new account>`,
/// whatever the names. The message keeps the real name.
#[test]
fn a_drawn_account_is_one_site_whatever_its_name() {
    let names = input::id().min(2).max(20).prop_map(|name| Drawn::Call { name });
    let audit = Harness::new(fixture()).search(plans(names)).seed(3).cases(32).run();
    let found: Vec<String> = audit.found().iter().map(|f| f.finding.site.to_string()).collect();
    assert_eq!(found, ["<new account>::run (Call)"], "{audit}");
    assert!(audit.found()[0].finding.message.contains(".near does not exist"), "{audit}");
}

/// alice's program makes `k` calls to other.near, then sends 1 yocto to
/// ghost.near, which doesn't exist: the transfer is a different receipt
/// of its transaction for each `k`.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct Ghost {
    calls: u8,
}

impl Op for Ghost {
    fn submit(&self, sim: &mut Sim) -> Result<(), Skip> {
        let call = Fz::Call {
            to: "other.near".into(),
            args: encode(&[]),
            deposit: 0,
            gas: 5 * TGAS,
            weight: 0,
        };
        let mut program = vec![call; usize::from(self.calls)];
        program.push(Fz::Transfer { to: "ghost.near".into(), amount: 1 });
        sim.call("alice.near", "contract.near", "run").args(encode(&program)).submit();
        Ok(())
    }
}

/// A failed transfer is one finding at its receiver, whichever of its
/// transaction's receipts it is.
#[test]
fn a_transfer_is_one_site_whatever_its_position() {
    let ghosts = (0u8..=3).prop_map(|calls| Ghost { calls });
    let strategy = || plans(ghosts.clone()).faults(Faults::none());
    let audit = Harness::new(fixture()).search(strategy()).seed(2).cases(24).run();
    let found: Vec<String> = audit.found().iter().map(|f| f.finding.site.to_string()).collect();
    assert_eq!(found, ["ghost.near (Ghost)"], "{audit}");
}

// ---- the stack ----

/// An op strategy nested `depth` maps deep: each level a boxed
/// `prop_map`, as an op enum of tuples of nested strategies builds up.
/// Generating, shrinking and dropping one recurses once per level.
fn nested(depth: usize) -> BoxedStrategy<Prog> {
    let mut s = (0u8..=127).boxed();
    for _ in 0..depth {
        s = s.prop_map(|v| v.wrapping_add(1) % 128).boxed();
    }
    s.prop_map(Prog::Byte).boxed()
}

/// A search of deeply nested strategies runs on a small stack: the
/// library runs it on a large one of its own.
#[test]
fn a_deeply_nested_strategy_needs_no_stack_setting() {
    let small = std::thread::Builder::new().name("nested_strategy".into()).stack_size(1 << 20);
    let found = small
        .spawn(|| {
            let dir = std::env::temp_dir().join(format!("nested-{}", std::process::id()));
            let harness = Harness::new(fixture()).store(&dir);
            let audit = harness.search(plans(nested(20_000)).len(1..=2)).seed(1).cases(4).run();
            assert_eq!(audit.cases(), 4, "{audit}");
            let found = audit.found().first().unwrap_or_else(|| panic!("{audit}"));
            assert!(found.replay.is_some(), "a replay file: {audit}");
            std::fs::remove_dir_all(dir).ok();
            audit.found().len()
        })
        .unwrap()
        .join()
        .expect("no overflow, no panic");
    assert!(found > 0);
}

// ---- just enough gas ----

/// alice, and settle_starves.wat on pay.near.
fn pay_world() -> Sim {
    let wasm = wat::parse_str(std::fs::read_to_string(res_path("settle_starves.wat")).unwrap());
    Fixture::new()
        .account("alice.near", Balance::from_near(100))
        .contract("pay.near", wasm.unwrap())
        .build()
}

fn res_path(name: &str) -> std::path::PathBuf {
    std::path::Path::new(&std::env::var("CARGO_MANIFEST_DIR").unwrap()).join("tests/res").join(name)
}

fn pay() -> near_contract_sim::explore::Call {
    near_contract_sim::explore::Call::new("alice.near", "pay.near", "pay")
}

/// The default search finds a settlement that starves only when the
/// caller attaches just enough. Unset gas (300 Tgas) settles.
#[test]
fn the_default_search_finds_a_callback_that_starves_at_just_enough_gas() {
    let harness = Harness::new(pay_world());
    harness.run(&Plan::new().op(pay())).assert_clean();
    let audit = harness.search(plans(Just(pay()))).seed(1).run();
    let found = audit.assert_found(builtin::UNHANDLED_FAILURE, "pay.near::on_pay");
    assert!(!found.finding.injected, "{audit}");
    let gas = found.run.plan().steps.iter().filter_map(|s| s.gas);
    assert!(gas.into_iter().any(|g| matches!(g, StepGas::LeastPlus(_))), "{audit}");
}

// ---- hostile replies ----

const LEDGER: &str = "ledger.near";
const RECEIVER: &str = "receiver.near";
/// What the receiver holds before any case.
const LEDGER_HELD: u64 = 1000;

/// alice and bob with 10^6 credit each and the receiver with `LEDGER_HELD`, on
/// refund_ledger.wat: as written (the refund capped by what the receiver
/// holds) or `fixed` (capped by what it was sent). The receiver is a stub.
fn ledger_world(fixed: bool) -> Sim {
    let wat = std::fs::read_to_string(res_path("refund_ledger.wat")).unwrap();
    let cap = "(if (i64.gt_u (local.get $take) (local.get $amount)) \
               (then (local.set $take (local.get $amount))))";
    let wat = if fixed { wat.replace(";; {cap}", cap) } else { wat };
    let fund = |who: &'static str, n: u64| {
        move |sim: &mut Sim| {
            sim.call(who, LEDGER, "fund").args(n.to_le_bytes()).transact().assert_success();
        }
    };
    Fixture::new()
        .account("alice.near", Balance::from_near(100))
        .account("bob.near", Balance::from_near(100))
        .contract(LEDGER, wat::parse_str(&wat).unwrap())
        .stub(RECEIVER, Stub::new().on("on_receive", Reply::json("0")))
        .setup("fund alice", fund("alice.near", 1_000_000))
        .setup("fund bob", fund("bob.near", 1_000_000))
        .setup("fund the receiver", fund(RECEIVER, LEDGER_HELD))
        .build()
}

fn ledger_credit(sim: &Sim, who: &str) -> u64 {
    let bytes = sim.view_raw(LEDGER, "credit_of", who.as_bytes().to_vec()).result.unwrap();
    u64::from_le_bytes(bytes.try_into().unwrap())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
enum Holder {
    Alice,
    Bob,
}

impl Holder {
    fn id(self) -> &'static str {
        match self {
            Holder::Alice => "alice.near",
            Holder::Bob => "bob.near",
        }
    }
    fn other(self) -> Holder {
        match self {
            Holder::Alice => Holder::Bob,
            Holder::Bob => Holder::Alice,
        }
    }
}

/// What the receiver answers a send: the refund it asks, relative to
/// what it was sent, in `answer`'s shape, or a panic.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct Answered {
    refund: Amount,
    answer: input::Answer,
}

/// The ledger's operations, typed as the docs teach: amounts are
/// `Amount`s, the receiver's side an `Answer` bound to the send.
#[derive(Clone, Debug, Serialize, Deserialize)]
enum LedgerOp {
    /// Credit to the receiver; with `answer`, it answers this send,
    /// else it keeps everything.
    Send { from: Holder, amount: Amount, answer: Option<Answered> },
    /// Credit to the other holder, who has no contract: it all comes back.
    Give { from: Holder, amount: Amount },
    /// More credit, of up to 1000.
    Fund { who: Holder, amount: Amount },
}

/// A send of `amount` of `from`'s credit to `to`.
fn ledger_send<'a>(
    sim: &'a mut Sim,
    from: Holder,
    amount: Amount,
    to: &str,
) -> Result<(u128, near_contract_sim::Tx<'a>), Skip> {
    let n = amount.of(u128::from(ledger_credit(sim, from.id())))?;
    if n == 0 {
        return Err(Skip::from("nothing to send"));
    }
    let mut args = (n as u64).to_le_bytes().to_vec();
    args.extend(to.as_bytes());
    Ok((n, sim.call(from.id(), LEDGER, "send").args(args)))
}

impl Op for LedgerOp {
    fn submit(&self, sim: &mut Sim) -> Result<(), Skip> {
        match self {
            LedgerOp::Send { from, amount, answer } => {
                let (n, mut tx) = ledger_send(sim, *from, *amount, RECEIVER)?;
                if let Some(Answered { refund, answer }) = answer {
                    let back = refund.of(n)?.to_string();
                    tx = tx.stub_reply(RECEIVER, "on_receive", answer.reply(back));
                }
                tx.submit();
            }
            LedgerOp::Give { from, amount } => {
                ledger_send(sim, *from, *amount, from.other().id())?.1.submit();
            }
            LedgerOp::Fund { who, amount } => {
                let n = amount.of(1000)?;
                if n == 0 || n > 1000 {
                    return Err("funds 1 to 1000".into());
                }
                sim.call(who.id(), LEDGER, "fund").args((n as u64).to_le_bytes()).submit();
            }
        }
        Ok(())
    }
}

fn ledger_ops() -> impl Strategy<Value = LedgerOp> {
    let holder = || prop_oneof![Just(Holder::Alice), Just(Holder::Bob)];
    let answered =
        (amount::any(), input::answer()).prop_map(|(refund, answer)| Answered { refund, answer });
    prop_oneof![
        (holder(), amount::any(), proptest::option::of(answered))
            .prop_map(|(from, amount, answer)| LedgerOp::Send { from, amount, answer }),
        (holder(), amount::any()).prop_map(|(from, amount)| LedgerOp::Give { from, amount }),
        (holder(), amount::any()).prop_map(|(who, amount)| LedgerOp::Fund { who, amount }),
    ]
}

const RECEIVER_KEEPS: &str = "the receiver keeps what it held";

fn ledger_harness(fixed: bool) -> Harness<LedgerOp> {
    Harness::new(ledger_world(fixed)).always(RECEIVER_KEEPS, |sim: &Sim| {
        let held = ledger_credit(sim, RECEIVER);
        if held >= LEDGER_HELD {
            Ok(())
        } else {
            Err(format!("the receiver holds {held}, below {LEDGER_HELD}"))
        }
    })
}

/// A receiver that answers more than it was sent loses what it held: the
/// default search finds it, shrunk to one send, and nothing on the
/// ledger that caps the refund at what was sent.
#[test]
fn the_default_search_finds_a_reply_that_refunds_more_than_sent() {
    let search = |fixed| ledger_harness(fixed).search(plans(ledger_ops())).seed(1).run();
    let audit = search(false);
    let found = audit.assert_found(RECEIVER_KEEPS, "world");
    assert!(!found.finding.injected, "{audit}");
    assert_eq!(found.run.plan().steps.len(), 1, "{audit}");
    search(true).assert_clean();
}

/// One cause, one row keeps a receipt's own failure: two cases of one op
/// whose transaction first fails at `missing-a.near`, then fails a
/// detached call back into contract.near, once by a panic and once out
/// of gas, are two rows of `unhandled failure` at that receipt, never one
/// keyed by the earlier failure they share.
///
/// Verification boundary: report rows over actual scans of the fuzz
/// contract.
#[test]
fn a_receipts_findings_fold_only_when_it_failed_the_same_way() {
    let inner = |p: Fz| Fz::Call {
        to: "contract.near".into(),
        args: encode(&[p]),
        deposit: 0,
        gas: 10 * TGAS,
        weight: 0,
    };
    let case = |p: Fz| Plan::new().op(Prog::Run(vec![detached_call("missing-a.near"), inner(p)]));
    let plans = [case(Fz::Panic), case(Fz::BurnGas(16_000))];
    let audit = Harness::new(fixture()).scan(&plans).faults(Faults::none()).run();
    let at = |f: &&near_contract_sim::explore::Found<Prog>| {
        f.finding.check == builtin::UNHANDLED_FAILURE
            && f.finding.site.to_string().starts_with("contract.near::run")
    };
    assert_eq!(audit.found().iter().filter(at).count(), 2, "{audit}");
}

/// Writes contract.near's `x` directly: an op with no transaction.
#[derive(Clone, Debug, Serialize, Deserialize)]
enum Edit {
    A,
    B,
}

impl Op for Edit {
    fn submit(&self, sim: &mut Sim) -> Result<(), Skip> {
        let v: &[u8] = match self {
            Edit::A => b"a",
            Edit::B => b"b",
        };
        sim.set_state("contract.near", b"x", Some(v));
        Ok(())
    }
}

/// Two kinds of op with no transaction, each breaking one check in its
/// own case, are two rows with a replay each. Neither was a
/// breaker, so both had one identity, and the second was a case of the
/// first, its replay lost.
#[test]
fn ops_with_no_transaction_breaking_one_check_are_two_rows() {
    const CHECK: &str = "x is never written";
    let unwritten = |sim: &Sim| match sim.state("contract.near", b"x") {
        Some(_) => Err("x was written".to_string()),
        None => Ok(()),
    };
    let h = Harness::new(fixture()).at_rest(CHECK, unwritten);
    let plans = [Plan::new().op(Edit::A), Plan::new().op(Edit::B)];
    let audit = h.scan(&plans).faults(Faults::none()).run();
    let rows: Vec<_> = audit.found().iter().filter(|f| f.finding.check == CHECK).collect();
    let replays: std::collections::BTreeSet<_> =
        rows.iter().filter_map(|f| f.replay.clone()).collect();
    assert_eq!((rows.len(), replays.len()), (2, 2), "{audit}");
}

/// A search drawing only `Settle` gaps keeps them through shrinking: the
/// only `Blocks(0)` a reduced case holds is a drawn repeat's,
/// into a copy of the same op. The plan reducer dropped a repeat's last
/// copy and left its first `Blocks(0)` into the next, unrelated op: an
/// overlap the strategy never drew, in a saved replay. (Proptest's own
/// shrinking usually drops the repeat first, so this guards the whole
/// path; `shrink.rs`'s unit test pins the reducer's rule.)
///
/// Verification boundary: the plan reducer's candidates, over actual
/// searches. The check needs both keys written, so the reduced case keeps
/// two ops.
#[test]
fn a_reduced_case_keeps_only_drawn_gaps() {
    let write = |k: &[u8]| Prog::Run(vec![Fz::Write { key: k.to_vec(), value: b"1".to_vec() }]);
    let h = Harness::new(fixture()).at_rest("a and b never both", |sim: &Sim| {
        let has = |k: &[u8]| sim.state("contract.near", k).is_some();
        match has(b"a") && has(b"b") {
            true => Err("a and b both written"),
            false => Ok(()),
        }
    });
    let ops = prop_oneof![Just(write(b"a")), Just(write(b"b")), Just(write(b"n"))];
    let strategy =
        plans(ops).len(2..=6).gaps(Just(Gap::Settle)).gas(gas::off()).faults(Faults::none());
    let audit = h.search(strategy).seed(5).cases(64).run();
    assert!(!audit.found().is_empty(), "{audit}");
    for f in audit.found() {
        let steps = &f.run.plan().steps;
        for (i, s) in steps.iter().enumerate() {
            if s.then == Gap::Blocks(0) {
                let next = steps.get(i + 1).map(|n| &n.act);
                assert_eq!(next, Some(&s.act), "an undrawn overlap at step {i}: {steps:#?}");
            }
        }
    }
}

/// Faults reach only receipts a transaction spawns (continuations,
/// callbacks): on single-receipt ops none lands, and the summary says how
/// many were drawn and landed, and why none did; with a
/// callback, some land.
///
/// Verification boundary: the audit's count of injections over actual
/// searches. No schedule changes.
#[test]
fn the_summary_counts_faults_drawn_and_landed() {
    let write = Prog::Run(vec![Fz::Write { key: b"k".to_vec(), value: b"1".to_vec() }]);
    let faults = Faults::default().per_case(1.0);
    let alone = plans(Just(write)).len(1..=2).gas(gas::off()).faults(faults.clone());
    let audit = Harness::new(fixture()).search(alone).seed(3).cases(8).run();
    let shown = audit.to_string();
    let line = shown.lines().find(|l| l.starts_with("faults:"));
    let line = line.unwrap_or_else(|| panic!("no faults line:\n{shown}"));
    assert!(line.contains(" drawn, 0 landed") && !line.starts_with("faults: 0 drawn"), "{line}");
    assert!(shown.contains("only receipts a transaction spawns"), "{shown}");
    let callback = Prog::Run(vec![
        Fz::Call {
            to: "other.near".into(),
            args: encode(&[]),
            deposit: 0,
            gas: 10 * TGAS,
            weight: 0,
        },
        Fz::Then { args: encode(&[]), gas: 10 * TGAS, weight: 0 },
        Fz::PromiseReturn,
    ]);
    let chained = plans(Just(callback)).len(1..=2).gas(gas::off()).faults(faults);
    let audit = Harness::new(fixture()).search(chained).seed(3).cases(8).run();
    let shown = audit.to_string();
    let line = shown.lines().find(|l| l.starts_with("faults:")).unwrap_or_default();
    assert!(!line.contains(" 0 landed") && !line.is_empty(), "{shown}");
    // A case whose run errs before its faults are placed drew them all the
    // same.
    let write = Prog::Run(vec![Fz::Write { key: b"k".to_vec(), value: b"1".to_vec() }]);
    let errs =
        plans(Just(write)).len(1..=1).gas(gas::off()).faults(Faults::default().per_case(1.0));
    let h = Harness::new(fixture())
        .per_op("panics", |_: &OpDone<Prog>| -> Result<(), Fail> { panic!("a broken check") });
    let audit = h.search(errs).seed(3).cases(4).run();
    let shown = audit.to_string();
    let line = shown.lines().find(|l| l.starts_with("faults:")).unwrap_or_default();
    assert!(line.starts_with("faults: 4 drawn"), "{shown}");
}

// ---- a panic in an op or a check is a harness error, not a verdict ----

/// An op's panic is a harness error printing the panic, never a skip; a
/// check's panic in a scan is the audit's error naming the check, never a
/// finding.
///
/// Verification boundary: the harness's verdicts and ledger over actual
/// runs and a search of a trivial world.
mod panics {
    use near_contract_sim::explore::{Faults, Harness, Op, Plan, Skip, Verdict, plans};
    use near_contract_sim::{Balance, Fixture, Sim};
    use proptest::prelude::*;
    use serde::{Deserialize, Serialize};

    const WAT: &str = r#"
    (module
      (memory (export "memory") 1)
      (func (export "ping")))
    "#;

    fn world() -> Sim {
        Fixture::new()
            .account("alice.near", Balance::from_near(10))
            .contract("echo.near", wat::parse_str(WAT).unwrap())
            .build()
    }

    /// Pings, unless its number ends in 7: then `submit` panics, as an
    /// unwrap on a view the plan made empty would.
    #[derive(Clone, Debug, Serialize, Deserialize)]
    struct Ping(u8);

    impl Op for Ping {
        fn submit(&self, sim: &mut Sim) -> Result<(), Skip> {
            assert!(self.0 % 10 != 7, "no ping ends in 7");
            sim.call("alice.near", "echo.near", "ping").submit();
            Ok(())
        }
    }

    /// A search whose op panics on one input in ten records a harness error
    /// that prints the panic for that case: not a silent skip; and runs on,
    /// every case run, as a scan does.
    #[test]
    fn an_op_that_panics_is_a_harness_error_printing_the_panic() {
        let harness = Harness::new(world()).audits(["echo.near"]);
        let audit =
            harness.search(plans(any::<u8>().prop_map(Ping)).len(1..=3)).cases(64).seed(1).run();
        assert_eq!(audit.cases(), 64, "{audit}");
        assert!(audit.errors().len() > 1, "one error per case it panicked in: {audit}");
        let error = audit.errors().first().unwrap_or_else(|| panic!("{audit}"));
        assert!(
            error.contains("Op::submit panicked") && error.contains("no ping ends in 7"),
            "{error}"
        );
        let open = audit.done().expect_err("the error is open");
        assert!(
            open.iter().any(|t| t.id == "errors" && t.why.contains("no ping ends in 7")),
            "{open:#?}"
        );
        let run = harness.run(&Plan::new().op(Ping(7)));
        assert_eq!(run.verdict(), Verdict::HarnessError, "{run}");
        let shown = run.to_string();
        assert!(shown.starts_with("verdict: harness error: Op::submit panicked"), "{shown}");
        assert!(shown.contains("no ping ends in 7"), "{shown}");
    }

    /// A harness error fails the test: a check that panics in a scan is the
    /// audit's error, naming the check, never a finding, and the audit is
    /// neither clean nor done.
    ///
    /// Verification boundary: the campaign verbs' handling of a harness
    /// error, over actual runs.
    #[test]
    fn a_harness_error_fails_the_scan() {
        let harness =
            Harness::new(world()).at_rest("ledger adds up", |_: &Sim| -> Result<(), String> {
                panic!("the view returned nothing");
            });
        let plan = Plan::new().op(Ping(1));
        let audit = harness.scan(&[plan]).faults(Faults::none()).run();
        let error = audit.errors().first().cloned().unwrap_or_default();
        assert!(error.contains("the check \"ledger adds up\" panicked"), "{error}");
        assert!(audit.found().is_empty() && !audit.is_clean(), "{audit}");
        let open = audit.done().expect_err("not done");
        assert!(
            open.iter().any(|t| t.id == "errors" && t.why.contains("ledger adds up")),
            "{open:#?}"
        );
    }
}
