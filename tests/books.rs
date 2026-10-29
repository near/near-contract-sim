//! `Books`: declare what a contract owes and holds; the checks follow.
//! Each regression runs a real specimen both ways, buggy and fixed (the
//! vault's are in `vault_books.rs`):
//!
//! - `refund_ledger.wat` over-refunds a hostile receiver: its credit falls
//!   in an op it didn't sign ("falls only for its holder"); the fixed
//!   ledger is clean. A scan finds it by default, the receiver keeping
//!   what a setup copy of the step sent, then answering twice that;
//! - `forwarder.wat` keeps a failed forward's NEAR (stranded); the fixed
//!   one refunds it. `keeps_no_near` fires on it, not on 1-yocto
//!   deposits; a tolerance is one asset's; a kept amount grown after a
//!   small first one shows its last value, and one naming nothing panics;
//! - `unwrapper.wat` attaches a 1-yocto proof to a wrap.near that pays
//!   `amount + 1` back: proofs are exempt both ways, per call, on the
//!   relay (`keeps_no_near`) and on the wrapper (backing);
//! - `near_wrapper.wat`: wrapping and unwrapping keeps the books, the
//!   liability read from the wrapper's own ledger; a deposit attached to a
//!   failing call comes back, so nothing is kept;
//! - a send relayed as a meta transaction debits the holder who signed
//!   it: `Books::authorized` names her, so "falls only for its holder"
//!   holds, and still fires on the over-refund in the same plan;
//! - a price at 24-decimal amounts falls and fires (u128 products would
//!   saturate); a price-only asset has no kept check; a stated supply
//!   off its holders' sum fires;
//! - `staking_pool.wat`: an unstaked claim is redeemed after its 4-epoch
//!   cooldown by liveness; a stuck one (its amount patched past what the
//!   pool has) fails it; one maturing past what liveness advances is not
//!   judged; `Books::near` counts its staked
//!   balance, so it backs what it was deposited.
//!
//! - `storage_ledger.wat` prepays storage from a ledger, as Omni Bridge
//!   does: booked as free NEAR (less its storage stake), a ledger debit
//!   that became bytes isn't kept, its own bytes staked from the pool are
//!   unbacked, and NEAR freed with its own bytes is kept.
//! - an inline pot that pays its caller, then clears in a callback: two
//!   pays in one block pay alice twice. `watch_near` reads her NEAR for
//!   `SERIALIZABLE`, which finds it, and derives no kept check.
//!
//! Verification boundary: the derived checks judge actual runs; liveness
//! plays redeem plans on a clone of the case's end state after advancing
//! epochs, which is scheduling, carried by the staking-pool regressions.

mod common;

use common::manifest_dir;
use near_contract_sim::explore::{
    Audit, Books, Call, Claim, Fail, Faults, Gap, Harness, LIVENESS_EPOCHS, Op, Plan, Report, Row,
    Shrink, Skip, Verdict, plans,
};
use near_contract_sim::{Balance, Fixture, KeyType, Reply, SecretKey, Sim, Stub};
use proptest::prelude::*;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::{BTreeMap, BTreeSet};

fn wat(name: &str) -> String {
    std::fs::read_to_string(manifest_dir().join("tests/res").join(name)).unwrap()
}

// ---- the refund ledger ----

const LEDGER: &str = "ledger.near";
const RECEIVER: &str = "receiver.near";
const HOLDERS: [&str; 3] = ["alice.near", "bob.near", RECEIVER];

fn ledger_world(fixed: bool) -> Sim {
    ledger_holding(fixed, 1_000)
}

/// The ledger with the receiver holding `held` credit (none: unfunded).
fn ledger_holding(fixed: bool, held: u64) -> Sim {
    let cap = "(if (i64.gt_u (local.get $take) (local.get $amount)) \
               (then (local.set $take (local.get $amount))))";
    let text = wat("refund_ledger.wat");
    let text = if fixed { text.replace(";; {cap}", cap) } else { text };
    let fund = |who: &'static str, n: u64| {
        move |sim: &mut Sim| {
            sim.call(who, LEDGER, "fund").args(n.to_le_bytes()).transact().assert_success();
        }
    };
    let f = Fixture::new()
        .account("alice.near", Balance::from_near(100))
        .contract(LEDGER, ::wat::parse_str(&text).unwrap())
        .stub(RECEIVER, Stub::new().on("on_receive", Reply::json("0")))
        .setup("fund alice", fund("alice.near", 1_000));
    if held == 0 { f } else { f.setup("fund the receiver", fund(RECEIVER, held)) }.build()
}

fn credit(sim: &Sim, who: &str) -> u128 {
    let bytes = sim.view_raw(LEDGER, "credit_of", who.as_bytes().to_vec()).result.unwrap();
    u128::from(u64::from_le_bytes(bytes.try_into().unwrap()))
}

/// alice sends `amount` to the receiver, which asks `refund` back.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct Send {
    amount: u64,
    refund: u64,
}

impl Op for Send {
    fn submit(&self, sim: &mut Sim) -> Result<(), Skip> {
        if let Ok(n) = std::env::var("BOOKS_ABORT_AFTER") {
            let at = SUBMITS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if at >= n.parse().unwrap() {
                std::process::abort();
            }
        }
        let args = [&self.amount.to_le_bytes()[..], RECEIVER.as_bytes()].concat();
        let reply = Reply::json(self.refund.to_string());
        sim.call("alice.near", LEDGER, "send")
            .args(args)
            .stub_reply(RECEIVER, "on_receive", reply)
            .submit();
        Ok(())
    }
}

#[test]
fn an_over_refund_takes_credit_its_holder_never_spent() {
    const OWNED: &str = "credit falls only for its holder";
    let harness = |fixed| {
        let books = Books::new()
            .liability("credit", |sim: &Sim| {
                HOLDERS.iter().map(|h| (h.to_string(), credit(sim, h))).collect()
            })
            .owned("credit");
        Harness::new(ledger_world(fixed)).books(books)
    };
    let over = Plan::new().op(Send { amount: 10, refund: 50 });
    let run = harness(false).run(&over);
    let f = run.assert_found(OWNED, "receiver.near");
    assert!(f.message.contains("receiver.near's credit fell from 1000 to 960"), "{run}");
    harness(true).run(&over).assert_clean();
    // From defaults: a search finds it, and nothing on the fixed ledger.
    let sends = (1u64..=100, 0u64..=200).prop_map(|(amount, refund)| Send { amount, refund });
    harness(false)
        .search(plans(sends.clone()))
        .seed(1)
        .cases(32)
        .run()
        .assert_found(OWNED, "receiver.near");
    harness(true).search(plans(sends)).seed(1).cases(32).run().assert_clean();
}

/// A scan from a witness whose receiver asks nothing back tries, by
/// default, the receiver keeping what it was sent in a setup copy of the
/// step, then answering twice what it was sent (its call carries
/// `{"amount": ..}`): the books fire there, shown, over the op's own
/// reply, whether or not the fixture funded the receiver; the fixed
/// ledger caps the refund and stays clean.
#[test]
fn a_scan_tries_a_stub_answering_more_than_it_was_sent() {
    const OWNED: &str = "credit falls only for its holder";
    let harness = |fixed, held| {
        let books = Books::new()
            .liability("credit", |sim: &Sim| {
                HOLDERS.iter().map(|h| (h.to_string(), credit(sim, h))).collect()
            })
            .owned("credit");
        Harness::new(ledger_holding(fixed, held)).books(books)
    };
    let witness = Plan::new().op(Send { amount: 10, refund: 0 });
    let at = "step 0, receiver.near::on_receive answers \"20\", twice what was sent, after a \
              setup copy it kept";
    for (held, fell) in [(1_000, "fell from 1010 to 1000"), (0, "fell from 10 to 0")] {
        let audit = harness(false, held).scan(std::slice::from_ref(&witness)).run();
        let f = audit.found().iter().find(|f| f.finding.check == OWNED);
        let f = f.unwrap_or_else(|| panic!("{audit}"));
        assert_eq!(f.label, near_contract_sim::explore::Label::ShownAt(at.into()), "{audit}");
        assert!(f.finding.message.contains(fell), "{}", f.finding.message);
        // It replays from its plan alone, saved and read back.
        let saved = serde_json::to_string(f.run.plan()).unwrap();
        assert!(saved.contains("\"setup\"") && saved.contains("\"answers\""), "{saved}");
        let back: Plan<Send> = serde_json::from_str(&saved).unwrap();
        harness(false, held).run(&back).assert_found(OWNED, "receiver.near");
        let fixed = harness(true, held).scan(std::slice::from_ref(&witness)).run();
        assert!(!fixed.found().iter().any(|f| f.finding.check == OWNED), "{fixed}");
    }
}

/// A send alice signs, or one she signs as a meta transaction that
/// `relayer.near` submits: the ledger sees alice either way, the
/// transaction comes from the relayer.
#[derive(Clone, Debug, Serialize, Deserialize)]
enum Ledger {
    Send { amount: u64, refund: u64 },
    Relayed { amount: u64 },
}

impl Op for Ledger {
    fn submit(&self, sim: &mut Sim) -> Result<(), Skip> {
        match *self {
            Ledger::Send { amount, refund } => Send { amount, refund }.submit(sim)?,
            Ledger::Relayed { amount } => {
                let args = [&amount.to_le_bytes()[..], RECEIVER.as_bytes()].concat();
                let h = sim.height();
                let signed =
                    sim.call("alice.near", LEDGER, "send").args(args).sign_delegate(h + 100);
                sim.tx(RELAYER, "alice.near").action(signed).submit();
            }
        }
        Ok(())
    }
}

const RELAYER: &str = "relayer.near";

/// `owned` with who authorized each op: a relayed send debits alice, who
/// signed it, so with the hook naming her it is no finding (without it,
/// the relayer looks like the spender); the over-refund in the same plan
/// still takes the receiver's credit, which nobody authorized.
#[test]
fn owned_follows_who_authorized_a_relayed_op() {
    const OWNED: &str = "credit falls only for its holder";
    let harness = |hook: bool| {
        let mut sim = ledger_world(false);
        sim.create_account(RELAYER, Balance::from_near(10));
        let mut books = Books::new()
            .liability("credit", |sim: &Sim| {
                HOLDERS.iter().map(|h| (h.to_string(), credit(sim, h))).collect()
            })
            .owned("credit");
        if hook {
            books = books.authorized(|d| match d.op() {
                Ledger::Relayed { .. } => Some(vec!["alice.near".into()]),
                Ledger::Send { .. } => None,
            });
        }
        Harness::new(sim).books(books)
    };
    let relayed = Plan::new().op(Ledger::Relayed { amount: 10 });
    harness(false).run(&relayed).assert_found(OWNED, "alice.near");
    harness(true).run(&relayed).assert_clean();
    let both = relayed.op(Ledger::Send { amount: 10, refund: 50 });
    let run = harness(true).run(&both);
    run.assert_found(OWNED, "receiver.near");
    assert!(run.found_at(OWNED, "alice.near").is_none(), "{run}");
}

/// alice pays bob out of the pool's NEAR, or bob pays the pool: shares are
/// alice's balance (about 1e26 yocto), the asset the pool's (1e26), so the
/// price's products pass u128. Paying out lowers the price, and the check
/// fires at that scale; paying in doesn't. A price-only asset has no
/// "kept" check, and a stated supply must equal its holders' sum.
#[derive(Clone, Debug, Serialize, Deserialize)]
enum Pool {
    Out { near: u64 },
    In { near: u64 },
}

impl Op for Pool {
    fn submit(&self, sim: &mut Sim) -> Result<(), Skip> {
        let (from, to, n) = match *self {
            Pool::Out { near } => ("pool.near", "bob.near", near),
            Pool::In { near } => ("bob.near", "pool.near", near),
        };
        sim.tx(from, to).transfer(Balance::from_near(n.into())).submit();
        Ok(())
    }
}

#[test]
fn price_fires_at_24_decimal_amounts_and_supply_is_the_holders_sum() {
    let near = |sim: &Sim, who: &str| sim.balance(who).as_yoctonear();
    let mut sim = Sim::new();
    for who in ["alice.near", "bob.near", "pool.near"] {
        sim.create_account(who, Balance::from_near(500));
    }
    let books = |stated: u128| {
        Books::new()
            .liability("shares", move |sim: &Sim| {
                BTreeMap::from([("alice.near".to_string(), near(sim, "alice.near"))])
            })
            .asset("pool", move |sim: &Sim| near(sim, "pool.near"))
            .price("shares", "pool")
            .supply("shares", move |sim: &Sim| near(sim, "alice.near") + stated)
    };
    let h = Harness::new(sim.clone()).books(books(0));
    let out = h.run(&Plan::new().op(Pool::Out { near: 100 }));
    let f = out.assert_found("shares's price never falls", "world");
    assert!(f.message.contains("per 5"), "{out}");
    h.run(&Plan::new().op(Pool::In { near: 100 })).assert_clean();
    let names: Vec<&str> = out.judged().iter().map(|j| j.check.as_str()).collect();
    assert!(!names.iter().any(|n| n.contains("kept")), "{names:?}");
    // Off on the fixture itself: the harness's error, not a finding.
    let off = Harness::new(sim).books(books(1)).run(&Plan::new().op(Pool::In { near: 1 }));
    let why = off.reason().unwrap_or_default();
    let fails = "\"shares's supply is the sum of its holders\" fails on the fixture";
    assert!(off.verdict() == Verdict::HarnessError && why.contains(fails), "{off}");
}

// ---- a share pool that starts empty ----

const SHARE_POOL: &str = "shares.near";
const SHARES_PRICE: &str = "shares's price never falls";
const SHARES_OWNED: &str = "shares falls only for its holder";

/// A share pool: its shares per holder in its state, its asset its NEAR
/// (10 NEAR stray at the start, no shares).
#[derive(Clone, Debug, Serialize, Deserialize)]
enum Shares {
    /// `who` pays `near` NEAR in and is minted `shares`.
    Mint { who: String, shares: u64, near: u64 },
    /// The pool pays bob `near` NEAR, burning nothing.
    Pay { near: u64 },
    /// `who`'s shares are taken, by no transaction of theirs.
    Take { who: String },
    /// `who` is given `shares`, by no transaction of theirs.
    Give { who: String, shares: u64 },
    /// `who`'s shares are taken, and bob pays the pool 1 NEAR: bob signs.
    Steal { who: String },
    /// `who` pays the pool 1 NEAR, its shares unchanged: a registration.
    Register { who: String },
}

fn shares_of(sim: &Sim, who: &str) -> u128 {
    sim.state(SHARE_POOL, who.as_bytes()).map_or(0, |v| u128::from_le_bytes(v.try_into().unwrap()))
}

impl Op for Shares {
    fn submit(&self, sim: &mut Sim) -> Result<(), Skip> {
        match self {
            Shares::Mint { who, shares, near } => {
                let now = (shares_of(sim, who) + u128::from(*shares)).to_le_bytes();
                sim.set_state(SHARE_POOL, who.as_bytes(), Some(&now));
                sim.tx(who, SHARE_POOL).transfer(Balance::from_near((*near).into())).submit();
            }
            Shares::Pay { near } => {
                sim.tx(SHARE_POOL, "bob.near")
                    .transfer(Balance::from_near((*near).into()))
                    .submit();
            }
            Shares::Take { who } => sim.set_state(SHARE_POOL, who.as_bytes(), None),
            Shares::Steal { who } => {
                sim.set_state(SHARE_POOL, who.as_bytes(), None);
                sim.tx("bob.near", SHARE_POOL).transfer(Balance::from_near(1)).submit();
            }
            Shares::Register { who } => {
                sim.tx(who, SHARE_POOL).transfer(Balance::from_near(1)).submit();
            }
            Shares::Give { who, shares } => {
                let now = (shares_of(sim, who) + u128::from(*shares)).to_le_bytes();
                sim.set_state(SHARE_POOL, who.as_bytes(), Some(&now));
            }
        }
        Ok(())
    }
}

fn share_pool_world() -> Sim {
    Fixture::new()
        .account("alice.near", Balance::from_near(100))
        .account("bob.near", Balance::from_near(100))
        .account(SHARE_POOL, Balance::from_near(10))
        .build()
}

fn share_pool() -> Harness<Shares> {
    let world = share_pool_world();
    let books = Books::new()
        .liability("shares", |sim: &Sim| {
            ["alice.near", "bob.near"].iter().map(|h| (h.to_string(), shares_of(sim, h))).collect()
        })
        .asset("pool NEAR", |sim: &Sim| sim.balance(SHARE_POOL).as_yoctonear())
        .price("shares", "pool NEAR")
        .owned("shares");
    Harness::new(world).books(books)
}

/// With no shares at stake a price and an owned check are idle, and a
/// campaign where they never judge lists them as `unjudged:`.
/// Both passed: a price over zero shares, a fall of nothing.
#[test]
fn price_and_owned_are_idle_with_no_shares() {
    let h = share_pool();
    let plan = Plan::new().op(Shares::Pay { near: 1 });
    let run = h.run(&plan);
    for check in [SHARES_PRICE, SHARES_OWNED] {
        let j = run.judged().iter().find(|j| j.check == check).unwrap();
        assert!(j.idle > 0 && j.ok == 0 && j.failed == 0, "{j:?}\n{run}");
    }
    let report = h.scan(&[plan]).faults(Faults::none()).run().report();
    for check in [SHARES_PRICE, SHARES_OWNED] {
        let todo = report.todo.iter().find(|t| t.id == format!("unjudged:{check}"));
        assert!(todo.is_some_and(|t| t.why.contains("nothing at stake")), "{:?}", report.todo);
    }
}

/// On an empty pool, a first depositor's shares are judged from the
/// first reading that holds them: a price that falls after the
/// first mint fires, and so does a holder who appears and is drained by
/// an op she didn't sign: the mint that gave her them is not consent.
/// Both passed: the case start had no shares and no holder. Mints that
/// keep the price are clean.
#[test]
fn a_first_depositor_is_judged_from_her_first_reading() {
    let h = share_pool();
    let mint = |who: &str, shares, near| Shares::Mint { who: who.into(), shares, near };
    let paid = h.run(&Plan::new().op(mint("alice.near", 100, 10)).op(Shares::Pay { near: 5 }));
    let f = paid.assert_found(SHARES_PRICE, "world");
    assert!(f.message.contains("per 100 shares fell to"), "{paid}");
    let taken =
        Plan::new().op(mint("alice.near", 100, 10)).op(Shares::Take { who: "alice.near".into() });
    let run = h.run(&taken);
    let f = run.assert_found(SHARES_OWNED, "alice.near");
    let why = "alice.near's shares fell from 100 to 0; it signed no op but those that raised it";
    assert!(f.message.contains(why), "{run}");
    h.run(&Plan::new().op(mint("alice.near", 100, 10)).op(mint("bob.near", 50, 10))).assert_clean();
}

/// Any op a holder signed but those that raised her amount covers a
/// later fall: an approve (a mint of nothing) then a pull, her
/// amount unchanged, risen by her own mint, or risen by a gift she didn't
/// sign; an approve while she held nothing, then a gift, then a pull; and
/// an approve, then a call in flight as the pull lands. Judged from her
/// highest reading on, only her ops from there covered her, and a rise
/// between the approve and the pull fired.
#[test]
fn an_op_she_signed_covers_a_later_fall() {
    let h = share_pool();
    let mint = |who: &str, shares, near| Shares::Mint { who: who.into(), shares, near };
    let (approve, pull) = (mint("alice.near", 0, 1), Shares::Take { who: "alice.near".into() });
    let give = Shares::Give { who: "alice.near".into(), shares: 50 };
    let deposit = mint("alice.near", 100, 10);
    let plans = [
        vec![deposit.clone(), approve.clone(), mint("bob.near", 50, 10), pull.clone()],
        vec![deposit.clone(), approve.clone(), mint("alice.near", 50, 10), pull.clone()],
        vec![deposit.clone(), approve.clone(), give.clone(), pull.clone()],
        vec![approve.clone(), give.clone(), pull.clone()],
    ];
    for ops in plans {
        let run = h.run(&ops.iter().fold(Plan::new(), |p, o| p.op(o.clone())));
        assert!(run.findings().iter().all(|f| f.check != SHARES_OWNED), "{run}");
    }
    let overlapped =
        Plan::new().op(deposit).op(approve).op(mint("bob.near", 50, 10)).blocks(0).op(pull);
    let run = h.run(&overlapped);
    assert!(run.findings().iter().all(|f| f.check != SHARES_OWNED), "{run}");
}

/// No op that raised a holder's amount authorizes its fall: a
/// first deposit, then a top-up, then a drain fires, and so does a drain
/// whose own call another op lands on before it completes, read as it
/// submitted at rest. An op that changed nothing is consent, so one who
/// registered, deposited and was drained is missed unless `authorized`
/// says a registration authorizes no one.
#[test]
fn ops_that_raised_her_amount_dont_cover_its_fall() {
    let h = share_pool();
    let mint = |who: &str, shares, near| Shares::Mint { who: who.into(), shares, near };
    let (deposit, take) = (mint("alice.near", 100, 10), Shares::Take { who: "alice.near".into() });
    let topped = Plan::new().op(deposit.clone()).op(mint("alice.near", 50, 5)).op(take.clone());
    let run = h.run(&topped);
    let f = run.assert_found(SHARES_OWNED, "alice.near");
    assert!(f.message.contains("fell from 150 to 0; it signed no op but those"), "{run}");
    let stolen = Plan::new()
        .op(deposit.clone())
        .op(Shares::Steal { who: "alice.near".into() })
        .blocks(0)
        .op(mint("bob.near", 0, 1));
    h.run(&stolen).assert_found(SHARES_OWNED, "alice.near");
    let registered =
        Plan::new().op(Shares::Register { who: "alice.near".into() }).op(deposit).op(take);
    let run = h.run(&registered);
    assert!(run.findings().iter().all(|f| f.check != SHARES_OWNED), "a known miss: {run}");
    let books = Books::new()
        .liability("shares", |sim: &Sim| {
            ["alice.near", "bob.near"].iter().map(|h| (h.to_string(), shares_of(sim, h))).collect()
        })
        .asset("pool NEAR", |sim: &Sim| sim.balance(SHARE_POOL).as_yoctonear())
        .owned("shares")
        .authorized(|d: &near_contract_sim::explore::OpDone<Shares>| match d.op() {
            Shares::Register { .. } => Some(vec![]),
            _ => None,
        });
    let strict = Harness::new(share_pool_world()).books(books);
    strict.run(&registered).assert_found(SHARES_OWNED, "alice.near");
}

// ---- NEAR stranded ----

const FWD: &str = "fwd.near";
const WRAP: &str = "wrap.near";

fn forwarder(refunds: bool) -> Sim {
    let text = wat("forwarder.wat");
    let flag = "(global $refunds i32 (i32.const 0)) ;; REFUNDS";
    let text =
        if refunds { text.replace(flag, "(global $refunds i32 (i32.const 1))") } else { text };
    Fixture::new()
        .account("alice.near", Balance::from_near(100))
        .account("sink.near", Balance::from_near(1))
        .contract(FWD, ::wat::parse_str(text).unwrap())
        .build()
}

#[test]
fn wrapped_near_is_backed_and_a_refunded_deposit_keeps_nothing() {
    let sim = Fixture::new()
        .account("alice.near", Balance::from_near(100))
        .contract(WRAP, ::wat::parse_str(wat("near_wrapper.wat")).unwrap())
        .build();
    let books = Books::<Call>::new()
        .liability("wNEAR", |sim: &Sim| {
            let b = sim.view(WRAP, "ft_balance_of", json!({ "account_id": "alice.near" }));
            BTreeMap::from([("alice.near".into(), b.u128())])
        })
        .near("wrap NEAR", WRAP)
        .backs("wNEAR", "wrap NEAR")
        .owned("wNEAR");
    let harness = Harness::new(sim).books(books);
    let amount: u64 = 1_000_000_000_000_000_000;
    let wrap = Call::new("alice.near", WRAP, "near_deposit")
        .deposit(Balance::from_yoctonear(amount.into()));
    let unwrap = Call::new("alice.near", WRAP, "near_withdraw").args(amount.to_le_bytes().to_vec());
    harness.run(&Plan::new().op(wrap.clone()).op(unwrap)).assert_clean();
    let sent = Plan::new()
        .call(wrap)
        .call(Call::new("alice.near", WRAP, "nope").deposit(Balance::from_near(1)));
    let run = harness.run(&sent);
    assert!(
        run.findings().iter().all(|f| f.check != "wrap NEAR is kept, owed to no one"),
        "a failed call refunds: {run}"
    );
}

// ---- liveness: the staking pool ----

const POOL: &str = "pool.near";

fn pool(stuck: bool) -> Sim {
    let mut sim = Fixture::new()
        .account("alice.near", Balance::from_near(1000))
        .account(POOL, Balance::from_near(2))
        .build();
    sim.config.epoch_length = 10;
    sim.deploy(POOL, ::wat::parse_str(wat("staking_pool.wat")).unwrap());
    let key = SecretKey::from_seed(KeyType::ED25519, POOL).public_key();
    sim.call(POOL, POOL, "new").args(borsh::to_vec(&key).unwrap()).transact().assert_success();
    let stake = sim.call("alice.near", POOL, "deposit_and_stake").deposit(Balance::from_near(100));
    stake.transact().assert_success();
    if stuck {
        // More than the pool will ever have: a claim that can't be paid.
        let key = [&b"u"[..], b"alice.near"].concat();
        sim.set_state(POOL, &key, Some(&Balance::from_near(5000).as_yoctonear().to_le_bytes()));
    }
    sim
}

fn number(v: Vec<u8>) -> u128 {
    v.iter().rev().fold(0u128, |n, b| n << 8 | u128::from(*b))
}

/// alice's unstaked amount, once she has one, maturing at its unlock
/// epoch (plus `later` epochs).
fn unstaked(later: u64) -> impl Fn(&Sim) -> Vec<Claim> {
    move |sim: &Sim| {
        let read =
            |p: &[u8]| number(sim.state(POOL, &[p, b"alice.near"].concat()).unwrap_or_default());
        let amount = read(b"u");
        let matures = Some(read(b"e") as u64 + later);
        (amount > 0)
            .then(|| Claim { holder: "alice.near".into(), amount, matures })
            .into_iter()
            .collect()
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
enum PoolOp {
    Stake { yocto: u128 },
    Unstake,
    Withdraw,
}

impl Op for PoolOp {
    fn submit(&self, sim: &mut Sim) -> Result<(), Skip> {
        let (method, deposit) = match self {
            PoolOp::Stake { yocto } => ("deposit_and_stake", *yocto),
            PoolOp::Unstake => ("unstake", 0),
            PoolOp::Withdraw => ("withdraw", 0),
        };
        sim.call("alice.near", POOL, method).deposit(Balance::from_yoctonear(deposit)).submit();
        Ok(())
    }
}

fn pool_harness(stuck: bool, later: u64) -> Harness<PoolOp> {
    let books = Books::new().near("pool NEAR", POOL).claim(
        "unstaked NEAR",
        "pool NEAR",
        unstaked(later),
        |_| Plan::new().op(PoolOp::Withdraw),
    );
    Harness::new(pool(stuck)).books(books)
}

#[test]
fn an_unstaked_claim_is_redeemed_after_its_cooldown() {
    const LIVE: &str = "unstaked NEAR is redeemable";
    let unstake = Plan::new().op(PoolOp::Unstake);
    let run = pool_harness(false, 0).run(&unstake);
    run.assert_clean();
    assert!(run.judged().iter().any(|j| j.check == LIVE && j.ok == 1), "{run}");
    // Stuck: the claim exceeds what the pool can pay.
    let stuck = pool_harness(true, 0).run(&unstake);
    let f = stuck.assert_found(LIVE, "alice.near");
    assert!(f.message.contains("alice.near's claim of"), "{stuck}");
    // Maturing past what liveness advances: not judged, never a pass.
    let far = pool_harness(false, LIVENESS_EPOCHS + 1).run(&unstake);
    assert_eq!(far.verdict(), Verdict::Incomplete, "{far}");
    assert!(far.judged().iter().any(|j| j.check == LIVE && j.unjudged == 1 && j.ok == 0), "{far}");
}

/// `consume` logs the nonce it is sent, then refuses one already used:
/// the refused receipt rolls back but keeps its log. A `once` key read
/// from `TxOutcome::logs()` sees only receipts that succeeded, so a
/// nonce refused the second time is consumed once, not twice.
#[test]
fn a_once_key_from_logs_skips_a_rolled_back_receipt() {
    const WAT: &str = r#"(module
      (import "env" "input" (func $input (param i64)))
      (import "env" "register_len" (func $len (param i64) (result i64)))
      (import "env" "read_register" (func $read (param i64 i64)))
      (import "env" "log_utf8" (func $log (param i64 i64)))
      (import "env" "storage_has_key" (func $has (param i64 i64) (result i64)))
      (import "env" "storage_write" (func $write (param i64 i64 i64 i64 i64) (result i64)))
      (import "env" "panic_utf8" (func $panic (param i64 i64)))
      (memory 1)
      (export "memory" (memory 0))
      (data (i32.const 0) "used")
      (func (export "consume") (local $n i64)
        (call $input (i64.const 0))
        (local.set $n (call $len (i64.const 0)))
        (call $read (i64.const 0) (i64.const 64))
        (call $log (local.get $n) (i64.const 64))
        (if (i64.eq (call $has (local.get $n) (i64.const 64)) (i64.const 1))
          (then (call $panic (i64.const 4) (i64.const 0))))
        (drop (call $write (local.get $n) (i64.const 64) (i64.const 4) (i64.const 0) (i64.const 1)))))"#;
    #[derive(Clone, Debug, Serialize, Deserialize)]
    struct Consume(String);
    impl Op for Consume {
        fn submit(&self, sim: &mut Sim) -> Result<(), Skip> {
            sim.call("alice.near", "nonces.near", "consume").args(self.0.clone()).submit();
            Ok(())
        }
    }
    let world = Fixture::new()
        .account("alice.near", Balance::from_near(10))
        .contract("nonces.near", wat::parse_str(WAT).unwrap())
        .build();
    let books = Books::new().once("nonce", |d| Some(d.outcome()?.logs().first()?.to_string()));
    let h = Harness::new(world).books(books);
    let twice = Plan::new().op(Consume("n1".into())).op(Consume("n1".into())).refused();
    let run = h.run(&twice);
    run.assert_clean();
    let refused = &run.outcomes()[1];
    assert!(refused.is_failure() && refused.logs().is_empty(), "{refused}");
    assert_eq!(refused.all_logs(), ["n1"], "{refused}");
}

/// A nonce box that takes a nonce twice. A curve's fresh callers re-send
/// alice's signed call as raw calls, each the op's copy: `once` judges
/// them as the op, its key read from the call's own receipt, so
/// the second consume fires. It filtered copies out, and passed.
#[test]
fn once_judges_the_copies_a_curve_re_sends() {
    const WAT: &str = r#"(module
      (import "env" "input" (func $input (param i64)))
      (import "env" "storage_write" (func $write (param i64 i64 i64 i64 i64) (result i64)))
      (memory 1)
      (export "memory" (memory 0))
      (func (export "consume")
        (call $input (i64.const 0))
        (drop (call $write (i64.const 0) (i64.const 0) (i64.const 0) (i64.const 0) (i64.const 1)))))"#;
    #[derive(Clone, Debug, Serialize, Deserialize)]
    struct Consume(String);
    impl Op for Consume {
        fn submit(&self, sim: &mut Sim) -> Result<(), Skip> {
            sim.call("alice.near", "nonces.near", "consume").args_json(json!(self.0)).submit();
            Ok(())
        }
    }
    let world = Fixture::new()
        .account("alice.near", Balance::from_near(10))
        .contract("nonces.near", wat::parse_str(WAT).unwrap())
        .build();
    let books = Books::new().once("nonce", |d| d.entries().first()?.args_json::<String>().ok());
    let h = Harness::new(world).books(books);
    let witness = Plan::new().op(Consume("n1".into()));
    h.run(&witness).assert_clean();
    let audit = h.curve(&witness, 0, "repeat fresh callers of nonces.near::consume").run();
    audit.assert_found("nonce at most once", "n1");
}

// ---- keeps no NEAR, per-asset tolerance, locked balance ----

const KEPT_BY_FWD: &str = "fwd.near NEAR is kept, owed to no one";

/// A call the bug build's `on_forward` accepts and does nothing with: the
/// deposit stays.
fn kept_noise(yocto: u128) -> Call {
    Call::new("alice.near", FWD, "on_forward").deposit(Balance::from_yoctonear(yocto))
}

fn forward_3() -> Call {
    Call::new("alice.near", FWD, "forward").args("sink.near").deposit(Balance::from_near(3))
}

/// Two kinds of op that forward to a missing account and keep the NEAR.
#[derive(Clone, Debug, Serialize, Deserialize)]
enum Fwd {
    Forward,
    Relay,
}

impl Op for Fwd {
    fn submit(&self, sim: &mut Sim) -> Result<(), Skip> {
        forward_3().submit(sim)
    }
}

/// One check, one site, one failure on the path, broken by two kinds of
/// op: two rows, each with its own replay, not one overwriting the other.
#[test]
fn causes_at_one_check_and_site_keep_their_own_replays() {
    let h = Harness::new(forwarder(false)).books(Books::new().keeps_no_near(FWD));
    let plans = [Plan::new().op(Fwd::Forward), Plan::new().op(Fwd::Relay)];
    let audit = h.scan(&plans).faults(Faults::none()).run();
    let rows: Vec<_> = audit.found().iter().filter(|f| f.finding.check == KEPT_BY_FWD).collect();
    let replays: std::collections::BTreeSet<_> =
        rows.iter().filter_map(|f| f.replay.clone()).collect();
    assert_eq!((rows.len(), replays.len()), (2, 2), "{audit}");
}

/// `keeps_no_near`: NEAR the forwarder keeps from a failed forward fires;
/// 1-yocto deposits it keeps don't, 2 yocto do.
#[test]
fn keeps_no_near_fires_on_what_arrives_but_one_yocto_deposits() {
    let harness = |refunds| Harness::new(forwarder(refunds)).books(Books::new().keeps_no_near(FWD));
    let run = harness(false).run(&Plan::new().op(forward_3()));
    let f = run.assert_found(KEPT_BY_FWD, "world");
    assert!(f.message.contains("3000000000000000000000000 kept"), "{run}");
    assert!(!f.message.contains("tells them apart"), "backs nothing: {run}");
    let fixed = harness(true).run(&Plan::new().op(forward_3()));
    assert!(fixed.findings().iter().all(|f| f.check != KEPT_BY_FWD), "{fixed}");
    let noise = harness(false).run(&Plan::new().op(kept_noise(1)).op(kept_noise(1)));
    assert!(noise.findings().iter().all(|f| f.check != KEPT_BY_FWD), "{noise}");
    let two = harness(false).run(&Plan::new().op(kept_noise(2)));
    assert!(two.assert_found(KEPT_BY_FWD, "world").message.contains("2 kept"), "{two}");
}

/// A tolerance is the named asset's alone; and a kept amount that grows
/// after a first, small one shows its last value too.
#[test]
fn tolerance_is_per_asset_and_a_late_kept_amount_shows() {
    let books = |tol: &str| {
        Books::new().keeps_no_near(FWD).near("alice NEAR", "alice.near").tolerance(tol, 2)
    };
    let noise = Plan::new().op(kept_noise(2));
    let run = Harness::new(forwarder(false)).books(books("fwd.near NEAR")).run(&noise);
    assert!(run.findings().iter().all(|f| f.check != KEPT_BY_FWD), "{run}");
    let other = Harness::new(forwarder(false)).books(books("alice NEAR")).run(&noise);
    other.assert_found(KEPT_BY_FWD, "world");
    let late = Plan::new().op(kept_noise(2)).op(forward_3());
    let run = Harness::new(forwarder(false)).books(books("alice NEAR")).run(&late);
    let f = run.assert_found(KEPT_BY_FWD, "world");
    assert!(f.message.contains(": 2 kept"), "{}", f.message);
    assert!(f.message.contains("last failed at block"), "{}", f.message);
    assert!(f.message.contains("3000000000000000000000002 kept"), "{}", f.message);
}

const RELAY: &str = "relay.near";

/// `unwrapper.wat` holding 1e19 yocto of wNEAR (the wrapper's balances are
/// u64s) at a wrap.near that pays `amount + 1`,
/// as the real one does; the relay keeping `keeps` yocto of each payout.
fn unwrapping(keeps: u64) -> Harness<Call> {
    let relay = wat("unwrapper.wat").replace(
        "(global $keeps i64 (i64.const 0))",
        &format!("(global $keeps i64 (i64.const {keeps}))"),
    );
    let wrap = wat("near_wrapper.wat").replace(
        "(i64.store (i32.const 600) (local.get $n))",
        "(i64.store (i32.const 600) (i64.add (local.get $n) (i64.const 1)))",
    );
    let sim = Fixture::new()
        .account("alice.near", Balance::from_near(100))
        .contract(WRAP, ::wat::parse_str(wrap).unwrap())
        .contract(RELAY, ::wat::parse_str(relay).unwrap())
        .call(RELAY, WRAP, "near_deposit")
        .deposit(Balance::from_yoctonear(10u128.pow(19)))
        .build();
    let books = Books::<Call>::new()
        .liability("wNEAR", |sim: &Sim| {
            let b = sim.view(WRAP, "ft_balance_of", json!({ "account_id": RELAY }));
            BTreeMap::from([(RELAY.into(), b.u128())])
        })
        .near("wrap NEAR", WRAP)
        .backs("wNEAR", "wrap NEAR")
        .keeps_no_near(RELAY);
    Harness::new(sim).books(books)
}

/// 1-yocto proofs are exempt both ways, per call: a relay that attaches
/// 1 yocto to wrap.near's `near_withdraw` and gets `amount + 1` back,
/// paying the amount on, keeps nothing (it read "1 kept" before), and the
/// wrapper paying the proof back with the amount still backs its wNEAR
/// ("1 unbacked" before); with alice's own 1-yocto proof attached too.
/// A relay that keeps 2 yocto of each payout, 3 with the proof, is past
/// what its 2 proofs allow.
#[test]
fn one_yocto_proofs_are_exempt_both_ways_per_call() {
    const KEPT: &str = "relay.near NEAR is kept, owed to no one";
    let amount: u64 = 2_000_000_000_000_000_000;
    let withdraw = |proof: u128| {
        Call::new("alice.near", RELAY, "withdraw")
            .args(amount.to_le_bytes().to_vec())
            .deposit(Balance::from_yoctonear(proof))
    };
    let h = unwrapping(0);
    for proof in [0, 1] {
        let run = h.run(&Plan::new().op(withdraw(proof)));
        run.assert_clean();
        let paid = run.outcomes()[0].effects().near_paid(RELAY, "alice.near");
        assert_eq!(paid.as_yoctonear(), u128::from(amount), "{run}");
    }
    let run = unwrapping(2).run(&Plan::new().op(withdraw(1)));
    let f = run.assert_found(KEPT, "world");
    assert!(f.message.contains(": 3 kept"), "{}", f.message);
    assert!(f.message.contains("2 yocto allowed for 2 1-yocto proof(s)"), "{}", f.message);
}

/// A tolerance naming no asset or liability is a typo: `books` panics.
#[test]
#[should_panic(expected = "tolerance(\"fwd NEAR\", ..) names no asset or liability")]
fn a_tolerance_naming_nothing_panics() {
    let books = Books::<Call>::new().keeps_no_near(FWD).tolerance("fwd NEAR", 1);
    let _ = Harness::new(forwarder(false)).books(books);
}

/// `Books::near` counts locked balance, and the epoch rewards paid into a
/// pool's stake: a liability that includes them (alice owns all the pool
/// holds past its own 2 NEAR) stays backed as epochs pass, where moves
/// alone would read the rewards as unbacked. The tolerance covers the gas
/// rewards its calls earn it.
#[test]
fn near_counts_staking_rewards() {
    const BACKS: &str = "pool NEAR backs what it owes";
    let mut sim = pool(false);
    let locked = sim.account(POOL).unwrap().locked;
    assert_eq!(locked, Balance::from_near(100), "the pool stakes what it holds");
    sim.config.epoch_reward = Balance::from_near(1);
    let books = Books::new()
        .liability("stake", |sim: &Sim| {
            let a = sim.account(POOL).unwrap();
            let all = a.amount.saturating_add(a.locked).saturating_sub(Balance::from_near(2));
            BTreeMap::from([("alice.near".to_owned(), all.as_yoctonear())])
        })
        .near("pool NEAR", POOL)
        .backs("stake", "pool NEAR")
        .tolerance("pool NEAR", Balance::from_millinear(1).as_yoctonear());
    let stake = Balance::from_near(10).as_yoctonear();
    let plan =
        Plan::new().op(PoolOp::Stake { yocto: stake }).epochs(3).op(PoolOp::Stake { yocto: stake });
    let run = Harness::new(sim).books(books).run(&plan);
    let w = run.world::<Sim>().expect("a Sim world");
    assert!(w.rewarded(POOL) > Balance::ZERO, "no reward paid: {run}");
    assert!(run.judged().iter().any(|j| j.check == BACKS && j.ok > 0), "{run}");
    assert!(run.findings().is_empty(), "{run}");
}

/// An asset that reads zero, and backs nothing that is owed, in every
/// case is a to-do ("never held anything"), not a silent pass.
#[test]
fn a_book_that_never_holds_anything_is_a_todo() {
    let books = Books::new()
        .liability("credit", |sim: &Sim| {
            HOLDERS.iter().map(|h| (h.to_string(), credit(sim, h))).collect()
        })
        .asset("escrow", |_: &Sim| 0);
    let h = Harness::new(ledger_world(false)).books(books);
    let audit = ledger_scan(&h).run();
    let report = audit.report();
    let todo = report.todo.iter().find(|t| t.id == "unjudged:escrow is kept, owed to no one");
    let todo = todo.unwrap_or_else(|| panic!("{:?}\n{audit}", report.todo));
    assert!(todo.why.contains("escrow never held anything"), "{todo:?}");
}

// ---- a killed scan resumes ----

/// Sends that abort the process after `BOOKS_ABORT_AFTER` submits: a
/// campaign killed mid-way.
static SUBMITS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

fn ledger_scan(h: &Harness<Send>) -> near_contract_sim::explore::Scan<'_, Send, Sim> {
    h.scan(&[Plan::new().op(Send { amount: 10, refund: 0 }).op(Send { amount: 3, refund: 1 })])
}

fn ledger_books() -> Harness<Send> {
    let books = Books::new()
        .liability("credit", |sim: &Sim| {
            HOLDERS.iter().map(|h| (h.to_string(), credit(sim, h))).collect()
        })
        .owned("credit");
    Harness::new(ledger_world(false)).books(books)
}

/// What a resumed scan must match: cases, findings (each
/// with its label and cases) and to-do ids.
fn same(r: &near_contract_sim::explore::Report) -> (u32, Vec<String>, Vec<String>) {
    let mut rows: Vec<String> = (r.findings.iter())
        .map(|f| format!("{} | {} | {} | {} | {:?}", f.check, f.site, f.label, f.cases, f.also))
        .collect();
    rows.sort();
    let mut todo: Vec<String> = r.todo.iter().map(|t| t.id.clone()).collect();
    todo.sort();
    (r.cases, rows, todo)
}

#[test]
#[ignore = "run by a_killed_scan_resumes_to_the_same_report, which kills it"]
fn killed_scan_child() {
    let Ok(path) = std::env::var("BOOKS_SAVES") else { return };
    let _ = ledger_scan(&ledger_books()).saves(path, 2).run();
}

#[test]
fn a_killed_scan_resumes_to_the_same_report() {
    let h = ledger_books();
    let whole = ledger_scan(&h).run().report();
    assert!(!whole.findings.is_empty(), "{whole:?}");
    let path = std::env::temp_dir().join(format!("books-resume-{}.json", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let status = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "killed_scan_child", "--ignored", "--test-threads=1"])
        .env("BOOKS_SAVES", &path)
        .env("BOOKS_ABORT_AFTER", "40")
        .status()
        .unwrap();
    assert!(!status.success(), "the child was meant to die mid-way");
    let partial: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    let done = partial["done"].as_u64().unwrap();
    assert!(done > 0 && u64::from(whole.cases) > done + 2, "{done} of {}", whole.cases);
    eprintln!(
        "killed after {done} of {} cases; {} finding row(s)",
        whole.cases,
        whole.findings.len()
    );
    let resumed = ledger_scan(&h).saves(&path, 2).run().report();
    assert_eq!(same(&resumed), same(&whole));
    let _ = std::fs::remove_file(&path);
}

// ---- a killed search resumes; its findings are shrunk; saves load back ----

/// Six sends a case, settled apart, the receiver asking back up to twice
/// what it got. Nothing is in flight together, so no case runs serial
/// orders: each is cheap, and the shrink, whose budget is wall-clock,
/// finishes on a slow runner too.
fn ledger_search(h: &Harness<Send>) -> near_contract_sim::explore::Search<'_, Send, Sim> {
    let send = (1u64..20, 0u64..40).prop_map(|(amount, refund)| Send { amount, refund });
    let plans = plans(send).len(6..=6).faults(Faults::none());
    h.search(plans.gaps(Just(Gap::Settle)).repeats(false)).seed(7).cases(24)
}

/// The rows a search's cases showed, as check, site and label: a saved
/// part's (`prior`, all from its cases) and its own not `incidental`. A
/// row first shown while another shrank depends on the shrink's time.
fn shown(audit: &Audit<Send>, prior: &[Row]) -> BTreeSet<String> {
    let own = audit.found().iter().filter(|f| !f.incidental);
    let own = own.map(|f| format!("{} | {} | {}", f.finding.check, f.finding.site, f.label));
    let saved = prior.iter().map(|r| format!("{} | {} | {}", r.check, r.site, r.label));
    own.chain(saved).collect()
}

#[test]
#[ignore = "run by a_killed_search_resumes_to_the_same_report, which kills it"]
fn killed_search_child() {
    let Ok(path) = std::env::var("BOOKS_SAVES") else { return };
    let _ = ledger_search(&ledger_books()).saves(path, 2).run();
}

/// A search killed mid-way resumes to the rows its cases showed and
/// the same cases; each finding's padded six-send case shrinks to the
/// sends that cause it.
#[test]
fn a_killed_search_resumes_to_the_same_report_and_its_findings_shrink() {
    let h = ledger_books();
    let whole = ledger_search(&h).run();
    let owned = whole.assert_found("credit falls only for its holder", "receiver.near");
    let budget = "the shrink ran out of its wall-clock budget (a minute a shown row at least)";
    assert!(matches!(owned.shrink, Shrink::Done(_)), "{budget}: {:?}", owned.shrink);
    let steps = &owned.run.plan().steps;
    assert!(steps.len() <= 2, "shrunk to its cause: {steps:?}");
    let path = std::env::temp_dir().join(format!("search-resume-{}.json", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let status = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "killed_search_child", "--ignored", "--test-threads=1"])
        .env("BOOKS_SAVES", &path)
        // Six sends a case, the first case run twice: dies about case 9.
        .env("BOOKS_ABORT_AFTER", "60")
        .status()
        .unwrap();
    assert!(!status.success(), "the child was meant to die mid-way");
    let partial = Report::from_json(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert!(partial.done > 0 && partial.done < 24, "killed after {} of 24 cases", partial.done);
    assert!(!partial.findings.is_empty(), "rows shown before the kill: {partial:?}");
    let resumed = ledger_search(&h).saves(&path, 2).run();
    assert_eq!(shown(&resumed, &partial.findings), shown(&whole, &[]), "{resumed}");
    assert_eq!(resumed.report().cases, whole.report().cases);
    // The resumed audit ran only the cases after the save, and its report
    // counts each saved row's cases as the whole run does. A resume that
    // ignored the file ran them all again; one that lost the saved rows
    // counted only its own cases: the cases after the kill show the same
    // row again, so both passed the two checks above.
    let ran = (resumed.cases(), partial.cases, whole.cases());
    assert_eq!(ran.0 + ran.1, ran.2, "its own cases and the saved part's: {ran:?}");
    let counted = |r: Report| r.findings.into_iter().map(|f| (f.id, f.cases)).collect();
    let (now, all): (BTreeMap<String, u32>, BTreeMap<_, _>) =
        (counted(resumed.report()), counted(whole.report()));
    for row in &partial.findings {
        let (now, all) = (now.get(&row.id), all.get(&row.id));
        assert_eq!(now, all, "a saved row's cases, resumed and whole: {}", row.id);
    }
    let _ = std::fs::remove_file(&path);
}

/// What `saves` writes loads as a report; curves save and resume, and
/// their to-dos join a union; a file saved for another fixture, or by a
/// harness with other checks, is not resumed, while one saved by another
/// build of the test is; a missing directory is made.
#[test]
fn saves_load_back_and_resume_only_on_their_fixture() {
    let dir = std::env::temp_dir().join(format!("saves-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    // Its own empty store: replays an earlier build left in the default
    // one would read `stale:` and add to-dos the saved report lacks.
    let found = dir.join("found");
    // A check with nothing at stake: a to-do every campaign of it lists.
    let h = ledger_books().store(&found);
    let h = h.at_rest("an idle check", |_: &Sim| Err(Fail::idle("nothing at stake")));
    let path = dir.join("deeper/curves.json");
    let witness = Plan::new().op(Send { amount: 10, refund: 0 });
    let curves = h.curves(std::slice::from_ref(&witness)).saves(&path, 1).run().report();
    let saved = Report::from_json(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert!(saved.done > 0 && !saved.todo.is_empty(), "{saved:?}");
    let ids = |r: &Report| r.todo.iter().map(|t| t.id.clone()).collect::<Vec<_>>();
    let union = h.union(std::slice::from_ref(&saved));
    assert!(ids(&curves).iter().all(|t| ids(&union).contains(t)), "{:?}", ids(&union));
    // Resumed, every axis done: the saved report, and no axis climbed again.
    let again = h.curves(std::slice::from_ref(&witness)).saves(&path, 1).run().report();
    assert_eq!((again.cases, ids(&again)), (curves.cases, ids(&curves)));
    // A planted item resumes with its report saved by this build; saved
    // by another build of the test or a harness with other checks, the
    // report isn't trusted: the campaign starts over without it.
    let planted = |field: Option<(&str, &str)>| {
        let mut v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let todo = json!({ "id": "planted", "why": "planted", "close": "planted" });
        v["todo"].as_array_mut().unwrap().push(todo);
        match field {
            Some(("checks", other)) => {
                let fixture = v["fixture"].as_str().unwrap().split(' ').next().unwrap().to_owned();
                v["fixture"] = json!(format!("{fixture} {other}"));
            }
            Some((key, other)) => v[key] = json!(other),
            None => {}
        }
        std::fs::write(&path, v.to_string()).unwrap();
        let r = h.curves(std::slice::from_ref(&witness)).saves(&path, 1).run().report();
        ids(&r).contains(&"planted".to_owned())
    };
    assert!(planted(None), "resumed with what it saved");
    assert!(!planted(Some(("build", "another"))), "another build started over");
    assert!(!planted(Some(("checks", "00000000"))), "other checks started over");
    // Another fixture (the receiver unfunded) starts over.
    let other = Harness::new(ledger_holding(false, 0)).store(&found);
    let fresh = other.curves(std::slice::from_ref(&witness)).saves(&path, 1).run().report();
    let alone = other.curves(std::slice::from_ref(&witness)).run().report();
    assert_eq!((fresh.cases, ids(&fresh)), (alone.cases, ids(&alone)), "started over");
    let now = Report::from_json(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_ne!(now.fixture, saved.fixture);
    let _ = std::fs::remove_dir_all(&dir);
}

// ---- a contract that prepays storage from a ledger ----

const PREPAID: &str = "prepaid.near";
const KEPT_PREPAID: &str = "prepaid's free NEAR is kept, owed to no one";
const UNBACKED_PREPAID: &str = "prepaid's free NEAR backs what it owes";

/// `storage_ledger.wat` with alice's 1 NEAR on its ledger; `paused` with
/// its own 69 bytes written in the fixture.
fn prepaid(paused: bool) -> Sim {
    let f = Fixture::new()
        .account("alice.near", Balance::from_near(10))
        .contract(PREPAID, ::wat::parse_str(wat("storage_ledger.wat")).unwrap())
        .call("alice.near", PREPAID, "deposit")
        .deposit(Balance::from_near(1));
    if paused { f.call("alice.near", PREPAID, "pause").build() } else { f.build() }
}

/// What the ledger owes alice: her deposit less the storage her bytes take.
fn storage_owed(sim: &Sim) -> BTreeMap<String, u128> {
    let deposit =
        sim.state(PREPAID, b"D").map_or(0, |v| u128::from_le_bytes(v[..16].try_into().unwrap()));
    let bytes = sim.state(PREPAID, b"U").map_or(0, |v| 1 + v.len() as u64 + 40);
    BTreeMap::from([("alice.near".into(), deposit - sim.storage_cost(bytes).as_yoctonear())])
}

/// Native fees the contract holds for others (none here).
fn pending_fees(_: &Sim) -> BTreeMap<String, u128> {
    BTreeMap::new()
}

/// The contract's NEAR less what its storage stakes: what can leave it.
fn free_near(sim: &Sim) -> u128 {
    let a = sim.account(PREPAID).expect("the contract");
    a.amount.as_yoctonear() - sim.storage_cost(a.storage_usage).as_yoctonear()
}

/// The storage-prepaying shape, as the Books docs give it.
fn prepaid_books() -> Books<Call> {
    Books::new()
        .liability("the storage ledger", storage_owed) // its own entry left out
        .liability("the pending fees", pending_fees)
        .asset("prepaid's free NEAR", free_near)
        .backs("the storage ledger", "prepaid's free NEAR")
        .backs("the pending fees", "prepaid's free NEAR")
        .tolerance("prepaid's free NEAR", 10_000) // yoctos attached to calls
}

fn prepaid_call(method: &str) -> Plan<Call> {
    Plan::new().op(Call::new("alice.near", PREPAID, method))
}

/// A contract that prepays storage from a ledger, booked as free NEAR
/// (its balance less its storage stake): a ledger debit that became
/// bytes is no NEAR kept, nor unbacked. Bytes of its own (a pause flag:
/// 110 with its key and record) staked out of the NEAR backing the ledger
/// are reported: the pool that backs what it owes shrank by 1.1e21. NEAR
/// freed with its own bytes is kept, owed to no one.
///
/// Verification boundary: the books' judgement of actual storage staking.
#[test]
fn a_contract_paying_its_own_storage_from_the_pool_is_reported() {
    let on = |paused| Harness::new(prepaid(paused)).books(prepaid_books());
    let none = |run: &near_contract_sim::explore::Run<Call>, check: &str| {
        assert!(run.findings().iter().all(|f| f.check != check), "{run}");
    };
    // A ledger debit that became bytes.
    let write = on(false).run(&prepaid_call("write"));
    none(&write, KEPT_PREPAID);
    none(&write, UNBACKED_PREPAID);
    // Bytes of its own, out of the pool.
    let pause = on(false).run(&prepaid_call("pause"));
    let f = pause.assert_found(UNBACKED_PREPAID, "world");
    assert!(f.message.contains(": 1100000000000000000000 unbacked"), "{}", f.message);
    none(&pause, KEPT_PREPAID);
    // Its own bytes freed: free NEAR again, owed to no one.
    let unpause = on(true).run(&prepaid_call("unpause"));
    let f = unpause.assert_found(KEPT_PREPAID, "world");
    assert!(f.message.contains(": 1100000000000000000000 kept"), "{}", f.message);
}

/// Readings past `i128::MAX` keep their sign: an asset that
/// grows from nothing to 2^127 + 3 with nothing owed keeps it, and a
/// liability that grows that much over a flat asset is unbacked. Cast to
/// `i128`, both read as losses and passed.
#[test]
fn books_past_i128_keep_their_sign() {
    const BIG: u128 = (1 << 127) + 3;
    let sim = forwarder(false);
    let start = sim.height();
    let grown = move |sim: &Sim| if sim.height() > start { BIG } else { 0 };
    let plan = Plan::new().op(kept_noise(1));
    let books = Books::new().asset("big", grown);
    let run = Harness::new(forwarder(false)).books(books).run(&plan);
    let f = run.assert_found("big is kept, owed to no one", "world");
    assert!(f.message.contains(&format!("{BIG} kept")), "{}", f.message);
    let owed = move |sim: &Sim| BTreeMap::from([("alice.near".to_owned(), grown(sim))]);
    let books =
        Books::new().liability("debt", owed).asset("flat", |_: &Sim| 1).backs("debt", "flat");
    let run = Harness::new(forwarder(false)).books(books).run(&plan);
    let f = run.assert_found("flat backs what it owes", "world");
    assert!(f.message.contains(&format!("{BIG} unbacked")), "{}", f.message);
}

// ---- 1-yocto proofs a contract books ----

/// `near_wrapper.wat` booking each deposit it is sent as wNEAR, its own
/// balance a holder too; `overpays`: its `near_withdraw` pays `amount + 1`.
fn self_booking(overpays: bool) -> Harness<Call> {
    let text = wat("near_wrapper.wat");
    let text = if overpays {
        text.replace(
            "(i64.store (i32.const 600) (local.get $n))",
            "(i64.store (i32.const 600) (i64.add (local.get $n) (i64.const 1)))",
        )
    } else {
        text
    };
    let sim = Fixture::new()
        .account("alice.near", Balance::from_near(100))
        .contract(WRAP, ::wat::parse_str(text).unwrap())
        .build();
    let books = Books::<Call>::new()
        .liability("wNEAR", |sim: &Sim| {
            let of = |who: &str| sim.view(WRAP, "ft_balance_of", json!({ "account_id": who }));
            [("alice.near", of("alice.near").u128()), (WRAP, of(WRAP).u128())]
                .into_iter()
                .map(|(h, v)| (h.to_owned(), v))
                .collect()
        })
        .near("wrap NEAR", WRAP)
        .backs("wNEAR", "wrap NEAR");
    Harness::new(sim).books(books)
}

/// At tolerance 0: a contract that books the 1-yocto proof it is sent
/// passes (the yocto arrived); an unproved yocto paid out is 1 unbacked;
/// and a proof on a call the contract signs to itself, booked, is 1
/// unbacked too. No NEAR entered (a self-move nets zero, as on a real
/// network), so the ledger grew from nothing: the contract's drift, not a
/// proof `Books::near` leaves out.
#[test]
fn a_booked_proof_passes_at_tolerance_0_and_a_self_call_one_is_unbacked() {
    const BACKS: &str = "wrap NEAR backs what it owes";
    let proof =
        |from: &str| Call::new(from, WRAP, "near_deposit").deposit(Balance::from_yoctonear(1));
    self_booking(false).run(&Plan::new().op(proof("alice.near"))).assert_clean();
    let amount: u64 = 1_000_000_000_000_000_000;
    let wrap = Call::new("alice.near", WRAP, "near_deposit")
        .deposit(Balance::from_yoctonear(amount.into()));
    let unwrap = Call::new("alice.near", WRAP, "near_withdraw").args(amount.to_le_bytes().to_vec());
    let leak = self_booking(true).run(&Plan::new().op(wrap).op(unwrap));
    let f = leak.assert_found(BACKS, "world");
    assert!(f.message.ends_with(": 1 unbacked"), "{leak}");
    let run = self_booking(false).run(&Plan::new().op(proof(WRAP)));
    let f = run.assert_found(BACKS, "world");
    assert!(f.message.ends_with(": 1 unbacked"), "{run}");
}

/// A liability reader that misses a holder (bob) reads his deposit as
/// kept, and says `.supply` tells the two apart; the declared supply (the
/// sum here stands in for the contract's stated total) fails too.
#[test]
fn a_missed_holder_reads_as_kept_and_supply_fires() {
    let sim = Fixture::new()
        .account("alice.near", Balance::from_near(100))
        .account("bob.near", Balance::from_near(100))
        .contract(WRAP, ::wat::parse_str(wat("near_wrapper.wat")).unwrap())
        .build();
    let of =
        |sim: &Sim, who: &str| sim.view(WRAP, "ft_balance_of", json!({ "account_id": who })).u128();
    let books = Books::<Call>::new()
        .liability("wNEAR", move |sim: &Sim| {
            BTreeMap::from([("alice.near".into(), of(sim, "alice.near"))])
        })
        .supply("wNEAR", move |sim: &Sim| of(sim, "alice.near") + of(sim, "bob.near"))
        .near("wrap NEAR", WRAP)
        .backs("wNEAR", "wrap NEAR");
    let deposit = Call::new("bob.near", WRAP, "near_deposit").deposit(Balance::from_near(1));
    let run = Harness::new(sim).books(books).run(&Plan::new().op(deposit));
    let kept = run.assert_found("wrap NEAR is kept, owed to no one", "world");
    assert!(kept.message.contains("reader misses: `.supply` tells them apart"), "{run}");
    run.assert_found("wNEAR's supply is the sum of its holders", "world");
}

// ---- a plain account's NEAR, watched ----

/// A pot that pays its caller what it holds, then clears in a callback: two
/// pays in one block both read the pot before either clears, so the caller
/// is paid twice; settled apart, the second pays nothing.
const POT_WAT: &str = r#"(module
  (import "env" "storage_read" (func $read (param i64 i64 i64) (result i64)))
  (import "env" "storage_write" (func $write (param i64 i64 i64 i64 i64) (result i64)))
  (import "env" "read_register" (func $reg (param i64 i64)))
  (import "env" "register_len" (func $len (param i64) (result i64)))
  (import "env" "predecessor_account_id" (func $caller (param i64)))
  (import "env" "current_account_id" (func $me (param i64)))
  (import "env" "promise_batch_create" (func $batch (param i64 i64) (result i64)))
  (import "env" "promise_batch_action_transfer" (func $transfer (param i64 i64)))
  (import "env" "promise_then"
    (func $then (param i64 i64 i64 i64 i64 i64 i64 i64 i64) (result i64)))
  (memory 1)
  (export "memory" (memory 0))
  (data (i32.const 0) "pot")
  (data (i32.const 8) "clear")
  ;; 16: a zero u128; 32: the pot, a u128; 64: an account id
  (func (export "pay")
    (local $p i64)
    (if (i64.eq (call $read (i64.const 3) (i64.const 0) (i64.const 1)) (i64.const 1))
      (then (call $reg (i64.const 1) (i64.const 32))))
    (call $caller (i64.const 2))
    (call $reg (i64.const 2) (i64.const 64))
    (local.set $p (call $batch (call $len (i64.const 2)) (i64.const 64)))
    (call $transfer (local.get $p) (i64.const 32))
    (call $me (i64.const 2))
    (call $reg (i64.const 2) (i64.const 64))
    (drop (call $then (local.get $p) (call $len (i64.const 2)) (i64.const 64)
      (i64.const 5) (i64.const 8) (i64.const 0) (i64.const 0) (i64.const 16)
      (i64.const 5000000000000))))
  (func (export "clear")
    (drop (call $write (i64.const 3) (i64.const 0) (i64.const 16) (i64.const 16) (i64.const 1)))))"#;

const POT: &str = "pot.near";

/// alice, and the pot holding 1 NEAR to pay out.
fn pot() -> Sim {
    let mut sim = Sim::new();
    sim.create_account("alice.near", Balance::from_near(10));
    sim.deploy(POT, ::wat::parse_str(POT_WAT).unwrap());
    let one = Balance::from_near(1).as_yoctonear().to_le_bytes();
    sim.set_state(POT, b"pot", Some(&one[..]));
    sim
}

/// alice asks the pot to pay her.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct Pay;

impl Op for Pay {
    fn submit(&self, sim: &mut Sim) -> Result<(), Skip> {
        sim.call("alice.near", POT, "pay").submit();
        Ok(())
    }
}

/// `Books::watch_near`: alice's NEAR as a reading only. Her two pays in
/// one block are paid twice, which no serial order gives: `SERIALIZABLE`
/// finds it, in a run and in an enumeration. Booked with `keeps_no_near`
/// instead, the 1 NEAR any pay gives her is also "kept, owed to no one";
/// watched, no such check exists. A diff reads it too.
///
/// Verification boundary: the books' judgement over actual runs: a new
/// serial-order reading, and no derived check.
#[test]
fn a_watched_account_s_near_is_read_by_serializable_and_never_kept() {
    use near_contract_sim::explore::builtin::SERIALIZABLE;
    // The kept check a watched account's name would have.
    const KEPT: &str = "alice.near NEAR is kept, owed to no one";
    let race = Plan::new().op(Pay).blocks(0).op(Pay);
    let booked = Harness::new(pot()).books(Books::new().keeps_no_near("alice.near"));
    let run = booked.run(&race);
    run.assert_found(SERIALIZABLE, "world");
    run.assert_found(KEPT, "world");
    let watched = Harness::new(pot()).books(Books::new().watch_near("alice.near"));
    let run = watched.run(&race);
    let f = run.assert_found(SERIALIZABLE, "world");
    assert!(f.message.contains("alice.near NEAR 2000000000000000000000000 here"), "{run}");
    assert!(run.judged().iter().all(|j| !j.check.contains("kept")), "{run}");
    watched.run(&Plan::new().op(Pay).op(Pay)).assert_clean();
    // An enumeration of pays finds the race, and no kept row.
    let audit = watched.enumerate(&[Pay]).depth(2).run();
    audit.assert_found(SERIALIZABLE, "world");
    assert!(audit.found_at(KEPT, "world").is_none(), "{audit}");
    let todo = audit.todo();
    assert!(todo.iter().all(|t| !t.id.contains("alice.near NEAR")), "{todo:?}");
    let audit = booked.enumerate(&[Pay]).depth(2).run();
    audit.assert_found(KEPT, "world");
    // A diff reads it: a pot of 2 NEAR pays alice more.
    let mut two = pot();
    two.set_state(POT, b"pot", Some(&Balance::from_near(2).as_yoctonear().to_le_bytes()[..]));
    let two = Harness::new(two).books(Books::new().watch_near("alice.near"));
    let diff = watched.diff(&two, &[Plan::new().op(Pay)]);
    assert!(diff.lines().any(|l| l.contains("alice.near NEAR held")), "{diff}");
}

/// A watched account's NEAR backs nothing: `books` panics on `backs`
/// naming it, where it would derive a check from a reading.
#[test]
#[should_panic(expected = "\"alice.near NEAR\" is a reading (watch_near), not an asset: use near")]
fn a_watched_reading_backs_nothing() {
    let owed = |_: &Sim| BTreeMap::from([("alice.near".to_owned(), 1)]);
    let books = Books::<Pay>::new().watch_near("alice.near").liability("debt", owed);
    let _ = Harness::new(pot()).books(books.backs("debt", "alice.near NEAR"));
}

/// Asset names are unique: `watch_near(a)` and `keeps_no_near(a)` both
/// name theirs `"a NEAR"`. `books` panics naming both, where a rule naming
/// that asset read whichever came first and the two shared a tolerance.
#[test]
#[should_panic(
    expected = "Books: two assets named \"alice.near NEAR\", watch_near(\"alice.near\") \
                and keeps_no_near(\"alice.near\")"
)]
fn two_assets_of_one_name_panic_naming_both() {
    let books = Books::<Pay>::new().watch_near("alice.near").keeps_no_near("alice.near");
    let _ = Harness::new(pot()).books(books);
}
