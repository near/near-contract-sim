;; Tiny contract for the invariant-testing example (tests/invariants.rs).
;; The input of both calls is the raw receiver account id (not JSON).
;;   tip          -> sends 1 NEAR to the receiver in a detached promise
;;                   (fire and forget: nobody hears if the transfer fails)
;;   tip_checked  -> sends 1 NEAR, then calls self.on_tip with the result
;;                   and returns that promise (the failure is handled)
;;   on_tip       -> logs "tip ok" / "tip failed"
(module
  (import "env" "input" (func $input (param i64)))
  (import "env" "register_len" (func $register_len (param i64) (result i64)))
  (import "env" "read_register" (func $read_register (param i64 i64)))
  (import "env" "current_account_id" (func $current_account_id (param i64)))
  (import "env" "promise_batch_create" (func $promise_batch_create (param i64 i64) (result i64)))
  (import "env" "promise_batch_action_transfer" (func $transfer (param i64 i64)))
  (import "env" "promise_then" (func $promise_then (param i64 i64 i64 i64 i64 i64 i64 i64 i64) (result i64)))
  (import "env" "promise_return" (func $promise_return (param i64)))
  (import "env" "promise_result" (func $promise_result (param i64 i64) (result i64)))
  (import "env" "log_utf8" (func $log_utf8 (param i64 i64)))
  (memory 1)
  (export "memory" (memory 0))
  ;; 0: u128 LE = 10^24 yocto (1 NEAR). 16: u128 zero.
  (data (i32.const 0) "\00\00\00\a1\ed\cc\ce\1b\c2\d3\00\00\00\00\00\00")
  (data (i32.const 32) "on_tip")
  (data (i32.const 48) "tip ok")
  (data (i32.const 64) "tip failed")
  ;; 256: receiver id, 512: current account id

  (func $send (result i64)
    (local $p i64)
    (call $input (i64.const 0))
    (call $read_register (i64.const 0) (i64.const 256))
    (local.set $p (call $promise_batch_create (call $register_len (i64.const 0)) (i64.const 256)))
    (call $transfer (local.get $p) (i64.const 0))
    (local.get $p))

  (func (export "tip")
    (drop (call $send)))

  (func (export "tip_checked")
    (local $p i64)
    (local.set $p (call $send))
    (call $current_account_id (i64.const 1))
    (call $read_register (i64.const 1) (i64.const 512))
    (call $promise_return
      (call $promise_then (local.get $p)
        (call $register_len (i64.const 1)) (i64.const 512)
        (i64.const 6) (i64.const 32)       ;; on_tip
        (i64.const 0) (i64.const 0)        ;; no args
        (i64.const 16)                     ;; no deposit
        (i64.const 5000000000000))))       ;; 5 Tgas

  (func (export "on_tip")
    (if (i64.eq (call $promise_result (i64.const 0) (i64.const 2)) (i64.const 1))
      (then (call $log_utf8 (i64.const 6) (i64.const 48)))
      (else (call $log_utf8 (i64.const 10) (i64.const 64))))))
