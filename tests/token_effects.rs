//! Token effects on real contracts: a watched token's moves are measured per
//! receipt by its own views (`Sim::watch`), so they stay exact while other
//! transactions run in the same blocks, on tokens that emit no event for
//! some moves.
//!
//! Two oracles judge them:
//! - closure: at rest, each holder's `token_net` summed over the
//!   transactions is its balance change by the view, and the same for the
//!   supply;
//! - isolation: a transaction run alone on a clone of the same state
//!   changes the views by what its overlapped run's effects say (unless the
//!   two runs end differently: interfering transactions aren't comparable);
//! - a watch changes nothing else: the same transactions with and without
//!   it give the same state root after every block, and the same outcomes.
//!
//! Verification boundary: the watch reads state and runs views, read-only,
//! inside block production; it changes no block content, receipt order,
//! gas or state. Passing shows the measurement holds on these executed
//! cases only.

mod common;

use common::{ft_wasm, near, res};
use near_contract_sim::{Balance, Delta, Fault, Fixture, Receipts, Sim, Token, Tx, TxOutcome};
use serde_json::{Value, json};
use std::collections::HashSet;

fn u128_view(sim: &Sim, id: &str, method: &str, args: Value) -> u128 {
    match sim.view(id, method, args).json::<Value>() {
        Value::String(s) => s.parse().unwrap(),
        v => v.to_string().parse().unwrap(),
    }
}

/// The same chain: after every block the same state root, and every
/// receipt with the same id, status, gas and logs.
fn assert_same_chain(a: &Sim, b: &Sim) {
    assert_eq!(a.state_root(), b.state_root(), "state roots");
    let key = |s: &Sim| -> Vec<_> {
        s.receipts_in(..)
            .into_iter()
            .map(|r| (r.receipt_id, r.status, r.gas_burnt, r.tokens_burnt, r.logs, r.receipt_ids))
            .collect()
    };
    assert_eq!(key(a), key(b), "receipts");
}

/// Steps `a` and `b` together until both are idle, comparing state roots
/// after every block.
fn settle_in_lockstep(a: &mut Sim, b: &mut Sim) {
    while !(a.is_idle() && b.is_idle()) {
        a.step();
        b.step();
        assert_eq!(a.state_root(), b.state_root(), "state root at #{}", a.height());
    }
}

// ---------------------------------------------------------------------------
// The token vault: deposits and withdrawals, token and shares
// ---------------------------------------------------------------------------

const TOKEN: &str = "token.near";
const VAULT: &str = "vault.near";

fn vault_fixture() -> Fixture {
    Fixture::new()
        .account("alice.near", Balance::from_near(10))
        .account("bob.near", Balance::from_near(10))
        .contract(TOKEN, res("fungible_token.wasm"))
        .init("new_default_meta")
        .args_json(json!({ "owner_id": TOKEN, "total_supply": "2000" }))
        .contract(VAULT, res("vault.wasm"))
        .init("new")
        .args_json(json!({ "token_id": TOKEN }))
        .ft_register(TOKEN, [VAULT, "alice.near", "bob.near"])
        .ft_transfer(TOKEN, TOKEN, "alice.near", 1000)
        .ft_transfer(TOKEN, TOKEN, "bob.near", 1000)
}

fn world() -> Sim {
    vault_fixture()
        .token(Token::nep141(TOKEN))
        .token(Token::view(VAULT, "shares_of").supply("total_shares"))
        .build()
}

fn ft(sim: &Sim, who: &str) -> u128 {
    sim.view(TOKEN, "ft_balance_of", json!({ "account_id": who })).u128()
}
fn shares(sim: &Sim, who: &str) -> u128 {
    sim.view(VAULT, "shares_of", json!({ "account_id": who })).u128()
}
fn deposit(sim: &mut Sim, who: &str, amount: u128) -> TxOutcome {
    sim.call(who, TOKEN, "ft_transfer_call")
        .args_json(json!({ "receiver_id": VAULT, "amount": amount.to_string(), "msg": "" }))
        .deposit(Balance::from_yoctonear(1))
        .transact()
}
fn withdraw<'a>(sim: &'a mut Sim, who: &str, amount: u128) -> Tx<'a> {
    sim.call(who, VAULT, "withdraw")
        .args_json(json!({ "amount": amount.to_string() }))
        .deposit(Balance::from_yoctonear(1))
}

/// Each move names the receipt that made it: the token's transfer moved
/// alice's tokens to the vault, the vault's `ft_on_transfer` minted her
/// shares. The supply of shares moved with them; the token's didn't.
#[test]
fn each_receipt_carries_its_own_moves() {
    let mut sim = world();
    let d = deposit(&mut sim, "alice.near", 100);
    let moves = |method: &str| {
        let r = d.expect_receipt(method);
        let mut m: Vec<_> = r.token_moves.iter().map(|m| (m.holder.clone(), m.delta)).collect();
        m.sort();
        assert!(r.token_moves.iter().all(|m| m.receipt_id == r.receipt_id));
        m
    };
    let (alice, vault) = (Some("alice.near".parse().unwrap()), Some(VAULT.parse().unwrap()));
    assert_eq!(
        moves("ft_transfer_call"),
        [(alice.clone(), Delta::loss(100)), (vault, Delta::gain(100))]
    );
    assert_eq!(moves("ft_on_transfer"), [(None, Delta::gain(100)), (alice, Delta::gain(100))]);
    assert_eq!(moves("ft_resolve_transfer"), []);
    let e = d.effects();
    assert_eq!(
        (e.token_supply_net(TOKEN), e.token_supply_net(VAULT)),
        (Ok(Delta::ZERO), Ok(Delta::from(100)))
    );
    assert_eq!(e.token_moves.len(), 4);
}

/// Overlapping deposits and withdrawals of two users in the same blocks:
/// each operation's effects are its own, and they add up to the views'
/// change.
#[test]
fn overlapping_vault_ops_are_exact() {
    let mut sim = world();
    deposit(&mut sim, "alice.near", 100).assert_success();
    let base = sim.clone();
    let submit_deposit = |sim: &mut Sim, who: &str, n: u128| {
        sim.call(who, TOKEN, "ft_transfer_call")
            .args_json(json!({ "receiver_id": VAULT, "amount": n.to_string(), "msg": "" }))
            .deposit(Balance::from_yoctonear(1))
            .submit()
    };
    let hs = [
        submit_deposit(&mut sim, "bob.near", 30),
        withdraw(&mut sim, "alice.near", 25).submit(),
        submit_deposit(&mut sim, "alice.near", 7),
    ];
    sim.settle();
    let outs: Vec<TxOutcome> = hs.iter().map(|h| sim.outcome(h)).collect();
    let shared: HashSet<u64> = outs[0].receipts.iter().map(|r| r.block_height).collect();
    assert!(outs[1].receipts.iter().any(|r| shared.contains(&r.block_height)), "overlapped");
    let net = |i: usize, t: &str, who: &str| outs[i].effects().token_net(t, who).unwrap();
    assert_eq!([net(0, TOKEN, "bob.near"), net(0, VAULT, "bob.near")], [-30, 30]);
    assert_eq!([net(1, TOKEN, "alice.near"), net(1, VAULT, "alice.near")], [25, -25]);
    assert_eq!([net(2, TOKEN, "alice.near"), net(2, VAULT, "alice.near")], [-7, 7]);
    assert_eq!(net(1, TOKEN, "bob.near"), 0, "bob's deposit is not alice's withdraw's");
    for who in ["alice.near", "bob.near", VAULT] {
        let sum: Delta = (0..3).map(|i| net(i, TOKEN, who)).sum();
        assert_eq!(sum, Delta::between(ft(&base, who), ft(&sim, who)), "closure {who}");
    }
}

/// A token nobody watches is unobserved, never 0.
#[test]
fn an_undeclared_token_is_unobserved() {
    let mut sim = vault_fixture().token(Token::nep141(TOKEN)).build();
    let d = deposit(&mut sim, "alice.near", 100);
    assert_eq!(d.effects().token_net(TOKEN, "alice.near"), Ok(Delta::from(-100)));
    let e = d.effects().token_net(VAULT, "alice.near").unwrap_err();
    assert_eq!(e.token, VAULT);
    assert!(e.why.contains("not a watched token"), "{e}");
    // Watched with no supply view: holders are measured, the supply isn't.
    let mut sim = vault_fixture().token(Token::view(VAULT, "shares_of")).build();
    let d = deposit(&mut sim, "alice.near", 100);
    assert_eq!(d.effects().token_net(VAULT, "alice.near"), Ok(Delta::from(100)));
    let e = d.effects().token_supply_net(VAULT).unwrap_err();
    assert!(e.why.contains("no supply view"), "{e}");
}

/// A watch that starts while a transaction is in flight saw only part of
/// it: that transaction's moves of the token are unobserved, and the next
/// one's are exact.
#[test]
fn a_watch_started_mid_flight_is_unobserved() {
    let mut sim = vault_fixture().build();
    let h = sim
        .call("alice.near", TOKEN, "ft_transfer_call")
        .args_json(json!({ "receiver_id": VAULT, "amount": "100", "msg": "" }))
        .deposit(Balance::from_yoctonear(1))
        .submit();
    sim.step();
    sim.step(); // the token's transfer ran, unwatched
    sim.watch(Token::nep141(TOKEN));
    sim.settle();
    let e = sim.outcome(h).effects().token_net(TOKEN, "alice.near").unwrap_err();
    assert!(e.why.contains("is watched from block"), "{e}");
    let d = deposit(&mut sim, "alice.near", 5);
    assert_eq!(d.effects().token_net(TOKEN, "alice.near"), Ok(Delta::from(-5)));
}

/// A pure token's measurement carries over between blocks only while the
/// state root is the one it was taken on: a direct write in between (here
/// alice's shares, patched to 500) is seen.
#[test]
fn a_pure_watch_sees_direct_writes() {
    let mut sim = vault_fixture()
        .token(Token::view(VAULT, "shares_of").supply("total_shares").pure())
        .build();
    assert_eq!(
        deposit(&mut sim, "alice.near", 100).effects().token_net(VAULT, "alice.near"),
        Ok(Delta::from(100))
    );
    let key = [b"s".as_slice(), &borsh::to_vec("alice.near").unwrap()].concat();
    sim.set_state(VAULT, &key, Some(&borsh::to_vec(&500u128).unwrap()));
    let w = withdraw(&mut sim, "alice.near", 40).transact();
    w.assert_success();
    assert_eq!(w.effects().token_net(VAULT, "alice.near"), Ok(Delta::from(-40)));
    assert_eq!(shares(&sim, "alice.near"), 460);
}

/// The views run read-only: with and without watches, the same
/// transactions give the same state root after every block, and the same
/// outcomes.
#[test]
fn a_watch_changes_no_state_gas_or_outcome() {
    let mut plain = vault_fixture().build();
    deposit(&mut plain, "alice.near", 100).assert_success();
    let mut watched = plain.clone();
    watched.watch(Token::nep141(TOKEN));
    watched.watch(Token::view(VAULT, "shares_of").supply("total_shares"));
    for sim in [&mut plain, &mut watched] {
        withdraw(sim, "alice.near", 40).submit();
        withdraw(sim, "alice.near", 10)
            .inject(Receipts::callback("on_withdraw"), Fault::PromiseFailed)
            .submit();
        sim.call("bob.near", TOKEN, "ft_transfer_call")
            .args_json(json!({ "receiver_id": VAULT, "amount": "30", "msg": "" }))
            .deposit(Balance::from_yoctonear(1))
            .submit();
    }
    settle_in_lockstep(&mut plain, &mut watched);
    assert_same_chain(&plain, &watched);
    let moved = |s: &Sim| s.receipts_in(..).iter().map(|r| r.token_moves.len()).sum::<usize>();
    assert_eq!(moved(&plain), 0);
    assert!(moved(&watched) > 0);
}

// ---------------------------------------------------------------------------
// Holders a contract created, and a token that rebases with time
// ---------------------------------------------------------------------------

/// `make` creates `sub.factory.near`, a sub-account the contract creates.
fn factory_wasm() -> Vec<u8> {
    wat::parse_str(
        r#"(module
  (import "env" "promise_batch_create" (func $create (param i64 i64) (result i64)))
  (import "env" "promise_batch_action_create_account" (func $ca (param i64)))
  (memory 1)
  (export "memory" (memory 0))
  (data (i32.const 0) "sub.factory.near")
  (func (export "make")
    (call $ca (call $create (i64.const 16) (i64.const 0)))))"#,
    )
    .unwrap()
}

const FT: &str = "ft.near";
const SUB: &str = "sub.factory.near";

/// alice holds 1000 of `ft.near`; `sub.factory.near`, which the factory
/// contract created, is registered with it.
fn sub_account_world() -> Sim {
    let mut sim = Sim::new();
    sim.create_account("alice.near", near(100));
    sim.deploy("factory.near", factory_wasm());
    sim.deploy(FT, ft_wasm());
    sim.call(FT, FT, "new_default_meta")
        .args_json(json!({"owner_id": "alice.near", "total_supply": "1000"}))
        .transact()
        .assert_success();
    sim.call("alice.near", "factory.near", "make").transact().assert_success();
    assert!(sim.account(SUB).is_some());
    sim.call("alice.near", FT, "storage_deposit")
        .args_json(json!({"account_id": SUB}))
        .deposit(Balance::from_millinear(125))
        .transact()
        .assert_success();
    sim
}

fn send(sim: &mut Sim, to: &str, amount: u128) -> TxOutcome {
    sim.call("alice.near", FT, "ft_transfer")
        .args_json(json!({"receiver_id": to, "amount": amount.to_string()}))
        .deposit(Balance::from_yoctonear(1))
        .transact()
}

/// An account a contract created is a holder by default.
/// An id that isn't an account is one once a standard event of the
/// token's own receipt names it (the token's `ft_transfer` names
/// `new_owner_id`), so tokens sent to it add up. Declared, it is
/// measured too; a token that logs no event leaves it unobserved, saying
/// how much went missing.
///
/// Verification boundary: which holders a watch measures, against actual
/// views; no scheduling change.
#[test]
fn a_contract_created_account_is_a_holder_and_a_ghost_is_unobserved() {
    let mut sim = sub_account_world();
    sim.watch(Token::nep141(FT));
    let e = send(&mut sim, SUB, 40).effects().clone();
    assert_eq!(
        (e.token_net(FT, "alice.near"), e.token_net(FT, SUB)),
        (Ok(Delta::from(-40)), Ok(Delta::from(40)))
    );

    // An id the token registers that is no account.
    const GHOST: &str = "ghost.near";
    let register = |sim: &mut Sim| {
        sim.call("alice.near", FT, "storage_deposit")
            .args_json(json!({"account_id": GHOST}))
            .deposit(Balance::from_millinear(125))
            .transact()
            .assert_success();
    };
    register(&mut sim);
    assert!(sim.account(GHOST).is_none());
    let out = send(&mut sim, GHOST, 40);
    out.assert_success();
    let e = out.effects();
    assert_eq!(
        (e.token_net(FT, "alice.near"), e.token_net(FT, GHOST)),
        (Ok(Delta::from(-40)), Ok(Delta::from(40)))
    );
    assert_eq!(e.token_supply_net(FT), Ok(Delta::ZERO));
    // A user's own account, created after the watch, is a holder at once.
    sim.create_account("bob.near", near(1));
    sim.call("bob.near", FT, "storage_deposit")
        .args_json(json!({}))
        .deposit(Balance::from_millinear(125))
        .transact()
        .assert_success();
    assert_eq!(
        send(&mut sim, "bob.near", 5).effects().token_net(FT, "bob.near"),
        Ok(Delta::from(5))
    );

    let mut sim = sub_account_world();
    sim.watch(Token::nep141(FT).holders([GHOST]).holders([GHOST]));
    register(&mut sim);
    let e = send(&mut sim, GHOST, 40).effects().clone();
    assert_eq!(
        (e.token_net(FT, "alice.near"), e.token_net(FT, GHOST)),
        (Ok(Delta::from(-40)), Ok(Delta::from(40)))
    );
    assert_eq!(e.token_supply_net(FT), Ok(Delta::ZERO));
}

/// A token whose balances grow with the clock: `balance_of` is the stored
/// units times the block's time in seconds, and `bump` adds a unit to the
/// caller's. The storage key is the view's own JSON input, so the view
/// needs no parsing.
fn rebasing_wasm() -> Vec<u8> {
    wat::parse_str(
        r#"(module
  (import "env" "input" (func $input (param i64)))
  (import "env" "predecessor_account_id" (func $pred (param i64)))
  (import "env" "register_len" (func $rlen (param i64) (result i64)))
  (import "env" "read_register" (func $rr (param i64 i64)))
  (import "env" "storage_read" (func $sr (param i64 i64 i64) (result i64)))
  (import "env" "storage_write" (func $sw (param i64 i64 i64 i64 i64) (result i64)))
  (import "env" "block_timestamp" (func $ts (result i64)))
  (import "env" "value_return" (func $vr (param i64 i64)))
  (memory 1)
  (export "memory" (memory 0))
  (data (i32.const 100) "{\"account_id\":\"")
  ;; The units stored under the key of length $len at 100.
  (func $stored (param $len i64) (result i64)
    (if (result i64)
      (i64.eq (call $sr (local.get $len) (i64.const 100) (i64.const 1)) (i64.const 1))
      (then (call $rr (i64.const 1) (i64.const 512)) (i64.load (i32.const 512)))
      (else (i64.const 0))))
  (func (export "bump")
    (local $end i64)
    (call $pred (i64.const 0))
    (call $rr (i64.const 0) (i64.const 115))
    (local.set $end (i64.add (call $rlen (i64.const 0)) (i64.const 115)))
    (i32.store8 (i32.wrap_i64 (local.get $end)) (i32.const 34))
    (i32.store8 (i32.add (i32.wrap_i64 (local.get $end)) (i32.const 1)) (i32.const 125))
    (local.set $end (i64.sub (i64.add (local.get $end) (i64.const 2)) (i64.const 100)))
    (i64.store (i32.const 520) (i64.add (call $stored (local.get $end)) (i64.const 1)))
    (drop (call $sw (local.get $end) (i64.const 100) (i64.const 8) (i64.const 520) (i64.const 2))))
  (func (export "balance_of")
    (local $n i64) (local $p i32)
    (call $input (i64.const 0))
    (call $rr (i64.const 0) (i64.const 100))
    (local.set $n (i64.mul (call $stored (call $rlen (i64.const 0)))
                           (i64.div_u (call $ts) (i64.const 1000000000))))
    (local.set $p (i32.const 700))
    (loop $digits
      (local.set $p (i32.sub (local.get $p) (i32.const 1)))
      (i32.store8 (local.get $p)
        (i32.add (i32.const 48) (i32.wrap_i64 (i64.rem_u (local.get $n) (i64.const 10)))))
      (local.set $n (i64.div_u (local.get $n) (i64.const 10)))
      (br_if $digits (i64.ne (local.get $n) (i64.const 0))))
    (call $vr (i64.extend_i32_u (i32.sub (i32.const 700) (local.get $p)))
              (i64.extend_i32_u (local.get $p)))))"#,
    )
    .unwrap()
}

/// Both views of a receipt run in its own block, so the clock's drift
/// between blocks isn't the receipt's: a bump moves exactly one unit at
/// that block's rate, while the naive diff around the call is larger.
#[test]
fn a_rebasing_token_attributes_no_drift() {
    const REBASE: &str = "rebase.near";
    let mut sim = Sim::new();
    sim.create_account("alice.near", near(10));
    sim.deploy(REBASE, rebasing_wasm());
    sim.watch(Token::view(REBASE, "balance_of"));
    let bal = |sim: &Sim| u128_view(sim, REBASE, "balance_of", json!({"account_id": "alice.near"}));
    for _ in 0..2 {
        sim.call("alice.near", REBASE, "bump").transact().assert_success();
    }
    let before = bal(&sim);
    let out = sim.call("alice.near", REBASE, "bump").transact();
    out.assert_success();
    let r = out.expect_receipt("bump");
    let rate = (r.block_timestamp_ns / 1_000_000_000) as i128;
    assert_eq!(out.effects().token_net(REBASE, "alice.near"), Ok(Delta::from(rate)));
    let naive = bal(&sim) as i128 - before as i128;
    assert!(naive > rate, "the drift is in the naive diff: {naive} vs {rate}");
}

/// A move of all of `u128::MAX` is exact: an account at 0 that receives
/// `u128::MAX` gains `+u128::MAX`, where an `i128` difference wrapped to
/// -1 (and the sender's loss to +1), a false finding.
#[test]
fn a_move_of_u128_max_is_exact() {
    let max = u128::MAX.to_string();
    let mut sim = Fixture::new()
        .account("alice.near", Balance::from_near(10))
        .contract(TOKEN, res("fungible_token.wasm"))
        .init("new_default_meta")
        .args_json(json!({ "owner_id": TOKEN, "total_supply": max }))
        .ft_register(TOKEN, ["alice.near"])
        .token(Token::nep141(TOKEN).supply("ft_total_supply"))
        .build();
    let out = sim
        .call(TOKEN, TOKEN, "ft_transfer")
        .args_json(json!({ "receiver_id": "alice.near", "amount": max }))
        .deposit(Balance::from_yoctonear(1))
        .transact();
    out.assert_success();
    let mut moves: Vec<_> = (out.expect_receipt("ft_transfer").token_moves.iter())
        .map(|m| (m.holder.as_ref().map(ToString::to_string), m.delta))
        .collect();
    moves.sort();
    let (alice, token) = (Some("alice.near".to_string()), Some(TOKEN.to_string()));
    assert_eq!(moves, [(alice, Delta::gain(u128::MAX)), (token, Delta::loss(u128::MAX))]);
    let e = out.effects();
    assert_eq!(e.token_net(TOKEN, "alice.near"), Ok(Delta::gain(u128::MAX)));
    assert_eq!(e.token_net(TOKEN, TOKEN), Ok(Delta::loss(u128::MAX)));
    assert_eq!(e.token_supply_net(TOKEN), Ok(Delta::ZERO));
    assert_eq!(i128::try_from(Delta::gain(u128::MAX)), Err(Delta::gain(u128::MAX)));
    assert_eq!(Delta::gain(u128::MAX).to_string(), max);
    assert_eq!(serde_json::to_string(&Delta::loss(5)).unwrap(), "\"-5\"");
    assert!(Delta::loss(u128::MAX) < Delta::from(i128::MIN) && Delta::loss(1) < 0);
    assert_eq!(Delta::gain(u128::MAX).checked_add(Delta::gain(1)), None);
}

// ---- ids minted mid-case, discovered from NEP-245 events ----

const MT: &str = "mt.near";
const ISSUER: &str = "issuer.near";

/// `tests/res/mtoken_events.wasm` (the keeper specimen's mtoken with the
/// `events` feature): `create` mints a new id to the owner with an
/// `mt_mint` event; `give` moves it with `mt_transfer`; `mt_balance_of`
/// and `mt_supply` panic for an id never created.
fn mt_world(watch: Option<Token>) -> Sim {
    let mut f = Fixture::new()
        .account(ISSUER, Balance::from_near(100))
        .account("alice.near", Balance::from_near(100))
        .contract(MT, res("mtoken_events.wasm"))
        .init("new")
        .args_json(json!({ "owner": ISSUER, "ids": [] }));
    if let Some(w) = watch {
        f = f.token(w);
    }
    f.build()
}

/// Mints `id` and gives alice 250 of it, then gives 5 to `ghost.near`, an
/// id that isn't an account: one transaction each.
fn mint_and_give(sim: &mut Sim, id: &str) -> Vec<TxOutcome> {
    let mut out = vec![sim.call(ISSUER, MT, "create").args_json(json!({ "id": id })).transact()];
    let give = json!({ "to": "alice.near", "id": id, "amount": "250" });
    out.push(sim.call(ISSUER, MT, "give").args_json(give).transact());
    let ghost = json!({ "to": "ghost.near", "id": id, "amount": "5" });
    out.push(sim.call("alice.near", MT, "give").args_json(ghost).transact());
    for o in &out {
        o.assert_success();
    }
    out
}

/// With `discover()`, an id minted mid-case is watched from its mint: the
/// mint's own receipt reads its pre-state as 0 (its `mt_mint` first names
/// it), alice's net is what she was given, and `ghost.near`, named by an
/// `mt_transfer` event but no account, is watched too (supply and
/// holders add up). Without it the id is `Unobserved`, naming it. A watch
/// changes no outcome.
///
/// Verification boundary: the watch's per-receipt measurement, read-only
/// inside block production, over actual runs of the specimen.
#[test]
fn a_discovering_watch_measures_ids_minted_mid_case() {
    let discover = Token::nep245_ids(MT, Vec::<String>::new()).discover();
    let mut watched = mt_world(Some(discover));
    let out = mint_and_give(&mut watched, "X");
    let mint = out[0].effects();
    assert_eq!(mint.token_net((MT, "X"), ISSUER), Ok(Delta::from(1_000_000_000)));
    assert_eq!(mint.token_supply_net((MT, "X")), Ok(Delta::from(1_000_000_000)));
    assert_eq!(out[1].effects().token_net((MT, "X"), "alice.near"), Ok(Delta::from(250)));
    let ghost = out[2].effects();
    assert_eq!(ghost.token_net((MT, "X"), "ghost.near"), Ok(Delta::from(5)));
    assert_eq!(ghost.token_net((MT, "X"), "alice.near"), Ok(Delta::from(-5)));

    let mut plain = mt_world(Some(Token::nep245_ids(MT, ["Y"])));
    let out = mint_and_give(&mut plain, "X");
    let err = out[1].effects().token_net((MT, "X"), "alice.near").unwrap_err();
    assert!(err.why.contains("X"), "{err}");

    let mut none = mt_world(None);
    let bare = mint_and_give(&mut none, "X");
    let key = |o: &TxOutcome| {
        let r: Vec<_> =
            o.receipts.iter().map(|r| (r.receipt_id, r.gas_burnt, r.logs.clone())).collect();
        (format!("{:?}", o.status), r)
    };
    let with = mint_and_give(&mut mt_world(Some(Token::nep245_ids(MT, ["Y"]).discover())), "X");
    assert_eq!(bare.iter().map(key).collect::<Vec<_>>(), with.iter().map(key).collect::<Vec<_>>());
    assert_eq!(none.state_root(), plain.state_root());
}

/// Without the mint in the same receipt, a failed view before is never a
/// silent 0: `Y`, made at init (no event, and no supply record, so its
/// views panic), is discovered from an `mt_transfer`; that receipt's
/// moves of `Y` are `Unobserved`, naming it.
#[test]
fn a_discovered_id_not_minted_there_is_unobserved_if_its_views_fail() {
    let mut sim = Fixture::new()
        .account(ISSUER, Balance::from_near(100))
        .contract(MT, res("mtoken_events.wasm"))
        .init("new")
        .args_json(json!({ "owner": ISSUER, "ids": ["Y"] }))
        .token(Token::nep245_ids(MT, Vec::<String>::new()).discover())
        .build();
    let give = json!({ "to": "carol.near", "id": "Y", "amount": "7" });
    let out = sim.call(ISSUER, MT, "give").args_json(give).transact();
    out.assert_success();
    let err = out.effects().token_net((MT, "Y"), "carol.near").unwrap_err();
    assert!(err.why.contains("Y") && err.why.contains("no such token"), "{err}");
}

/// `ft_transfer_call` to a standard NEP-141 receiver
/// (`tests/res/ft_receiver.wat`): the token refunds what the receiver
/// returns as unused, all, part or none of it, and all of it when the
/// receiver panics.
#[test]
fn ft_transfer_call_keeps_or_refunds() {
    let receiver = format!("{}/tests/res/ft_receiver.wat", env!("CARGO_MANIFEST_DIR"));
    let mut sim = Fixture::new()
        .account("alice.near", near(10))
        .contract("token.near", ft_wasm())
        .init("new_default_meta")
        .args_json(json!({ "owner_id": "alice.near", "total_supply": "1000" }))
        .contract("receiver.near", wat::parse_file(receiver).unwrap())
        .ft_register("token.near", ["receiver.near"])
        .token(Token::nep141("token.near"))
        .build();
    let send = |sim: &mut Sim, msg: &str| {
        sim.call("alice.near", "token.near", "ft_transfer_call")
            .args_json(json!({ "receiver_id": "receiver.near", "amount": "100", "msg": msg }))
            .deposit(Balance::from_yoctonear(1))
            .transact()
    };
    let net = |out: &TxOutcome, who: &str| out.effects().token_net("token.near", who);
    let kept = send(&mut sim, "take-my-money");
    assert_eq!(kept.u128(), 100, "{kept}"); // what the receiver used
    assert_eq!(net(&kept, "receiver.near"), Ok(Delta::gain(100)));

    // A number is what the receiver didn't use: it keeps the rest.
    let partial = send(&mut sim, "40");
    assert_eq!(partial.u128(), 60, "{partial}");
    assert_eq!(net(&partial, "alice.near"), Ok(Delta::loss(60)), "{partial}");
    assert_eq!(net(&partial, "receiver.near"), Ok(Delta::gain(60)), "{partial}");

    // All of it back: returned as unused, or the receiver panicked.
    for msg in ["100", "no such msg"] {
        let refunded = send(&mut sim, msg);
        assert_eq!(refunded.u128(), 0, "{refunded}");
        assert_eq!(net(&refunded, "alice.near"), Ok(Delta::ZERO), "{refunded}");
        assert_eq!(net(&refunded, "receiver.near"), Ok(Delta::ZERO), "{refunded}");
        if msg == "no such msg" {
            let receiver = refunded.expect_receipt("ft_on_transfer").failure().unwrap();
            assert_eq!(receiver.message(), Some("Not an integer"), "{refunded}");
        }
    }
    let balance = json!({ "account_id": "receiver.near" });
    assert_eq!(u128_view(&sim, "token.near", "ft_balance_of", balance), 160);
    sim.findings().assert_none();
}
