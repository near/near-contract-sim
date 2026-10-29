;; A split that forwards a quarter of what it was attached, past a reserve
;; it keeps, to a child (tests/gas_edges.rs, `Tx::gas_edges`' solved
;; points):
;;   split -> below the reserve C (9.876543210123 Tgas, not a constant
;;            the fixture's thresholds hold) it does nothing; from it on,
;;            calls its own `child` with (prepaid - C) / 4 gas, detached,
;;            and returns. Prepaid C to C + 3 attaches 0, which nearcore
;;            refuses (FunctionCallZeroAttachedGas), so `split` fails
;;            there and only there: a 4-gas window with the transaction
;;            succeeding on both sides.
;;            The quarter ($share) is capped at $cap (no cap here;
;;            tests/gas_edges.rs sets one, and other shares and reserves),
;;            so the child's gas is a line that can bend flat.
;;   child -> does nothing; with a few gas it runs out, detached.
(module
  (import "env" "current_account_id" (func $current_account_id (param i64)))
  (import "env" "read_register" (func $read_register (param i64 i64)))
  (import "env" "register_len" (func $register_len (param i64) (result i64)))
  (import "env" "prepaid_gas" (func $prepaid_gas (result i64)))
  (import "env" "promise_batch_create" (func $promise_batch_create (param i64 i64) (result i64)))
  (import "env" "promise_batch_action_function_call"
    (func $call (param i64 i64 i64 i64 i64 i64 i64)))
  (memory 1)
  (export "memory" (memory 0))
  (data (i32.const 0) "child")
  ;; 64: u128 zero (no deposit); 128: the current account id
  (global $reserve i64 (i64.const 9876543210123))
  (global $cap i64 (i64.const -1))
  (global $share i64 (i64.const 4))

  (func (export "split")
    (local $p i64) (local $g i64)
    (if (i64.lt_u (call $prepaid_gas) (global.get $reserve)) (then (return)))
    (call $current_account_id (i64.const 1))
    (call $read_register (i64.const 1) (i64.const 128))
    (local.set $p
      (call $promise_batch_create (call $register_len (i64.const 1)) (i64.const 128)))
    (local.set $g
      (i64.div_u (i64.sub (call $prepaid_gas) (global.get $reserve)) (global.get $share)))
    (if (i64.gt_u (local.get $g) (global.get $cap)) (then (local.set $g (global.get $cap))))
    (call $call (local.get $p) (i64.const 5) (i64.const 0) (i64.const 0) (i64.const 0)
      (i64.const 64) (local.get $g)))

  (func (export "child")))
