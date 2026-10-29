//! A finding's root cause is the first step after which its check
//! fails, built in. `bank.near` keeps two counters, `a` and `b`;
//! `pair` adds to both, `skew` to `a` alone (silently breaking `a ==
//! b`), `skew_b` to `b` alone, and `boom` panics, refunding the deposit
//! it was sent: a loud value failure.
//!
//! Verification boundary: the breaking step and the per-cause split, over
//! actual runs of the specimen (prefixes of the case re-executed).

use near_contract_sim::explore::{Fail, Faults, Harness, Op, Plan, Skip};
use near_contract_sim::{Balance, Fixture, Sim};
use serde::{Deserialize, Serialize};

const WAT: &str = r#"
(module
  (import "env" "storage_read" (func $read (param i64 i64 i64) (result i64)))
  (import "env" "read_register" (func $register (param i64 i64)))
  (import "env" "storage_write" (func $write (param i64 i64 i64 i64 i64) (result i64)))
  (import "env" "panic_utf8" (func $panic (param i64 i64)))
  (memory (export "memory") 1)
  (data (i32.const 0) "ab")
  (data (i32.const 16) "boom")
  (func $add (param $key i64)
    (i64.store (i32.const 100) (i64.const 0))
    (if (i64.eq (call $read (i64.const 1) (local.get $key) (i64.const 0)) (i64.const 1))
      (then (call $register (i64.const 0) (i64.const 100))))
    (i64.store (i32.const 100) (i64.add (i64.load (i32.const 100)) (i64.const 1)))
    (drop (call $write (i64.const 1) (local.get $key) (i64.const 8) (i64.const 100) (i64.const 1))))
  (func (export "pair") (call $add (i64.const 0)) (call $add (i64.const 1)))
  (func (export "skew") (call $add (i64.const 0)))
  (func (export "skew_b") (call $add (i64.const 1)))
  (func (export "boom") (call $panic (i64.const 4) (i64.const 16))))
"#;

const BANK: &str = "bank.near";

fn world() -> Sim {
    Fixture::new()
        .account("alice.near", Balance::from_near(100))
        .contract(BANK, wat::parse_str(WAT).unwrap())
        .build()
}

#[derive(Clone, Debug, Serialize, Deserialize)]
enum Call {
    Pair,
    Skew,
    SkewB,
    Boom,
}

impl Op for Call {
    fn submit(&self, sim: &mut Sim) -> Result<(), Skip> {
        let (method, deposit) = match self {
            Call::Pair => ("pair", 0),
            Call::Skew => ("skew", 0),
            Call::SkewB => ("skew_b", 0),
            Call::Boom => ("boom", 1),
        };
        sim.call("alice.near", BANK, method).deposit(Balance::from_near(deposit)).submit();
        Ok(())
    }
}

fn counter(sim: &Sim, key: &[u8]) -> u64 {
    let bytes = sim.state(BANK, key).unwrap_or_default();
    bytes.try_into().map_or(0, u64::from_le_bytes)
}

/// `a == b` at rest; its failure names which counter ran ahead.
fn harness() -> Harness<Call> {
    Harness::new(world()).at_rest("counters agree", |sim: &Sim| {
        let (a, b) = (counter(sim, b"a"), counter(sim, b"b"));
        match a.cmp(&b) {
            std::cmp::Ordering::Equal => Ok(()),
            std::cmp::Ordering::Greater => {
                Err(Fail::new(format!("a {a} > b {b}")).cause("a ahead"))
            }
            std::cmp::Ordering::Less => Err(Fail::new(format!("b {b} > a {a}")).cause("b ahead")),
        }
    })
}

/// Step 2 (`skew`) breaks the check with nothing failing; step 3
/// (`boom`), in the same block, fails loudly with a deposit. The breaking
/// step is step 2, not step 3's failure.
#[test]
fn the_cause_is_the_first_step_after_which_the_check_fails() {
    let plan = Plan::new().op(Call::Pair).op(Call::Skew).blocks(0).op(Call::Boom);
    let harness = harness();
    let run = harness.run(&plan);
    run.assert_found("counters agree", "world");
    assert!(run.trees().contains("boom"), "{}", run.trees());
    let audit = harness.scan(&[plan]).faults(Faults::none()).run();
    let row = audit.assert_found("counters agree", "world");
    assert_eq!(row.broke.as_deref(), Some("breaking step 2 of 3: Skew"), "{audit}");
}

/// One check failing at one site with two causes is two findings.
#[test]
fn findings_of_one_check_split_by_their_cause() {
    let harness = harness();
    let run = harness.run(&Plan::new().op(Call::Skew).op(Call::SkewB).op(Call::SkewB));
    let causes: Vec<_> = (run.findings().iter())
        .filter(|x| x.check == "counters agree")
        .map(|x| x.cause.clone())
        .collect();
    assert_eq!(causes, [Some("a ahead".to_owned()), Some("b ahead".to_owned())], "{run}");
}
