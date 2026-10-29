;; A registry any caller joins: `register` stores a 1000-byte profile under
;; "p" + the predecessor's id, staked from the registry's own balance. The
;; paying build replaces the `;; {charge}` marker so that `register`
;; panics unless the caller attaches what the entry stakes (0.011 NEAR).
;;   register -> stores the predecessor's profile. Payable.
(module
  (import "env" "predecessor_account_id" (func $predecessor_account_id (param i64)))
  (import "env" "register_len" (func $register_len (param i64) (result i64)))
  (import "env" "read_register" (func $read_register (param i64 i64)))
  (import "env" "attached_deposit" (func $attached_deposit (param i64)))
  (import "env" "storage_write" (func $storage_write (param i64 i64 i64 i64 i64) (result i64)))
  (import "env" "panic_utf8" (func $panic_utf8 (param i64 i64)))
  (memory 1)
  (export "memory" (memory 0))
  (data (i32.const 0) "attach 0.011 NEAR")
  ;; 64: the deposit. 512: the key. 2048: the profile (zeros).
  (func (export "register")
    (local $len i64)
    (call $attached_deposit (i64.const 64))
    ;; {charge}
    (call $predecessor_account_id (i64.const 0))
    (local.set $len (call $register_len (i64.const 0)))
    (i32.store8 (i32.const 512) (i32.const 112))
    (call $read_register (i64.const 0) (i64.const 513))
    (drop (call $storage_write (i64.add (local.get $len) (i64.const 1)) (i64.const 512)
                               (i64.const 1000) (i64.const 2048) (i64.const 1))))
)
