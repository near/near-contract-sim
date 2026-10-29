;; Hand-written contract for failure-semantics tests.
;;   hello             -> returns "hello"
;;   write             -> storage_write("key", "value")
;;   write_then_panic  -> storage_write("key", "value"), then panics ("boom")
;;   send_to_missing   -> promise_create(missing.near, "ping", deposit = 1 NEAR)
;;                        .then(self.on_cb) and returns that promise
;;   on_cb             -> logs + returns "cb-ok" / "cb-failed" from promise_result(0)
(module
  (import "env" "storage_write" (func $storage_write (param i64 i64 i64 i64 i64) (result i64)))
  (import "env" "panic_utf8" (func $panic_utf8 (param i64 i64)))
  (import "env" "value_return" (func $value_return (param i64 i64)))
  (import "env" "log_utf8" (func $log_utf8 (param i64 i64)))
  (import "env" "promise_create" (func $promise_create (param i64 i64 i64 i64 i64 i64 i64 i64) (result i64)))
  (import "env" "promise_then" (func $promise_then (param i64 i64 i64 i64 i64 i64 i64 i64 i64) (result i64)))
  (import "env" "promise_return" (func $promise_return (param i64)))
  (import "env" "promise_result" (func $promise_result (param i64 i64) (result i64)))
  (import "env" "current_account_id" (func $current_account_id (param i64)))
  (import "env" "register_len" (func $register_len (param i64) (result i64)))
  (import "env" "read_register" (func $read_register (param i64 i64)))
  (memory 1)
  (export "memory" (memory 0))
  (data (i32.const 0) "\"hello\"")
  (data (i32.const 16) "key")
  (data (i32.const 32) "value")
  (data (i32.const 48) "boom")
  (data (i32.const 64) "missing.near")
  (data (i32.const 80) "ping")
  (data (i32.const 96) "on_cb")
  (data (i32.const 112) "\"cb-ok\"")
  (data (i32.const 128) "\"cb-failed\"")
  ;; 160: u128 LE = 10^24 yocto (1 NEAR). 176: u128 zero.
  (data (i32.const 160) "\00\00\00\a1\ed\cc\ce\1b\c2\d3\00\00\00\00\00\00")
  ;; 256: current account id (read from register)

  (func (export "hello")
    (call $value_return (i64.const 7) (i64.const 0)))

  (func (export "write")
    (drop (call $storage_write (i64.const 3) (i64.const 16) (i64.const 5) (i64.const 32) (i64.const 1))))

  (func (export "write_then_panic")
    (drop (call $storage_write (i64.const 3) (i64.const 16) (i64.const 5) (i64.const 32) (i64.const 1)))
    (call $panic_utf8 (i64.const 4) (i64.const 48)))

  (func (export "send_to_missing")
    (local $p i64) (local $len i64)
    (call $current_account_id (i64.const 0))
    (local.set $len (call $register_len (i64.const 0)))
    (call $read_register (i64.const 0) (i64.const 256))
    (local.set $p
      (call $promise_create
        (i64.const 12) (i64.const 64)      ;; missing.near
        (i64.const 4) (i64.const 80)       ;; ping
        (i64.const 0) (i64.const 0)        ;; no args
        (i64.const 160)                    ;; 1 NEAR
        (i64.const 5000000000000)))        ;; 5 Tgas
    (call $promise_return
      (call $promise_then (local.get $p)
        (local.get $len) (i64.const 256)   ;; self
        (i64.const 5) (i64.const 96)       ;; on_cb
        (i64.const 0) (i64.const 0)
        (i64.const 176)                    ;; 0 deposit
        (i64.const 10000000000000))))      ;; 10 Tgas

  (func (export "on_cb")
    (if (i64.eq (call $promise_result (i64.const 0) (i64.const 0)) (i64.const 1))
      (then
        (call $log_utf8 (i64.const 5) (i64.const 113))
        (call $value_return (i64.const 7) (i64.const 112)))
      (else
        (call $log_utf8 (i64.const 9) (i64.const 129))
        (call $value_return (i64.const 11) (i64.const 128)))))
)
