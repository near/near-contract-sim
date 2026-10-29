;; A payment whose settlement starves when the caller attaches just enough
;; gas (tests/explore_search.rs, a tight draw and `Tx::least_gas`):
;;   pay     -> calls its own ping with a fixed 5 Tgas, then its own
;;              on_pay with no gas of its own and all the unused gas
;;              (weight 1), and returns "ok": the transaction's result
;;              doesn't wait on on_pay, so it succeeds when on_pay starves
;;   on_pay  -> writes "settled" = "1"
;;   ping    -> does nothing
;;   settled -> (view) "settled", or nothing
(module
  (import "env" "current_account_id" (func $current_account_id (param i64)))
  (import "env" "read_register" (func $read_register (param i64 i64)))
  (import "env" "register_len" (func $register_len (param i64) (result i64)))
  (import "env" "promise_batch_create" (func $promise_batch_create (param i64 i64) (result i64)))
  (import "env" "promise_batch_then" (func $promise_batch_then (param i64 i64 i64) (result i64)))
  (import "env" "promise_batch_action_function_call_weight"
    (func $call_weight (param i64 i64 i64 i64 i64 i64 i64 i64)))
  (import "env" "storage_write" (func $storage_write (param i64 i64 i64 i64 i64) (result i64)))
  (import "env" "storage_read" (func $storage_read (param i64 i64 i64) (result i64)))
  (import "env" "value_return" (func $value_return (param i64 i64)))
  (memory 1)
  (export "memory" (memory 0))
  (data (i32.const 0) "ping")
  (data (i32.const 16) "on_pay")
  (data (i32.const 32) "settled")
  (data (i32.const 48) "\"ok\"")
  (data (i32.const 56) "1")
  ;; 64: u128 zero (no deposit); 128: the current account id

  (func (export "pay")
    (local $p i64)
    (call $current_account_id (i64.const 1))
    (call $read_register (i64.const 1) (i64.const 128))
    (local.set $p
      (call $promise_batch_create (call $register_len (i64.const 1)) (i64.const 128)))
    (call $call_weight (local.get $p) (i64.const 4) (i64.const 0) (i64.const 0) (i64.const 0)
      (i64.const 64) (i64.const 5000000000000) (i64.const 0))
    (local.set $p (call $promise_batch_then (local.get $p)
      (call $register_len (i64.const 1)) (i64.const 128)))
    (call $call_weight (local.get $p) (i64.const 6) (i64.const 16) (i64.const 0) (i64.const 0)
      (i64.const 64) (i64.const 0) (i64.const 1))
    (call $value_return (i64.const 4) (i64.const 48)))

  (func (export "on_pay")
    (drop (call $storage_write (i64.const 7) (i64.const 32) (i64.const 1) (i64.const 56)
      (i64.const 2))))

  (func (export "ping"))

  (func (export "settled")
    (if (i64.eq (call $storage_read (i64.const 7) (i64.const 32) (i64.const 0)) (i64.const 1))
      (then (call $value_return (i64.const 1) (i64.const 56))))))
