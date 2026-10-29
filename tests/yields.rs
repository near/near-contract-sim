//! Yield / resume (NEP-519) through the real runtime, including timeouts.
//! Mostly uses probe.wasm (source: tests/res/probe_src.rs.txt).

mod common;
use common::*;
use near_contract_sim::{FinalExecutionStatus, Gas, Sim};

fn last(sim: &Sim) -> String {
    sim.view("probe.near", "last", ()).json()
}

#[test]
fn yield_then_resume_via_separate_tx() {
    let mut sim = probe_sim();
    let h0 = sim.height();
    let y = sim.call("alice.near", "probe.near", "start_yield").transact();
    y.assert_success(); // start_yield returns (); the callback is parked
    // transact() did NOT fast-forward 200 blocks to fire the timeout.
    assert!(sim.height() < h0 + 10, "height {}", sim.height());
    let pending = sim.yields();
    assert_eq!(pending.len(), 1, "{pending:?}");
    assert_eq!(pending[0].receiver_id.as_str(), "probe.near");
    assert_eq!(pending[0].callback_methods, vec!["on_resume"]);
    assert_eq!(pending[0].callback_args, [b"{}"]);
    assert_eq!(pending[0].tx, y.hash);
    let yielded = y.expect_receipt("start_yield");
    assert_eq!(yielded.receipt_ids[0], pending[0].receipt_id);
    assert!(last(&sim).starts_with("yielded:"));
    // The callback receipt has not executed yet.
    assert!(y.receipt("on_resume").is_none());

    // Resume later, from another transaction.
    sim.advance_to(sim.height() + 5);
    let r = sim.call("bob.near", "probe.near", "resume").transact();
    assert!(r.json::<bool>());
    assert!(sim.yields().is_empty());
    assert_eq!(last(&sim), "resumed:\"hello\"");

    // Re-query the original tx: its receipts now include the executed callback.
    let y = sim.outcome(y.hash);
    let cb = y.receipt("on_resume").expect("callback executed");
    assert!(cb.is_success());
    assert_eq!(cb.receipt_id, pending[0].receipt_id);
}

/// Block `expires_at` fires the timeout, and `advance_to(expires_at)` gets
/// there cheaply however far it is; `advance_to(expires_at - 1)` leaves the
/// yield parked. The callback, with `PromiseResult::Failed`, runs in the
/// block after.
#[test]
fn advance_to_expires_at_fires_the_timeout() {
    let mut sim = probe_sim();
    let y = sim.call("alice.near", "probe.near", "start_yield").transact();
    let parked = sim.yields();
    let expires_at = parked[0].expires_at;
    sim.advance_to(expires_at - 1);
    assert_eq!(sim.yields(), parked);
    sim.advance_to(expires_at);
    assert!(sim.yields().is_empty());
    assert!(!sim.is_idle(), "the timeout's resume is in flight");
    sim.settle();
    let last: String = sim.view("probe.near", "last", ()).json();
    assert_eq!(last, "timeout");
    let y = sim.outcome(y.hash);
    let cb = y.receipt("on_resume").unwrap();
    assert_eq!(cb.block_height, expires_at + 1);
    assert!(cb.is_success() && matches!(y.status, FinalExecutionStatus::SuccessValue(_)));
}

/// The race between a resume call and the timeout. From another account,
/// submitted while the yield is listed, at `height() == expires_at - 1`, the
/// call runs in block `expires_at + 1` ahead of the timeout's resume
/// receipt, and wins; submitted once block `expires_at` fired the timeout, it
/// runs after the callback got `PromiseResult::Failed`. From the yield's own
/// account, the call's receipt is local: it runs first in block
/// `expires_at + 1`, and still wins.
#[test]
fn a_resume_that_runs_before_the_timeouts_receipt_wins() {
    let mut sim = probe_sim();
    sim.call("alice.near", "probe.near", "start_yield").transact().assert_success();
    let expires_at = sim.yields()[0].expires_at;
    let resume_at = |signer: &str, h: u64| {
        let mut sim = sim.clone();
        sim.advance_to(h);
        let r = sim.call(signer, "probe.near", "resume").transact();
        assert_eq!(r.transaction.as_ref().unwrap().block_height, h + 1);
        (r.json::<bool>(), last(&sim))
    };
    let resumed = (true, "resumed:\"hello\"".to_string());
    assert_eq!(resume_at("bob.near", expires_at - 1), resumed);
    assert_eq!(resume_at("bob.near", expires_at), (false, "timeout".into()));
    assert_eq!(resume_at("probe.near", expires_at), resumed);
}

/// `set_height` past a parked yield's expiry skips the block that would
/// have fired the timeout: it is overdue, so the chain is busy, and the
/// next block fires it.
#[test]
fn set_height_past_the_expiry_makes_the_timeout_overdue() {
    let mut sim = probe_sim();
    let y = sim.call("alice.near", "probe.near", "start_yield").transact();
    let expires_at = sim.yields()[0].expires_at;
    sim.set_height(expires_at);
    assert!(!sim.is_idle());
    sim.settle();
    assert_eq!(last(&sim), "timeout");
    let cb = sim.outcome(y.hash).receipt("on_resume").unwrap().block_height;
    assert_eq!(cb, expires_at + 2, "fired in block expires_at + 1");
}

/// A resumed yield whose `PromiseResume` receipt is still in flight is no
/// longer waiting for resume or timeout, so `yields` must not list it.
#[test]
fn yields_excludes_resumed_in_flight() {
    let mut sim = sim_with_users();
    sim.deploy("probe.near", probe_wasm());
    sim.call("alice.near", "probe.near", "start_yield").transact().assert_success();
    assert_eq!(sim.yields().len(), 1);

    let h = sim.call("bob.near", "probe.near", "resume").submit();
    sim.step(); // bob's tx converted
    sim.step(); // `resume` runs; PromiseResume goes out for the next block
    assert!(sim.outcome(h).json::<bool>());
    assert!(sim.yields().is_empty(), "{:?}", sim.yields());

    sim.settle();
    assert_eq!(sim.view("probe.near", "last", ()).json::<String>(), "resumed:\"hello\"");
}

/// A yield callback on the result path keeps the transaction `Started`
/// until the timeout fires; re-querying then gives the callback's value.
#[test]
fn yield_on_result_path_is_pending_until_timeout() {
    let mut sim = sim_with_users();
    sim.deploy("y.near", yield_wasm());
    let out = sim.call("alice.near", "y.near", "yield_return").transact();
    assert_eq!(out.status, FinalExecutionStatus::Started);
    assert!(out.is_pending() && !out.is_success() && !out.is_failure());
    assert!(out.value().is_none() && out.failure().is_none());
    let p = sim.yields();
    assert_eq!(p.len(), 1);
    assert_eq!(p[0].callback_methods, ["cb"]);
    assert_eq!(p[0].expires_at, out.receipt("yield_return").unwrap().block_height + 200);
    sim.advance_to(p[0].expires_at);
    assert!(sim.yields().is_empty());
    assert!(sim.outcome(out.hash).is_pending(), "the callback runs in the next block");
    sim.settle();
    let out = sim.outcome(out.hash);
    assert_eq!(out.json::<String>(), "timeout");
    assert!(out.is_success() && !out.is_pending());
}

/// Yield and resume inside one receipt: the yielded callback never sits in
/// the timeout queue at a block boundary, but the outcome still names it.
#[test]
fn yield_and_resume_in_same_receipt() {
    let mut sim = sim_with_users();
    sim.deploy("y.near", yield_wasm());
    let out = sim.call("alice.near", "y.near", "yield_and_resume").transact();
    out.assert_success();
    assert!(sim.yields().is_empty());
    let cb = out.receipt("cb").expect("callback outcome");
    assert!(cb.is_success());
    assert_eq!(cb.predecessor_id.as_str(), "y.near");
    assert_eq!(
        cb.status,
        near_contract_sim::ExecutionStatus::SuccessValue(b"\"resumed\"".to_vec())
    );
}

/// `PendingYield::tx` names the transaction, also when the receipt that
/// yields runs two receipts below it: a relayed call, under the `Delegate`.
#[test]
fn pending_yield_names_its_transaction() {
    let mut sim = probe_sim();
    sim.call("alice.near", "probe.near", "start_yield").transact().assert_success();
    let relayed = sim
        .call("bob.near", "probe.near", "start_yield")
        .gas(Gas::from_teragas(50))
        .relay("alice.near");
    let tx: Vec<_> = sim.yields().iter().map(|y| y.tx).collect();
    assert_eq!(tx.len(), 2);
    assert_eq!(tx[1], relayed.hash);
    assert_ne!(tx[0], tx[1]);
    assert!(sim.outcome(tx[0]).receipt("start_yield").is_some());
}

/// The tree shows the parked callback until the timeout, then the callback
/// that received `PromiseResult::Failed`.
#[test]
fn renders_yield_timeout() {
    let mut sim = sim_with_users();
    sim.deploy("y.near", yield_wasm());
    let out = sim.call("alice.near", "y.near", "yield_return").transact();
    assert_rendered(&out.to_string(), "\
tx 4oCvxJ… signed by alice.near at #3 (conversion 0.31 Tgas) -> PENDING
└─ #4 y.near::yield_return  attached 300.00 Tgas, burnt 1.43 Tgas (0.54 + 0.89 exec fees), headroom 288.57 Tgas  ok (returns a promise)
   └─ EGsJsT… y.near::cb (not executed yet: parked yield, times out at #204)
(1 refund receipt(s) not shown)

gas (Tgas)                 wasm    load storage    host    send    exec   total
conversion alice.near         -       -       -       -   0.309       -   0.309
#4 y.near::yield_return   0.000   0.001       -   0.163   0.381   0.888   1.432
refunds (not charged)         -       -       -       -       -       -   0.223
total 1.96 Tgas (1964212705299 gas)
");
    sim.advance_to(sim.height() + 205);
    sim.settle();
    assert_rendered(&sim.outcome(out.hash).to_string(), "\
tx 4oCvxJ… signed by alice.near at #3 (conversion 0.31 Tgas) -> SUCCESS
└─ #4 y.near::yield_return  attached 300.00 Tgas, burnt 1.43 Tgas (0.54 + 0.89 exec fees), headroom 288.57 Tgas  ok (returns a promise)
   └─ #205 y.near::cb [callback of yield/resume]  attached 10.00 Tgas, burnt 0.89 Tgas (0.00 + 0.89 exec fees), headroom 10.00 Tgas  ok -> \"timeout\"
(2 refund receipt(s) not shown)

gas (Tgas)                 wasm    load storage    host    send    exec   total
conversion alice.near         -       -       -       -   0.309       -   0.309
#4 y.near::yield_return   0.000   0.001       -   0.163   0.381   0.888   1.432
#205 y.near::cb           0.000   0.001       -   0.003       -   0.888   0.892
refunds (not charged)         -       -       -       -       -       -   0.446
total 3.08 Tgas (3079152859341 gas)
");
}

/// A resumed yield is no longer parked, so `expire` refuses it.
#[test]
fn expire_on_a_resumed_yield_panics() {
    use std::panic::{AssertUnwindSafe, catch_unwind};
    let mut sim = probe_sim();
    sim.call("alice.near", "probe.near", "start_yield").transact().assert_success();
    let y = sim.yields().pop().unwrap();
    sim.call("bob.near", "probe.near", "resume").transact().assert_success();
    let h = sim.height();
    let msg = panic_message(catch_unwind(AssertUnwindSafe(|| sim.expire(&y))));
    assert!(msg.starts_with("expire: the yield of on_resume on probe.near"), "{msg}");
    assert!(msg.ends_with("is not parked: resumed or timed out already"), "{msg}");
    assert_eq!(sim.height(), h);
}

/// Expiring the later of two yields fires the earlier first, in queue
/// order; expiring the earlier leaves the later parked.
#[test]
fn expire_fires_earlier_yields_first() {
    let mut sim = sim_with_users();
    sim.deploy("y.near", yield_wasm());
    let first = sim.call("alice.near", "y.near", "yield_return").submit();
    sim.settle();
    sim.advance_to(sim.height() + 3);
    let second = sim.call("bob.near", "y.near", "yield_return").submit();
    sim.settle();
    let [y1, y2] = <[_; 2]>::try_from(sim.yields()).expect("two parked yields");
    assert_eq!((y1.tx, y2.tx), (first, second));

    let mut sooner = sim.clone();
    sooner.expire(&y1).assert_success();
    assert_eq!(sooner.yields(), std::slice::from_ref(&y2));

    let out = sim.expire(&y2);
    assert_eq!(out.json::<String>(), "timeout");
    let cb = |tx| sim.outcome(tx).expect_receipt("cb").block_height;
    assert_eq!((cb(first), cb(second)), (y1.expires_at + 1, y2.expires_at + 1));
    assert!(y1.expires_at < y2.expires_at);
    assert!(sim.yields().is_empty() && sim.is_idle());
}

/// An overdue yield (`set_height` skipped its block) expires without
/// moving backwards: the next block fires it.
#[test]
fn expire_an_overdue_yield() {
    let mut sim = probe_sim();
    let y = sim.call("alice.near", "probe.near", "start_yield").transact();
    let parked = sim.yields().pop().unwrap();
    sim.set_height(parked.expires_at + 10);
    let out = sim.expire(&parked);
    assert_eq!(out.hash, y.hash);
    assert_eq!(out.expect_receipt("on_resume").block_height, parked.expires_at + 12);
    assert_eq!(last(&sim), "timeout");
}
