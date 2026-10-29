;; Methods only the contract itself is meant to call, guarded or not
;; (tests/outsider.rs). No args anywhere.
;;   start           -> calls on_done, on_done_guarded, on_ping and relay
;;                      on itself (detached, 5 Tgas each, relay 15)
;;   on_done         -> adds 1 to the counter "n": no predecessor check,
;;                      the missing-#[private] bug
;;   on_done_guarded -> panics unless the predecessor is the contract
;;                      itself (same length and first 8 bytes, enough
;;                      for these ids), then adds 1 to "g"
;;   relay           -> calls on_done_guarded on itself: no check, and the
;;                      guarded method sees its own contract as caller
;;   on_ping         -> does nothing: open to anyone, it changes no state
;;   count           -> view: "n" as a u64 (LE), 0 if unset
;;   admin_reset     -> sets "n" to 0 (no witness calls it)
(module
  (import "env" "current_account_id" (func $current_account_id (param i64)))
  (import "env" "predecessor_account_id" (func $predecessor_account_id (param i64)))
  (import "env" "register_len" (func $register_len (param i64) (result i64)))
  (import "env" "read_register" (func $read_register (param i64 i64)))
  (import "env" "storage_read" (func $storage_read (param i64 i64 i64) (result i64)))
  (import "env" "storage_write" (func $storage_write (param i64 i64 i64 i64 i64) (result i64)))
  (import "env" "value_return" (func $value_return (param i64 i64)))
  (import "env" "panic_utf8" (func $panic_utf8 (param i64 i64)))
  (import "env" "promise_create"
    (func $promise_create (param i64 i64 i64 i64 i64 i64 i64 i64) (result i64)))
  (memory 1)
  (export "memory" (memory 0))
  (data (i32.const 0) "n")
  (data (i32.const 8) "g")
  (data (i32.const 16) "on_done")
  (data (i32.const 32) "on_done_guarded")
  (data (i32.const 64) "only the contract itself")
  (data (i32.const 96) "on_ping")
  (data (i32.const 112) "relay")
  ;; 128: u128 zero deposit. 256: the counter value. 512: current id.
  ;; 768: predecessor id.

  ;; Adds 1 to the u64 under the 1-byte key at `key`.
  (func $bump (param $key i64)
    (i64.store (i32.const 256) (i64.const 0))
    (if (i64.eq (call $storage_read (i64.const 1) (local.get $key) (i64.const 3)) (i64.const 1))
      (then (call $read_register (i64.const 3) (i64.const 256))))
    (i64.store (i32.const 256) (i64.add (i64.load (i32.const 256)) (i64.const 1)))
    (drop (call $storage_write (i64.const 1) (local.get $key) (i64.const 8) (i64.const 256)
                               (i64.const 4))))

  (func $self (param $len i64) (param $name i64) (param $gas i64)
    (call $current_account_id (i64.const 0))
    (call $read_register (i64.const 0) (i64.const 512))
    (drop (call $promise_create
      (call $register_len (i64.const 0)) (i64.const 512)
      (local.get $len) (local.get $name)
      (i64.const 0) (i64.const 0)
      (i64.const 128)
      (local.get $gas))))

  (func (export "start")
    (call $self (i64.const 7) (i64.const 16) (i64.const 5000000000000))
    (call $self (i64.const 15) (i64.const 32) (i64.const 5000000000000))
    (call $self (i64.const 7) (i64.const 96) (i64.const 5000000000000))
    (call $self (i64.const 5) (i64.const 112) (i64.const 15000000000000)))

  (func (export "relay")
    (call $self (i64.const 15) (i64.const 32) (i64.const 5000000000000)))

  (func (export "on_ping"))

  (func (export "on_done")
    (call $bump (i64.const 0)))

  (func (export "on_done_guarded")
    (local $len i64)
    (call $current_account_id (i64.const 0))
    (call $read_register (i64.const 0) (i64.const 512))
    (call $predecessor_account_id (i64.const 1))
    (local.set $len (call $register_len (i64.const 1)))
    (call $read_register (i64.const 1) (i64.const 768))
    (if (i64.ne (local.get $len) (call $register_len (i64.const 0)))
      (then (call $panic_utf8 (i64.const 24) (i64.const 64))))
    (if (i64.ne (i64.load (i32.const 512)) (i64.load (i32.const 768)))
      (then (call $panic_utf8 (i64.const 24) (i64.const 64))))
    (call $bump (i64.const 8)))

  (func (export "count")
    (i64.store (i32.const 256) (i64.const 0))
    (if (i64.eq (call $storage_read (i64.const 1) (i64.const 0) (i64.const 3)) (i64.const 1))
      (then (call $read_register (i64.const 3) (i64.const 256))))
    (call $value_return (i64.const 8) (i64.const 256)))

  (func (export "admin_reset")
    (i64.store (i32.const 256) (i64.const 0))
    (drop (call $storage_write (i64.const 1) (i64.const 0) (i64.const 8) (i64.const 256)
                               (i64.const 4))))
)
