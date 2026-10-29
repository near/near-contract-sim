//! The floor specimen (`tests/res/floor.wat`): a keeper pays out through a
//! token's `transfer_call`, forwarding the prepaid gas less 20 Tgas, but at
//! least the `min_gas` the caller signs and at least a 30 Tgas floor. The
//! floor is the token's own reserve, and the token wants more than that;
//! the keeper counts a failed call as delivered. So a caller who signs
//! `min_gas` at the floor and attaches just enough gas makes the keeper
//! forward exactly the floor, the token refuses, and the amount is gone,
//! while the transaction succeeds. The shape of a signed floor equal to a
//! counterparty's reserve, as a generic specimen. The fixed build's floor
//! is 35 Tgas.
//!
//! The op signs 40 Tgas by default, so the caller's gas alone never
//! reaches the floor. The scan finds it with nothing declared: `min_gas`
//! is a number of the op's serde form between 1 Tgas and 1 PGas, so its
//! edges 0, 1 and its type's max run at the least caller gas.
//!
//! Verification boundary: the scan's edges over actual runs of the
//! specimen on the pinned runtime.

mod common;
use common::{manifest_dir, panic_message};
use near_contract_sim::explore::{Audit, Faults, Found, Harness, Label, Op, Plan, Skip, StepGas};
use near_contract_sim::{Balance, Fixture, Gas, ReceiptOutcome, Sim, TxOutcome};
use serde::{Deserialize, Serialize};

const KEEPER: &str = "keeper.near";
const TOKEN: &str = "token.near";
const ALICE: &str = "alice.near";
/// Alice's credit, and the tokens backing it, before any case.
const HELD: u64 = 1000;
/// The keeper's floor, the token's reserve: the least `min_gas` it takes.
const FLOOR: Gas = Gas::from_teragas(30);
/// The `min_gas` an op signs unless set: safely above the floor.
const SIGNED: Gas = Gas::from_teragas(40);
const CHECK: &str = "alice's credit and tokens add up";

/// The module, with the floor at 30 Tgas, or at 35 if `fixed`.
fn wasm(fixed: bool) -> Vec<u8> {
    let wat = std::fs::read_to_string(manifest_dir().join("tests/res/floor.wat")).unwrap();
    let floor = "(global $floor i64 (i64.const 30000000000000))";
    assert!(wat.contains(floor));
    let wat = match fixed {
        true => wat.replace(floor, "(global $floor i64 (i64.const 35000000000000))"),
        false => wat,
    };
    wat::parse_str(&wat).unwrap()
}

/// `wat` (the floor module's text) with a registry: the token refuses a
/// receiver that unregistered (`"u"` + its id set), past its gas check,
/// with `receiver not registered`.
fn registry(wat: &str) -> Vec<u8> {
    let to = "(local.set $to (i64.sub (call $read_input) (i64.const 8)))";
    let refuse = "\n    (if (i64.ne (call $get (i32.const 117) (i64.const 1032) (local.get $to)) \
                  (i64.const 0)) (then (call $panic_utf8 (i64.const 23) (i64.const 2900))))";
    let unregister = "(func (export \"unregister\") \
                      (call $credit_predecessor (i32.const 117) (i64.const 1)))\n  \
                      (func (export \"register\")";
    let data = "(data (i32.const 2900) \"receiver not registered\")\n  (data (i32.const 208)";
    assert!(wat.contains(to));
    let wat = (wat.replacen(to, &format!("{to}{refuse}"), 1))
        .replacen("(func (export \"register\")", unregister, 1)
        .replacen("(data (i32.const 208)", data, 1);
    wat::parse_str(&wat).unwrap()
}

/// The keeper holds `HELD` tokens and owes them to alice.
fn world(fixed: bool) -> Sim {
    world_of(wasm(fixed), false)
}

/// The keeper's world on `code`; `unregistered`: alice unregisters from
/// the token first.
fn world_of(code: Vec<u8>, unregistered: bool) -> Sim {
    let fixture = Fixture::new()
        .account(ALICE, Balance::from_near(100))
        .contract(KEEPER, code.clone())
        .contract(TOKEN, code)
        .setup("the token backs the keeper", |sim| {
            sim.call(KEEPER, TOKEN, "mint").args(HELD.to_le_bytes()).transact().assert_success();
        })
        .setup("alice's credit", |sim| {
            sim.call(ALICE, KEEPER, "fund").args(HELD.to_le_bytes()).transact().assert_success();
        });
    match unregistered {
        true => fixture.setup("alice unregisters from the token", |sim| {
            sim.call(ALICE, TOKEN, "unregister").transact().assert_success();
        }),
        false => fixture,
    }
    .build()
}

/// A u64 view of `who` on `account`.
fn read(sim: &Sim, account: &str, method: &str, who: &str) -> u64 {
    let bytes = sim.view_raw(account, method, who.as_bytes().to_vec()).result.unwrap();
    u64::from_le_bytes(bytes.try_into().unwrap())
}

fn withdraw(sim: &mut Sim, amount: u64, min_gas: Gas) -> near_contract_sim::Tx<'_> {
    let args = [amount.to_le_bytes(), min_gas.as_gas().to_le_bytes()].concat();
    sim.call(ALICE, KEEPER, "withdraw").args(args)
}

// ---- everyday tests ----

/// At the default 300 Tgas, a withdraw pays alice; signing `min_gas` at
/// the floor and attaching the least gas the transaction succeeds with,
/// the keeper forwards exactly the floor, the token refuses, and alice
/// loses the amount, though the transaction succeeds. The fixed floor
/// clears the token's reserve; so does the default `min_gas`.
#[test]
fn just_enough_gas_loses_the_amount_at_the_floor() {
    let mut sim = world(false);
    withdraw(&mut sim, 10, FLOOR).transact().assert_success();
    assert_eq!(read(&sim, TOKEN, "balance_of", ALICE), 10);

    for (fixed, min_gas, delivered) in [(false, FLOOR, 0), (true, FLOOR, 10), (false, SIGNED, 10)] {
        let mut sim = world(fixed);
        let least = withdraw(&mut sim, 10, min_gas).least_gas().expect("it succeeds at the most");
        assert!((40..70).contains(&(least.as_gas() / 1_000_000_000_000)), "{least}");
        let out = withdraw(&mut sim, 10, min_gas).gas(least).transact();
        out.assert_success();
        assert_eq!(read(&sim, KEEPER, "credit_of", ALICE), HELD - 10, "{out}");
        let at = format!("fixed {fixed}, min_gas {min_gas}: {out}");
        assert_eq!(read(&sim, TOKEN, "balance_of", ALICE), delivered, "{at}");
    }
}

/// Where alice loses the amount: the transaction succeeds and the token's
/// call fails.
fn lost(out: &TxOutcome) -> bool {
    let refused = |r: &ReceiptOutcome| r.method().is_some_and(|m| m == "transfer_call");
    out.is_success() && out.receipts.iter().any(|r| refused(r) && !r.is_success())
}

/// `Tx::gas_edges` finds the band of caller gas where the floor loses
/// the amount, read every 10 Tgas over 1 to 300 Tgas: about 7 Tgas wide
/// (42.8 to 50), it lies between reads at 41 and 51, which a sweep at
/// that step stepped over. Its sides differ in shape (the withdraw fails
/// below, the token's call succeeds above), and bisecting them lands on
/// it at 46, so under `OutcomeShape::of` the grid finds it, both edges exact
/// to 1 gas, from the least gas the withdraw succeeds with. Under the
/// `lost` predicate both reads are `false`, and the solved point where
/// the keeper forwards exactly the 30 Tgas floor finds it. The fixed
/// floor has none.
#[test]
fn gas_edges_find_the_band_where_the_amount_is_lost() {
    use near_contract_sim::explore::{OutcomeShape, Why};
    let tgas = Gas::from_teragas;
    let mut sim = world(false);
    let height = sim.height();
    let shapes =
        withdraw(&mut sim, 10, FLOOR).gas_edges(tgas(1)..=tgas(300), tgas(10), OutcomeShape::of);
    assert_eq!(sim.height(), height, "nothing submitted");
    let least = withdraw(&mut sim, 10, FLOOR).least_gas().expect("it succeeds at the most");
    let i = (shapes.iter().position(|r| *r.range.start() == least.as_gas()))
        .unwrap_or_else(|| panic!("{shapes:#?}"));
    let [below, band, above] = &shapes[i - 1..=i + 1] else { panic!("{shapes:#?}") };
    assert!(below.key.to_string().starts_with("failed"), "{}", below.key);
    assert!(band.key.to_string().contains("token.near::transfer_call#0 Panic"), "{}", band.key);
    assert!(above.key.to_string().contains("token.near::transfer_call#0 ok"), "{}", above.key);
    assert_eq!(band.why, Why::Grid);
    let at =
        |gas: u64| lost(&withdraw(&mut sim.clone(), 10, FLOOR).gas(Gas::from_gas(gas)).transact());
    let (start, end) = (*band.range.start(), *band.range.end());
    assert_eq!([start - 1, start, end, end + 1].map(at), [false, true, true, false]);
    assert!(end - start < tgas(10).as_gas(), "narrower than the step: {band:?}");

    let found = withdraw(&mut sim, 10, FLOOR).gas_edges(tgas(1)..=tgas(300), tgas(10), lost);
    let windows: Vec<_> = found.iter().filter(|r| r.key).collect();
    let [window] = &windows[..] else { panic!("{found:#?}") };
    assert_eq!((&window.range, window.why), (&band.range, Why::Solved));

    let mut fixed = world(true);
    let none = withdraw(&mut fixed, 10, FLOOR).gas_edges(tgas(1)..=tgas(300), tgas(5), lost);
    assert!(none.iter().all(|r| !r.key), "{none:#?}");
}

/// `gas_edges` refuses what it can't run: a step of 0, a range that is
/// empty, starts at 0 or ends past what the call can attach, and a
/// transaction with no unpinned call. 1 gas to the most is fine.
#[test]
fn gas_edges_refuse_what_they_cannot_run() {
    use std::panic::{AssertUnwindSafe, catch_unwind};
    let mut sim = world(false);
    let max = sim.gas_ceiling();
    let one = Gas::from_gas(1);
    let refused = |sim: &mut Sim, range, step, pin: bool| {
        let tx = withdraw(sim, 10, FLOOR);
        let tx = if pin { tx.gas(max) } else { tx };
        panic_message(catch_unwind(AssertUnwindSafe(|| tx.gas_edges(range, step, |_| true))))
    };
    let step = Gas::from_teragas(500);
    assert!(refused(&mut sim, one..=max, Gas::ZERO, false).contains("a step of 0"));
    for range in [Gas::ZERO..=max, max..=one] {
        assert!(refused(&mut sim, range, step, false).contains("is empty or starts at 0"));
    }
    let past = Gas::from_gas(max.as_gas() + 1);
    assert!(refused(&mut sim, one..=past, step, false).contains("the most each unpinned call"));
    assert!(refused(&mut sim, one..=max, step, true).contains("has none"));
    let all = withdraw(&mut sim, 10, FLOOR).gas_edges(one..=max, step, |_| true);
    assert_eq!(all.len(), 1);
    assert_eq!(all[0].range, 1..=max.as_gas());
}

// ---- the audit harness ----

#[derive(Clone, Debug, Serialize, Deserialize)]
enum KeeperOp {
    /// Withdraws `amount`, signing `min_gas` for the token's call.
    Withdraw { amount: u64, min_gas: Gas },
    /// The same withdraw at the signed default, as another kind of op.
    Payout { amount: u64 },
}

impl Op for KeeperOp {
    fn submit(&self, sim: &mut Sim) -> Result<(), Skip> {
        let (amount, min_gas) = match *self {
            KeeperOp::Withdraw { amount, min_gas } => (amount, min_gas),
            KeeperOp::Payout { amount } => (amount, SIGNED),
        };
        if amount == 0 || read(sim, KEEPER, "credit_of", ALICE) < amount {
            return Err("nothing to withdraw".into());
        }
        withdraw(sim, amount, min_gas).submit();
        Ok(())
    }
}

/// What alice holds in credit and tokens never changes: a withdraw moves
/// one into the other.
fn keeper(fixed: bool) -> Harness<KeeperOp> {
    Harness::new(world(fixed)).at_rest(CHECK, |sim: &Sim| {
        let credit = read(sim, KEEPER, "credit_of", ALICE);
        let tokens = read(sim, TOKEN, "balance_of", ALICE);
        match credit + tokens {
            HELD => Ok(()),
            n => Err(format!("alice holds {credit} credit and {tokens} tokens, {n} of {HELD}")),
        }
    })
}

/// A withdraw of 10, signing `min_gas`.
fn withdraw_op(min_gas: Gas) -> KeeperOp {
    KeeperOp::Withdraw { amount: 10, min_gas }
}

/// The witness: a withdraw as a wallet signs it, at the default.
fn witness() -> Plan<KeeperOp> {
    Plan::new().op(withdraw_op(SIGNED))
}

/// The edges at which the check failed with nothing forced.
fn shown(audit: &Audit<KeeperOp>) -> Vec<String> {
    (audit.found().iter())
        .filter(|x| x.finding.check == CHECK)
        .filter_map(|x| match &x.label {
            Label::ShownAt(at) => Some(at.clone()),
            Label::Shown => Some("as written".into()),
            Label::Forced(_) => None,
        })
        .collect()
}

/// The four controls, nothing forced: the signed default is clean at
/// the caller's full gas and at the least it succeeds with; the floor is
/// clean at full gas and loses the amount at the least.
#[test]
fn a_signed_floor_loses_value_only_at_the_least_gas() {
    let least = StepGas::LeastPlus(Gas::ZERO);
    let case = |min_gas, gas: Option<StepGas>| {
        let plan = Plan::new().op(withdraw_op(min_gas));
        keeper(false).run(&match gas {
            Some(g) => plan.gas(g),
            None => plan,
        })
    };
    case(SIGNED, None).assert_clean();
    case(SIGNED, Some(least)).assert_clean();
    case(FLOOR, None).assert_clean();
    let lost = case(FLOOR, Some(least));
    let f = lost.assert_found(CHECK, "world");
    assert!(!f.injected, "{lost}");
    let tx = &lost.outcomes()[0];
    assert!(tx.is_success(), "the result path succeeds: {lost}");
    let call = tx.expect_receipt("transfer_call");
    assert_eq!(call.attached_gas, FLOOR, "{lost}");
}

/// A signed floor from numeric edges, nothing declared: the default scan of the
/// witness (which signs the safe default) sets `min_gas` to 0 (clamped
/// up to the floor) and to the floor, at the least caller gas, and finds
/// the loss with nothing forced. The caller's gas alone doesn't reach it,
/// and the fixed build (floor 35 Tgas) shows it at no edge.
#[test]
fn the_default_scan_finds_a_signed_floor_from_its_numeric_edges() {
    let audit = keeper(false).scan(&[witness()]).run();
    let at = shown(&audit);
    assert!(!at.is_empty(), "shown at no edge:\n{audit}");
    let min_gas = "/Withdraw/min_gas";
    let edge = |e: &str| {
        at.iter().any(|a| a.contains(&format!("{min_gas} = {e}")) && a.contains("least + 0"))
    };
    // One finding: the first edge that showed it labels it; the faults
    // that force the same loss fold into it as evidence, not rows.
    assert!(edge("0"), "{at:#?}");
    let x = audit.found().iter().find(|x| matches!(x.label, Label::ShownAt(_))).unwrap();
    assert_eq!(audit.found().len(), 1, "{audit}");
    assert!(!x.also.is_empty(), "{audit}");
    assert!(!at.iter().any(|a| !a.contains(min_gas)), "the caller's gas alone: {at:#?}");
    let fixed = keeper(true).scan(&[witness()]).run();
    assert!(shown(&fixed).is_empty(), "{fixed}");
    // Faults::none() runs the witness alone.
    let plain = keeper(false).scan(&[witness()]).faults(Faults::none()).run();
    assert_eq!((plain.cases(), plain.found().len()), (1, 0), "{plain}");
}

/// A second keeper, paying through its own token.
const KEEPER2: &str = "keeper2.near";
const TOKEN2: &str = "tokem.near";

/// A withdraw of 10 at the signed default, through the second keeper if
/// `.0`.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct WithdrawVia(bool);

impl Op for WithdrawVia {
    fn submit(&self, sim: &mut Sim) -> Result<(), Skip> {
        let keeper = if self.0 { KEEPER2 } else { KEEPER };
        let args = [10u64.to_le_bytes(), SIGNED.as_gas().to_le_bytes()].concat();
        sim.call(ALICE, keeper, "withdraw").args(args).submit();
        Ok(())
    }
}

/// One failure class at two receipts is two findings. Two keepers, each
/// paying through its own registry token, and alice unregistered from
/// both: a withdraw through either loses its amount to `receiver not
/// registered`, at `token.near::transfer_call` or at
/// `tokem.near::transfer_call`, breaking one at-rest check at the world
/// with nothing forced. Those are two contracts' code, two root causes;
/// keyed by the failing receipt's method alone they were one finding.
/// They stay two in one scan and in two scans saved into one store.
///
/// Verification boundary: finding identity, over actual scans of the
/// floor specimen.
#[test]
fn one_class_at_two_receipts_is_two_findings() {
    use near_contract_sim::explore::Status;
    let src = std::fs::read_to_string(manifest_dir().join("tests/res/floor.wat")).unwrap();
    let token = "(data (i32.const 144) \"token.near\")";
    assert!(src.contains(token));
    let code = registry(&src);
    let second = registry(&src.replacen(token, &format!("(data (i32.const 144) \"{TOKEN2}\")"), 1));
    let sim = Fixture::new()
        .account(ALICE, Balance::from_near(100))
        .contract(KEEPER, code.clone())
        .contract(TOKEN, code.clone())
        .contract(KEEPER2, second)
        .contract(TOKEN2, code)
        .setup("each token backs its keeper", |sim| {
            for (k, t) in [(KEEPER, TOKEN), (KEEPER2, TOKEN2)] {
                sim.call(k, t, "mint").args(HELD.to_le_bytes()).transact().assert_success();
            }
        })
        .setup("alice's credit at each, unregistered from each token", |sim| {
            for (k, t) in [(KEEPER, TOKEN), (KEEPER2, TOKEN2)] {
                sim.call(ALICE, k, "fund").args(HELD.to_le_bytes()).transact().assert_success();
                sim.call(ALICE, t, "unregister").transact().assert_success();
            }
        })
        .build();
    let dir = std::env::temp_dir().join(format!("ncs-two-receipts-{}", std::process::id()));
    let harness = Harness::<WithdrawVia>::new(sim)
        .at_rest(CHECK, |sim: &Sim| {
            let pairs = [(KEEPER, TOKEN), (KEEPER2, TOKEN2)];
            let held: u64 = (pairs.iter())
                .map(|(k, t)| read(sim, k, "credit_of", ALICE) + read(sim, t, "balance_of", ALICE))
                .sum();
            match held {
                n if n == 2 * HELD => Ok(()),
                n => Err(format!("alice holds {n} of {}", 2 * HELD)),
            }
        })
        .store(&dir);
    // The receipt that failed in each shown finding's case.
    let failed = |a: &Audit<WithdrawVia>| -> Vec<String> {
        let shown = a.found().iter().filter(|x| x.finding.check == CHECK && !x.finding.injected);
        let mut at: Vec<String> = shown
            .filter_map(|x| {
                let receipts = x.run.outcomes().iter().flat_map(|o| &o.receipts);
                let r = receipts.into_iter().find(|r| r.failure().is_some())?;
                Some(format!("{}::{}", r.receiver_id, r.method().unwrap_or_default()))
            })
            .collect();
        at.sort();
        at
    };
    let plans = [Plan::new().op(WithdrawVia(false)), Plan::new().op(WithdrawVia(true))];
    let both = ["tokem.near::transfer_call", "token.near::transfer_call"];
    let _ = std::fs::remove_dir_all(&dir);
    let one = harness.scan(&plans).faults(Faults::none()).run();
    assert_eq!(failed(&one), both, "{one}");
    let _ = std::fs::remove_dir_all(&dir);
    for plan in &plans {
        let _ = harness.scan(std::slice::from_ref(plan)).faults(Faults::none()).run();
    }
    let replays = harness.clone().store(&dir).regressions();
    let shows = replays.iter().filter(|r| r.check == CHECK && matches!(r.status, Status::Shows(_)));
    assert_eq!(shows.count(), 2, "{:?}", replays.iter().map(|r| &r.path).collect::<Vec<_>>());
    std::fs::remove_dir_all(&dir).unwrap();
}

/// The op of a wallet that leaves `min_gas` unset (`None`: the keeper's
/// default, the safe `SIGNED`).
#[derive(Clone, Debug, Serialize, Deserialize)]
enum OptionalOp {
    Withdraw { amount: u64, min_gas: Option<Gas> },
}

impl Op for OptionalOp {
    fn submit(&self, sim: &mut Sim) -> Result<(), Skip> {
        let OptionalOp::Withdraw { amount, min_gas } = *self;
        withdraw(sim, amount, min_gas.unwrap_or(SIGNED)).submit();
        Ok(())
    }
}

/// A field the witness leaves unset is still tried at its numeric edges:
/// `min_gas: None` is set to 0 and to each gas edge, as whichever of a
/// number or a decimal string the type takes, and the scan finds the
/// signed floor at the least caller gas, as it does for a signed value.
/// Curves don't climb it (no witness value).
#[test]
fn an_unset_field_is_tried_at_its_numeric_edges() {
    let h = Harness::new(world(false)).at_rest(CHECK, |sim: &Sim| {
        let (credit, tokens) =
            (read(sim, KEEPER, "credit_of", ALICE), read(sim, TOKEN, "balance_of", ALICE));
        match credit + tokens {
            HELD => Ok(()),
            n => Err(format!("alice holds {n} of {HELD}")),
        }
    });
    let witness = Plan::new().op(OptionalOp::Withdraw { amount: 10, min_gas: None });
    let audit = h.scan(std::slice::from_ref(&witness)).run();
    let edge = |x: &&Found<OptionalOp>| match &x.label {
        Label::ShownAt(at) => at.contains("/Withdraw/min_gas = ") && at.contains("least + 0"),
        _ => false,
    };
    assert!(audit.found().iter().any(|x| x.finding.check == CHECK && edge(&x)), "{audit}");
    let curves = h.curves(std::slice::from_ref(&witness)).rungs(4).run();
    assert!(curves.curves().iter().all(|r| !r.axis.name.contains("min_gas")), "{curves}");
}

/// The gas a site forwards to the receipts it makes is not its own use.
/// Along the caller's gas the keeper forwards more and more to the token,
/// while its own use stays flat, far below what it was attached: no row
/// of the keeper's crosses or projects to cross.
///
/// Verification boundary: what a curve counts as a receipt's use, over
/// actual runs of the floor specimen.
#[test]
fn forwarded_gas_is_not_the_sites_own_use() {
    use near_contract_sim::explore::{Reach, Resource};
    let audit = keeper(false).curve(&witness(), 0, "caller gas").run();
    let row = audit
        .curves()
        .iter()
        .find(|r| r.site == format!("{KEEPER}::withdraw") && r.resource == Resource::ReceiptGas);
    let row = row.unwrap_or_else(|| panic!("no keeper row:\n{audit}"));
    let (own, sent) = (row.points.last().unwrap().1, row.forwarded.last().unwrap().1);
    assert!(sent > 10 * own, "forwarded {sent}, own {own}: {row}");
    assert!(own * 4 < row.limit, "own {own} of {}: {row}", row.limit);
    let keeper_rows = audit.curves().iter().filter(|r| r.site.starts_with(KEEPER));
    for r in keeper_rows {
        assert!(!matches!(r.reach, Reach::Crossed { .. }), "{r}");
    }
}

/// A shown loss plus a hold that changes nothing is one shown finding,
/// not a forced one: its case is the plan without the hold. The forced
/// variants of the same loss fold into it as evidence.
#[test]
fn a_shown_loss_plus_an_irrelevant_hold_is_one_shown_finding() {
    use near_contract_sim::{Fault, Receipts};
    let lost = Plan::new()
        .op(withdraw_op(FLOOR))
        .gas(StepGas::LeastPlus(Gas::ZERO))
        .inject(Receipts::method("transfer_call"), Fault::Hold(1));
    let run = keeper(false).run(&lost);
    assert!(run.assert_found(CHECK, "world").injected, "a hold anywhere marks it forced: {run}");
    let audit = keeper(false).scan(&[lost]).faults(Faults::none()).run();
    assert_eq!(audit.found().len(), 1, "{audit}");
    let x = &audit.found()[0];
    assert_eq!(x.label, Label::Shown, "{audit}");
    assert!(x.run.plan().steps[0].inject.is_empty(), "the hold dropped: {audit}");
    // A default scan of the shown witness: every forced variant of the
    // loss is evidence of the one finding.
    let shown = Plan::new().op(withdraw_op(FLOOR)).gas(StepGas::LeastPlus(Gas::ZERO));
    let audit = keeper(false).scan(&[shown]).run();
    let at: Vec<&Found<KeeperOp>> =
        audit.found().iter().filter(|x| x.finding.check == CHECK).collect();
    assert_eq!(at.len(), 1, "{audit}");
    assert!(!at[0].finding.injected && !at[0].also.is_empty(), "{audit}");
}

/// Of a check of the world, a forced loss in another kind of op is its own
/// cause: it stays its own row, never evidence of the shown one.
#[test]
fn a_forced_loss_in_another_kind_of_op_does_not_fold() {
    let shown = Plan::new().op(withdraw_op(FLOOR)).gas(StepGas::LeastPlus(Gas::ZERO));
    let payout = Plan::new().op(KeeperOp::Payout { amount: 10 });
    let audit = keeper(false).scan(&[shown, payout]).run();
    let at: Vec<&Found<KeeperOp>> =
        audit.found().iter().filter(|x| x.finding.check == CHECK).collect();
    let of = |x: &&&Found<KeeperOp>| {
        matches!(x.run.plan().steps[0].act.op(), Some(KeeperOp::Payout { .. }))
    };
    let (payouts, withdraws): (Vec<&&Found<KeeperOp>>, Vec<_>) = at.iter().partition(of);
    assert_eq!(withdraws.len(), 1, "{audit}");
    assert!(!withdraws[0].finding.injected && !withdraws[0].also.is_empty(), "{audit}");
    assert!(!payouts.is_empty() && payouts.iter().all(|x| x.finding.injected), "{audit}");
}

/// The scan reports the band of caller gas where the transaction
/// succeeds but the token's call fails, from a witness signing the floor,
/// with no sweep: one row for the step, from the least gas up to where
/// the keeper forwards past the token's reserve. The fixed floor and the
/// signed default have none.
#[test]
fn the_scan_reports_the_gas_band_where_an_inner_receipt_fails() {
    let at_floor = Plan::new().op(withdraw_op(FLOOR));
    let bands = |fixed, plan: &Plan<KeeperOp>| {
        keeper(fixed).scan(std::slice::from_ref(plan)).faults(Faults::edges()).run().report().bands
    };
    let found = bands(false, &at_floor);
    let [band] = &found[..] else { panic!("{found:#?}") };
    eprintln!("{band}");
    assert!(band.contains("(step 0): the transaction succeeds from "), "{band}");
    assert!(band.ends_with("between, token.near::transfer_call fails"), "{band}");
    assert!(bands(true, &at_floor).is_empty());
    assert!(bands(false, &witness()).is_empty());
}

/// `tests/res/floor.wasm` is `floor.wat` compiled: the first harness
/// (`tests/first_harness.rs`) loads the wasm, as a consumer would.
#[test]
fn floor_wasm_is_floor_wat() {
    let wat = std::fs::read_to_string(manifest_dir().join("tests/res/floor.wat")).unwrap();
    let path = manifest_dir().join("tests/res/floor.wasm");
    let built = wat::parse_str(wat).unwrap();
    if std::env::var_os("NCS_WRITE_FLOOR").is_some() {
        std::fs::write(&path, &built).unwrap();
    }
    let committed = std::fs::read(&path).unwrap_or_default();
    assert!(committed == built, "rebuild it: NCS_WRITE_FLOOR=1 cargo test --test floor floor_wasm");
}
