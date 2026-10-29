;; A guestbook whose public `sign` stores an entry keyed by the caller and
;; asks for no deposit: every signer grows the contract's storage, and the
;; contract pays for it from its own balance (tests/curves.rs).
;; `sign_paid` is the fixed method: it refuses less than 542 * 2^64 yocto
;; (just under 0.01 NEAR), which covers an entry of up to 900 bytes at 1e19
;; yocto a byte.
;;   sign       -> input: the message, raw. Stores it under "g" + the
;;                 predecessor's id.
;;   sign_paid  -> the same, with about 0.01 NEAR attached.
(module
  (import "env" "input" (func $input (param i64)))
  (import "env" "register_len" (func $register_len (param i64) (result i64)))
  (import "env" "read_register" (func $read_register (param i64 i64)))
  (import "env" "predecessor_account_id" (func $predecessor_account_id (param i64)))
  (import "env" "attached_deposit" (func $attached_deposit (param i64)))
  (import "env" "storage_write" (func $storage_write (param i64 i64 i64 i64 i64) (result i64)))
  (import "env" "panic_utf8" (func $panic_utf8 (param i64 i64)))
  (memory 1)
  (export "memory" (memory 0))
  (data (i32.const 0) "attach 0.01 NEAR")
  ;; 64: the attached deposit (u128). 512: the key. 4096: the message.

  ;; "g" + the predecessor's id at 512; the message at 4096. Stores it.
  (func $store
    (local $id i64) (local $msg i64)
    (call $predecessor_account_id (i64.const 1))
    (local.set $id (call $register_len (i64.const 1)))
    (i32.store8 (i32.const 512) (i32.const 103))
    (call $read_register (i64.const 1) (i64.const 513))
    (call $input (i64.const 0))
    (local.set $msg (call $register_len (i64.const 0)))
    (call $read_register (i64.const 0) (i64.const 4096))
    (drop (call $storage_write (i64.add (local.get $id) (i64.const 1)) (i64.const 512)
                               (local.get $msg) (i64.const 4096) (i64.const 2))))

  (func (export "sign")
    (call $store))

  (func (export "sign_paid")
    (call $attached_deposit (i64.const 64))
    ;; Refuse a deposit whose high 64 bits are under 542.
    (if (i64.lt_u (i64.load (i32.const 72)) (i64.const 542))
      (then (call $panic_utf8 (i64.const 16) (i64.const 0))))
    (call $store)))
