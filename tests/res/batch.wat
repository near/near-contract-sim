;; A keeper that hands a batch to a worker on a fixed budget
;; (tests/batch.rs). One module, two roles: deployed on keeper.near it
;; forwards, on worker.near it works.
;; Inputs are raw: a count is a u64, little-endian.
;;
;; The keeper:
;;   batch    -> input: n. Calls worker.near's work(n) with 20 Tgas, a
;;               constant, whatever n is, and returns that promise
;;   register -> calls worker.near's touch with 20 Tgas
;; The worker:
;;   work     -> input: n. Works n items, each a fixed spin and a storage
;;               write, about 2.6 Tgas: at 8 items the 20 Tgas budget runs
;;               out
;;   touch    -> registers the signer once: a storage write the first
;;               time, a read after. A branch, well under 1 Tgas apart
;;   items    -> (view) the items worked
(module
  (import "env" "input" (func $input (param i64)))
  (import "env" "read_register" (func $read_register (param i64 i64)))
  (import "env" "register_len" (func $register_len (param i64) (result i64)))
  (import "env" "signer_account_id" (func $signer_account_id (param i64)))
  (import "env" "storage_write" (func $storage_write (param i64 i64 i64 i64 i64) (result i64)))
  (import "env" "storage_read" (func $storage_read (param i64 i64 i64) (result i64)))
  (import "env" "promise_create"
    (func $promise_create (param i64 i64 i64 i64 i64 i64 i64 i64) (result i64)))
  (import "env" "promise_return" (func $promise_return (param i64)))
  (import "env" "value_return" (func $value_return (param i64 i64)))
  (memory 1)
  (export "memory" (memory 0))
  (data (i32.const 0) "worker.near")
  (data (i32.const 16) "work")
  (data (i32.const 32) "touch")
  (data (i32.const 48) "i")
  ;; 64: u128 zero (no deposit). 512: a storage key. 600: a value.
  ;; 1024: input.

  ;; Calls worker.near's `method` (`len` bytes at `ptr`) with the input as
  ;; its args and 20 Tgas, returning that promise.
  (func $forward (param $ptr i64) (param $len i64)
    (local $n i64)
    (call $input (i64.const 0))
    (local.set $n (call $register_len (i64.const 0)))
    (if (i64.gt_s (local.get $n) (i64.const 0))
      (then (call $read_register (i64.const 0) (i64.const 1024)))
      (else (local.set $n (i64.const 0))))
    (call $promise_return
      (call $promise_create (i64.const 11) (i64.const 0)
        (local.get $len) (local.get $ptr)
        (local.get $n) (i64.const 1024)
        (i64.const 64) (i64.const 20000000000000))))

  (func (export "batch") (call $forward (i64.const 16) (i64.const 4)))

  (func (export "register") (call $forward (i64.const 32) (i64.const 5)))

  ;; The items worked so far.
  (func $items (result i64)
    (if (result i64)
        (i64.eq (call $storage_read (i64.const 1) (i64.const 48) (i64.const 1)) (i64.const 1))
      (then
        (call $read_register (i64.const 1) (i64.const 600))
        (i64.load (i32.const 600)))
      (else (i64.const 0))))

  (func (export "work")
    (local $n i64) (local $i i64) (local $spin i32)
    (call $input (i64.const 0))
    (call $read_register (i64.const 0) (i64.const 1024))
    (local.set $n (i64.load (i32.const 1024)))
    (block $done (loop $item
      (br_if $done (i64.ge_u (local.get $i) (local.get $n)))
      (local.set $spin (i32.const 0))
      (block $spun (loop $turn
        (br_if $spun (i32.ge_u (local.get $spin) (i32.const 300000)))
        (local.set $spin (i32.add (local.get $spin) (i32.const 1)))
        (br $turn)))
      (i64.store (i32.const 600) (i64.add (call $items) (i64.const 1)))
      (drop (call $storage_write (i64.const 1) (i64.const 48) (i64.const 8) (i64.const 600)
                                 (i64.const 2)))
      (local.set $i (i64.add (local.get $i) (i64.const 1)))
      (br $item))))

  (func (export "touch")
    (local $len i64)
    (i32.store8 (i32.const 512) (i32.const 114))
    (call $signer_account_id (i64.const 0))
    (local.set $len (call $register_len (i64.const 0)))
    (call $read_register (i64.const 0) (i64.const 513))
    (if (i64.eq (call $storage_read (i64.add (local.get $len) (i64.const 1)) (i64.const 512)
                                    (i64.const 1))
                (i64.const 0))
      (then
        (drop (call $storage_write (i64.add (local.get $len) (i64.const 1)) (i64.const 512)
                                   (i64.const 1) (i64.const 64) (i64.const 2))))))

  (func (export "items")
    (i64.store (i32.const 600) (call $items))
    (call $value_return (i64.const 8) (i64.const 600))))
