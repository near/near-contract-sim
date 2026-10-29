//! `TxOutcome::effects` and pending receipts, on real contracts: each
//! transaction's NEAR moves come from its own receipts, so they stay exact
//! while other transactions run in the same blocks.
//!
//! Two oracles judge them:
//! - closure: at rest, the sum over transactions of each account's
//!   `near_net`, less the gas it paid as a signer, is its balance change;
//! - isolation: a transaction run alone on a clone of the same state
//!   changes balances by what its overlapped run's effects say (unless the
//!   two runs end differently: interfering transactions aren't comparable).
//!
//! Verification boundary: these read the block log and state; they change
//! no block content or receipt order. Passing shows the attribution rules
//! hold on these executed cases only.

mod common;

use common::{failure_probe_wasm, ft_wasm, near, yield_wasm};
use near_contract_sim::{
    Balance, CryptoHash, Delta, Fault, Gas, NearKind, PendingState, Receipts, Sim, TxOutcome,
};
use serde_json::json;
use std::collections::HashSet;

const NEAR: u128 = 1_000_000_000_000_000_000_000_000;

/// A transaction an oracle submits, by name.
type SubmitFn = Box<dyn Fn(&mut Sim) -> CryptoHash>;

fn tgas(n: u64) -> Gas {
    Gas::from_teragas(n)
}

fn bal(sim: &Sim, who: &str) -> u128 {
    sim.balance(who).as_yoctonear()
}

fn d(after: u128, before: u128) -> i128 {
    after as i128 - before as i128
}

/// What `who` paid for gas as `out`'s signer.
fn gas_of(out: &TxOutcome, who: &str) -> Delta {
    let signer = &out.transaction.as_ref().expect("converted").receiver_id;
    if signer == who { Delta::gain(out.effects().gas_paid().as_yoctonear()) } else { Delta::ZERO }
}

/// Closure, at rest: for each of `users` (accounts that run no code), the
/// sum over `outs` of value effects less gas paid is the balance change.
fn assert_closure(label: &str, users: &[&str], before: &Sim, after: &Sim, outs: &[TxOutcome]) {
    for who in users {
        let sum: Delta = outs.iter().map(|o| o.effects().near_net(*who) - gas_of(o, who)).sum();
        assert_eq!(sum, d(bal(after, who), bal(before, who)), "{label}: native closure for {who}");
    }
}

/// Each op's receipts ran in a block where another op's did.
fn assert_overlapped(label: &str, outs: &[TxOutcome]) {
    let blocks: Vec<HashSet<u64>> =
        outs.iter().map(|o| o.receipts.iter().map(|r| r.block_height).collect()).collect();
    for (i, b) in blocks.iter().enumerate() {
        let shared = blocks.iter().enumerate().any(|(j, c)| i != j && !b.is_disjoint(c));
        assert!(shared, "{label}: op {i} never shared a block with another op");
    }
}

type Op<'a> = (&'a str, &'a dyn Fn(&mut Sim) -> CryptoHash);

/// Isolation: each op alone on `base` moves `users`' balances by its
/// overlapped effects, gas from its own run. Returns the ops that ended
/// differently alone, which it can't judge.
fn isolation(
    label: &str,
    users: &[&str],
    base: &Sim,
    ops: &[Op],
    outs: &[TxOutcome],
) -> Vec<usize> {
    let mut interfered = vec![];
    for (i, (name, f)) in ops.iter().enumerate() {
        let mut alone = base.clone();
        let h = f(&mut alone);
        alone.settle();
        let out = alone.outcome(h);
        if out.is_success() != outs[i].is_success() {
            eprintln!("{label}: op {i} `{name}` interfered: {:?} alone", out.status);
            interfered.push(i);
            continue;
        }
        for who in users {
            assert_eq!(
                outs[i].effects().near_net(*who) - gas_of(&out, who),
                d(bal(&alone, who), bal(base, who)),
                "{label}: op {i} `{name}` native {who}"
            );
        }
    }
    interfered
}

// ---------------------------------------------------------------------------
// NEP-141 transfers in one block, one failing (native parts)
// ---------------------------------------------------------------------------

const TOKEN: &str = "token.near";

fn ft_world() -> Sim {
    let mut sim = Sim::new();
    for a in ["alice.near", "bob.near", "carol.near"] {
        sim.create_account(a, near(100));
    }
    sim.deploy(TOKEN, ft_wasm());
    sim.call(TOKEN, TOKEN, "new_default_meta")
        .args_json(json!({"owner_id": "alice.near", "total_supply": "1000000"}))
        .transact()
        .assert_success();
    for a in ["bob.near", "carol.near"] {
        sim.call("alice.near", TOKEN, "storage_deposit")
            .args_json(json!({"account_id": a}))
            .deposit(Balance::from_millinear(125))
            .transact()
            .assert_success();
    }
    sim.call("alice.near", TOKEN, "ft_transfer")
        .args_json(json!({"receiver_id": "bob.near", "amount": "100"}))
        .deposit(Balance::from_yoctonear(1))
        .transact()
        .assert_success();
    sim
}

fn ft_transfer(sim: &mut Sim, from: &str, to: &str, amount: u128) -> CryptoHash {
    sim.call(from, TOKEN, "ft_transfer")
        .args_json(json!({"receiver_id": to, "amount": amount.to_string()}))
        .deposit(Balance::from_yoctonear(1))
        .gas(tgas(30))
        .submit()
}

/// Alice sends carol 10 and bob sends carol 7 in the same block, while bob
/// also tries to send 1000 he doesn't have. Natively each moves its 1 yocto
/// to the token; the failed one gets it back as a deposit refund, apart from
/// its gas refund, though both land on bob.
#[test]
fn ft_transfers_in_one_block() {
    let base = ft_world();
    let ops: [(&str, &str, u128); 3] = [
        ("alice.near", "carol.near", 10),
        ("bob.near", "carol.near", 7),
        ("bob.near", "carol.near", 1000),
    ];
    let mut sim = base.clone();
    let hashes: Vec<_> = ops.iter().map(|(f, t, a)| ft_transfer(&mut sim, f, t, *a)).collect();
    sim.settle();
    let outs: Vec<_> = hashes.iter().map(|h| sim.outcome(h)).collect();
    let heights: HashSet<_> =
        outs.iter().map(|o| o.expect_receipt("ft_transfer").block_height).collect();
    assert_eq!(heights.len(), 1, "transfers did not share a block");
    assert!(outs.iter().all(|o| o.is_complete() && o.pending.is_empty()));

    let e: Vec<_> = outs.iter().map(TxOutcome::effects).collect();
    assert_eq!(
        (e[0].near_net("alice.near"), e[0].near_net(TOKEN)),
        (Delta::loss(1), Delta::gain(1))
    );
    assert_eq!(e[0].near_paid("alice.near", TOKEN), Balance::from_yoctonear(1));
    assert_eq!((e[1].near_net("bob.near"), e[1].near_net(TOKEN)), (Delta::loss(1), Delta::gain(1)));
    assert!(outs[2].is_failure());
    let failed = e[2];
    assert_eq!(failed.near_net("bob.near"), 0);
    let kinds: Vec<_> = failed.near.iter().map(|m| (m.kind, m.amount.as_yoctonear())).collect();
    assert_eq!(kinds, [(NearKind::DepositRefund, 1)], "the gas refund is no value move");
    let refund = &failed.near[0];
    assert_eq!((refund.from.as_str(), refund.to.as_str()), ("bob.near", "bob.near"));
    assert!(failed.gas_paid() > Balance::ZERO);
    // No token is watched: token effects are unobserved, never 0.
    let err = e[0].token_net(TOKEN, "carol.near").unwrap_err();
    assert_eq!(err.token, TOKEN);
    assert!(e[0].token_supply_net(TOKEN).is_err());

    assert_closure("ft", &["alice.near", "bob.near", "carol.near"], &base, &sim, &outs);
    let ops: Vec<(String, SubmitFn)> = ops
        .iter()
        .map(|&(f, t, a)| {
            let op: SubmitFn = Box::new(move |s| ft_transfer(s, f, t, a));
            (format!("{f} -> {t} {a}"), op)
        })
        .collect();
    let ops: Vec<Op> = ops.iter().map(|(n, f)| (n.as_str(), &**f)).collect();
    let users = ["alice.near", "bob.near", "carol.near"];
    assert!(isolation("ft", &users, &base, &ops, &outs).is_empty());
}

// ---------------------------------------------------------------------------
// A deposit refund that goes to a contract, not the signer
// ---------------------------------------------------------------------------

/// `send_to_missing` forwards 1 NEAR to an account that doesn't exist. The
/// runtime refunds that deposit to the contract (the failed receipt's
/// predecessor) and the gas to the signer. Two overlapping calls: each keeps
/// its own deposit on the probe.
#[test]
fn deposit_refund_to_a_contract() {
    let mut base = Sim::new();
    base.create_account("alice.near", near(100));
    base.create_account("bob.near", near(100));
    base.deploy("probe.near", failure_probe_wasm());
    let call = |sim: &mut Sim, who: &str, n: u128| {
        sim.call(who, "probe.near", "send_to_missing").deposit(near(n)).gas(tgas(50)).submit()
    };
    let mut sim = base.clone();
    let (ha, hb) = (call(&mut sim, "alice.near", 2), call(&mut sim, "bob.near", 3));
    sim.settle();
    let outs = [sim.outcome(ha), sim.outcome(hb)];
    assert_overlapped("probe", &outs);
    for (o, who, n) in [(&outs[0], "alice.near", 2), (&outs[1], "bob.near", 3)] {
        let e = o.effects();
        assert_eq!(e.near_net(who), -((n * NEAR) as i128));
        assert_eq!(e.near_net("probe.near"), (n * NEAR) as i128);
        assert_eq!(e.near_paid(who, "probe.near"), near(n));
        assert_eq!(e.near_net("missing.near"), 0);
        let refund =
            e.near.iter().find(|m| m.kind == NearKind::DepositRefund).expect("deposit refund");
        assert_eq!(
            (refund.from.as_str(), refund.to.as_str(), refund.amount),
            ("probe.near", "probe.near", near(1))
        );
        // A self-move: listed, but neither paid nor net.
        assert_eq!(e.near_paid("probe.near", "probe.near"), Balance::ZERO);
    }
    assert_closure("probe", &["alice.near", "bob.near"], &base, &sim, &outs);
    let (a, b) = (|s: &mut Sim| call(s, "alice.near", 2), |s: &mut Sim| call(s, "bob.near", 3));
    let ops: [Op; 2] = [("alice", &a), ("bob", &b)];
    assert!(isolation("probe", &["alice.near", "bob.near"], &base, &ops, &outs).is_empty());
}

/// The signer's balance changes by exactly `near_net - gas_paid`: a call
/// with a deposit, a plain transfer, and a refused transaction that costs
/// nothing.
#[test]
fn the_signer_pays_value_and_gas_exactly() {
    let mut sim = Sim::new();
    sim.create_account("alice.near", near(100));
    sim.create_account("bob.near", near(1));
    sim.deploy("probe.near", failure_probe_wasm());
    let before = bal(&sim, "alice.near");
    let out = sim
        .call("alice.near", "probe.near", "send_to_missing")
        .deposit(near(2))
        .gas(tgas(50))
        .transact();
    let e = out.effects();
    assert_eq!(
        e.near_net("alice.near") - Delta::gain(e.gas_paid().as_yoctonear()),
        d(bal(&sim, "alice.near"), before)
    );
    assert!(e.gas_paid() <= out.tokens_burnt(), "gas refunds came back");

    let before = bal(&sim, "alice.near");
    let out = sim.tx("alice.near", "bob.near").transfer(near(3)).transact();
    let e = out.effects();
    assert_eq!(e.near_paid("alice.near", "bob.near"), near(3));
    assert_eq!(e.near[0].kind, NearKind::Deposit);
    assert_eq!(
        d(bal(&sim, "alice.near"), before),
        -((3 * NEAR) as i128) - e.gas_paid().as_yoctonear() as i128
    );
    assert_eq!(e.gas_paid(), out.tokens_burnt(), "a transfer burns all it buys");
    assert!(out.is_complete());

    // Refused at conversion: nothing paid, nothing moved.
    let before = bal(&sim, "bob.near");
    let out = sim.tx("bob.near", "alice.near").transfer(near(50)).transact();
    assert!(out.is_failure());
    assert_eq!((out.effects().gas_paid(), out.effects().near.len()), (Balance::ZERO, 0));
    assert_eq!(bal(&sim, "bob.near"), before);
}

// ---------------------------------------------------------------------------
// Pending: holds, callbacks awaiting results, parked yields
// ---------------------------------------------------------------------------

/// The probe's call, whose 1 NEAR call to a missing account (`ping`) is
/// held `blocks`.
fn held_probe(blocks: u64) -> (Sim, CryptoHash) {
    let mut sim = Sim::new();
    sim.create_account("alice.near", near(10));
    sim.deploy("probe.near", failure_probe_wasm());
    let h = sim
        .call("alice.near", "probe.near", "send_to_missing")
        .deposit(near(2))
        .gas(tgas(50))
        .inject(Receipts::method("ping"), Fault::Hold(blocks))
        .submit();
    (sim, h)
}

/// A held call is pending, with its deposit, until it runs, and so is the
/// callback awaiting it; the transaction isn't complete meanwhile, and its
/// effects don't count the held deposit. Once its result path ends, a
/// refund still pending keeps it incomplete though it succeeded.
#[test]
fn a_held_receipt_is_pending() {
    let (mut sim, h) = held_probe(3);
    let held =
        |s: &Sim| s.outcome(h).pending.iter().any(|p| matches!(p.state, PendingState::Held { .. }));
    sim.run_until(5, held).unwrap();
    let out = sim.outcome(h);
    let p = out.pending.iter().find(|p| p.receiver_id == "missing.near").expect("the held call");
    assert_eq!(
        (p.tx, p.predecessor_id.as_str(), p.method_names.as_slice()),
        (h, "probe.near", &["ping".to_string()][..])
    );
    assert_eq!((p.deposit, p.state), (near(1), PendingState::Held { blocks_left: 3 }));
    // The callback goes out with it, then waits in state for its result.
    let cb = out.pending.iter().find(|p| p.method_names == ["on_cb"]).expect("the callback");
    assert_eq!(cb.state, PendingState::InFlight);
    assert!(!out.is_complete());
    assert_eq!(out.effects().pending, out.pending);
    assert_eq!(
        out.effects().near_net("probe.near"),
        (2 * NEAR) as i128,
        "the held 1 NEAR still counts as the probe's"
    );
    assert_eq!(sim.pending_receipts(), out.pending);
    assert!(
        out.to_string().contains("missing.near::ping (not executed yet: held, 3 more blocks)"),
        "{out}"
    );
    sim.step();
    let state =
        |r: &str| sim.outcome(h).pending.into_iter().find(|p| p.receiver_id == r).unwrap().state;
    assert_eq!(state("missing.near"), PendingState::Held { blocks_left: 2 });
    assert_eq!(state("probe.near"), PendingState::AwaitingResults);

    sim.run_until(10, |s| s.outcome(h).is_success()).unwrap();
    let out = sim.outcome(h);
    assert!(!out.pending.is_empty() && !out.is_complete(), "a refund is still pending: {out}");
    sim.settle();
    let out = sim.outcome(h);
    assert!(out.is_complete() && out.pending.is_empty() && sim.pending_receipts().is_empty());
    assert!(
        out.effects().near.iter().any(|m| m.kind == NearKind::DepositRefund && m.amount == near(1))
    );
}

/// A parked yield's callback is pending until the timeout runs it; the
/// transaction (whose result is that callback) isn't complete meanwhile.
#[test]
fn a_parked_yield_callback_is_pending() {
    let mut sim = Sim::new();
    sim.create_account("alice.near", near(10));
    sim.deploy("y.near", yield_wasm());
    let h = sim.call("alice.near", "y.near", "yield_return").submit();
    sim.settle();
    let [y] = &sim.yields()[..] else { panic!() };
    let out = sim.outcome(h);
    let [p] = &out.pending[..] else { panic!("{:?}", out.pending) };
    assert_eq!(
        (p.receipt_id, p.receiver_id.as_str(), p.method_names.as_slice()),
        (y.receipt_id, "y.near", &["cb".to_string()][..])
    );
    assert_eq!(p.state, PendingState::Yield { expires_at: y.expires_at });
    assert!(!out.is_complete() && out.is_pending());
    assert_eq!(sim.pending_receipts(), out.pending);
    let shown =
        format!("y.near::cb (not executed yet: parked yield, times out at #{})", y.expires_at);
    assert!(out.to_string().contains(&shown), "{out}");

    let y = y.clone();
    let out = sim.expire(&y);
    assert!(out.is_complete() && out.pending.is_empty() && sim.pending_receipts().is_empty());
}

/// The storage recipe: what the bytes a call added lock, against the
/// deposit that reached the contract. `storage_bytes` is a signed byte
/// count, `near_net` a `Delta`.
#[test]
fn storage_bytes_price_with_storage_cost() {
    let mut sim = Sim::new();
    for id in ["alice.near", "bob.near"] {
        sim.create_account(id, near(100));
    }
    sim.deploy(TOKEN, ft_wasm());
    let init = json!({"owner_id": "alice.near", "total_supply": "1000"});
    sim.call(TOKEN, TOKEN, "new_default_meta").args_json(init).transact().assert_success();
    let out = sim
        .call("alice.near", TOKEN, "storage_deposit")
        .args_json(json!({"account_id": "bob.near"}))
        .deposit(Balance::from_millinear(125))
        .transact();
    out.assert_success();
    let e = out.effects();
    let bytes: i64 = e.storage_bytes(TOKEN);
    assert!(bytes > 0, "{out}");
    let cost = sim.storage_cost(u64::try_from(bytes).unwrap_or(0));
    let kept: Delta = e.near_net(TOKEN);
    assert!(kept >= Delta::gain(cost.as_yoctonear()), "kept {kept} < {}", cost.as_yoctonear());
    assert_eq!(kept.magnitude(), i128::try_from(kept).unwrap().unsigned_abs());
    assert!(e.near_deposited(TOKEN) >= cost, "the caller paid for its storage");
}
