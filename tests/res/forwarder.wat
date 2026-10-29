;; A contract that forwards the NEAR attached to it (tests/books.rs):
;; `forward` calls the receiver's `take` with the
;; deposit, then its own `on_forward`. The bug build's `on_forward` does
;; nothing, so a failed `take` returns the deposit to this contract, which
;; keeps it, crediting no one. tests/books.rs builds the fixed one by
;; turning the `$refunds` flag on: then a failed `take` is refunded to the
;; signer.
;;   forward     -> input: the receiver id, raw. Payable.
;;   on_forward  -> input: the amount (u128 little-endian), then the
;;                  signer id. Private in spirit (not checked here).
(module
  (import "env" "input" (func $input (param i64)))
  (import "env" "register_len" (func $register_len (param i64) (result i64)))
  (import "env" "read_register" (func $read_register (param i64 i64)))
  (import "env" "attached_deposit" (func $attached_deposit (param i64)))
  (import "env" "signer_account_id" (func $signer_account_id (param i64)))
  (import "env" "current_account_id" (func $current_account_id (param i64)))
  (import "env" "promise_create"
    (func $promise_create (param i64 i64 i64 i64 i64 i64 i64 i64) (result i64)))
  (import "env" "promise_then"
    (func $promise_then (param i64 i64 i64 i64 i64 i64 i64 i64 i64) (result i64)))
  (import "env" "promise_return" (func $promise_return (param i64)))
  (import "env" "promise_result" (func $promise_result (param i64 i64) (result i64)))
  (import "env" "promise_batch_create" (func $batch (param i64 i64) (result i64)))
  (import "env" "promise_batch_action_transfer" (func $transfer (param i64 i64)))
  (memory 1)
  (export "memory" (memory 0))
  (data (i32.const 0) "take")
  (data (i32.const 16) "on_forward")
  ;; 64: u128 zero. 1024: input. 2048: on_forward's args (amount, then
  ;; the signer id). 3072: this contract's id.
  (global $refunds i32 (i32.const 0)) ;; REFUNDS

  (func (export "forward")
    (local $len i64) (local $signer i64) (local $me i64) (local $p i64)
    (call $input (i64.const 0))
    (local.set $len (call $register_len (i64.const 0)))
    (call $read_register (i64.const 0) (i64.const 1024))
    (call $attached_deposit (i64.const 2048))
    (call $signer_account_id (i64.const 1))
    (local.set $signer (call $register_len (i64.const 1)))
    (call $read_register (i64.const 1) (i64.const 2064))
    (call $current_account_id (i64.const 2))
    (local.set $me (call $register_len (i64.const 2)))
    (call $read_register (i64.const 2) (i64.const 3072))
    (local.set $p
      (call $promise_create (local.get $len) (i64.const 1024) (i64.const 4) (i64.const 0)
                            (i64.const 0) (i64.const 0) (i64.const 2048)
                            (i64.const 5000000000000)))
    (call $promise_return
      (call $promise_then (local.get $p) (local.get $me) (i64.const 3072)
                          (i64.const 10) (i64.const 16)
                          (i64.add (i64.const 16) (local.get $signer)) (i64.const 2048)
                          (i64.const 64) (i64.const 10000000000000))))

  (func (export "on_forward")
    (local $len i64)
    (if (i32.eqz (global.get $refunds)) (then (return)))
    ;; 1: the forward succeeded.
    (if (i64.eq (call $promise_result (i64.const 0) (i64.const 3)) (i64.const 1))
      (then (return)))
    (call $input (i64.const 0))
    (local.set $len (call $register_len (i64.const 0)))
    (call $read_register (i64.const 0) (i64.const 1024))
    (call $transfer
      (call $batch (i64.sub (local.get $len) (i64.const 16)) (i64.const 1040))
      (i64.const 1024)))
)
