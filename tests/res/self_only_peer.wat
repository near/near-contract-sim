;; A contract whose self-only method only changes another contract
;; (tests/outsider.rs), deployed as fwd.near and as peer.near. No args.
;;   start   -> calls forward on itself (detached, 30 Tgas)
;;   forward -> calls peer.near::land and peer.near::picky (5 Tgas
;;              each): it writes nothing of its own
;;   land    -> writes "l"
;;   picky   -> panics unless the signer's id is 10 bytes long (as
;;              alice.near's is and axis.near's isn't)
(module
  (import "env" "current_account_id" (func $current_account_id (param i64)))
  (import "env" "signer_account_id" (func $signer_account_id (param i64)))
  (import "env" "register_len" (func $register_len (param i64) (result i64)))
  (import "env" "read_register" (func $read_register (param i64 i64)))
  (import "env" "storage_write" (func $storage_write (param i64 i64 i64 i64 i64) (result i64)))
  (import "env" "panic_utf8" (func $panic_utf8 (param i64 i64)))
  (import "env" "promise_create"
    (func $promise_create (param i64 i64 i64 i64 i64 i64 i64 i64) (result i64)))
  (memory 1)
  (export "memory" (memory 0))
  (data (i32.const 0) "forward")
  (data (i32.const 16) "peer.near")
  (data (i32.const 32) "land")
  (data (i32.const 40) "picky")
  (data (i32.const 48) "l")
  (data (i32.const 64) "not a member")
  ;; 128: u128 zero deposit. 512: current id.

  (func (export "start")
    (call $current_account_id (i64.const 0))
    (call $read_register (i64.const 0) (i64.const 512))
    (drop (call $promise_create
      (call $register_len (i64.const 0)) (i64.const 512)
      (i64.const 7) (i64.const 0)
      (i64.const 0) (i64.const 0)
      (i64.const 128)
      (i64.const 30000000000000))))

  (func (export "forward")
    (drop (call $promise_create
      (i64.const 9) (i64.const 16) (i64.const 4) (i64.const 32)
      (i64.const 0) (i64.const 0) (i64.const 128) (i64.const 5000000000000)))
    (drop (call $promise_create
      (i64.const 9) (i64.const 16) (i64.const 5) (i64.const 40)
      (i64.const 0) (i64.const 0) (i64.const 128) (i64.const 5000000000000))))

  (func (export "land")
    (drop (call $storage_write (i64.const 1) (i64.const 48) (i64.const 1) (i64.const 48)
                               (i64.const 4))))

  (func (export "picky")
    (call $signer_account_id (i64.const 0))
    (if (i64.ne (call $register_len (i64.const 0)) (i64.const 10))
      (then (call $panic_utf8 (i64.const 12) (i64.const 64)))))
)
