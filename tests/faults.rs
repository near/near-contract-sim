//! Fault injection (`Tx::inject`, and `explore`'s scan and search) on a
//! token and a vault (`tests/res/vault_pair.wat`): each fault against the
//! same failure reached without one, and a vault whose callback can't fail
//! safely, found by a scan and by a search.

mod common;
mod fuzz_common;
use common::*;
use fuzz_common::{Op, TGAS, encode, fuzz_wasm};
use near_contract_sim::explore::{
    Call, Faults, Harness, Label, Op as XOp, OpDone, Plan, Skip, plans,
};
use near_contract_sim::{
    Balance, CryptoHash, Failure, Fault, Gas, ReceiptOutcome, Receipts, Sim, Tx, TxOutcome, builtin,
};
use proptest::prelude::*;
use serde::{Deserialize, Serialize};
use std::panic::{AssertUnwindSafe, catch_unwind};

fn vault_pair_wasm() -> Vec<u8> {
    wat::parse_file(manifest_dir().join("tests/res/vault_pair.wat")).unwrap()
}

/// alice.near with 100 tokens on token.near, and the vault at vault.near.
fn vault_sim() -> Sim {
    let mut sim = sim_with_users();
    sim.deploy("token.near", vault_pair_wasm());
    sim.deploy("vault.near", vault_pair_wasm());
    sim.call("alice.near", "token.near", "mint")
        .args(100u64.to_le_bytes())
        .transact()
        .assert_success();
    sim
}

/// The same, with the vault crediting only in its callback.
fn safe_vault_sim() -> Sim {
    let mut sim = vault_sim();
    sim.set_state("vault.near", b"*safe", Some(b"1"));
    sim
}

/// alice.near deposits `amount` tokens into the vault.
fn deposit(sim: &mut Sim, amount: u64) -> Tx<'_> {
    let args = [&amount.to_le_bytes()[..], b"vault.near"].concat();
    sim.call("alice.near", "token.near", "transfer_call").args(args)
}

/// The u64 `contract` keeps for `who`.
fn held(sim: &Sim, contract: &str, who: &str) -> u64 {
    sim.state(contract, who.as_bytes()).map_or(0, |v| u64::from_le_bytes(v.try_into().unwrap()))
}

/// alice.near's tokens, the vault's tokens, and alice.near's shares.
fn books(sim: &Sim) -> (u64, u64, u64) {
    let tokens = |who| held(sim, "token.near", who);
    (tokens("alice.near"), tokens("vault.near"), held(sim, "vault.near", "alice.near"))
}

/// Every share is backed by a token the vault holds, and no token appears.
fn conserved(sim: &Sim) -> Result<(), String> {
    match books(sim) {
        (mine, vault, shares) if mine + vault == 100 && vault == shares => Ok(()),
        b => Err(format!("alice's tokens, the vault's, alice's shares: {b:?}")),
    }
}

#[test]
fn the_happy_path_conserves() {
    let mut sim = vault_sim();
    let out = deposit(&mut sim, 40).transact();
    assert_eq!(out.value(), Some(&0u64.to_le_bytes()[..]), "the token took nothing back");
    assert_eq!(books(&sim), (60, 40, 40));
    sim.findings().assert_none();
}

/// The vault credits in `on_transfer` and takes the credit back in
/// `resolve_deposit` if `verify` fails. If `resolve_deposit` itself fails,
/// the token reads `on_transfer` as failed and takes every token back, while
/// the credit stays: shares nothing backs.
#[test]
fn a_resolver_out_of_gas_leaves_a_credit_and_a_refund() {
    let mut sim = vault_sim();
    let out = deposit(&mut sim, 40)
        .inject(Receipts::callback("resolve_deposit"), Fault::OutOfGas)
        .transact();
    let resolve = out.expect_receipt("resolve_deposit");
    assert_eq!(
        (resolve.failure(), &resolve.fault),
        (Some(Failure::OutOfGas), &Some(Fault::OutOfGas))
    );
    assert_eq!(books(&sim), (100, 0, 40), "{out}");
    assert!(conserved(&sim).is_err());

    // The built-in checks excuse it: the fault forced the callback out of
    // gas. The vault's own property above is what catches the credit.
    sim.findings().assert_none();
    assert!(
        out.to_string().contains(
            "vault.near::resolve_deposit [callback of vault.near::verify] [fault: OutOfGas]"
        ),
        "{out}"
    );
}

/// alice.near deposits into the vault: the vault pair's one op.
#[derive(Clone, Debug, Serialize, Deserialize)]
enum VaultOp {
    Deposit {
        amount: u64,
    },
    /// Writes alice.near one share more than she has: damaged state, no
    /// transaction.
    MintShare,
}

impl XOp for VaultOp {
    fn submit(&self, sim: &mut Sim) -> Result<(), Skip> {
        match *self {
            VaultOp::Deposit { amount } => _ = deposit(sim, amount).submit(),
            VaultOp::MintShare => {
                let shares = held(sim, "vault.near", "alice.near");
                sim.set_state("vault.near", b"alice.near", Some(&(shares + 1).to_le_bytes()));
            }
        }
        Ok(())
    }
}

/// The vault pair with `books` checked at rest (the built-in checks run too).
fn books_harness(sim: Sim) -> Harness<VaultOp> {
    Harness::new(sim).at_rest("books", conserved)
}

fn deposit_40() -> Plan<VaultOp> {
    Plan::new().op(VaultOp::Deposit { amount: 40 })
}

/// A scan of one deposit, each of `PromiseFailed` and `OutOfGas` at each
/// receipt it fits, finds the resolver: starved, it leaves shares nothing
/// backs. The variant names the receipt exactly and replays.
#[test]
fn a_scan_finds_the_resolver() {
    let harness = books_harness(vault_sim());
    let faults = Faults::new(Receipts::all(), [Fault::PromiseFailed, Fault::OutOfGas]);
    let report = harness.scan(&[deposit_40()]).faults(faults).run();
    // on_transfer and verify take OutOfGas; the two callbacks take both.
    assert_eq!(report.cases(), 1 + 2 + 2 * 2, "{report}");
    let [found] = report.found() else { panic!("{report}") };
    assert_eq!(
        (found.finding.check.as_str(), found.finding.site.to_string()),
        ("books", "world".into())
    );
    assert_eq!(found.label, Label::Forced("OutOfGas at vault.near::resolve_deposit".into()));
    let [inject] = &found.run.plan().steps[0].inject[..] else { panic!("{report}") };
    assert_eq!(
        (&inject.at, &inject.fault),
        (&Receipts::occurrence("vault.near", "resolve_deposit", 0), &Fault::OutOfGas)
    );
    assert_eq!(found.finding.message, "alice's tokens, the vault's, alice's shares: (100, 0, 40)");
    // The variant's plan replays the finding from the same fixture.
    harness.run(found.run.plan()).assert_found("books", "world");
}

/// The vault that credits only in its callback survives every variant, the
/// scan's `ReturnValue` and `Gas` included.
#[test]
fn a_scan_clears_a_vault_that_credits_in_its_callback() {
    let faults = [
        Fault::PromiseFailed,
        Fault::OutOfGas,
        Fault::Gas(Gas::from_gigagas(500)),
        Fault::ReturnValue(0u64.to_le_bytes().to_vec()),
    ];
    let report = books_harness(safe_vault_sim())
        .scan(&[deposit_40()])
        .faults(Faults::new(Receipts::all(), faults))
        .run();
    report.assert_clean();
    // 4 receipts: on_transfer and verify take OutOfGas and Gas; the two
    // callbacks take all four. And the plain case.
    assert_eq!(report.cases(), 1 + 2 * 2 + 2 * 4, "{report}");
}

/// With only the built-in checks, starving each receipt (`OutOfGas`)
/// finds nothing: the resolvers that fail after their contract wrote are
/// the receipts the fault starved, and the built-in checks excuse a failure
/// the fault forced. The vault's own property is the one that breaks.
#[test]
fn the_built_in_checks_excuse_what_a_fault_forced() {
    let starve = || Faults::new(Receipts::all(), [Fault::OutOfGas]);
    let report = Harness::new(vault_sim()).scan(&[deposit_40()]).faults(starve()).run();
    report.assert_clean();
    assert!(report.cases() > 1, "variants ran: {report}");
    let report = books_harness(vault_sim()).scan(&[deposit_40()]).faults(starve()).run();
    assert!(report.found_at("books", "world").is_some(), "{report}");
}

/// The sweep a fixed budget needs: below what the resolver burns it fails
/// and the books break, at it and above they hold.
#[test]
fn a_gas_sweep_finds_where_the_resolver_fails() {
    let mut probe = vault_sim();
    let out = deposit(&mut probe, 40).transact();
    let resolve = out.expect_receipt("resolve_deposit");
    let needs = resolve.gas_burnt.checked_sub(resolve.prepaid_exec_fees).unwrap();
    for (gas, holds) in [(needs.checked_sub(Gas::from_gas(1)).unwrap(), false), (needs, true)] {
        let mut sim = vault_sim();
        let out = deposit(&mut sim, 40)
            .inject(Receipts::callback("resolve_deposit"), Fault::Gas(gas))
            .transact();
        assert_eq!(conserved(&sim).is_ok(), holds, "{gas}: {out}");
        assert_eq!(out.expect_receipt("resolve_deposit").attached_gas, gas);
    }
}

/// Along `grid`, the level each answer of `at` starts at, each change
/// bisected to `step`. A band that starts and ends between two samples with
/// the same answer goes unseen.
fn levels<T: PartialEq>(grid: &[u64], step: u64, at: impl Fn(u64) -> T) -> Vec<(u64, T)> {
    let mut found = vec![(grid[0], at(grid[0]))];
    for w in grid.windows(2) {
        let mut lo = w[0];
        while at(lo) != at(w[1]) {
            let (was, mut hi) = (at(lo), w[1]);
            while hi - lo > step {
                let mid = lo + (hi - lo) / 2;
                if at(mid) == was { lo = mid } else { hi = mid }
            }
            found.push((hi, at(hi)));
            lo = hi;
        }
    }
    found
}

/// Reachability by the caller's gas alone, with no fault, from 10 Tgas to
/// the 1 PGas cap: at every level the only receipt to fail is
/// `transfer_call`, which then changes nothing, and the books hold. No gas
/// a caller attaches starves `resolve_deposit`, so the scan's finding on it
/// shows a consequence, not a failure a caller can cause by gas.
#[test]
fn the_callers_gas_never_starves_the_resolver() {
    let base = vault_sim();
    let failed = |ggas: u64| {
        let mut sim = base.clone(); // every level starts from the same state
        let out = deposit(&mut sim, 40).gas(Gas::from_gigagas(ggas)).transact();
        assert!(conserved(&sim).is_ok(), "{ggas} Ggas: {out}");
        let failed = out.receipts.iter().filter(|r| !r.is_refund() && r.failure().is_some());
        let mut failed: Vec<String> =
            failed.map(|r| format!("{}.{}", r.receiver_id, r.method_names.join("+"))).collect();
        failed.sort();
        failed
    };
    let grid: Vec<u64> = (1..=100).map(|t| t * 10_000).collect(); // 10 Tgas to the 1 PGas cap
    let found = levels(&grid, 100, failed); // to 0.1 Tgas
    assert_eq!(found, [(10_000, vec!["token.near.transfer_call".to_string()]), (72_890, vec![])]);
}

/// Every account's NEAR, every receipt's gas and status, and the books, as
/// the chain has them when the vault attaches `gas` to `resolve_deposit`
/// itself.
fn genuine(gas: Gas) -> (Sim, TxOutcome) {
    let mut sim = vault_sim();
    sim.set_state("vault.near", b"*gas", Some(&gas.as_gas().to_le_bytes()));
    let out = deposit(&mut sim, 40).transact();
    (sim, out)
}

/// The same with the vault attaching 10 Tgas, and `fault` on
/// `resolve_deposit`.
fn faulted(fault: Fault) -> (Sim, TxOutcome) {
    let mut sim = vault_sim();
    sim.set_state("vault.near", b"*gas", Some(&Gas::from_teragas(10).as_gas().to_le_bytes()));
    let out = deposit(&mut sim, 40).inject(Receipts::callback("resolve_deposit"), fault).transact();
    (sim, out)
}

/// What can tell two runs apart: balances, the books, and each receipt.
fn chain(sim: &Sim, out: &TxOutcome) -> impl PartialEq + std::fmt::Debug {
    let near: Vec<Balance> =
        ["alice.near", "token.near", "vault.near"].map(|a| sim.balance(a)).to_vec();
    let receipts: Vec<_> = out
        .receipts
        .iter()
        .map(|r| {
            (r.receipt_id, r.receiver_id.clone(), r.status.clone(), r.gas_burnt, r.tokens_burnt)
        })
        .collect();
    (near, books(sim), receipts, out.tokens_burnt())
}

/// `Gas(g)` is the chain where the vault attached `g`: every balance,
/// refund and receipt, to the yocto, at a budget that fails midway, one that
/// is enough, and none (`OutOfGas`). Only the gas the receipts attach shows
/// the fault, and conservation holds.
#[test]
fn gas_is_what_attaching_it_does() {
    let burnt = faulted(Fault::Gas(Gas::from_teragas(10))).1;
    let resolve = burnt.expect_receipt("resolve_deposit");
    let midway = resolve.gas_burnt.checked_sub(resolve.prepaid_exec_fees).unwrap().as_gas() / 2;
    let cases = [
        (Fault::Gas(Gas::from_gas(midway)), Gas::from_gas(midway)),
        (Fault::Gas(Gas::from_teragas(3)), Gas::from_teragas(3)),
        (Fault::OutOfGas, Gas::from_gas(1)),
    ];
    for (fault, gas) in cases {
        let (want_sim, want) = genuine(gas);
        let (got_sim, got) = faulted(fault.clone());
        assert_eq!(chain(&got_sim, &got), chain(&want_sim, &want), "{fault}: {got:#}\n{want:#}");
        let fails = gas < Gas::from_teragas(1);
        assert_eq!(got.expect_receipt("resolve_deposit").failure().is_some(), fails, "{got}");
        if fails {
            assert_eq!(got.expect_receipt("resolve_deposit").failure(), Some(Failure::OutOfGas));
        }
        for sim in [&got_sim, &want_sim] {
            let mut vs = sim.findings().into_iter().collect::<Vec<_>>();
            vs.retain(|v| v.check == builtin::CONSERVATION);
            assert!(vs.is_empty(), "{vs:?}");
        }
    }
}

/// Gas taken from a receipt goes into its creator's refund, and gas added
/// comes out of it, up to what the creator had left; past that, no creator
/// could have attached it.
#[test]
fn more_gas_comes_out_of_the_creators_refund() {
    let (sim, out) = faulted(Fault::Gas(Gas::from_teragas(20)));
    assert_eq!(out.expect_receipt("resolve_deposit").attached_gas, Gas::from_teragas(20));
    let (want_sim, want) = genuine(Gas::from_teragas(20));
    assert_eq!(chain(&sim, &out), chain(&want_sim, &want));
    let r = catch_unwind(|| faulted(Fault::Gas(Gas::from_teragas(100))));
    let m = panic_message(r);
    // 90 Tgas more at the 1e9 yocto a receipt's gas is bought at (`min_gas_purchase_price`).
    assert!(
        m.contains(": Fault::Gas adds 90000000000000000000000 yocto of gas to receipt "),
        "{m}"
    );
    assert!(m.ends_with("no creator could have attached it"), "{m}");
}

/// A raise that takes all the refund its creator had left leaves it none,
/// as attaching that gas does: nearcore sends no empty refund, so neither
/// does the fault. Gas burns at the price it was bought here, so the
/// refund is unused gas alone, and a raise can take exactly all of it.
#[test]
fn a_raise_that_takes_the_whole_refund_sends_none() {
    let run = |attached: Gas, fault: Option<Fault>| {
        let mut sim = vault_sim();
        sim.config.gas_price = Balance::from_yoctonear(PRICE);
        sim.set_state("vault.near", b"*gas", Some(&attached.as_gas().to_le_bytes()));
        let mut tx = deposit(&mut sim, 40);
        if let Some(f) = fault {
            tx = tx.inject(Receipts::callback("resolve_deposit"), f);
        }
        let out = tx.transact();
        (sim, out)
    };
    let refunds = |out: &TxOutcome| -> Vec<Balance> {
        let ids = &out.expect_receipt("on_transfer").receipt_ids;
        let refund =
            |id: &CryptoHash| out.receipts.iter().find(|r| r.receipt_id == *id && r.is_refund());
        ids.iter().filter_map(refund).map(|r| r.deposit).collect()
    };
    let (_, base) = run(Gas::from_teragas(10), None);
    let [left] = refunds(&base)[..] else { panic!("{base:#}") };
    assert_eq!(left.as_yoctonear() % PRICE, 0, "unused gas at the price it was bought");
    let gas = Gas::from_gas(10 * TGAS + (left.as_yoctonear() / PRICE) as u64);
    let (sim, out) = run(Gas::from_teragas(10), Some(Fault::Gas(gas)));
    assert_eq!(out.expect_receipt("resolve_deposit").attached_gas, gas);
    assert_eq!(refunds(&out), [], "{out:#}");
    let (want_sim, want) = run(gas, None);
    assert_eq!(chain(&sim, &out), chain(&want_sim, &want));
}

/// `PromiseFailed` at `resolve_deposit` fails the `verify` it waits on, as
/// a `verify` that panics does: the callback reads a failure and returns
/// the whole amount, and the books are the same. Only `verify`'s failure
/// (out of gas, not a panic) and its gas differ.
#[test]
fn promise_failed_is_a_failing_callee() {
    let mut want_sim = vault_sim();
    want_sim.set_state("vault.near", b"*reject", Some(b"1"));
    let want = deposit(&mut want_sim, 40).transact();
    let mut got_sim = vault_sim();
    let got = deposit(&mut got_sim, 40)
        .inject(Receipts::callback("resolve_deposit"), Fault::PromiseFailed)
        .transact();
    let outcome = |out: &TxOutcome, m| {
        let r: &ReceiptOutcome = out.expect_receipt(m);
        (r.failure(), r.value().map(<[u8]>::to_vec))
    };
    let back = Some(40u64.to_le_bytes().to_vec());
    assert_eq!(outcome(&want, "resolve_deposit"), (None, back.clone()));
    assert_eq!(outcome(&got, "resolve_deposit"), (None, back));
    assert_eq!(outcome(&want, "verify").0, Some(Failure::Panic("rejected".into())));
    assert_eq!(outcome(&got, "verify").0, Some(Failure::OutOfGas));
    assert_eq!(got.expect_receipt("verify").fault, Some(Fault::OutOfGas));
    assert_eq!(got.expect_receipt("resolve_deposit").fault, Some(Fault::PromiseFailed));
    assert_eq!(books(&got_sim), books(&want_sim));
    assert_eq!(books(&got_sim), (100, 0, 0));
    got_sim.findings().assert_none();
    want_sim.findings().assert_none();
}

/// A callee that returns the wrong thing: the vault keeps the deposit
/// (`resolve_deposit` returns 0), but the token's resolver reads that it
/// kept none of it and takes it all back.
#[test]
fn a_return_value_the_callee_never_sent() {
    let mut sim = vault_sim();
    let all = 40u64.to_le_bytes().to_vec();
    let out = deposit(&mut sim, 40)
        .inject(Receipts::callback("resolve_transfer"), Fault::ReturnValue(all))
        .transact();
    assert_eq!(out.expect_receipt("resolve_deposit").value(), Some(&0u64.to_le_bytes()[..]));
    assert_eq!(out.expect_receipt("resolve_transfer").value(), Some(&40u64.to_le_bytes()[..]));
    assert_eq!(books(&sim), (100, 0, 40));
    // A failed result stays failed: the callee returned nothing.
    let mut sim = vault_sim();
    let out = deposit(&mut sim, 40)
        .inject(Receipts::call("vault.near", "on_transfer"), Fault::OutOfGas)
        .inject(Receipts::callback("resolve_transfer"), Fault::ReturnValue(vec![0; 8]))
        .transact();
    assert_eq!(out.expect_receipt("on_transfer").failure(), Some(Failure::OutOfGas));
    assert_eq!(out.expect_receipt("resolve_transfer").value(), Some(&40u64.to_le_bytes()[..]));
}

/// A receipt takes the first injection that selects it for each slot:
/// `PromiseFailed`, chained before `ReturnValue` on the same callback,
/// wins it, and the bytes never apply.
#[test]
fn the_first_injection_that_selects_a_receipt_wins_it() {
    let mut sim = vault_sim();
    let out = deposit(&mut sim, 40)
        .inject(Receipts::callback("resolve_transfer"), Fault::PromiseFailed)
        .inject(Receipts::callbacks(), Fault::ReturnValue(vec![0; 8]))
        .transact();
    let resolve = out.expect_receipt("resolve_transfer");
    assert_eq!(resolve.fault, Some(Fault::PromiseFailed), "{out}");
    assert_eq!(resolve.value(), Some(&40u64.to_le_bytes()[..]));
    // on_transfer failed, so it never made resolve_deposit: the second
    // injection selected nothing.
    assert!(out.receipts.iter().all(|r| r.method_names != ["resolve_deposit"]), "{out}");
    // A hold is another slot: the same receipt is starved and held.
    let mut sim = vault_sim();
    let out = deposit(&mut sim, 40)
        .inject(Receipts::callback("resolve_deposit"), Fault::OutOfGas)
        .inject(Receipts::callback("resolve_deposit"), Fault::Hold(2))
        .inject(Receipts::callback("resolve_deposit"), Fault::Hold(5))
        .transact();
    let resolve = out.expect_receipt("resolve_deposit");
    assert_eq!((resolve.fault.clone(), resolve.held), (Some(Fault::OutOfGas), Some(2)), "{out}");
    assert_eq!(books(&sim), (100, 0, 40));
}

/// The vault's deposit spawns, in order, `on_transfer` (0), its callback
/// `resolve_transfer` (1), `verify` (2) and its callback `resolve_deposit`
/// (3). `Receipts::callbacks().nth(1)` is the second callback, the same
/// receipt `Receipts::index(3)` names, and the run is the same to the byte.
#[test]
fn nth_hits_the_receipt_its_index_names() {
    let run = |at: Receipts, fault: Fault| {
        let mut sim = vault_sim();
        let out = deposit(&mut sim, 40).inject(at, fault).transact();
        (format!("{out:#}\n{:?}", out.receipts), books(&sim), sim.state_root())
    };
    for fault in [Fault::OutOfGas, Fault::PromiseFailed] {
        let (by_nth, by_index) =
            (run(Receipts::callbacks().nth(1), fault.clone()), run(Receipts::index(3), fault));
        assert_eq!(by_nth, by_index);
        assert!(by_nth.0.contains("resolve_deposit [callback of vault.near::verify] [fault:"));
    }
    assert_eq!(
        run(Receipts::callbacks().nth(0), Fault::OutOfGas),
        run(Receipts::index(1), Fault::OutOfGas)
    );
    assert_eq!(
        run(Receipts::method("verify").nth(0), Fault::Gas(Gas::from_teragas(1))),
        run(Receipts::index(2), Fault::Gas(Gas::from_teragas(1)))
    );
    // `index(n)` counts in the order receipts leave their blocks, the same
    // on every run from the same state.
    let named = |n| {
        let mut sim = vault_sim();
        let at = Receipts::index(n);
        let out = deposit(&mut sim, 40).inject(at, Fault::Gas(Gas::from_teragas(4))).transact();
        out.receipts.iter().find(|r| r.fault.is_some()).unwrap().method_names.join("")
    };
    let names: Vec<String> = (0..4).map(named).collect();
    assert_eq!(names, ["on_transfer", "resolve_transfer", "verify", "resolve_deposit"]);
}

/// `nth` counts only receipts its fault fits: of every receipt, the
/// second `PromiseFailed` fits is `resolve_deposit` (`on_transfer` and
/// `verify` wait on nothing), the second `OutOfGas` fits is
/// `resolve_transfer`. One past the last fitting receipt, or on receipts
/// none of which fit, applies nothing and doesn't panic, where the same
/// selector without `nth` does.
#[test]
fn nth_skips_receipts_the_fault_does_not_fit() {
    let faulted = |at: Receipts, fault: Fault| {
        let mut sim = vault_sim();
        let out = deposit(&mut sim, 40).inject(at, fault).transact();
        let hit = out.receipts.iter().filter(|r| r.fault.is_some());
        hit.map(|r| (r.method_names.join("+"), r.fault.clone().unwrap())).collect::<Vec<_>>()
    };
    let starved = |m: &str, f: Fault| vec![(m.to_string(), f)];
    let pf = Fault::PromiseFailed;
    // PromiseFailed at resolve_deposit starves verify, which shows OutOfGas.
    assert_eq!(
        faulted(Receipts::all().nth(1), pf.clone()),
        [("verify".to_string(), Fault::OutOfGas), ("resolve_deposit".to_string(), pf.clone())]
    );
    assert_eq!(
        faulted(Receipts::all().nth(1), Fault::OutOfGas),
        starved("resolve_transfer", Fault::OutOfGas)
    );
    assert_eq!(faulted(Receipts::callbacks().nth(2), pf.clone()), []);
    assert_eq!(faulted(Receipts::method("verify").nth(0), pf.clone()), []);
    let strict = catch_unwind(AssertUnwindSafe(|| faulted(Receipts::method("verify"), pf.clone())));
    assert!(panic_message(strict).ends_with("it waits on no promise"));
    // A Gas no lower than what the call attaches doesn't fit: the first
    // call it fits is the next one.
    let big = Fault::Gas(Gas::from_teragas(30));
    assert_eq!(faulted(Receipts::all().nth(0), big.clone()), starved("on_transfer", big));
}

/// a.near (the fuzz contract) runs `program`, with a fault on b.near's
/// receipt if some. Gas burns at the price receipts buy it at, so a receipt
/// that uses all its gas refunds nothing.
fn fuzz_pair(program: &[Op], fault: Option<Fault>) -> (Sim, TxOutcome) {
    let mut sim = sim_with_users();
    sim.config.gas_price = Balance::from_yoctonear(PRICE);
    sim.deploy("a.near", fuzz_wasm());
    sim.deploy("b.near", fuzz_wasm());
    let mut tx = sim.call("alice.near", "a.near", "run").args(encode(program));
    if let Some(f) = fault {
        let at = match f {
            Fault::PromiseFailed => Receipts::call("a.near", "run"),
            _ => Receipts::receiver("b.near"),
        };
        tx = tx.inject(at, f);
    }
    let out = tx.transact();
    (sim, out)
}

/// `min_gas_purchase_price`: what a receipt's gas is bought at, whatever
/// `config.gas_price` is below it.
const PRICE: u128 = 1_000_000_000;

/// A creator that gave its calls all its unused gas by weight (near-sdk's
/// default) sends no gas refund: the fault adds one, under it, with the gas
/// it took at the price it was bought, and conservation holds.
#[test]
fn a_creator_without_a_refund_gets_one() {
    let program =
        [Op::Call { to: "b.near".into(), args: encode(&[]), deposit: 0, gas: 5 * TGAS, weight: 1 }];
    let a_refunds = |out: &TxOutcome| {
        let a = out.receipts.iter().find(|r| r.receiver_id == "a.near").unwrap();
        let ids = a.receipt_ids.iter();
        ids.filter_map(|id| out.receipts.iter().find(|r| r.receipt_id == *id && r.is_refund()))
            .count()
    };
    let (_, base) = fuzz_pair(&program, None);
    assert_eq!(a_refunds(&base), 0, "weight 1 gives b.near all of it: {base:#}");
    let attached = base.receipts.iter().find(|r| r.receiver_id == "b.near").unwrap().attached_gas;
    assert!(attached > Gas::from_teragas(100), "{base:#}");

    let (sim, out) = fuzz_pair(&program, Some(Fault::OutOfGas));
    let a = out.receipts.iter().find(|r| r.receiver_id == "a.near").unwrap();
    let refund = a
        .receipt_ids
        .iter()
        .find_map(|id| out.receipts.iter().find(|r| r.receipt_id == *id && r.is_refund()));
    let refund = refund.unwrap_or_else(|| panic!("{out:#}"));
    assert_eq!(refund.receiver_id, "alice.near");
    assert_eq!(refund.deposit.as_yoctonear(), u128::from(attached.as_gas() - 1) * PRICE);
    let b = out.receipts.iter().find(|r| r.receiver_id == "b.near").unwrap();
    assert_eq!((b.failure(), b.attached_gas), (Some(Failure::OutOfGas), Gas::from_gas(1)));
    let mut vs = sim.findings().into_iter().collect::<Vec<_>>();
    vs.retain(|v| v.check == builtin::CONSERVATION);
    assert!(vs.is_empty(), "{vs:?}");
}

/// `PromiseFailed` on a callback whose call attached a deposit: the call
/// fails before it runs, its deposit comes back to the caller, and the
/// callback logs the same results as when the callee panics. a.near ends
/// with the same balance to the yocto (b.near's differs by the share of
/// gas it would have earned).
#[test]
fn promise_failed_refunds_the_deposit_as_a_panic_does() {
    let program = |callee: Op| {
        let args = encode(&[callee]);
        let deposit = Balance::from_near(1).as_yoctonear();
        [
            Op::Call { to: "b.near".into(), args, deposit, gas: 10 * TGAS, weight: 0 },
            Op::Then { args: encode(&[Op::Results]), gas: 10 * TGAS, weight: 0 },
            Op::PromiseReturn,
        ]
    };
    let (want_sim, want) = fuzz_pair(&program(Op::Panic), None);
    // `Results` is as long as `Panic`, so a.near's call costs the same.
    let (sim, got) = fuzz_pair(&program(Op::Results), Some(Fault::PromiseFailed));
    let callback_logs = |out: &TxOutcome| {
        let cb = out.receipts.iter().find(|r| r.is_callback()).unwrap();
        cb.logs.clone()
    };
    assert_eq!(callback_logs(&got), callback_logs(&want));
    let b = |out: &TxOutcome| {
        out.receipts.iter().find(|r| r.receiver_id == "b.near").unwrap().failure()
    };
    assert_eq!(b(&want), Some(Failure::Panic("fuzz panic".into())));
    assert_eq!(b(&got), Some(Failure::OutOfGas));
    assert_eq!(sim.balance("a.near"), want_sim.balance("a.near"));
    let deposit_back = |out: &TxOutcome| {
        out.receipts.iter().filter(|r| r.is_refund() && r.receiver_id == "a.near").count()
    };
    assert_eq!((deposit_back(&got), deposit_back(&want)), (1, 1));
}

/// A selector matches only receipts the transaction spawns: not its own
/// (`transfer_call`), refunds or data receipts. One that matches nothing
/// changes nothing; the outcome shows which receipts a fault changed.
#[test]
fn selectors_match_spawned_receipts() {
    let mut sim = vault_sim();
    let out = deposit(&mut sim, 40)
        .inject(Receipts::method("transfer_call"), Fault::OutOfGas)
        .inject(Receipts::receiver("nobody.near"), Fault::OutOfGas)
        .transact();
    assert!(out.receipts.iter().all(|r| r.fault.is_none() && r.is_success()), "{out}");
    let mut sim = vault_sim();
    let out =
        deposit(&mut sim, 40).inject(Receipts::receiver("vault.near"), Fault::OutOfGas).transact();
    let faulted: Vec<_> = out
        .receipts
        .iter()
        .filter(|r| r.fault.is_some())
        .map(|r| r.method_names.join(""))
        .collect();
    assert_eq!(faulted, ["on_transfer"], "it failed before it made the others");
    assert_eq!(books(&sim), (100, 0, 0));
    let out = deposit(&mut sim, 40)
        .inject(Receipts::call("vault.near", "verify"), Fault::OutOfGas)
        .transact();
    assert_eq!(out.expect_receipt("verify").fault, Some(Fault::OutOfGas));
    // `callback` selects only callbacks; `method` any call.
    let out =
        deposit(&mut sim, 40).inject(Receipts::callback("verify"), Fault::OutOfGas).transact();
    assert!(out.receipts.iter().all(|r| r.fault.is_none()), "{out}");
}

#[test]
fn a_fault_that_cant_apply_panics() {
    let cases: [(Receipts, Fault, &str); 4] = [
        (
            Receipts::method("verify"),
            Fault::PromiseFailed,
            "PromiseFailed at Receipts::method(\"verify\") (vault.near::verify): it waits on no promise",
        ),
        (
            Receipts::method("verify"),
            Fault::ReturnValue(vec![]),
            "ReturnValue([]) at Receipts::method(\"verify\") (vault.near::verify): it waits on no \
             promise",
        ),
        (
            Receipts::method("verify"),
            Fault::ReturnValue(vec![0; 4 << 20 | 1]),
            "Fault::ReturnValue of 4194305 bytes: no receipt returns more than max_length_returned_data, 4194304",
        ),
        (
            Receipts::method("verify"),
            Fault::Gas(Gas::ZERO),
            "Fault::Gas(0): nearcore refuses a call with no gas (FunctionCallZeroAttachedGas); OutOfGas runs it with 1",
        ),
    ];
    for (at, fault, want) in cases {
        let mut sim = vault_sim();
        let r =
            catch_unwind(AssertUnwindSafe(|| deposit(&mut sim, 40).inject(at, fault).transact()));
        assert_eq!(panic_message(r), want);
    }
}

/// A copy of a submitted transaction (the same nonce, before a block) must
/// carry the same faults.
#[test]
fn a_copy_carries_the_same_faults() {
    let mut sim = vault_sim();
    let key = sim.secret_key("alice.near").public_key();
    let nonce = sim.access_key("alice.near", &key).unwrap().nonce + 1;
    let a = deposit(&mut sim, 40).nonce(nonce).inject(Receipts::index(0), Fault::OutOfGas).submit();
    let b = deposit(&mut sim, 40).nonce(nonce).inject(Receipts::index(0), Fault::OutOfGas).submit();
    assert_eq!(a, b);
    let r = catch_unwind(AssertUnwindSafe(|| deposit(&mut sim, 40).nonce(nonce).submit()));
    assert_eq!(
        panic_message(r),
        format!(
            "transaction {a} was submitted before with other injections: its hash doesn't cover \
             them, so a copy must have the same"
        )
    );
}

/// `PromiseFailed` fails calls, and a transfer isn't one: on a callback
/// that waits on one (the tip jar's `on_tip`), `Tx::inject` panics, and a
/// scan lists it as not tried, with why; so is a `Gas` no lower than what
/// a call attaches.
#[test]
fn promise_failed_needs_a_call_to_fail() {
    fn tip(sim: &mut Sim) -> Tx<'_> {
        sim.call("alice.near", "jar.near", "tip_checked").args(b"bob.near".to_vec())
    }
    let mut sim = sim_with_users();
    sim.deploy("jar.near", tip_jar_wasm());
    let r = catch_unwind(AssertUnwindSafe(|| {
        tip(&mut sim.clone()).inject(Receipts::callback("on_tip"), Fault::PromiseFailed).transact()
    }));
    assert_eq!(
        panic_message(r),
        "PromiseFailed at Receipts::callback(\"on_tip\") (jar.near::on_tip): a call it waits on left an \
         earlier block or makes no function call, so nothing can fail it"
    );
    let faults = [Fault::PromiseFailed, Fault::Gas(Gas::from_teragas(400))];
    let report = Harness::new(sim)
        .scan(&[Plan::new().op(tip_op())])
        .faults(Faults::new(Receipts::all(), faults))
        .run();
    assert_eq!((report.cases(), report.found().len()), (1, 0), "the plain case alone:\n{report}");
}

/// The tip jar's checked tip, as an op.
fn tip_op() -> Call {
    Call::new("alice.near", "jar.near", "tip_checked").args(b"bob.near".to_vec())
}

/// A transfer returns nothing, as does any receipt that makes no function
/// call, so `ReturnValue` can't give a callback that waits on one bytes for
/// its result: `Tx::inject` panics and a scan lists it as not tried.
/// No bytes, what it does return, still apply.
#[test]
fn a_transfer_returns_no_bytes() {
    fn tip(sim: &mut Sim) -> Tx<'_> {
        sim.call("alice.near", "jar.near", "tip_checked").args(b"bob.near".to_vec())
    }
    let mut sim = sim_with_users();
    sim.deploy("jar.near", tip_jar_wasm());
    let bytes = Fault::ReturnValue(vec![1]);
    let r = catch_unwind(AssertUnwindSafe(|| {
        tip(&mut sim.clone()).inject(Receipts::callback("on_tip"), bytes.clone()).transact()
    }));
    assert_eq!(
        panic_message(r),
        "ReturnValue([1]) at Receipts::callback(\"on_tip\") (jar.near::on_tip): a receipt it waits on makes \
         no function call, so its result is always empty"
    );
    let none = Fault::ReturnValue(vec![]);
    let out = tip(&mut sim.clone()).inject(Receipts::callback("on_tip"), none.clone()).transact();
    assert_eq!(out.expect_receipt("on_tip").fault, Some(none));
    let report = Harness::new(sim)
        .scan(&[Plan::new().op(tip_op())])
        .faults(Faults::new(Receipts::all(), [bytes]))
        .run();
    assert_eq!((report.cases(), report.found().len()), (1, 0), "the plain case alone: {report}");
}

/// The gas a fault frees goes to its own creator's refund, not another
/// transaction's in the same block: bob's deposit, sent first, and alice's
/// end the same whether alice's resolver had its 10 Tgas or 3.
#[test]
fn freed_gas_goes_to_the_faulted_receipts_creator() {
    let base = || {
        let mut sim = vault_sim();
        sim.call("bob.near", "token.near", "mint")
            .args(100u64.to_le_bytes())
            .transact()
            .assert_success();
        sim
    };
    let both = |fault: Option<Fault>| {
        let mut sim = base();
        let bobs = [&10u64.to_le_bytes()[..], b"vault.near"].concat();
        sim.call("bob.near", "token.near", "transfer_call").args(bobs).submit();
        let mut alices = deposit(&mut sim, 10);
        if let Some(f) = fault {
            alices = alices.inject(Receipts::callback("resolve_deposit"), f);
        }
        alices.submit();
        sim.settle();
        (sim.balance("alice.near"), sim.balance("bob.near"))
    };
    let (alice, bob) = both(None);
    let (faulted_alice, faulted_bob) = both(Some(Fault::Gas(Gas::from_teragas(3))));
    // The resolver needs less than 3 Tgas, so the gas alice no longer
    // attaches comes back to her either way: both end the same.
    assert_eq!((faulted_alice, faulted_bob), (alice, bob));
}

/// alice.near, bob.near and relayer.near, and c.near running the fuzz
/// contract.
fn delegate_sim() -> Sim {
    let mut sim = sim_with_users();
    sim.create_account("relayer.near", near(1_000));
    sim.deploy("c.near", fuzz_wasm());
    sim
}

/// relayer.near sends a delegate in which alice.near calls c.near with
/// `gas`. The call is the receipt the delegate makes: `Receipts::index(0)`.
fn relayed(sim: &mut Sim, gas: Gas) -> Tx<'_> {
    let h = sim.height();
    let call = sim.call("alice.near", "c.near", "run").args(encode(&[Op::Env(4)]));
    let delegate = call.gas(gas).sign_delegate(h + 100);
    sim.tx("relayer.near", "alice.near").action(delegate)
}

/// The receipt a delegate makes is a first-version action receipt, faulted
/// as any other: `OutOfGas` and `Gas(g)` leave every balance as the
/// delegate attaching that gas itself does, the gas taken going back to
/// the relayer, which bought it, and conservation holds. It waits on no
/// promise, so `PromiseFailed` and `ReturnValue` panic, and a scan tries
/// only the other two, and lists them as not tried.
#[test]
fn a_delegates_call_takes_faults() {
    let relay = |gas: Gas, fault: Option<Fault>| {
        let mut sim = delegate_sim();
        let mut tx = relayed(&mut sim, gas);
        if let Some(f) = fault {
            tx = tx.inject(Receipts::index(0), f);
        }
        let out = tx.transact();
        (sim, out)
    };
    let near = |sim: &Sim| ["relayer.near", "alice.near", "c.near"].map(|a| sim.balance(a));
    let cases = [
        (Fault::OutOfGas, Gas::from_gas(1)),
        (Fault::Gas(Gas::from_teragas(5)), Gas::from_teragas(5)),
    ];
    for (fault, gas) in cases {
        let (sim, out) = relay(Gas::from_teragas(50), Some(fault.clone()));
        let (want_sim, want) = relay(gas, None);
        let run = out.expect_receipt("run");
        assert_eq!((&run.fault, run.attached_gas), (&Some(fault.clone()), gas), "{out:#}");
        assert_eq!(run.failure(), want.expect_receipt("run").failure(), "{fault}");
        assert_eq!(near(&sim), near(&want_sim), "{fault}: {out:#}\n{want:#}");
        assert!(out.receipts.iter().any(|r| r.is_refund() && r.receiver_id == "relayer.near"));
        let mut vs = sim.findings().into_iter().collect::<Vec<_>>();
        vs.retain(|v| v.check == builtin::CONSERVATION);
        assert!(vs.is_empty(), "{vs:?}");
    }
    let starved = relay(Gas::from_teragas(50), Some(Fault::OutOfGas)).1;
    assert_eq!(starved.expect_receipt("run").failure(), Some(Failure::OutOfGas));
    for fault in [Fault::PromiseFailed, Fault::ReturnValue(vec![1])] {
        let r =
            catch_unwind(AssertUnwindSafe(|| relay(Gas::from_teragas(50), Some(fault.clone()))));
        assert_eq!(
            panic_message(r),
            format!("{fault:?} at Receipts::index(0) (c.near::run): it waits on no promise")
        );
    }
    let faults = [
        Fault::OutOfGas,
        Fault::PromiseFailed,
        Fault::ReturnValue(vec![1]),
        Fault::Gas(Gas::from_teragas(5)),
    ];
    let report = Harness::new(delegate_sim())
        .scan(&[Plan::new().op(Relayed { tgas: 50 })])
        .faults(Faults::new(Receipts::all(), faults))
        .run();
    assert_eq!(report.cases(), 1 + 2, "{report}");
    assert!(report.found_at(builtin::CONSERVATION, "world").is_none(), "{report}");
}

/// relayer.near relays alice.near's call to c.near with `tgas`.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct Relayed {
    tgas: u64,
}

impl XOp for Relayed {
    fn submit(&self, sim: &mut Sim) -> Result<(), Skip> {
        relayed(sim, Gas::from_teragas(self.tgas)).submit();
        Ok(())
    }
}

/// A callback `Tx::promise_results` delivers is the transaction's own
/// receipt, which no fault selects: a selector that would pick it
/// selects nothing, and the callback runs unfaulted.
#[test]
fn a_delivered_callback_is_not_selected() {
    use near_contract_sim::PromiseResult;
    let mut sim = vault_sim();
    let args = [&10u64.to_le_bytes()[..], b"alice.near"].concat();
    let out = sim
        .call("alice.near", "vault.near", "resolve_deposit")
        .args(args)
        .promise_results([PromiseResult::Failed])
        .inject(Receipts::all(), Fault::OutOfGas)
        .transact();
    let resolve = out.expect_receipt("resolve_deposit");
    assert_eq!((resolve.fault.as_ref(), resolve.is_callback()), (None, true), "{out}");
}

/// The vault pair's books as a pure model of what a deposit should do:
/// alice's tokens, the vault's tokens and alice's shares.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Model(u64, u64, u64);

impl Model {
    /// A deposit alice can pay moves to the vault and stays, a share per
    /// token, only if the vault confirmed it; otherwise the token's resolver
    /// takes it back. `ran(m)` says whether the receipt that ran `m`
    /// succeeded on the chain, faults included.
    fn deposit(self, amount: u64, ran: impl Fn(&str) -> bool) -> Model {
        let Model(alice, vault, shares) = self;
        if amount > alice {
            return self;
        }
        let kept = ran("verify") && ran("resolve_deposit");
        let back = if !kept && ran("resolve_transfer") { amount } else { 0 };
        let credit = if kept { amount } else { 0 };
        Model(alice - amount + back, vault + amount - back, shares + credit)
    }
}

/// A deposit of `amount`, stepped through the model at rest.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct ModelledDeposit {
    amount: u64,
}

impl XOp for ModelledDeposit {
    fn submit(&self, sim: &mut Sim) -> Result<(), Skip> {
        deposit(sim, self.amount).submit();
        Ok(())
    }
}

/// Deposits of 1 to 50, at the gas a plan explores.
fn deposits() -> impl Strategy<Value = VaultOp> {
    (1u64..=50).prop_map(|amount| VaultOp::Deposit { amount })
}

fn starve_or_fail() -> Faults {
    Faults::new(Receipts::all(), [Fault::OutOfGas, Fault::PromiseFailed])
}

/// A search with one forced failure in every case finds the resolver: the
/// business property is judged in every faulted case.
#[test]
fn generated_faults_find_the_resolver() {
    let report = books_harness(vault_sim())
        .search(plans(deposits()).len(1..=3).faults(starve_or_fail().per_case(1.0)))
        .seed(0x5eed)
        .cases(64)
        .run();
    let found = report.found_at("books", "world").unwrap_or_else(|| panic!("{report}"));
    assert!(found.finding.injected, "{report}");
}

/// The fixed vault passes the same search, and the faults reached its
/// resolver.
#[test]
fn the_fixed_vault_passes_generated_faults() {
    let report = books_harness(safe_vault_sim())
        .search(plans(deposits()).len(1..=3).faults(starve_or_fail().per_case(1.0)))
        .seed(0x5eed)
        .cases(64)
        .run();
    report.assert_clean();
}

/// Receipts and results held back 1, 2 or 4 blocks, at every receipt
/// they fit: the fixed vault's books hold.
#[test]
fn the_fixed_vault_passes_every_delay() {
    let holds = [1, 2, 4].map(Fault::Hold).into_iter().chain([1, 2, 4].map(Fault::HoldResult));
    let report = books_harness(safe_vault_sim())
        .scan(&[deposit_40()])
        .faults(Faults::new(Receipts::all(), holds))
        .run();
    report.assert_clean();
    assert!(report.found().is_empty(), "{report}");
}

/// Two to five deposits in overlapping blocks keep the fixed vault's books.
#[test]
fn the_fixed_vault_passes_overlapping_deposits() {
    let report = books_harness(safe_vault_sim())
        .search(plans(deposits()).len(2..=5).faults(Faults::none()))
        .seed(0x5eed)
        .cases(12)
        .run();
    report.assert_clean();
}

/// A failed result injected into a strict vault's resolver must not excuse
/// the resolver's own panic after it wrote: the built-in check finds it.
#[test]
fn faulted_results_do_not_hide_a_secondary_callback_failure() {
    let mut sim = vault_sim();
    sim.set_state("vault.near", b"*strict", Some(b"1"));
    let report = Harness::new(sim)
        .scan(&[deposit_40()])
        .faults(Faults::new(Receipts::all(), [Fault::PromiseFailed]))
        .run();
    let found = report.found().iter().find(|f| f.finding.check == builtin::FAILED_CALLBACK);
    assert!(found.is_some(), "{report}");
}

/// An op that damages state, with no transaction, is judged as any other:
/// the books finding names it.
#[test]
fn an_op_that_mints_a_share_breaks_the_books() {
    let run = books_harness(safe_vault_sim()).run(&Plan::new().op(VaultOp::MintShare).blocks(1));
    run.assert_found("books", "world");
}

/// The optional reference model is an ordinary at-rest check, stepped
/// through every deposit of the case (`at_rest_ops`). It reads actual
/// receipt outcomes; the harness has no copy of it, and the world keeps no
/// record of its own.
fn against_the_model(sim: Sim) -> Harness<ModelledDeposit> {
    let (alice, vault, shares) = books(&sim);
    let initial = Model(alice, vault, shares);
    Harness::new(sim).at_rest_ops(
        "follows the model",
        move |sim: &Sim, ops: &[OpDone<ModelledDeposit>]| {
            let expected = ops.iter().fold(initial, |model, d| {
                let outcome = d.outcome().expect("a deposit is one transaction");
                model.deposit(d.op().amount, |method| {
                    outcome.receipt(method).is_some_and(|r| r.is_success())
                })
            });
            let (alice, vault, shares) = books(sim);
            let actual = Model(alice, vault, shares);
            if actual == expected {
                Ok(())
            } else {
                Err(format!("chain has {actual:?}, model expects {expected:?}"))
            }
        },
    )
}

#[test]
fn the_vault_follows_its_model_under_targeted_faults() {
    let plan = Plan::new().op(ModelledDeposit { amount: 40 });
    let report = against_the_model(vault_sim())
        .scan(std::slice::from_ref(&plan))
        .faults(starve_or_fail())
        .run();
    assert!(report.found_at("follows the model", "world").is_some(), "{report}");
    against_the_model(safe_vault_sim()).scan(&[plan]).faults(starve_or_fail()).run().assert_clean();
}

/// A detached call a fault starves is unhandled, but forced: `findings()`
/// excuses it, and the tree marks it `FORCED (excused)`, not `UNHANDLED`.
/// The same failure unforced is a finding, marked `UNHANDLED`.
///
/// Verification boundary: the receipt tree's marker and `Sim::findings`,
/// against actual simulator output.
#[test]
fn a_forced_unhandled_failure_is_marked_excused_in_the_tree() {
    use near_contract_sim::{Reply, Stub};
    let mut sim = sim_with_users();
    let bob = Stub::new().on("ping", Reply::json("pong")).on("refuse", Reply::panic("no"));
    sim.deploy_stub("bob.near", bob);
    let ask = |m: &str| Reply::call("bob.near", m, ()).then(Reply::json("asked"));
    sim.deploy_stub(
        "app.near",
        Stub::new().on("ask", ask("ping")).on("ask_refused", ask("refuse")),
    );
    let base = sim.clone();

    let out = sim
        .call("alice.near", "app.near", "ask")
        .inject(Receipts::method("ping"), Fault::OutOfGas)
        .transact();
    let ping = out.expect_receipt("ping");
    assert!(ping.unhandled && ping.failure() == Some(Failure::OutOfGas), "{out:#}");
    let tree = format!("{out:#}");
    assert!(tree.contains("<-- FORCED (excused)") && !tree.contains("<-- UNHANDLED"), "{tree}");
    sim.findings().assert_none();

    let mut sim = base;
    let out = sim.call("alice.near", "app.near", "ask_refused").transact();
    let tree = format!("{out:#}");
    assert!(tree.contains("<-- UNHANDLED") && !tree.contains("FORCED"), "{tree}");
    let findings = sim.findings();
    assert_eq!(findings.first().map(|f| f.check.as_str()), Some(builtin::UNHANDLED_FAILURE));
    assert_eq!(findings.len(), 1, "{findings}");
}

/// alice.near deposits `amount` tokens, and bob.near with 100 of his own
/// deposits 10 in the same block: the second deposit, with
/// `injections`. Both are ok.
fn two_deposits(amount: u64, injections: &[(Receipts, Fault)]) -> (Sim, TxOutcome, TxOutcome) {
    let mut sim = vault_sim();
    sim.call("bob.near", "token.near", "mint")
        .args(100u64.to_le_bytes())
        .transact()
        .assert_success();
    let t1 = deposit(&mut sim, amount).submit();
    let args = [&10u64.to_le_bytes()[..], b"vault.near"].concat();
    let mut t2 = sim.call("bob.near", "token.near", "transfer_call").args(args);
    for (at, fault) in injections {
        t2 = t2.inject(at.clone(), fault.clone());
    }
    let t2 = t2.submit();
    sim.settle();
    let (one, two) = (sim.outcome(t1), sim.outcome(t2));
    one.assert_success();
    two.assert_success();
    (sim, one, two)
}

/// `Receipts::in_tx` holds a receipt of one transaction until another
/// transaction's has run. Plain, two deposits in one block run their
/// callbacks side by side; held until the first deposit's last receipt,
/// the token's `resolve_transfer`, has run, the second's
/// `resolve_deposit` runs in the block after it, and the books still
/// conserve.
///
/// Verification boundary: the order is forced, one the sim can run on one
/// shard; nothing here says a network delays a receipt that long.
#[test]
fn hold_until_waits_for_another_transactions_receipt() {
    let resolve = |out: &TxOutcome, m| out.expect_receipt(m).block_height;
    let (_, one, two) = two_deposits(40, &[]);
    assert!(resolve(&two, "resolve_deposit") < resolve(&one, "resolve_transfer"), "{one}\n{two}");

    let t1 = one.hash;
    let until = Receipts::callback("resolve_transfer").in_tx(t1);
    let (sim, one, two) =
        two_deposits(40, &[(Receipts::callback("resolve_deposit"), Fault::HoldUntil(until))]);
    assert_eq!(one.hash, t1, "the same first transaction");
    let cb = two.expect_receipt("resolve_deposit");
    assert_eq!(cb.block_height, resolve(&one, "resolve_transfer") + 1, "{one}\n{two}");
    assert!(cb.held.is_some(), "{two}");
    assert_eq!(held(&sim, "token.near", "vault.near"), 50);
    assert_eq!(held(&sim, "vault.near", "alice.near"), 40);
    assert_eq!(held(&sim, "vault.near", "bob.near"), 10);
    sim.findings().assert_none();
}

const POOL: &str = "pool.near";

/// pool.near runs `tests/res/pool_sync.wat`, which books as a reward
/// whatever its balance holds beyond its books; alice.near, bob.near and
/// relayer.near have NEAR.
fn pool_sim() -> Sim {
    let wat = std::fs::read_to_string(manifest_dir().join("tests/res/pool_sync.wat")).unwrap();
    let mut sim = sim_with_users();
    sim.create_account("relayer.near", near(1_000));
    sim.create_account(POOL, near(10));
    sim.deploy(POOL, wat::parse_str(&wat).unwrap());
    sim.call("alice.near", POOL, "init").transact().assert_success();
    sim
}

/// The pool's books less its balance, if they exceed it.
fn overbooked(sim: &Sim) -> Option<u128> {
    let b = sim.view_raw(POOL, "booked", vec![]).result.unwrap();
    let booked = u128::from_le_bytes(b.try_into().unwrap());
    booked.checked_sub(sim.account(POOL).unwrap().amount.as_yoctonear()).filter(|d| *d > 0)
}

/// relayer.near relays alice.near's 1 NEAR deposit, its `deposit` receipt
/// held a block (so it syncs after bob.near's deposit has booked), and
/// bob.near deposits 1 NEAR in the same block, with `injections`.
fn relayed_and_direct(injections: &[(Receipts, Fault)]) -> (Sim, CryptoHash, CryptoHash) {
    let mut sim = pool_sim();
    let h = sim.height();
    let call = sim.call("alice.near", POOL, "deposit").deposit(near(1));
    let delegate = call.sign_delegate(h + 100);
    let t1 = sim
        .tx("relayer.near", "alice.near")
        .action(delegate)
        .inject(Receipts::method("deposit"), Fault::Hold(1))
        .submit();
    let mut t2 = sim.call("bob.near", POOL, "deposit").deposit(near(1));
    for (at, fault) in injections {
        t2 = t2.inject(at.clone(), fault.clone());
    }
    let t2 = t2.submit();
    sim.settle();
    (sim, t1, t2)
}

/// bob.near's `on_deposit` held until alice.near's relayed `deposit` has
/// run, in another transaction: that sync sees bob's NEAR in the balance
/// and not yet in the books, and books it as a reward, which `on_deposit`
/// then books again. Plain, bob's books first, and the books match the
/// balance.
#[test]
fn hold_until_in_another_transaction_reproduces_a_double_count() {
    let (sim, t1, t2) = relayed_and_direct(&[]);
    assert_eq!(overbooked(&sim), None, "{}\n{}", sim.outcome(t1), sim.outcome(t2));

    let until = Receipts::method("deposit").in_tx(t1);
    let held = [(Receipts::method("on_deposit"), Fault::HoldUntil(until))];
    let (sim, t1, t2) = relayed_and_direct(&held);
    let (one, two) = (sim.outcome(t1), sim.outcome(t2));
    let on_deposit = two.expect_receipt("on_deposit");
    assert_eq!(on_deposit.block_height, one.expect_receipt("deposit").block_height + 1, "{two}");
    assert!(on_deposit.held.is_some(), "{two}");
    assert_eq!(overbooked(&sim), Some(near(1).as_yoctonear()), "{one}\n{two}");
}

/// How submitting a second deposit, held until `until` of the first,
/// panics, after the first settles.
fn held_until_panics(until: impl FnOnce(CryptoHash) -> Receipts, at: Receipts) -> String {
    let mut sim = vault_sim();
    let t1 = deposit(&mut sim, 40).submit();
    let fault = Fault::HoldUntil(until(t1));
    panic_message(catch_unwind(AssertUnwindSafe(|| {
        deposit(&mut sim, 10).inject(at, fault).transact()
    })))
}

/// A `HoldUntil` naming another transaction's receipt refuses, as one in
/// its own does: a receipt that never spawns (the first deposit has no
/// `nope`) once nothing else is in flight, one that already ran, and one
/// held behind it (the first deposit holds its `verify` a block, after the
/// second's `on_transfer` was held, and holds go out first in, first out,
/// so `verify` can't leave before the receipt waiting for it).
#[test]
fn hold_until_in_another_transaction_refuses_what_cant_apply() {
    let on_transfer = || Receipts::method("on_transfer");
    let msg = held_until_panics(|t| Receipts::method("nope").in_tx(t), on_transfer());
    let never = "(vault.near::on_transfer): nothing it waits for left a block, and nothing else \
                 is in flight";
    assert!(msg.ends_with(never), "{msg}");

    // The first deposit settled before the second was sent.
    let mut sim = vault_sim();
    let t1 = deposit(&mut sim, 40).transact().hash;
    let fault = Fault::HoldUntil(Receipts::method("verify").in_tx(t1));
    let msg = panic_message(catch_unwind(AssertUnwindSafe(|| {
        deposit(&mut sim, 10).inject(on_transfer(), fault).transact()
    })));
    assert!(msg.ends_with("which it waits for, already ran"), "{msg}");
    // So does one settled before a snapshot: `Sim::cached` returns the
    // sim it built (what it spawned known) or, the second time at the
    // latest, loads it (not known).
    for loaded in [false, true] {
        let mut sim = Sim::cached("faults.rs: a settled first deposit", || {
            let mut sim = vault_sim();
            deposit(&mut sim, 40).transact().assert_success();
            sim
        });
        let t1 = *sim.txs().last().unwrap();
        let fault = Fault::HoldUntil(Receipts::method("verify").in_tx(t1));
        let msg = panic_message(catch_unwind(AssertUnwindSafe(|| {
            deposit(&mut sim, 10).inject(on_transfer(), fault).transact()
        })));
        assert!(msg.ends_with("already ran"), "{msg}");
        assert!(!loaded || msg.contains("settled before the sim was loaded"), "{msg}");
    }

    let mut sim = vault_sim();
    let t1 = deposit(&mut sim, 40).inject(Receipts::method("verify"), Fault::Hold(1)).submit();
    let fault = Fault::HoldUntil(Receipts::method("verify").in_tx(t1));
    let t2 = deposit(&mut sim, 10).inject(on_transfer(), fault).submit();
    let msg = panic_message(catch_unwind(AssertUnwindSafe(|| sim.settle())));
    let behind = "never ran, and nothing else is in flight but the 1 held behind it, which can't \
                  leave before it (holds go out first in, first out)";
    assert!(msg.ends_with(behind), "{msg}");
    sim.settle();
    let (one, two) = (sim.outcome(t1), sim.outcome(t2));
    let ran = |out: &TxOutcome, m| out.expect_receipt(m).block_height;
    assert!(ran(&two, "on_transfer") < ran(&one, "verify"), "{one}\n{two}");
}

/// `in_tx` names whose receipt a `HoldUntil` waits for, so it panics at
/// submit as an injection's own selector, and when it names no
/// transaction submitted before.
#[test]
fn in_tx_only_names_an_earlier_transaction_for_hold_until() {
    let unknown = CryptoHash::default();
    let msg = held_until_panics(|_| Receipts::all().in_tx(unknown), Receipts::all());
    assert!(
        msg.ends_with(&format!("{unknown} names no transaction submitted before this one")),
        "{msg}"
    );
    let mut sim = vault_sim();
    let t1 = deposit(&mut sim, 40).submit();
    let msg = panic_message(catch_unwind(AssertUnwindSafe(|| {
        deposit(&mut sim, 10).inject(Receipts::method("verify").in_tx(t1), Fault::Hold(1)).submit()
    })));
    let want = format!(
        "Hold(1) at Receipts::method(\"verify\").in_tx({t1}): an injection selects its own \
         transaction's receipts; in_tx names whose receipt a Fault::HoldUntil waits for"
    );
    assert_eq!(msg, want);
    // Refused at submit, the sim is left as it was: the first deposit settles.
    sim.settle();
    sim.outcome(t1).assert_success();
    assert_eq!(sim.txs().len(), vault_sim().txs().len() + 1);
}

/// In a plan, a step's `HoldUntil` naming an earlier step's receipt that
/// never spawns is passed over: the receipt goes once nothing else is in
/// flight, and the run is incomplete, never clean. Naming one that runs,
/// it lands. The plan names the first step's transaction by its exact
/// hash, so this pins the refusal path, not a way to write plans.
#[test]
fn a_plans_hold_until_in_another_transaction_lands_or_is_incomplete() {
    let sim = vault_sim();
    // The first step's transaction, as the harness submits it.
    let t1 = deposit(&mut sim.clone(), 40).submit();
    let harness = books_harness(sim);
    let plan = |until: Receipts| {
        deposit_40()
            .blocks(0)
            .op(VaultOp::Deposit { amount: 10 })
            .inject(Receipts::method("on_transfer"), Fault::HoldUntil(until.in_tx(t1)))
    };
    let run = harness.run(&plan(Receipts::callback("resolve_transfer")));
    run.assert_clean();
    let run = harness.run(&plan(Receipts::method("nope")));
    assert_eq!(run.verdict(), near_contract_sim::explore::Verdict::Incomplete, "{run}");
    let shown = run.to_string();
    assert!(
        shown.contains("applied to no receipt: at vault.near::on_transfer: nothing it waits for"),
        "{shown}"
    );
}
