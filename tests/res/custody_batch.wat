;; A custody that pays out through a batch call on a fixed budget
;; (tests/custody_batch.rs). One module, two roles: on
;; custody.near it keeps credit, on mt.near it is the batch token. Inputs
;; are raw: amounts and counts u64 little-endian, account ids raw. A
;; credit sits under "c" + its id, what the token paid under "p" + its id.
;;
;; The token:
;;   work       -> input: amount, n, then the payee. Works n items, each a
;;                 fixed spin (about 2.6 Tgas), then pays the payee the
;;                 amount. At 8 items its 20 Tgas budget runs out
;;   paid_of    -> (view) input: an id. What it paid that id
;; The custody:
;;   fund       -> input: amount. Credits the predecessor
;;   deposit    -> input: n. Calls mt.near's work(0, n, predecessor) with
;;                 20 Tgas: it pays nothing, but reaches the same callee
;;   withdraw   -> input: amount, then n. Debits the predecessor's credit,
;;                 calls mt.near's work(amount, n, predecessor) with 20
;;                 Tgas, then its own on_withdraw (10 Tgas)
;;   on_withdraw -> logs a failed work, and never re-credits: a batch that
;;                 runs out of gas loses the amount
;;   credit_of  -> (view) input: an id. Its credit
(module
  (import "env" "input" (func $input (param i64)))
  (import "env" "register_len" (func $register_len (param i64) (result i64)))
  (import "env" "read_register" (func $read_register (param i64 i64)))
  (import "env" "predecessor_account_id" (func $predecessor_account_id (param i64)))
  (import "env" "current_account_id" (func $current_account_id (param i64)))
  (import "env" "storage_write" (func $storage_write (param i64 i64 i64 i64 i64) (result i64)))
  (import "env" "storage_read" (func $storage_read (param i64 i64 i64) (result i64)))
  (import "env" "promise_create"
    (func $promise_create (param i64 i64 i64 i64 i64 i64 i64 i64) (result i64)))
  (import "env" "promise_then"
    (func $promise_then (param i64 i64 i64 i64 i64 i64 i64 i64 i64) (result i64)))
  (import "env" "promise_return" (func $promise_return (param i64)))
  (import "env" "promise_result" (func $promise_result (param i64 i64) (result i64)))
  (import "env" "value_return" (func $value_return (param i64 i64)))
  (import "env" "log_utf8" (func $log_utf8 (param i64 i64)))
  (import "env" "panic_utf8" (func $panic_utf8 (param i64 i64)))
  (memory 1)
  (export "memory" (memory 0))
  (data (i32.const 0) "mt.near")
  (data (i32.const 16) "work")
  (data (i32.const 32) "on_withdraw")
  (data (i32.const 48) "work failed")
  (data (i32.const 96) "not enough credit")
  ;; 64: u128 zero. 300: the own id. 512: a storage key. 600: a value.
  ;; 700: a call's args (amount, n, payee). 1024: input. 2048: an id.

  ;; The u64 under `prefix` + the id at `ptr`, `len` bytes long; 0 if none.
  (func $get (param $prefix i32) (param $ptr i64) (param $len i64) (result i64)
    (call $key (local.get $prefix) (local.get $ptr) (local.get $len))
    (if (result i64)
        (i64.eq (call $storage_read (i64.add (local.get $len) (i64.const 1)) (i64.const 512)
                                    (i64.const 0))
                (i64.const 1))
      (then
        (call $read_register (i64.const 0) (i64.const 600))
        (i64.load (i32.const 600)))
      (else (i64.const 0))))

  (func $set (param $prefix i32) (param $ptr i64) (param $len i64) (param $v i64)
    (call $key (local.get $prefix) (local.get $ptr) (local.get $len))
    (i64.store (i32.const 600) (local.get $v))
    (drop (call $storage_write (i64.add (local.get $len) (i64.const 1)) (i64.const 512)
                               (i64.const 8) (i64.const 600) (i64.const 9))))

  (func $key (param $prefix i32) (param $ptr i64) (param $len i64)
    (i32.store8 (i32.const 512) (local.get $prefix))
    (call $copy (i32.const 513) (i32.wrap_i64 (local.get $ptr)) (i32.wrap_i64 (local.get $len))))

  ;; `len` bytes from `src` to `dst`.
  (func $copy (param $dst i32) (param $src i32) (param $len i32)
    (local $i i32)
    (block $done (loop $byte
      (br_if $done (i32.ge_u (local.get $i) (local.get $len)))
      (i32.store8 (i32.add (local.get $dst) (local.get $i))
                  (i32.load8_u (i32.add (local.get $src) (local.get $i))))
      (local.set $i (i32.add (local.get $i) (i32.const 1)))
      (br $byte))))

  (func $read_input (result i64)
    (call $input (i64.const 0))
    (call $read_register (i64.const 0) (i64.const 1024))
    (call $register_len (i64.const 0)))

  (func $predecessor (result i64)
    (call $predecessor_account_id (i64.const 1))
    (call $read_register (i64.const 1) (i64.const 2048))
    (call $register_len (i64.const 1)))

  (func $view (param $prefix i32)
    (local $n i64)
    (local.set $n (call $read_input))
    (i64.store (i32.const 600) (call $get (local.get $prefix) (i64.const 1024) (local.get $n)))
    (call $value_return (i64.const 8) (i64.const 600)))

  ;; ---- the token ----

  (func (export "work")
    (local $len i64) (local $n i64) (local $i i64) (local $spin i32)
    (local.set $len (call $read_input))
    (local.set $n (i64.load (i32.const 1032)))
    (block $done (loop $item
      (br_if $done (i64.ge_u (local.get $i) (local.get $n)))
      (local.set $spin (i32.const 0))
      (block $spun (loop $turn
        (br_if $spun (i32.ge_u (local.get $spin) (i32.const 320000)))
        (local.set $spin (i32.add (local.get $spin) (i32.const 1)))
        (br $turn)))
      (local.set $i (i64.add (local.get $i) (i64.const 1)))
      (br $item)))
    (call $set (i32.const 112) (i64.const 1040) (i64.sub (local.get $len) (i64.const 16))
      (i64.add (call $get (i32.const 112) (i64.const 1040) (i64.sub (local.get $len) (i64.const 16)))
               (i64.load (i32.const 1024)))))

  (func (export "paid_of") (call $view (i32.const 112)))

  ;; ---- the custody ----

  (func (export "fund")
    (local $n i64)
    (drop (call $read_input))
    (local.set $n (call $predecessor))
    (call $set (i32.const 99) (i64.const 2048) (local.get $n)
      (i64.add (call $get (i32.const 99) (i64.const 2048) (local.get $n))
               (i64.load (i32.const 1024)))))

  ;; Calls mt.near's work(amount, n, predecessor) with 20 Tgas; returns
  ;; the promise.
  (func $work (param $amount i64) (param $n i64) (result i64)
    (local $who i64)
    (local.set $who (call $predecessor))
    (i64.store (i32.const 700) (local.get $amount))
    (i64.store (i32.const 708) (local.get $n))
    (call $copy (i32.const 716) (i32.const 2048) (i32.wrap_i64 (local.get $who)))
    (call $promise_create (i64.const 7) (i64.const 0) (i64.const 4) (i64.const 16)
      (i64.add (i64.const 16) (local.get $who)) (i64.const 700)
      (i64.const 64) (i64.const 20000000000000)))

  (func (export "deposit")
    (drop (call $read_input))
    (call $promise_return (call $work (i64.const 0) (i64.load (i32.const 1024)))))

  (func (export "withdraw")
    (local $amount i64) (local $who i64) (local $have i64) (local $p i64)
    (drop (call $read_input))
    (local.set $amount (i64.load (i32.const 1024)))
    (local.set $who (call $predecessor))
    (local.set $have (call $get (i32.const 99) (i64.const 2048) (local.get $who)))
    (if (i64.lt_u (local.get $have) (local.get $amount))
      (then (call $panic_utf8 (i64.const 17) (i64.const 96))))
    (call $set (i32.const 99) (i64.const 2048) (local.get $who)
      (i64.sub (local.get $have) (local.get $amount)))
    (local.set $p (call $work (local.get $amount) (i64.load (i32.const 1032))))
    (call $current_account_id (i64.const 2))
    (call $read_register (i64.const 2) (i64.const 300))
    (call $promise_return (call $promise_then (local.get $p)
      (call $register_len (i64.const 2)) (i64.const 300)
      (i64.const 11) (i64.const 32)
      (i64.const 0) (i64.const 0)
      (i64.const 64) (i64.const 10000000000000))))

  (func (export "on_withdraw")
    (if (i64.ne (call $promise_result (i64.const 0) (i64.const 3)) (i64.const 1))
      (then (call $log_utf8 (i64.const 11) (i64.const 48)))))

  (func (export "credit_of") (call $view (i32.const 99))))
