;; A queue of claims (tests/live_keys.rs): a request queues a claim under
;; the next id, and only a redeem that names a queued claim pays it. Ids
;; are u64 little-endian; a claim sits under "q" + its id, holding its
;; owner's id.
;;   request -> queues a claim for the predecessor, under the next id (the
;;              count, from 1); returns the id
;;   redeem  -> input: an id. Removes its claim and logs "redeemed <id
;;              bytes>"; panics "no such claim" if none is queued there
;;   count   -> (view) the claims ever queued, a u64
(module
  (import "env" "input" (func $input (param i64)))
  (import "env" "register_len" (func $register_len (param i64) (result i64)))
  (import "env" "read_register" (func $read_register (param i64 i64)))
  (import "env" "predecessor_account_id" (func $predecessor_account_id (param i64)))
  (import "env" "storage_write" (func $storage_write (param i64 i64 i64 i64 i64) (result i64)))
  (import "env" "storage_read" (func $storage_read (param i64 i64 i64) (result i64)))
  (import "env" "storage_remove" (func $storage_remove (param i64 i64 i64) (result i64)))
  (import "env" "value_return" (func $value_return (param i64 i64)))
  (import "env" "log_utf8" (func $log_utf8 (param i64 i64)))
  (import "env" "panic_utf8" (func $panic_utf8 (param i64 i64)))
  (memory 1)
  (export "memory" (memory 0))
  (data (i32.const 0) "n")
  (data (i32.const 16) "no such claim")
  (data (i32.const 32) "redeemed ")
  ;; 64: the count. 128: a key ("q" + id). 256: the input or the owner.

  (func $count (result i64)
    (if (result i64) (i64.eq (call $storage_read (i64.const 1) (i64.const 0) (i64.const 0))
                             (i64.const 1))
      (then (call $read_register (i64.const 0) (i64.const 64)) (i64.load (i32.const 64)))
      (else (i64.const 0))))

  (func (export "request")
    (local $n i64)
    (local.set $n (i64.add (call $count) (i64.const 1)))
    (i64.store (i32.const 64) (local.get $n))
    (drop (call $storage_write (i64.const 1) (i64.const 0) (i64.const 8) (i64.const 64)
                               (i64.const 1)))
    (i32.store8 (i32.const 128) (i32.const 113))
    (i64.store (i32.const 129) (local.get $n))
    (call $predecessor_account_id (i64.const 1))
    (call $read_register (i64.const 1) (i64.const 256))
    (drop (call $storage_write (i64.const 9) (i64.const 128)
                               (call $register_len (i64.const 1)) (i64.const 256) (i64.const 1)))
    (call $value_return (i64.const 8) (i64.const 64)))

  (func (export "redeem")
    (call $input (i64.const 0))
    (call $read_register (i64.const 0) (i64.const 256))
    (i32.store8 (i32.const 128) (i32.const 113))
    (i64.store (i32.const 129) (i64.load (i32.const 256)))
    (if (i64.eqz (call $storage_remove (i64.const 9) (i64.const 128) (i64.const 2)))
      (then (call $panic_utf8 (i64.const 13) (i64.const 16))))
    (i64.store (i32.const 41) (i64.load (i32.const 256)))
    (call $log_utf8 (i64.const 17) (i64.const 32)))

  (func (export "count")
    (i64.store (i32.const 64) (call $count))
    (call $value_return (i64.const 8) (i64.const 64))))
