//! Margin on a callback whose gas budget is fixed when it is scheduled
//! (`budget_ledger.wat`): `on_put` gets 4 Tgas + 2 Tgas an id, and costs
//! more with each id's bytes and, once entries were appended while it
//! waited, with the bytes of the entry it moves. Neither alone takes it
//! under the margin; together they do. `margin_registry.wat`'s `resolve`
//! goes under 25% only with its ids' bytes, a tail and the trie shaper
//! (sibling keys forking the trie along the keys it writes) together.
//!
//! Verification boundary: curves' margin targets, legs and combined
//! ascent, over actual simulator runs.

mod common;

use common::manifest_dir;
use near_contract_sim::explore::{Audit, Harness, Op, Plan, Resource, Skip, Status};
use near_contract_sim::{Balance, Fault, Fixture, Receipts, Sim, StateRecord};
use serde::{Deserialize, Serialize};

const LEDGER: &str = "ledger.near";
const ON_PUT: &str = "ledger.near::on_put";

/// How many cases `audit` ran and what it found, each as it printed:
/// pinned so that copies and axes made faster must judge exactly as
/// before.
fn pinned<O>(audit: &Audit<O>) -> (u32, Vec<String>) {
    let found = audit
        .found()
        .iter()
        .map(|x| format!("{} at {}: {}", x.finding.check, x.finding.site, x.label));
    (audit.cases(), found.collect())
}

#[derive(Clone, Debug, Serialize, Deserialize)]
enum LedgerOp {
    Put { ids: Vec<String> },
}

impl Op for LedgerOp {
    fn submit(&self, sim: &mut Sim) -> Result<(), Skip> {
        let LedgerOp::Put { ids } = self;
        sim.call("alice.near", LEDGER, "put").args(ids.join("\n").into_bytes()).submit();
        Ok(())
    }
}

fn harness() -> Harness<LedgerOp> {
    harness_of(
        &std::fs::read_to_string(manifest_dir().join("tests/res/budget_ledger.wat")).unwrap(),
    )
}

fn harness_of(wat: &str) -> Harness<LedgerOp> {
    let world = Fixture::new()
        .account("alice.near", Balance::from_near(100))
        .contract(LEDGER, wat::parse_str(wat).unwrap())
        .build();
    Harness::new(world).at_rest("every entry is settled", |sim: &Sim| {
        let left = sim.view_raw(LEDGER, "unsettled", vec![]).result.map_err(|e| e.to_string())?;
        match u64::from_le_bytes(left.try_into().map_err(|_| "not a u64")?) {
            0 => Ok(()),
            n => Err(format!("{n} entries unsettled")),
        }
    })
}

fn witness() -> Plan<LedgerOp> {
    Plan::new().op(LedgerOp::Put { ids: vec!["key-0001".into(), "key-0002".into()] })
}

/// A receipt that awaits nothing is no margin target: `on_put` called
/// plainly, not after `noop`, still leaves entries unsettled when forced
/// out of gas and still has its gas curves, but its margin is no finding.
#[test]
fn a_plain_call_is_no_margin_target() {
    let wat = std::fs::read_to_string(manifest_dir().join("tests/res/budget_ledger.wat")).unwrap();
    let plain = wat.replace(
        "(call $promise_then (local.get $p) (local.get $own)",
        "(call $promise_create (local.get $own)",
    );
    assert_ne!(plain, wat);
    let h = harness_of(&plain);
    h.run(&witness()).assert_clean();
    let starved = witness().inject(Receipts::call(LEDGER, "on_put"), Fault::OutOfGas);
    h.run(&starved).assert_found("every entry is settled", "world");
    let audit = h.curves(&[witness()]).run();
    audit.assert_curve(ON_PUT, Resource::ReceiptGas, "/Put/ids/*");
    assert!(audit.found_at("gas margin", ON_PUT).is_none(), "{audit}");
}

/// `on_put` is a margin target (a forced `OutOfGas` there leaves
/// entries unsettled); along each id's bytes and along a tail during its hold its
/// margin falls, unpaid, but stays over 25%; moved together they take it
/// under, a finding found by margin, with a replay that shows it.
#[test]
fn a_budget_unpaid_along_two_axes_is_found_by_margin() {
    let h = harness();
    h.run(&witness()).assert_clean();
    let audit = h.curves(&[witness()]).run();
    eprintln!("{audit}");
    let bytes = audit.assert_curve(ON_PUT, Resource::ReceiptGas, "/Put/ids/*");
    let tail = "Put (step 0) while ledger.near::on_put is held";
    let held = audit.assert_curve(ON_PUT, Resource::ReceiptGas, tail);
    for row in [bytes, held] {
        let margins = row.margins();
        let (first, worst) = (margins[0].1, margins.iter().map(|m| m.1).min().unwrap());
        assert!(worst < first, "unpaid: {row}");
        let limit = row.limit as i128;
        assert!(worst * 100 >= 25 * limit, "{worst} of {limit} alone: {row}");
    }
    let row = audit.assert_found("gas margin", ON_PUT);
    // Its own kind: nothing failed.
    let report = audit.report();
    let kinds: Vec<&str> = report
        .findings
        .iter()
        .filter(|r| r.check == "gas margin")
        .map(|r| r.kind.as_str())
        .collect();
    assert_eq!(kinds, ["gas margin"], "{audit}");
    let message = &row.finding.message;
    assert!(message.contains("found by margin"), "{message}");
    assert!(message.contains("/Put/ids/*") && message.contains(tail), "{message}");
    let path = row.replay.as_ref().unwrap_or_else(|| panic!("no replay: {audit}"));
    let regressions = h.clone().store(path.parent().unwrap()).regressions();
    let mine = regressions.iter().find(|r| r.path == *path).expect("the replay");
    assert!(matches!(mine.status, Status::Shows(_)), "{message}");
}

/// `step.near::put(n)` schedules `on_step` with 8 Tgas; `on_step` writes
/// 8 keys once `n` has 3 digits, else nothing: its margin drops once,
/// by more than 1% of its budget, and is flat on either side.
const STEP: &str = r#"(module
  (import "env" "input" (func $input (param i64)))
  (import "env" "register_len" (func $len (param i64) (result i64)))
  (import "env" "read_register" (func $read (param i64 i64)))
  (import "env" "current_account_id" (func $me (param i64)))
  (import "env" "promise_batch_create" (func $create (param i64 i64) (result i64)))
  (import "env" "promise_batch_action_function_call"
    (func $call (param i64 i64 i64 i64 i64 i64 i64)))
  (import "env" "storage_write" (func $write (param i64 i64 i64 i64 i64) (result i64)))
  (memory 1)
  (export "memory" (memory 0))
  (data (i32.const 0) "on_step")
  (func (export "put") (local $n i64) (local $id i64)
    (call $input (i64.const 0))
    (local.set $n (call $len (i64.const 0)))
    (call $read (i64.const 0) (i64.const 256))
    (call $me (i64.const 1))
    (local.set $id (call $len (i64.const 1)))
    (call $read (i64.const 1) (i64.const 128))
    (call $call (call $create (local.get $id) (i64.const 128))
      (i64.const 7) (i64.const 0) (local.get $n) (i64.const 256)
      (i64.const 512) (i64.const 8000000000000)))
  (func (export "on_step") (local $i i64)
    (call $input (i64.const 0))
    (if (i64.gt_u (call $len (i64.const 0)) (i64.const 2))
      (then (loop $more
        (i64.store (i32.const 600) (local.get $i))
        (drop (call $write (i64.const 8) (i64.const 600) (i64.const 8) (i64.const 600)
                           (i64.const 2)))
        (local.set $i (i64.add (local.get $i) (i64.const 1)))
        (br_if $more (i64.lt_u (local.get $i) (i64.const 8))))))))"#;

#[derive(Clone, Debug, Serialize, Deserialize)]
enum StepOp {
    Put { n: u32 },
}

impl Op for StepOp {
    fn submit(&self, sim: &mut Sim) -> Result<(), Skip> {
        let StepOp::Put { n } = self;
        sim.call("alice.near", "step.near", "put").args(n.to_string()).submit();
        Ok(())
    }
}

/// An axis whose margin drops once and is flat on either side is not
/// unpaid: unpaid is a margin that keeps falling as the axis climbs.
#[test]
fn a_one_step_drop_is_not_unpaid() {
    let world = Fixture::new()
        .account("alice.near", Balance::from_near(100))
        .contract("step.near", wat::parse_str(STEP).unwrap())
        .build();
    let h = Harness::<StepOp>::new(world).must_not_fail("on_step");
    let witness = Plan::new().op(StepOp::Put { n: 1 });
    h.run(&witness).assert_clean();
    let audit = h.curves(&[witness]).run();
    eprintln!("{audit}");
    let row = audit.assert_curve("step.near::on_step", Resource::ReceiptGas, "/Put/n");
    let margins = row.margins();
    let drops = margins.windows(2).filter(|w| w[0].1 - w[1].1 > row.limit as i128 / 100);
    assert_eq!(drops.count(), 1, "one step: {row}");
    assert!(audit.found_at("gas margin", "step.near::on_step").is_none(), "{audit}");
}

/// `keep.near::put(n)` schedules `on_keep` with a flat 100 Tgas;
/// `on_keep` writes `n` keys: its cost grows with each, its budget doesn't.
const KEEP: &str = r#"(module
  (import "env" "input" (func $input (param i64)))
  (import "env" "register_len" (func $len (param i64) (result i64)))
  (import "env" "read_register" (func $read (param i64 i64)))
  (import "env" "current_account_id" (func $me (param i64)))
  (import "env" "promise_batch_create" (func $create (param i64 i64) (result i64)))
  (import "env" "promise_batch_action_function_call"
    (func $call (param i64 i64 i64 i64 i64 i64 i64)))
  (import "env" "storage_write" (func $write (param i64 i64 i64 i64 i64) (result i64)))
  (memory 1)
  (export "memory" (memory 0))
  (data (i32.const 0) "on_keep")
  (func (export "put") (local $id i64)
    (call $input (i64.const 0))
    (call $read (i64.const 0) (i64.const 1024))
    (call $me (i64.const 1))
    (local.set $id (call $len (i64.const 1)))
    (call $read (i64.const 1) (i64.const 512))
    (call $call (call $create (local.get $id) (i64.const 512))
      (i64.const 7) (i64.const 0) (i64.const 1) (i64.const 1024)
      (i64.const 0) (i64.const 100000000000000)))
  (func (export "on_keep") (local $i i64)
    (call $input (i64.const 0))
    (call $read (i64.const 0) (i64.const 1024))
    (block $done (loop $more
      (br_if $done (i64.ge_u (local.get $i) (i64.load8_u (i32.const 1024))))
      (i64.store8 (i32.const 600) (local.get $i))
      (drop (call $write (i64.const 1) (i64.const 600) (i64.const 8) (i64.const 1024)
                         (i64.const 2)))
      (local.set $i (i64.add (local.get $i) (i64.const 1)))
      (br $more)))))"#;

#[derive(Clone, Debug, Serialize, Deserialize)]
enum KeepOp {
    Put { n: u8 },
}

impl Op for KeepOp {
    fn submit(&self, sim: &mut Sim) -> Result<(), Skip> {
        let KeepOp::Put { n } = self;
        sim.call("alice.near", "keep.near", "put").args(vec![*n]).submit();
        Ok(())
    }
}

/// A cost that grows along an axis while the budget stays flat, though
/// the margin stays over 25%, is labelled as such, with where the cost
/// would meet the budget; never "keeps 60% … under 61%".
#[test]
fn a_growing_cost_on_a_flat_budget_is_labelled_with_its_projection() {
    let world = Fixture::new()
        .account("alice.near", Balance::from_near(100))
        .contract("keep.near", wat::parse_str(KEEP).unwrap())
        .build();
    let h = Harness::<KeepOp>::new(world).must_not_fail("on_keep");
    let witness = Plan::new().op(KeepOp::Put { n: 2 });
    h.run(&witness).assert_clean();
    let audit = h.curves(&[witness]).run();
    eprintln!("{audit}");
    let row = audit.assert_found("gas margin", "keep.near::on_keep");
    let message = &row.finding.message;
    assert!(message.contains("its cost growing faster than its budget"), "{message}");
    assert!(!message.contains("Tgas, under"), "{message}");
    let leg = "/Put/n (number) at step 0";
    assert!(
        message.contains(leg) && message.contains("cost grows, budget flat, meets it near"),
        "{message}"
    );
}

/// `margin_registry.wat`: `resolve` gets 3 Tgas + 1 Tgas an id and writes
/// "e" + each queued id, its own and those queued while it waited. The
/// trie shaper plants keys forking the trie along each key it writes.
mod registry {
    use super::*;

    const REGISTRY: &str = "registry.near";
    pub(super) const RESOLVE: &str = "registry.near::resolve";

    #[derive(Clone, Debug, Serialize, Deserialize)]
    pub(super) struct Put {
        pub ids: Vec<String>,
    }

    impl Op for Put {
        fn submit(&self, sim: &mut Sim) -> Result<(), Skip> {
            let args = serde_json::json!({ "ids": self.ids }).to_string();
            sim.call("alice.near", "front.near", "put").args(args.into_bytes()).submit();
            Ok(())
        }
    }

    pub(super) fn harness() -> Harness<Put> {
        auditing(REGISTRY)
    }

    fn auditing(audited: &str) -> Harness<Put> {
        let path = manifest_dir().join("tests/res/margin_registry.wat");
        let wasm = wat::parse_str(std::fs::read_to_string(path).unwrap()).unwrap();
        let world = Fixture::new()
            .account("alice.near", Balance::from_near(100))
            .contract("front.near", wasm.clone())
            .contract(REGISTRY, wasm)
            .build();
        Harness::new(world).must_not_fail("resolve").audits([audited]).at_rest(
            "every entry is settled",
            |sim: &Sim| {
                let left = sim.view_raw(REGISTRY, "unsettled", vec![]).result;
                let left = left.map_err(|e| e.to_string())?;
                match u64::from_le_bytes(left.try_into().map_err(|_| "not a u64")?) {
                    0 => Ok(()),
                    n => Err(format!("{n} entries unsettled")),
                }
            },
        )
    }

    pub(super) fn witness() -> Plan<Put> {
        Plan::new().op(Put { ids: vec!["key-0001".into(), "key-0002".into()] })
    }

    /// What `resolve` keeps of its attached gas, in percent, in a run of
    /// the witness on `sim`, and the keys it wrote.
    fn kept(sim: &mut Sim) -> (u64, Vec<(near_contract_sim::AccountId, Vec<u8>)>) {
        let args = serde_json::json!({ "ids": ["key-0001", "key-0002"] }).to_string();
        let out = sim.call("alice.near", "front.near", "put").args(args.into_bytes()).transact();
        let r = out.expect_receipt("resolve");
        let used = r.gas_burnt.as_gas() - r.prepaid_exec_fees.as_gas();
        let written = r.storage_ops().written.into_iter().map(|w| (w.account, w.key)).collect();
        ((r.attached_gas.as_gas() - used) * 100 / r.attached_gas.as_gas(), written)
    }

    /// Alone, each axis (the ids' bytes, a tail of calls queued while
    /// `resolve` waits, the list's length) and the trie shaper leave
    /// `resolve` over 25%; the ascent with all of them takes it under, and
    /// under 25% only with the shaper: a margin finding whose message gives
    /// both, with a replay that plants the siblings.
    #[test]
    fn the_shaper_takes_a_combined_margin_under_where_no_axis_alone_does() {
        let h = harness();
        h.run(&witness()).assert_clean();
        let audit = h.curves(&[witness()]).run();
        eprintln!("{audit}");
        let found = [
            "every entry is settled at world: shown at step 0, /ids/* = 2048",
            "every entry is settled at world: shown at step 0, /args/json/ids/* = 2048",
            "must not fail at registry.near::resolve (Put): shown at step 0, /ids/* = 2048",
            "must not fail at registry.near::resolve (on_receive): shown at step 0, \
             /args/json/ids/* = 2048",
            "must not fail at registry.near::resolve (Put): shown at margin of \
             registry.near::resolve",
            "gas margin at registry.near::resolve (Put): shown at margin of registry.near::resolve",
            "gas margin at registry.near::resolve (on_receive): forced (Hold(130) at \
             registry.near::resolve)",
        ];
        assert_eq!(pinned(&audit), (80, found.map(String::from).to_vec()), "{audit}");
        let bytes = audit.assert_curve(RESOLVE, Resource::ReceiptGas, "/ids/*");
        let tail = "registry.near::on_receive while registry.near::resolve is held";
        let held = audit.assert_curve(RESOLVE, Resource::ReceiptGas, tail);
        for row in [bytes, held] {
            let worst = row.margins().iter().map(|m| m.1).min().unwrap();
            assert!(worst * 100 >= 25 * row.limit as i128, "alone: {row}");
        }
        let length = audit.assert_curve(RESOLVE, Resource::ReceiptGas, "/ids");
        let m = length.margins();
        assert!(m.last().unwrap().1 > m[0].1, "the length is paid: {length}");
        // The shaper alone, at the witness.
        let mut sim = h.fixture().clone();
        let (clean, written) = kept(&mut sim);
        let mut shaped = h.fixture().clone();
        let planted =
            written.iter().flat_map(|(a, k)| near_contract_sim::explore::trie_siblings(a, k));
        shaped.apply_state_records(planted.map(|(account_id, key)| StateRecord::Data {
            account_id,
            data_key: key.into(),
            value: vec![1].into(),
        }));
        let (alone, _) = kept(&mut shaped);
        assert!(alone < clean && alone >= 25, "shaped {alone}%, clean {clean}%");
        eprintln!("the shaper alone: resolve keeps {alone}%, {clean}% on a clean trie");
        // Together: under 25%, and only with the shaper.
        let f = audit.assert_found("gas margin", RESOLVE);
        let (head, tail) = f.finding.message.split_once("% with a clean trie, ").unwrap();
        let number = |t: &str| t.parse::<u64>().unwrap();
        let clean = number(head.rsplit(|c: char| !c.is_ascii_digit()).next().unwrap());
        let shaped = number(tail.split('%').next().unwrap());
        assert!(clean >= 25 && shaped < 25, "{}", f.finding.message);
        let replay = audit.found().iter().find(|x| x.finding.check == "gas margin").unwrap();
        let saved = serde_json::to_string(replay.run.plan()).unwrap();
        assert!(saved.contains("\"planted\""), "{saved}");
        h.run(replay.run.plan()).assert_found("gas margin", RESOLVE);
    }

    /// With only `front.near` audited, `registry.near` is a counterparty:
    /// the shaper plants nothing in its state, so no plan
    /// carries planted keys and no margin is shown by them.
    #[test]
    fn the_shaper_plants_only_in_audited_contracts() {
        let audit = auditing("front.near").curves(&[witness()]).run();
        for x in audit.found() {
            let plan = serde_json::to_string(x.run.plan()).unwrap();
            assert!(!plan.contains("\"planted\""), "{}: {plan}", x.label);
        }
        let shaped = |x: &&near_contract_sim::explore::Found<Put>| {
            x.finding.check == "gas margin" && x.label.to_string().starts_with("shown at margin")
        };
        assert!(!audit.found().iter().any(|x| shaped(&x)), "{audit}");
        // The ascent ran there: `/ids/*` is unpaid, a margin finding.
        assert!(audit.found_at("gas margin", RESOLVE).is_some(), "{audit}");
    }
}

/// `refund_book.wat`: `resolve` gets 3 Tgas + 0.2 Tgas an id and, for each
/// id the sink gives back, removes its entry, logs it and writes a record:
/// a cost per distinct id that a copy of one id doesn't pay, and that only
/// the answer giving everything back reaches.
mod book {
    use super::*;
    use near_contract_sim::{Reply, Stub};

    const BOOK: &str = "book.near";
    const RESOLVE: &str = "book.near::resolve";

    #[derive(Clone, Debug, Serialize, Deserialize)]
    struct Deposit {
        ids: Vec<String>,
        sent: Vec<String>,
    }

    impl Op for Deposit {
        fn submit(&self, sim: &mut Sim) -> Result<(), Skip> {
            sim.call("alice.near", BOOK, "deposit").args_json(self).submit();
            Ok(())
        }
    }

    fn harness() -> Harness<Deposit> {
        let path = manifest_dir().join("tests/res/refund_book.wat");
        let wasm = wat::parse_str(std::fs::read_to_string(path).unwrap()).unwrap();
        let mut world = Fixture::new()
            .account("alice.near", Balance::from_near(100))
            .contract(BOOK, wasm)
            .build();
        world.deploy_stub("sink.near", Stub::new().on("on_deposit", Reply::json(["0"])));
        Harness::new(world).must_not_fail("resolve").audits([BOOK])
    }

    fn deposit(ids: &[&str]) -> Plan<Deposit> {
        let sent = vec!["1000".to_owned(); ids.len()];
        Plan::new().op(Deposit { ids: ids.iter().map(|s| s.to_string()).collect(), sent })
    }

    /// What `resolve` keeps of its attached gas, in percent, in `plan`'s run.
    fn kept(h: &Harness<Deposit>, plan: &Plan<Deposit>) -> u64 {
        let run = h.run(plan);
        let r = run.outcomes()[0].expect_receipt("resolve");
        let used = r.gas_burnt.as_gas() - r.prepaid_exec_fees.as_gas();
        (r.attached_gas.as_gas() - used) * 100 / r.attached_gas.as_gas()
    }

    /// The curves' own witness, where the sink gives everything back,
    /// grows `/ids` with distinct ids, each an entry removed: its margin
    /// falls (with the sink keeping all, or with copies of one id, it
    /// doesn't), and the ascent takes it under 25% along that length.
    #[test]
    fn a_cost_per_distinct_entry_removed_is_found_along_distinct_ids() {
        let h = harness();
        h.run(&deposit(&["id-0001"])).assert_clean();
        let audit = h.curves(&[deposit(&["id-0001"])]).run();
        eprintln!("{audit}");
        let found = [
            "failed callback at book.near::resolve (Deposit): shown at step 0, /ids = 52",
            "gas margin at book.near::resolve (Deposit): shown at margin of book.near::resolve",
        ];
        // 79 cases: the stub's `on_deposit` (unaudited) is no repeat axis.
        assert_eq!(pinned(&audit), (79, found.map(String::from).to_vec()), "{audit}");
        let rows = audit.curves().iter().filter(|r| {
            r.site == RESOLVE && r.resource == Resource::ReceiptGas && r.axis.name == "/ids"
        });
        let (back, keeps): (Vec<_>, Vec<_>) =
            rows.partition(|r| !r.plan.steps.last().unwrap().answers.is_empty());
        let falls = |r: &&near_contract_sim::explore::CurveRow<Deposit>| {
            let m = r.margins();
            m.last().unwrap().1 < m[0].1
        };
        assert!(!back.is_empty() && back.iter().all(falls), "{audit}");
        assert!(!keeps.is_empty() && !keeps.iter().any(falls), "{audit}");
        let f = audit.assert_found("gas margin", RESOLVE);
        let step = &f.run.plan().steps[0];
        let near_contract_sim::explore::Act::Op(op) = &step.act else { panic!("{step:?}") };
        let mut ids = op.ids.clone();
        ids.dedup();
        assert!(ids.len() > 8 && ids.len() == op.ids.len(), "distinct ids: {:?}", op.ids);
        let back = step.answers[0].json.as_array().unwrap();
        assert!(back.len() == op.ids.len() && !back.contains(&"0".into()), "gives back");
        // The same length, copies of one id: paid.
        let mut copies = f.run.plan().clone();
        if let near_contract_sim::explore::Act::Op(op) = &mut copies.steps[0].act {
            op.ids = vec![op.ids[0].clone(); op.ids.len()];
        }
        assert!(kept(&h, &copies) >= 25, "copies of one id keep 25% or more");
        h.run(f.run.plan()).assert_found("gas margin", RESOLVE);
        // The scan tries the answer too.
        let scan = h.scan(&[deposit(&["id-0001"])]).run();
        let tried =
            scan.report().timings.iter().any(|t| t.0.contains("gives back all it was sent"));
        assert!(tried, "{scan}");
    }
}
