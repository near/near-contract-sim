;; Holds wNEAR at wrap.near and pays it out as NEAR, as intents does:
;;   withdraw     -> input: an amount, u64 little-endian. Calls wrap.near's
;;                   `near_withdraw` with it, attaching the 1 yocto proof,
;;                   then its own `on_withdraw`
;;   on_withdraw  -> input: the amount, then the signer id. Once the
;;                   withdraw succeeded, transfers the signer the amount
;;                   less $KEEPS yocto
;; With wrap.near paying `amount + 1` (the proof back), the fixed build
;; keeps nothing it doesn't account for; a $KEEPS of 2 keeps 2 yocto more.
(module
  (import "env" "input" (func $input (param i64)))
  (import "env" "register_len" (func $register_len (param i64) (result i64)))
  (import "env" "read_register" (func $read_register (param i64 i64)))
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
  (data (i32.const 0) "wrap.near")
  (data (i32.const 16) "near_withdraw")
  (data (i32.const 32) "on_withdraw")
  ;; 64: u128 zero. 80: u128 one. 1024: input. 2048: on_withdraw's args
  ;; (amount, then the signer id). 3072: this contract's id. 4096: u128
  ;; amount to pay.
  (data (i32.const 80) "\01")
  (global $keeps i64 (i64.const 0)) ;; KEEPS

  (func (export "withdraw")
    (local $signer i64) (local $me i64) (local $p i64)
    (call $input (i64.const 0))
    (call $read_register (i64.const 0) (i64.const 2048))
    (call $signer_account_id (i64.const 1))
    (local.set $signer (call $register_len (i64.const 1)))
    (call $read_register (i64.const 1) (i64.const 2056))
    (call $current_account_id (i64.const 2))
    (local.set $me (call $register_len (i64.const 2)))
    (call $read_register (i64.const 2) (i64.const 3072))
    (local.set $p
      (call $promise_create (i64.const 9) (i64.const 0) (i64.const 13) (i64.const 16)
                            (i64.const 8) (i64.const 2048) (i64.const 80)
                            (i64.const 20000000000000)))
    (call $promise_return
      (call $promise_then (local.get $p) (local.get $me) (i64.const 3072)
                          (i64.const 11) (i64.const 32)
                          (i64.add (i64.const 8) (local.get $signer)) (i64.const 2048)
                          (i64.const 64) (i64.const 20000000000000))))

  (func (export "on_withdraw")
    (local $len i64)
    ;; 1: the withdraw succeeded.
    (if (i64.ne (call $promise_result (i64.const 0) (i64.const 3)) (i64.const 1))
      (then (return)))
    (call $input (i64.const 0))
    (local.set $len (call $register_len (i64.const 0)))
    (call $read_register (i64.const 0) (i64.const 1024))
    (i64.store (i32.const 4096) (i64.sub (i64.load (i32.const 1024)) (global.get $keeps)))
    (call $transfer
      (call $batch (i64.sub (local.get $len) (i64.const 8)) (i64.const 1032))
      (i64.const 4096)))
)
