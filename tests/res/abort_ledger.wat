;; A ledger laid out as a Rust contract built with panic = "abort" is
;; (tests/trap_site.rs): a failed check calls the standard library's panic,
;; which aborts with `unreachable`, so every failure reads the same.
;;   withdraw -> input: an amount, u64 little-endian. `ledger::touch` runs
;;               first and returns; then `ledger::debit` panics on more
;;               than 100
;;   relay    -> input: an amount. Calls this contract's `withdraw` with
;;               it, and nothing waits on the result
(module
  (import "env" "input" (func $input (param i64)))
  (import "env" "read_register" (func $read_register (param i64 i64)))
  (import "env" "current_account_id" (func $current_account_id (param i64)))
  (import "env" "register_len" (func $register_len (param i64) (result i64)))
  (import "env" "promise_create"
    (func $promise_create (param i64 i64 i64 i64 i64 i64 i64 i64) (result i64)))
  (import "env" "log_utf8" (func $log_utf8 (param i64 i64)))
  (memory 1)
  (export "memory" (memory 0))
  (data (i32.const 16) "withdraw")
  ;; 256: the input. 512: this account's id.

  (func $std::process::abort unreachable)
  (func $core::panicking::panic (call $std::process::abort))

  (func $ledger::touch (result i64) (i64.const 1))

  (func $ledger::debit (param $amount i64)
    (if (i64.gt_u (local.get $amount) (i64.const 100))
      (then (call $core::panicking::panic))))

  (func $amount (result i64)
    (call $input (i64.const 0))
    (call $read_register (i64.const 0) (i64.const 256))
    (i64.load (i32.const 256)))

  (func $withdraw (export "withdraw")
    (drop (call $ledger::touch))
    (call $ledger::debit (call $amount)))

  (func $relay (export "relay")
    (drop (call $amount))
    (call $current_account_id (i64.const 1))
    (call $read_register (i64.const 1) (i64.const 512))
    (drop (call $promise_create
      (call $register_len (i64.const 1)) (i64.const 512)
      (i64.const 8) (i64.const 16)
      (i64.const 8) (i64.const 256)
      (i64.const 0) (i64.const 20000000000000)))))
