//! A batched receipt is named by the call that failed, not by its
//! first. `app.near`'s `go` sends `token.near` one receipt with
//! two calls, `storage_deposit` then `ft_transfer_call`, on 10 and 20
//! Tgas of its own; `ft_transfer_call` panics.
//!
//! Verification boundary: the receipts the pinned runtime executes; the
//! names are the library's reading of nearcore's `ActionError::index`.

use near_contract_sim::explore::{Harness, Op, Plan, Skip};
use near_contract_sim::{Balance, Fixture, Sim};
use serde::{Deserialize, Serialize};

const WAT: &str = r#"
(module
  (import "env" "promise_batch_create" (func $batch (param i64 i64) (result i64)))
  (import "env" "promise_batch_action_function_call"
    (func $call (param i64 i64 i64 i64 i64 i64 i64)))
  (import "env" "panic_utf8" (func $panic (param i64 i64)))
  (memory (export "memory") 1)
  (data (i32.const 0) "token.near")
  (data (i32.const 16) "storage_deposit")
  (data (i32.const 32) "ft_transfer_call")
  (data (i32.const 64) "refused")
  ;; 128: a u128 zero deposit
  (func (export "go")
    (local $p i64)
    (local.set $p (call $batch (i64.const 10) (i64.const 0)))
    (call $call (local.get $p) (i64.const 15) (i64.const 16) (i64.const 0) (i64.const 0)
      (i64.const 128) (i64.const 10000000000000))
    (call $call (local.get $p) (i64.const 16) (i64.const 32) (i64.const 0) (i64.const 0)
      (i64.const 128) (i64.const 20000000000000)))
  (func (export "storage_deposit"))
  (func (export "ft_transfer_call") (call $panic (i64.const 7) (i64.const 64))))
"#;

const SITE: &str = "token.near::ft_transfer_call";

fn world() -> Sim {
    let code = wat::parse_str(WAT).unwrap();
    Fixture::new()
        .account("alice.near", Balance::from_near(100))
        .contract("app.near", code.clone())
        .contract("token.near", code)
        .build()
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Go;

impl Op for Go {
    fn submit(&self, sim: &mut Sim) -> Result<(), Skip> {
        sim.call("alice.near", "app.near", "go").submit();
        Ok(())
    }
}

/// The receipt reads `storage_deposit + ft_transfer_call (failed at
/// ft_transfer_call)`; the finding names `ft_transfer_call`; and each call
/// is a site the run reached.
#[test]
fn a_batch_is_named_by_the_call_that_failed() {
    let mut sim = world();
    let out = sim.call("alice.near", "app.near", "go").transact();
    let r = (out.receipts.iter())
        .find(|r| r.receiver_id.as_str() == "token.near" && !r.is_refund())
        .expect("the batch ran");
    assert_eq!(r.method_names, ["storage_deposit", "ft_transfer_call"], "{out}");
    assert_eq!(r.method(), Some("ft_transfer_call"), "{out}");
    let text = "token.near::storage_deposit,ft_transfer_call (failed at ft_transfer_call)";
    assert!(out.to_string().contains(text), "{out}");

    let harness = Harness::new(world()).must_not_fail("ft_transfer_call");
    let run = harness.run(&Plan::new().op(Go));
    let f = (run.findings().iter())
        .find(|f| f.site.to_string().starts_with(SITE))
        .unwrap_or_else(|| panic!("a finding at {SITE}: {run}"));
    assert!(!f.site.to_string().contains("storage_deposit"), "{run}");
    // Both calls of the batch are sites it reached.
    for site in ["token.near::storage_deposit", SITE] {
        assert!(run.reached().contains(site), "{site} not reached: {:?}", run.reached());
    }
}
