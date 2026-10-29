//! Axes against actual runs: what moves, what is a choice, and what the
//! world must supply for an axis to grow.
//!
//! - A list of account ids grows with fresh ids that are still valid ids
//!   of their kind, past 100 items.
//! - A unit variant of an enum is a choice: a scan tries each variant, a
//!   curve leaves it alone, and no to-do item asks to lengthen it. A list
//!   of them grows with plain copies. A string that changed doesn't
//!   deserialize (a hash), or a tuple's length, is named once as not an
//!   axis.
//! - A step refused whole after its copies stops its row at a bound,
//!   closed; a copy the world refuses is an open item naming what to
//!   supply. Neither is a cap.
//! - A list a call receives that no op moves grows by direct
//!   resubmission; if its receiver refuses it sent directly, a `list:`
//!   item says why.
//!
//! - Repeat copies settle one by one: a `Fraction(1, 2)` of a balance
//!   read at each copy's submit never overdraws, and a copy that skips
//!   for what is left closes the row as "world exhausted". An op's
//!   `Op::copy` gives each copy a fresh nonce, which a nonce check passes.
//! - A search whose cases send a list at one length has a `length:` item.
//!
//! Verification boundary: which plans scans and curves run (scheduling of
//! arguments and copies), over actual runs of the pinned runtime.

use near_contract_sim::CryptoHash;
use near_contract_sim::explore::{AxisKind, Call, Harness, Op, Plan, Reach, Resource, Skip};
use near_contract_sim::{AccountId, Balance, Fixture, Reply, Sim, Stub};
use serde::{Deserialize, Serialize};
use serde_json::json;

const PAYER: &str = "payer.near";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
enum User {
    Alice,
    Bob,
}

/// Pays each of `to`, for `who`, tagged; `receipt` is a hash the caller
/// quotes.
#[derive(Clone, Debug, Serialize, Deserialize)]
enum PayOp {
    Pay { who: User, tags: Vec<User>, to: Vec<AccountId>, receipt: CryptoHash, split: (u32, u32) },
}

impl Op for PayOp {
    fn submit(&self, sim: &mut Sim) -> Result<(), Skip> {
        let PayOp::Pay { who, tags, to, .. } = self;
        let signer = if *who == User::Alice { "alice.near" } else { "bob.near" };
        let args = json!({ "to": to, "tags": tags });
        sim.call(signer, PAYER, "pay").args_json(args).submit();
        Ok(())
    }
}

fn world() -> Sim {
    Fixture::new()
        .account("alice.near", Balance::from_near(100))
        .account("bob.near", Balance::from_near(100))
        .stub(PAYER, Stub::new().on("pay", Reply::json(())))
        .build()
}

fn witness() -> Plan<PayOp> {
    let to = vec!["carol.near".parse().unwrap()];
    let receipt = CryptoHash::hash_bytes(b"receipt");
    Plan::new().op(PayOp::Pay {
        who: User::Alice,
        tags: vec![User::Alice],
        to,
        receipt,
        split: (1, 2),
    })
}

/// Who pays is a choice, and so is each tag; the hash is no axis; the
/// ids are a list. A scan tries Bob, and the check that only alice pays
/// fires there, shown at that value.
#[test]
fn an_enum_is_a_choice_a_scan_tries() {
    let h = Harness::new(world()).at_rest("only alice pays", |sim: &Sim| {
        match sim.calls_to(PAYER).iter().all(|c| c.signer_id.as_str() == "alice.near") {
            true => Ok(()),
            false => Err("bob paid"),
        }
    });
    let axes = h.axes(&witness());
    let kind = |name: &str| axes.iter().find(|(_, a)| a.name == name).map(|(_, a)| a.kind);
    assert_eq!(kind("/Pay/who"), Some(AxisKind::Choice), "{axes:#?}");
    assert_eq!(kind("/Pay/tags/*"), Some(AxisKind::Choice), "{axes:#?}");
    assert_eq!(kind("/Pay/to"), Some(AxisKind::Len), "{axes:#?}");
    assert_eq!(kind("/Pay/receipt"), None, "a hash lengthened doesn't deserialize: {axes:#?}");
    assert_eq!(kind("/Pay/split"), None, "a tuple's length is fixed: {axes:#?}");
    assert_eq!(kind("/Pay/split/*"), Some(AxisKind::Number), "{axes:#?}");
    // A list as long as the tuple beside it is still a list.
    let PayOp::Pay { who, to, receipt, split, .. } = witness().steps[0].act.op().unwrap().clone();
    let tags = vec![User::Alice, User::Bob];
    let axes = h.axes(&Plan::new().op(PayOp::Pay { who, tags, to: to.clone(), receipt, split }));
    let kind = |name: &str| axes.iter().find(|(_, a)| a.name == name).map(|(_, a)| a.kind);
    assert_eq!(kind("/Pay/tags"), Some(AxisKind::Len), "{axes:#?}");
    assert_eq!(kind("/Pay/split"), None, "{axes:#?}");
    // A witness at a later variant stands at it, so a scan tries the first.
    let (who, tags) = (User::Bob, vec![User::Alice]);
    let axes = h.axes(&Plan::new().op(PayOp::Pay { who, tags, to, receipt, split }));
    let who = axes.iter().find(|(_, a)| a.name == "/Pay/who").map(|(_, a)| a.at);
    assert_eq!(who, Some(1), "Bob is the second variant: {axes:#?}");
    let audit = h.scan(&[witness()]).run();
    // Bob paying, and axis.near paying directly (the outsider sweep), are
    // two rows of the check.
    let rows = || audit.found().iter().filter(|f| f.finding.check == "only alice pays");
    let labels: Vec<String> = rows().map(|f| f.label.to_string()).collect();
    assert!(labels.iter().any(|l| l.contains("/Pay/who = Bob")), "{labels:?}");
    assert!(labels.iter().any(|l| l.contains("sent directly from axis.near")), "{labels:?}");
    let summary = audit.to_string();
    assert!(
        summary.contains("not axes (changed, they don't deserialize): /Pay/receipt"),
        "{summary}"
    );
    assert!(summary.contains("/Pay/split (step 0)"), "{summary}");
}

/// Curves leave a choice alone: no row along it, no to-do item. A list of
/// ids grows past 100 with valid fresh ids, and a list of enum values
/// with plain copies.
#[test]
fn lists_grow_with_values_their_type_takes() {
    let h = Harness::new(world());
    let audit = h.curves(&[witness()]).rungs(9).run();
    assert!(audit.curves().iter().all(|r| r.axis.kind != AxisKind::Choice), "{audit}");
    let open: Vec<String> = audit.todo().into_iter().map(|t| t.id).collect();
    assert!(!open.iter().any(|t| t.contains("/who") || t.contains("/tags/*")), "{open:#?}");
    for list in ["/Pay/to", "/Pay/tags"] {
        let row = audit.assert_curve("payer.near::pay", Resource::ReceiptGas, list);
        assert!(row.moved_to >= 256, "{list}: {row}");
    }
    let grown = audit.assert_curve("payer.near::pay", Resource::ReceiptGas, "/Pay/to");
    let PayOp::Pay { to, .. } = grown.plan.steps[0].act.op().unwrap();
    assert_eq!(to[1].as_str(), "carol-1.near", "{row}", row = grown);
}

/// A counter that takes three calls, then refuses: `full`.
const THREE: &str = r#"(module
  (import "env" "storage_read" (func $sr (param i64 i64 i64) (result i64)))
  (import "env" "storage_write" (func $sw (param i64 i64 i64 i64 i64) (result i64)))
  (import "env" "read_register" (func $rr (param i64 i64)))
  (import "env" "panic_utf8" (func $panic (param i64 i64)))
  (memory 1)
  (data (i32.const 0) "n")
  (data (i32.const 16) "full")
  (func (export "take") (local $n i64)
    (if (i64.eq (call $sr (i64.const 1) (i64.const 0) (i64.const 0)) (i64.const 1))
      (then (call $rr (i64.const 0) (i64.const 32)) (local.set $n (i64.load (i32.const 32)))))
    (if (i64.ge_u (local.get $n) (i64.const 3)) (then (call $panic (i64.const 4) (i64.const 16))))
    (i64.store (i32.const 32) (i64.add (local.get $n) (i64.const 1)))
    (drop (call $sw (i64.const 1) (i64.const 0) (i64.const 8) (i64.const 32) (i64.const 1)))))"#;

/// Copies before the step use up what the contract allows: the fourth
/// call, the step, is refused whole, nothing it did staying. A bound of
/// the contract, not a gap: the row closes (its use
/// flat, as no effect) and no to-do asks to supply more. (A copy refused
/// stays open, naming what to give its signer.)
#[test]
fn a_step_refused_whole_stops_the_row_at_a_bound() {
    let sim = Fixture::new()
        .account("alice.near", Balance::from_near(100))
        .contract("three.near", wat::parse_str(THREE).unwrap())
        .build();
    let plan = Plan::new().op(Call::new("alice.near", "three.near", "take"));
    let h = Harness::new(sim);
    let audit = h.curve(&plan, 0, "repeat take (step 0)").run();
    let row = audit.assert_curve("three.near::take", Resource::ReceiptGas, "repeat take (step 0)");
    assert_eq!(row.reach, Reach::NoEffect, "{row}");
    assert_eq!(row.open(), None, "{row}");
    assert!(audit.todo().iter().all(|t| !t.why.contains("full")), "{audit}");
}

/// A call the step's receipts make receives a list the op doesn't set:
/// payer.near forwards one fixed id to ledger.near. The curves resubmit
/// that call directly, and grow its list there with fresh ids.
#[test]
fn a_list_no_op_moves_grows_by_direct_resubmission() {
    let forward = Reply::call("ledger.near", "take", json!({ "ids": ["a"] })).then(Reply::json(()));
    let sim = Fixture::new()
        .account("alice.near", Balance::from_near(100))
        .stub(PAYER, Stub::new().on("pay", forward))
        .stub("ledger.near", Stub::new().on("take", Reply::json(())))
        .build();
    let audit = Harness::new(sim).curves(&[witness()]).rungs(4).run();
    let open: Vec<String> = audit.todo().into_iter().map(|t| t.id).collect();
    assert!(!open.iter().any(|t| t.starts_with("list:")), "{open:#?}");
    let row = audit.assert_curve("ledger.near::take", Resource::ReceiptGas, "/args/json/ids");
    assert!(row.moved_to >= 8, "{row}");
}

/// The same, but ledger.near takes calls from payer.near only: sent
/// directly, the list is refused, an open `list:` item naming why. Once
/// excused, the headline still says so.
#[test]
fn a_list_its_receiver_refuses_directly_is_open() {
    let only_payer = r#"(module
      (import "env" "predecessor_account_id" (func $pred (param i64)))
      (import "env" "register_len" (func $len (param i64) (result i64)))
      (import "env" "panic_utf8" (func $panic (param i64 i64)))
      (memory 1)
      (data (i32.const 0) "only payer.near")
      (func (export "take")
        (call $pred (i64.const 0))
        (if (i64.ne (call $len (i64.const 0)) (i64.const 10))
          (then (call $panic (i64.const 15) (i64.const 0))))))"#;
    let forward = Reply::call("ledger.near", "take", json!({ "ids": ["a"] })).then(Reply::json(()));
    let sim = Fixture::new()
        .account("alice.near", Balance::from_near(100))
        .stub(PAYER, Stub::new().on("pay", forward))
        .contract("ledger.near", wat::parse_str(only_payer).unwrap())
        .build();
    let h = Harness::new(sim);
    let audit = h.curves(&[witness()]).rungs(2).run();
    let item = audit.todo().into_iter().find(|t| t.id == "list:ledger.near::take");
    let item = item.unwrap_or_else(|| panic!("{audit}"));
    assert!(
        item.why.contains("only payer.near") && item.why.contains("/args/json/ids"),
        "{item:?}"
    );
    assert!(audit.to_string().contains("list:ledger.near::take"), "{audit}");
    let excused = h.excuse("list:*", "ledger.near takes ids from payer.near only");
    let summary = excused.curves(&[witness()]).rungs(2).run().to_string();
    assert!(summary.lines().nth(1).unwrap().contains("list:ledger.near::take"), "{summary}");
}

// ---- copies against the live state, re-signed copies, one length ----

const LEDGER: &str = "ledger.near";

fn ledger() -> Harness<Half> {
    let text = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/res/refund_ledger.wat"),
    );
    let f = Fixture::new()
        .account("alice.near", Balance::from_near(100))
        .contract(LEDGER, ::wat::parse_str(text.unwrap()).unwrap())
        .stub("sink.near", Stub::new().on("on_receive", Reply::json("0")))
        .setup("fund alice", |sim: &mut Sim| {
            let fund = sim.call("alice.near", LEDGER, "fund").args(1000u64.to_le_bytes());
            fund.transact().assert_success();
        });
    Harness::new(f.build())
}

/// alice sends `amount` of her credit, read when it submits; nothing
/// left to send is a skip.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct Half {
    amount: near_contract_sim::explore::Amount,
}

impl Op for Half {
    fn submit(&self, sim: &mut Sim) -> Result<(), Skip> {
        let held = sim.view_raw(LEDGER, "credit_of", b"alice.near".to_vec()).result.unwrap();
        let held = u64::from_le_bytes(held.try_into().unwrap());
        let n = self.amount.of(held.into())?;
        if n == 0 {
            return Err(Skip(format!("{:?} of a balance of {held} is 0", self.amount)));
        }
        let args = [&(n as u64).to_le_bytes()[..], b"sink.near"].concat();
        sim.call("alice.near", LEDGER, "send").args(args).submit();
        Ok(())
    }
}

#[test]
fn repeat_copies_halve_the_live_balance_until_the_world_is_exhausted() {
    use near_contract_sim::explore::Amount;
    let plan = Plan::new().op(Half { amount: Amount::Fraction(1, 2) });
    let audit = ledger().curve(&plan, 0, "repeat Half (step 0)").run();
    let row = audit.assert_curve("ledger.near::send", Resource::ReceiptGas, "repeat Half (step 0)");
    // 1000 halves at each copy (500, 250, ..., 1) until half of 1 is 0.
    match &row.reach {
        Reach::Exhausted(why) => assert!(why.contains("of a balance of 1 is 0"), "{row}"),
        _ => panic!("{row}"),
    }
    assert!(row.to_string().contains("world exhausted at"), "{row}");
    assert!(audit.todo().iter().all(|t| !t.id.contains("repeat Half")), "{audit}");
}

const NONCES: &str = "nonces.near";

/// Uses `nonce`; with `resign`, each copy uses a nonce of its own.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct Nonce {
    nonce: u64,
    resign: bool,
}

impl Op for Nonce {
    fn submit(&self, sim: &mut Sim) -> Result<(), Skip> {
        sim.call("alice.near", NONCES, "use").args(self.nonce.to_le_bytes()).submit();
        Ok(())
    }

    fn copy(&self, k: usize) -> Self {
        let nonce = if self.resign { self.nonce + 1 + k as u64 } else { self.nonce };
        Nonce { nonce, ..self.clone() }
    }
}

#[test]
fn a_copy_hook_gives_each_copy_a_fresh_nonce() {
    let text = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/res/nonce_box.wat"),
    );
    let f = Fixture::new()
        .account("alice.near", Balance::from_near(100))
        .contract(NONCES, ::wat::parse_str(text.unwrap()).unwrap());
    let h = Harness::new(f.build());
    let axis = "repeat Nonce (step 0)";
    let rows = |resign| {
        let plan = Plan::new().op(Nonce { nonce: 7, resign });
        h.curve(&plan, 0, axis).run().curves().to_vec()
    };
    let resigned = rows(true);
    let row = resigned
        .iter()
        .find(|r| r.site == "nonces.near::use" && r.resource == Resource::ReceiptGas);
    let row = row.unwrap_or_else(|| panic!("{resigned:?}"));
    assert_eq!(row.moved_to, 64, "{row}");
    assert!(!matches!(row.reach, Reach::Stopped(_)), "{row}");
    // Without it, the step after a copy reuses the copy's nonce: refused
    // from the first rung, no row climbs, and each says why: an axis
    // refused at its first rung is never silent.
    let refused = rows(false);
    assert!(!refused.is_empty(), "rows of an axis refused at once");
    for row in &refused {
        let why = match &row.reach {
            Reach::AtBound(why) | Reach::Stopped(why) => why,
            _ => panic!("{row}"),
        };
        assert!(why.starts_with("1 fails at"), "{row}");
    }
}

/// Pays `to`, a list.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct Batch {
    to: Vec<AccountId>,
}

impl Op for Batch {
    fn submit(&self, sim: &mut Sim) -> Result<(), Skip> {
        let args = json!({ "to": self.to }).to_string();
        sim.call("alice.near", "sink.near", "on_receive").args(args).submit();
        Ok(())
    }
}

#[test]
fn a_list_sent_at_one_length_in_every_case_is_a_todo() {
    use near_contract_sim::explore::plans;
    use proptest::prelude::*;
    let f = Fixture::new()
        .account("alice.near", Balance::from_near(100))
        .stub("sink.near", Stub::new().on("on_receive", Reply::json("0")));
    let h = Harness::new(f.build());
    let id: AccountId = "bob.near".parse().unwrap();
    let batch = |n: std::ops::RangeInclusive<usize>| {
        proptest::collection::vec(Just(id.clone()), n).prop_map(|to| Batch { to })
    };
    let one = h.search(plans(batch(1..=1)).len(1..=2)).cases(8).seed(1).run();
    let todo = one.todo();
    let item = todo.iter().find(|t| t.id == "length:Batch:/to");
    assert!(item.is_some_and(|t| t.why.contains("with 1 item(s)")), "{todo:?}");
    let many = h.search(plans(batch(1..=3)).len(1..=2)).cases(8).seed(1).run();
    assert!(many.todo().iter().all(|t| !t.id.starts_with("length:")), "{many}");
}

/// Held copies too: two blocks apart, each reads what the last left.
#[test]
fn tail_copies_halve_the_live_balance_without_overdrawing() {
    use near_contract_sim::explore::Amount;
    let plan = Plan::new().op(Half { amount: Amount::Fraction(1, 2) });
    let h = ledger();
    let tail = h.axes(&plan).into_iter().find(|(_, a)| a.kind == AxisKind::HoldTail);
    let audit = h.curve(&plan, 0, &tail.expect("a tail").1.name).run();
    assert!(!audit.curves().is_empty(), "{audit}");
    for row in audit.curves() {
        assert!(
            !matches!(&row.reach, Reach::Stopped(w) if w.contains("not enough credit")),
            "{row}"
        );
    }
}

/// What a send carries: data in two of its variants.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
enum Memo {
    Plain,
    Notify { refund: u64, to: String },
    Execute(u64),
}

#[derive(Clone, Debug, Serialize, Deserialize)]
enum MemoOp {
    Send { memo: Memo },
}

impl Op for MemoOp {
    fn submit(&self, sim: &mut Sim) -> Result<(), Skip> {
        let MemoOp::Send { memo } = self;
        sim.call("alice.near", PAYER, "pay").args_json(json!({ "memo": memo })).submit();
        Ok(())
    }
}

/// An enum whose variants carry data is a choice: each variant, unit or
/// carrying a payload its errors fill in (plain values), the witness's own
/// keeping its data; a scan tries each.
#[test]
fn an_enum_with_data_variants_is_a_choice_a_scan_tries() {
    let h = Harness::new(world());
    for memo in [Memo::Plain, Memo::Notify { refund: 7, to: "bob.near".into() }] {
        let witness = Plan::new().op(MemoOp::Send { memo: memo.clone() });
        let axes = h.axes(&witness);
        let (_, axis) = axes
            .iter()
            .find(|(_, a)| a.name == "/Send/memo")
            .unwrap_or_else(|| panic!("{axes:#?}"));
        assert_eq!(axis.kind, AxisKind::Choice, "{axis:?}");
        let memos: Vec<Memo> = (0..3)
            .map(|k| {
                let p = h.moved(&witness, 0, axis, k).unwrap_or_else(|| panic!("{k}: {axis:?}"));
                let near_contract_sim::explore::Act::Op(MemoOp::Send { memo }) = &p.steps[0].act
                else {
                    panic!()
                };
                memo.clone()
            })
            .collect();
        let own = match &memo {
            Memo::Notify { .. } => memo.clone(),
            _ => Memo::Notify { refund: 0, to: String::new() },
        };
        assert_eq!(memos, [Memo::Plain, own, Memo::Execute(0)], "{axis:?}");
        assert!(axes.iter().all(|(_, a)| a.name != "/Send/memo/Plain"), "{axes:#?}");
    }
    let witness = Plan::new().op(MemoOp::Send { memo: Memo::Plain });
    let audit = h.scan(&[witness]).run();
    let tried: Vec<String> = audit.report().timings.iter().map(|t| t.0.clone()).collect();
    for v in [r#"/Send/memo = {"Notify":{"refund":0,"to":""}}"#, r#"/Send/memo = {"Execute":0}"#] {
        assert!(tried.iter().any(|t| t.contains(v)), "{v}: {tried:#?}");
    }
}

// ---- raw-call copies go through Op::copy ----

const PAYLOADS: &str = "payloads.near";
const OWNED: &str = "credit falls only for its holder";

/// A relayer submits a spend whose args name the holder, as a signed
/// payload does: `As` names a user by an enum, and skips when her credit
/// is short; `For` names the holder by an account id.
#[derive(Clone, Debug, Serialize, Deserialize)]
enum Spend {
    As { user: User, amount: u64 },
    For { holder: String, amount: u64 },
}

fn credit(sim: &Sim, holder: &str) -> u64 {
    let r = sim.view_raw(PAYLOADS, "credit_of", holder.as_bytes().to_vec()).result.unwrap();
    u64::from_le_bytes(r.try_into().unwrap())
}

impl Op for Spend {
    fn submit(&self, sim: &mut Sim) -> Result<(), Skip> {
        let (holder, amount) = match self {
            Spend::As { amount, .. } => ("alice.near", *amount),
            Spend::For { holder, amount } => (holder.as_str(), *amount),
        };
        if matches!(self, Spend::As { .. }) && credit(sim, holder) < amount {
            return Err(Skip(format!("{holder}'s credit is short of {amount}")));
        }
        let args = [&amount.to_le_bytes()[..], holder.as_bytes()].concat();
        sim.call("relayer.near", PAYLOADS, "spend").args(args).submit();
        Ok(())
    }

    fn copy(&self, _: usize) -> Self {
        self.clone()
    }
}

/// `payload_ledger.wat`, alice holding 25; books whose spend is
/// authorized by the holder its op names.
fn payloads() -> Harness<Spend> {
    let text = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/res/payload_ledger.wat"),
    );
    let world = Fixture::new()
        .account("alice.near", Balance::from_near(10))
        .account("relayer.near", Balance::from_near(100))
        .contract(PAYLOADS, ::wat::parse_str(text.unwrap()).unwrap())
        .setup("alice holds 25", |sim| {
            let args = [&25u64.to_le_bytes()[..], b"alice.near"].concat();
            sim.call(PAYLOADS, PAYLOADS, "set").args(args).transact().assert_success();
        })
        .build();
    let books = near_contract_sim::explore::Books::new()
        .liability("credit", |sim: &Sim| {
            std::collections::BTreeMap::from([(
                "alice.near".into(),
                credit(sim, "alice.near").into(),
            )])
        })
        .owned("credit")
        .authorized(|d| match d.op() {
            Spend::As { .. } => Some(vec!["alice.near".into()]),
            Spend::For { holder, .. } => Some(vec![holder.clone()]),
        });
    Harness::new(world).books(books)
}

/// A fresh callers copy of a relayed op (the entry call, from a fresh
/// signer each) is copy k of the op (`Op::copy`): its payload names a
/// fresh holder where the op names one as an account id, and it is the
/// op's for `authorized`, so copies that spend alice's credit before the
/// op itself skips (short) don't read as "alice signed no op"; a copy
/// whose op skips skips too. An op naming its user by an enum can't have
/// its payload's signer varied, and no to-do says so:
/// which ops carry a signed payload isn't detectable, so the docs state
/// the limit instead.
#[test]
fn fresh_callers_copies_go_through_the_op() {
    let h = payloads();
    let axis = |plan: &Plan<Spend>| {
        let axes = h.axes(plan);
        let found = axes.into_iter().find(|(_, a)| a.name.starts_with("repeat fresh callers of"));
        found.unwrap_or_else(|| panic!("no fresh callers axis: {:?}", h.axes(plan))).1
    };
    let spent = |run: &near_contract_sim::explore::Run<Spend>| -> Vec<String> {
        let entries = run.outcomes().iter().filter_map(|o| o.receipts.first());
        let spends = entries.filter(|r| r.method_names == ["spend"]);
        spends.map(|r| String::from_utf8_lossy(&r.args[0][8..]).into_owned()).collect()
    };
    let named = Plan::new().op(Spend::For { holder: "alice.near".into(), amount: 10 });
    let run = h.run(&h.moved(&named, 0, &axis(&named), 2).unwrap());
    assert_eq!(spent(&run), ["alice-0.near", "alice-1.near", "alice.near"], "{run}");
    let user = Plan::new().op(Spend::As { user: User::Alice, amount: 10 });
    let run = h.run(&h.moved(&user, 0, &axis(&user), 3).unwrap());
    // Two copies spend 20 of her 25; the third, and the op, skip (short).
    assert_eq!(spent(&run), ["alice.near"; 2], "{run}");
    assert!(run.findings().iter().all(|f| f.check != OWNED), "{run}");
    let audit = h.curve(&user, 0, &axis(&user).name).run();
    let todo = audit.todo();
    assert!(todo.iter().all(|t| !t.id.starts_with("fresh-signer:")), "{todo:#?}");
}

/// Calls `nonces.near::use` with its input.
const RELAY_WAT: &str = r#"(module
  (import "env" "input" (func $input (param i64)))
  (import "env" "register_len" (func $len (param i64) (result i64)))
  (import "env" "read_register" (func $read (param i64 i64)))
  (import "env" "promise_create"
    (func $create (param i64 i64 i64 i64 i64 i64 i64 i64) (result i64)))
  (memory 1)
  (export "memory" (memory 0))
  (data (i32.const 0) "nonces.near")
  (data (i32.const 16) "use")
  (func (export "relay")
    (call $input (i64.const 0))
    (call $read (i64.const 0) (i64.const 1024))
    (drop (call $create (i64.const 11) (i64.const 0) (i64.const 3) (i64.const 16)
                        (call $len (i64.const 0)) (i64.const 1024) (i64.const 64)
                        (i64.const 20000000000000)))))"#;

/// Uses `nonce` through the relay; with `resign`, each copy a nonce of
/// its own.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct Relayed {
    nonce: u64,
    resign: bool,
}

impl Op for Relayed {
    fn submit(&self, sim: &mut Sim) -> Result<(), Skip> {
        sim.call("alice.near", "relay.near", "relay").args(self.nonce.to_le_bytes()).submit();
        Ok(())
    }

    fn copy(&self, k: usize) -> Self {
        let nonce = if self.resign { self.nonce + 1 + k as u64 } else { self.nonce };
        Relayed { nonce, ..self.clone() }
    }
}

/// A repeat of a call the op's receipts made (`nonces.near::use`, sent
/// directly) is copy k of the op: each carries the nonce `Op::copy` gives
/// it, so the row climbs instead of stopping at the first reuse.
#[test]
fn a_repeated_inner_call_carries_each_copys_nonce() {
    let text = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/res/nonce_box.wat"),
    );
    let f = Fixture::new()
        .account("alice.near", Balance::from_near(100))
        .contract(NONCES, ::wat::parse_str(text.unwrap()).unwrap())
        .contract("relay.near", ::wat::parse_str(RELAY_WAT).unwrap());
    let h = Harness::new(f.build());
    let axis = "repeat nonces.near::use";
    let plan = |resign| Plan::new().op(Relayed { nonce: 7, resign });
    let used = |resign| -> Vec<u64> {
        let (_, a) = h.axes(&plan(resign)).into_iter().find(|(_, a)| a.name == axis).unwrap();
        let run = h.run(&h.moved(&plan(resign), 0, &a, 3).unwrap());
        let receipts = run.outcomes().iter().flat_map(|o| o.receipts.clone()).collect::<Vec<_>>();
        let ok = receipts.iter().filter(|r| r.method_names == ["use"] && r.failure().is_none());
        ok.map(|r| u64::from_le_bytes(r.args[0][..8].try_into().unwrap())).collect()
    };
    assert_eq!(used(true), [8, 9, 10, 7]);
    assert_eq!(used(false), [7], "one nonce, used once");
}

/// A world that keeps the holders its ops registered, as a harness's own
/// bookkeeping does.
#[derive(Clone)]
struct Registry {
    sim: Sim,
    holders: Vec<String>,
}

impl AsRef<Sim> for Registry {
    fn as_ref(&self) -> &Sim {
        &self.sim
    }
}

impl AsMut<Sim> for Registry {
    fn as_mut(&mut self) -> &mut Sim {
        &mut self.sim
    }
}

/// The relayer sets `holder`'s credit; the op registers the holder.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct Register {
    holder: AccountId,
    amount: u64,
}

impl Op<Registry> for Register {
    fn submit(&self, w: &mut Registry) -> Result<(), Skip> {
        w.holders.push(self.holder.to_string());
        let args = [&self.amount.to_le_bytes()[..], self.holder.as_bytes()].concat();
        w.sim.call("relayer.near", PAYLOADS, "set").args(args).submit();
        Ok(())
    }
}

/// A fresh callers copy whose op names its holder by account id runs
/// through the op's own `submit`, so the world's
/// bookkeeping sees every copy: no holder on the ledger goes unregistered,
/// and each printed step is the copy that ran, not the witness's call.
#[test]
fn a_fresh_callers_copy_runs_through_the_op() {
    let text = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/res/payload_ledger.wat"),
    );
    let sim = Fixture::new()
        .account("alice.near", Balance::from_near(10))
        .account("relayer.near", Balance::from_near(100))
        .contract(PAYLOADS, ::wat::parse_str(text.unwrap()).unwrap())
        .build();
    let check = "every holder is registered";
    let h = Harness::new(Registry { sim, holders: vec![] }).at_rest(check, |w: &Registry| {
        let records = w.sim.state_records([PAYLOADS]);
        let keys = records.into_iter().filter_map(|r| match r {
            near_contract_sim::StateRecord::Data { data_key, .. } => Some(data_key.to_vec()),
            _ => None,
        });
        match keys
            .map(|k| String::from_utf8_lossy(&k).into_owned())
            .find(|k| !w.holders.contains(k))
        {
            Some(k) => Err(format!("{k} holds credit unregistered")),
            None => Ok(()),
        }
    });
    let plan = Plan::new().op(Register { holder: "alice.near".parse().unwrap(), amount: 5 });
    let axes = h.axes(&plan);
    let (_, axis) = axes
        .iter()
        .find(|(_, a)| a.name.starts_with("repeat fresh callers of"))
        .unwrap_or_else(|| panic!("no fresh callers axis: {axes:?}"));
    let run = h.run(&h.moved(&plan, 0, axis, 2).unwrap());
    assert!(run.findings().is_empty(), "{run}");
    let shown = run.to_string();
    assert!(shown.contains("copy 1 of ") && shown.contains("alice-1.near"), "{shown}");
    assert!(shown.contains("from fresh callers"), "{shown}");
    let audit = h.curve(&plan, 0, &axis.name).run();
    assert!(audit.found().iter().all(|f| f.finding.check != check), "{audit}");
}

/// `PayOp`, its axes narrowed by `Axes::only` to those `names` start.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct Narrow {
    pay: PayOp,
    names: Vec<String>,
}

impl Op for Narrow {
    fn submit(&self, sim: &mut Sim) -> Result<(), Skip> {
        self.pay.submit(sim)
    }

    fn axes(&self) -> near_contract_sim::explore::Axes {
        near_contract_sim::explore::Axes::only(self.names.iter().map(String::as_str))
    }
}

/// `Axes::only` narrows the receipt-derived axes too:
/// with pointers alone, no repeat, tail or fresh callers axis; a named
/// one (`repeat fresh callers`) comes back, alone of its kind; the
/// caller's gas stays.
#[test]
fn axes_only_narrows_repeats_and_tails() {
    let h = Harness::new(world());
    let pay = witness().steps[0].act.op().unwrap().clone();
    let axes = |names: &[&str]| {
        let names = names.iter().map(|n| n.to_string()).collect();
        let plan = Plan::new().op(Narrow { pay: pay.clone(), names });
        h.axes(&plan).into_iter().map(|(_, a)| (a.kind, a.name)).collect::<Vec<_>>()
    };
    let copies = |k: &AxisKind| matches!(k, AxisKind::Repeat | AxisKind::HoldTail);
    let all = Harness::new(world()).axes(&witness());
    assert!(all.iter().any(|(_, a)| copies(&a.kind)), "{all:?}");
    let pointers = axes(&["/pay/Pay/to"]);
    assert!(pointers.iter().all(|(k, _)| !copies(k)), "{pointers:?}");
    assert!(pointers.iter().any(|(_, n)| n == "/pay/Pay/to"), "{pointers:?}");
    let named = axes(&["/pay/Pay/to", "repeat fresh callers"]);
    let repeats: Vec<_> = named.iter().filter(|(k, _)| copies(k)).collect();
    assert!(!repeats.is_empty(), "{named:?}");
    assert!(repeats.iter().all(|(_, n)| n.starts_with("repeat fresh callers")), "{named:?}");
}

/// Deposits 500 NEAR from `from`.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct Big {
    from: AccountId,
}

impl Op for Big {
    fn submit(&self, sim: &mut Sim) -> Result<(), Skip> {
        let call = sim.call(self.from.as_str(), "sink.near", "take");
        call.deposit(Balance::from_near(500)).submit();
        Ok(())
    }
}

/// A fresh caller starts with what the witness's signer held: alice
/// holds 1000 NEAR, so each fresh copy's 500 NEAR deposit goes through,
/// as hers does.
#[test]
fn fresh_callers_are_funded_as_the_witness_signer() {
    let sim = Fixture::new()
        .account("alice.near", Balance::from_near(1000))
        .stub("sink.near", Stub::new().on("take", Reply::json(())))
        .build();
    let h = Harness::new(sim);
    let plan = Plan::new().op(Big { from: "alice.near".parse().unwrap() });
    let axes = h.axes(&plan);
    let (_, axis) = axes
        .iter()
        .find(|(_, a)| a.name.starts_with("repeat fresh callers of"))
        .unwrap_or_else(|| panic!("no fresh callers axis: {axes:?}"));
    let run = h.run(&h.moved(&plan, 0, axis, 3).unwrap());
    assert_eq!(run.outcomes().len(), 4, "{run}");
    assert!(run.outcomes().iter().all(|o| o.failure().is_none()), "{run}");
}

/// Who pays, as the whole op: an op enum of unit variants.
#[derive(Clone, Debug, Serialize, Deserialize)]
enum WhoOp {
    AlicePays,
    BobPays,
}

impl Op for WhoOp {
    fn submit(&self, sim: &mut Sim) -> Result<(), Skip> {
        let signer = if matches!(self, WhoOp::AlicePays) { "alice.near" } else { "bob.near" };
        sim.call(signer, PAYER, "pay").args_json(json!({})).submit();
        Ok(())
    }
}

/// An op enum's variant is a choice at the root: the whole op replaced.
/// Its axis and the label of what a scan shows there name it "the op",
/// never a blank.
///
/// Verification boundary: the names a scan gives its variants.
#[test]
fn a_whole_op_replaced_is_labeled_the_op() {
    let h = Harness::new(world()).at_rest("only alice pays", |sim: &Sim| {
        match sim.calls_to(PAYER).iter().all(|c| c.signer_id.as_str() == "alice.near") {
            true => Ok(()),
            false => Err("bob paid"),
        }
    });
    let witness = Plan::new().op(WhoOp::AlicePays);
    let axes = h.axes(&witness);
    assert!(axes.iter().all(|(_, a)| !a.name.is_empty()), "{axes:#?}");
    let root = axes.iter().find(|(_, a)| a.name == "the op").expect("a root choice");
    assert_eq!(root.1.kind, AxisKind::Choice, "{axes:#?}");
    let audit = h.scan(&[witness]).run();
    let labels: Vec<String> = audit.found().iter().map(|f| f.label.to_string()).collect();
    assert!(labels.contains(&"shown at step 0, the op = BobPays".to_owned()), "{audit}");
}

/// Pays from `from`: another account, or the payer itself.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct PayFrom(String);

impl Op for PayFrom {
    fn submit(&self, sim: &mut Sim) -> Result<(), Skip> {
        sim.call(self.0.as_str(), PAYER, "pay").args_json(json!({})).submit();
        Ok(())
    }
}

/// Fresh callers are other accounts calling in: a step whose call the
/// contract made to itself has none to vary.
#[test]
fn fresh_callers_come_from_another_account() {
    let fresh = |from: &str| {
        let axes = Harness::new(world()).axes(&Plan::new().op(PayFrom(from.into())));
        axes.iter().any(|(_, a)| a.name.starts_with("repeat fresh callers of payer.near::pay"))
    };
    assert!(fresh("alice.near"));
    assert!(!fresh(PAYER));
}
