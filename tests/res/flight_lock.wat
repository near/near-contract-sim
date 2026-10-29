;; A contract whose outcomes depend on what lands inside another
;; transaction's flight (tests/explore_enumerate.rs). Only work's input
;; 2 moves NEAR.
;;   start        -> sets "*busy", then calls its own finish (5 Tgas)
;;   finish       -> clears "*busy"
;;   sync         -> panics "busy" while "*busy" is set: a sync landing
;;                   between another op's start and finish is refused,
;;                   one before or after succeeds
;;   try_sync     -> returns `false` while "*busy" is set, `true` otherwise:
;;                   a refusal that doesn't panic
;;   work         -> input: one byte. While "*busy" is set, panics "busy"
;;                   (input 0), writes "*scratch" (input 1, a key no
;;                   reader reads) or pays the attached deposit back to
;;                   its caller (input 2); else adds 1 to "*worked" (a u64).
;;                   Either way that didn't panic, it then calls its own
;;                   fail and returns that promise: the transaction fails
;;   fail         -> panics "work failed"
;;   worked       -> (view) "*worked", a u64
;;   trap         -> input: a u64 height. Panics "sprung" at that height
;;   epoch_begin  -> stores env::epoch_height() under "*epoch", then calls
;;                   its own epoch_end (5 Tgas)
;;   epoch_end    -> panics "epoch moved" unless env::epoch_height() is
;;                   still the one stored: an epoch boundary between the
;;                   two receipts fails the second
(module
  (import "env" "current_account_id" (func $current_account_id (param i64)))
  (import "env" "register_len" (func $register_len (param i64) (result i64)))
  (import "env" "read_register" (func $read_register (param i64 i64)))
  (import "env" "epoch_height" (func $epoch_height (result i64)))
  (import "env" "block_index" (func $block_index (result i64)))
  (import "env" "storage_write" (func $storage_write (param i64 i64 i64 i64 i64) (result i64)))
  (import "env" "storage_read" (func $storage_read (param i64 i64 i64) (result i64)))
  (import "env" "storage_remove" (func $storage_remove (param i64 i64 i64) (result i64)))
  (import "env" "storage_has_key" (func $storage_has_key (param i64 i64) (result i64)))
  (import "env" "promise_create"
    (func $promise_create (param i64 i64 i64 i64 i64 i64 i64 i64) (result i64)))
  (import "env" "panic_utf8" (func $panic_utf8 (param i64 i64)))
  (import "env" "input" (func $input (param i64)))
  (import "env" "promise_return" (func $promise_return (param i64)))
  (import "env" "value_return" (func $value_return (param i64 i64)))
  (import "env" "predecessor_account_id" (func $predecessor_account_id (param i64)))
  (import "env" "attached_deposit" (func $attached_deposit (param i64)))
  (import "env" "promise_batch_create" (func $promise_batch_create (param i64 i64) (result i64)))
  (import "env" "promise_batch_action_transfer"
    (func $promise_batch_action_transfer (param i64 i64)))
  (memory 1)
  (export "memory" (memory 0))
  (data (i32.const 0) "*busy")
  (data (i32.const 16) "finish")
  (data (i32.const 32) "busy")
  (data (i32.const 48) "*epoch")
  (data (i32.const 64) "epoch_end")
  (data (i32.const 80) "epoch moved")
  (data (i32.const 160) "*worked")
  (data (i32.const 176) "fail")
  (data (i32.const 192) "work failed")
  (data (i32.const 208) "*scratch")
  (data (i32.const 224) "sprung")
  (data (i32.const 240) "false")
  (data (i32.const 248) "true")
  ;; 96: one byte. 128: an epoch. 144: a count. 256: input. 512: the own id.
  ;; 640: the caller's id. 768: the attached deposit.

  ;; Calls its own `method` (`len` bytes at `ptr`) with no args, 5 Tgas.
  (func $self_call (param $len i64) (param $ptr i64) (result i64)
    (call $current_account_id (i64.const 1))
    (call $read_register (i64.const 1) (i64.const 512))
    (call $promise_create
      (call $register_len (i64.const 1)) (i64.const 512)
      (local.get $len) (local.get $ptr)
      (i64.const 0) (i64.const 0)
      (i64.const 96) (i64.const 5000000000000)))

  ;; Pays the attached deposit back to the caller.
  (func $pay_back
    (call $predecessor_account_id (i64.const 3))
    (call $read_register (i64.const 3) (i64.const 640))
    (call $attached_deposit (i64.const 768))
    (call $promise_batch_action_transfer
      (call $promise_batch_create (call $register_len (i64.const 3)) (i64.const 640))
      (i64.const 768)))

  (func $busy (result i32)
    (i64.eq (call $storage_has_key (i64.const 5) (i64.const 0)) (i64.const 1)))

  (func (export "start")
    (drop (call $storage_write (i64.const 5) (i64.const 0) (i64.const 1) (i64.const 96)
                               (i64.const 2)))
    (drop (call $self_call (i64.const 6) (i64.const 16))))

  (func (export "finish")
    (drop (call $storage_remove (i64.const 5) (i64.const 0) (i64.const 2))))

  (func (export "sync")
    (if (call $busy) (then (call $panic_utf8 (i64.const 4) (i64.const 32)))))

  (func (export "try_sync")
    (if (call $busy)
      (then (call $value_return (i64.const 5) (i64.const 240)))
      (else (call $value_return (i64.const 4) (i64.const 248)))))

  (func (export "work")
    (call $input (i64.const 0))
    (call $read_register (i64.const 0) (i64.const 256))
    (if (call $busy)
      (then
        (if (i32.eqz (i32.load8_u (i32.const 256)))
          (then (call $panic_utf8 (i64.const 4) (i64.const 32))))
        (if (i32.eq (i32.load8_u (i32.const 256)) (i32.const 2))
          (then (call $pay_back))
          (else
            (drop (call $storage_write (i64.const 8) (i64.const 208) (i64.const 1)
                                       (i64.const 96) (i64.const 2))))))
      (else
        (i64.store (i32.const 144) (i64.const 0))
        (if (i64.eq (call $storage_read (i64.const 7) (i64.const 160) (i64.const 2)) (i64.const 1))
          (then (call $read_register (i64.const 2) (i64.const 144))))
        (i64.store (i32.const 144) (i64.add (i64.load (i32.const 144)) (i64.const 1)))
        (drop (call $storage_write (i64.const 7) (i64.const 160) (i64.const 8) (i64.const 144)
                                   (i64.const 2)))))
    (call $promise_return (call $self_call (i64.const 4) (i64.const 176))))

  (func (export "fail")
    (call $panic_utf8 (i64.const 11) (i64.const 192)))

  (func (export "worked")
    (i64.store (i32.const 144) (i64.const 0))
    (if (i64.eq (call $storage_read (i64.const 7) (i64.const 160) (i64.const 2)) (i64.const 1))
      (then (call $read_register (i64.const 2) (i64.const 144))))
    (call $value_return (i64.const 8) (i64.const 144)))

  (func (export "trap")
    (call $input (i64.const 0))
    (call $read_register (i64.const 0) (i64.const 256))
    (if (i64.eq (i64.load (i32.const 256)) (call $block_index))
      (then (call $panic_utf8 (i64.const 6) (i64.const 224)))))

  (func (export "epoch_begin")
    (i64.store (i32.const 128) (call $epoch_height))
    (drop (call $storage_write (i64.const 6) (i64.const 48) (i64.const 8) (i64.const 128)
                               (i64.const 2)))
    (drop (call $self_call (i64.const 9) (i64.const 64))))

  (func (export "epoch_end")
    (drop (call $storage_read (i64.const 6) (i64.const 48) (i64.const 2)))
    (call $read_register (i64.const 2) (i64.const 128))
    (if (i64.ne (i64.load (i32.const 128)) (call $epoch_height))
      (then (call $panic_utf8 (i64.const 11) (i64.const 80))))))
