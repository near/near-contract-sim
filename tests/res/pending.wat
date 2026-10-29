;; A withdrawal queue whose callbacks only the contract itself is meant
;; to call (tests/outsider.rs). No args anywhere.
;;   request     -> sets the pending entry "p"
;;   process     -> calls on_withdraw on itself (10 Tgas), then on_result
;;                  (5 Tgas)
;;   on_withdraw -> panics unless "p" is set ("no pending"); removes it
;;                  and adds 1 to "d": damage only after a request
;;   on_result   -> reads promise result 0, which a call sent with no
;;                  promise before it lacks; writes nothing
;;   batch       -> sends itself one receipt: a 1 yocto transfer, then
;;                  migrate (5 Tgas)
;;   migrate     -> sets "m"
;;   keyed       -> sends itself one receipt: a new full-access key, then
;;                  pay_in with 1 yocto (5 Tgas)
;;   pay_in      -> sets "q"
;;   pair        -> sends itself one receipt: left, then right (5 Tgas each)
;;   left, right -> do nothing
(module
  (import "env" "current_account_id" (func $current_account_id (param i64)))
  (import "env" "register_len" (func $register_len (param i64) (result i64)))
  (import "env" "read_register" (func $read_register (param i64 i64)))
  (import "env" "storage_read" (func $storage_read (param i64 i64 i64) (result i64)))
  (import "env" "storage_write" (func $storage_write (param i64 i64 i64 i64 i64) (result i64)))
  (import "env" "storage_remove" (func $storage_remove (param i64 i64 i64) (result i64)))
  (import "env" "panic_utf8" (func $panic_utf8 (param i64 i64)))
  (import "env" "promise_create"
    (func $promise_create (param i64 i64 i64 i64 i64 i64 i64 i64) (result i64)))
  (import "env" "promise_then"
    (func $promise_then (param i64 i64 i64 i64 i64 i64 i64 i64 i64) (result i64)))
  (import "env" "promise_result" (func $promise_result (param i64 i64) (result i64)))
  (import "env" "promise_batch_create" (func $batch_create (param i64 i64) (result i64)))
  (import "env" "promise_batch_action_transfer" (func $batch_transfer (param i64 i64)))
  (import "env" "promise_batch_action_function_call"
    (func $batch_call (param i64 i64 i64 i64 i64 i64 i64)))
  (import "env" "promise_batch_action_add_key_with_full_access"
    (func $batch_add_key (param i64 i64 i64 i64)))
  (memory 1)
  (export "memory" (memory 0))
  (data (i32.const 0) "p")
  (data (i32.const 8) "d")
  (data (i32.const 16) "on_withdraw")
  (data (i32.const 32) "on_result")
  (data (i32.const 48) "migrate")
  (data (i32.const 64) "m")
  (data (i32.const 72) "no pending")
  (data (i32.const 144) "\01")
  (data (i32.const 96) "pay_in")
  (data (i32.const 104) "q")
  (data (i32.const 112) "left")
  (data (i32.const 120) "right")
  ;; An ed25519 public key: its curve byte, then 32 bytes.
  (data (i32.const 160) "\00\11\11\11\11\11\11\11\11\11\11\11\11\11\11\11\11\11\11\11\11\11\11\11\11\11\11\11\11\11\11\11\11")
  ;; 128: u128 zero. 144: u128 one. 256: the counter value. 512: current id.

  ;; The current account's id at 512; its length.
  (func $me (result i64)
    (call $current_account_id (i64.const 0))
    (call $read_register (i64.const 0) (i64.const 512))
    (call $register_len (i64.const 0)))

  (func (export "request")
    (drop (call $storage_write (i64.const 1) (i64.const 0) (i64.const 1) (i64.const 0)
                               (i64.const 4))))

  (func (export "process")
    (local $len i64)
    (local.set $len (call $me))
    (drop (call $promise_then
      (call $promise_create
        (local.get $len) (i64.const 512) (i64.const 11) (i64.const 16)
        (i64.const 0) (i64.const 0) (i64.const 128) (i64.const 10000000000000))
      (local.get $len) (i64.const 512) (i64.const 9) (i64.const 32)
      (i64.const 0) (i64.const 0) (i64.const 128) (i64.const 5000000000000))))

  (func (export "on_withdraw")
    (if (i64.eqz (call $storage_remove (i64.const 1) (i64.const 0) (i64.const 3)))
      (then (call $panic_utf8 (i64.const 10) (i64.const 72))))
    (i64.store (i32.const 256) (i64.const 0))
    (if (i64.eq (call $storage_read (i64.const 1) (i64.const 8) (i64.const 3)) (i64.const 1))
      (then (call $read_register (i64.const 3) (i64.const 256))))
    (i64.store (i32.const 256) (i64.add (i64.load (i32.const 256)) (i64.const 1)))
    (drop (call $storage_write (i64.const 1) (i64.const 8) (i64.const 8) (i64.const 256)
                               (i64.const 4))))

  (func (export "on_result")
    (drop (call $promise_result (i64.const 0) (i64.const 0))))

  (func (export "batch")
    (local $b i64)
    (local.set $b (call $batch_create (call $me) (i64.const 512)))
    (call $batch_transfer (local.get $b) (i64.const 144))
    (call $batch_call (local.get $b) (i64.const 7) (i64.const 48) (i64.const 0) (i64.const 0)
                      (i64.const 128) (i64.const 5000000000000)))

  (func (export "migrate")
    (drop (call $storage_write (i64.const 1) (i64.const 64) (i64.const 1) (i64.const 64)
                               (i64.const 4))))

  (func (export "keyed")
    (local $b i64)
    (local.set $b (call $batch_create (call $me) (i64.const 512)))
    (call $batch_add_key (local.get $b) (i64.const 33) (i64.const 160) (i64.const 0))
    (call $batch_call (local.get $b) (i64.const 6) (i64.const 96) (i64.const 0) (i64.const 0)
                      (i64.const 144) (i64.const 5000000000000)))

  (func (export "pay_in")
    (drop (call $storage_write (i64.const 1) (i64.const 104) (i64.const 1) (i64.const 104)
                               (i64.const 4))))

  (func (export "pair")
    (local $b i64)
    (local.set $b (call $batch_create (call $me) (i64.const 512)))
    (call $batch_call (local.get $b) (i64.const 4) (i64.const 112) (i64.const 0) (i64.const 0)
                      (i64.const 128) (i64.const 5000000000000))
    (call $batch_call (local.get $b) (i64.const 5) (i64.const 120) (i64.const 0) (i64.const 0)
                      (i64.const 128) (i64.const 5000000000000)))

  (func (export "left"))

  (func (export "right"))
)
