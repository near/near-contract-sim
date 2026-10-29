;; A staking pool that counts its holders (tests/holder_counter.rs), in
;; the shape of a liquid staking token's holder counter. Each partial
;; unstake lowers the count, not only one that leaves the holder at zero,
;; so counter + 1 unstakes in one plan, from one user or several, settled
;; or in one block, underflow it: the callback traps.
;; Inputs are raw: amounts u64 little-endian, account ids raw. A stake
;; sits under "s" + its id, the count under "n".
;;   stake       -> input: amount. Credits the predecessor; a new holder
;;                  counts one more
;;   unstake     -> input: amount. Debits the predecessor, calls its own
;;                  pay, then on_unstake(remaining stake)
;;   unstake_all -> unstake of the whole stake
;;   transfer    -> input: amount, then the receiver. Moves stake; a new
;;                  holder counts one more, a holder left at zero one less
;;   pay         -> pays nothing: the payout's stand-in
;;   on_unstake  -> the bug: lowers the count whatever remains, and traps
;;                  ("holder count underflow") below zero
;;   ping, claim -> log
;;   holders     -> (view) the count
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
  (import "env" "value_return" (func $value_return (param i64 i64)))
  (import "env" "log_utf8" (func $log_utf8 (param i64 i64)))
  (import "env" "panic_utf8" (func $panic_utf8 (param i64 i64)))
  (memory 1)
  (export "memory" (memory 0))
  (data (i32.const 0) "pay")
  (data (i32.const 16) "on_unstake")
  (data (i32.const 32) "holder count underflow")
  (data (i32.const 64) "not enough stake")
  (data (i32.const 96) "pinged")
  (data (i32.const 112) "n")
  ;; 128: u128 zero. 300: the own id. 512: a storage key. 600: a value.
  ;; 1024: input. 2048: an id.

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

  ;; The count: "n" alone.
  (func $count (result i64) (call $get (i32.const 110) (i64.const 0) (i64.const 0)))
  (func $set_count (param $v i64) (call $set (i32.const 110) (i64.const 0) (i64.const 0) (local.get $v)))

  ;; Adds `amount` to the stake of the id at `ptr`, counting a new holder.
  (func $credit (param $ptr i64) (param $len i64) (param $amount i64)
    (local $had i64)
    (local.set $had (call $get (i32.const 115) (local.get $ptr) (local.get $len)))
    (if (i32.and (i64.eqz (local.get $had)) (i64.ne (local.get $amount) (i64.const 0)))
      (then (call $set_count (i64.add (call $count) (i64.const 1)))))
    (call $set (i32.const 115) (local.get $ptr) (local.get $len)
      (i64.add (local.get $had) (local.get $amount))))

  (func (export "stake")
    (drop (call $read_input))
    (call $credit (i64.const 2048) (call $predecessor) (i64.load (i32.const 1024))))

  ;; Debits `amount` from the predecessor; returns what remains.
  (func $debit (param $amount i64) (result i64)
    (local $n i64) (local $had i64)
    (local.set $n (call $predecessor))
    (local.set $had (call $get (i32.const 115) (i64.const 2048) (local.get $n)))
    (if (i32.or (i64.eqz (local.get $amount)) (i64.lt_u (local.get $had) (local.get $amount)))
      (then (call $panic_utf8 (i64.const 16) (i64.const 64))))
    (call $set (i32.const 115) (i64.const 2048) (local.get $n)
      (i64.sub (local.get $had) (local.get $amount)))
    (i64.sub (local.get $had) (local.get $amount)))

  ;; pay, then on_unstake(remaining).
  (func $unstake (param $amount i64)
    (local $p i64)
    (i64.store (i32.const 700) (call $debit (local.get $amount)))
    (call $current_account_id (i64.const 2))
    (call $read_register (i64.const 2) (i64.const 300))
    (local.set $p (call $promise_create (call $register_len (i64.const 2)) (i64.const 300)
      (i64.const 3) (i64.const 0) (i64.const 0) (i64.const 0)
      (i64.const 128) (i64.const 5000000000000)))
    (call $promise_return (call $promise_then (local.get $p)
      (call $register_len (i64.const 2)) (i64.const 300)
      (i64.const 10) (i64.const 16)
      (i64.const 8) (i64.const 700)
      (i64.const 128) (i64.const 5000000000000))))

  (func (export "unstake")
    (drop (call $read_input))
    (call $unstake (i64.load (i32.const 1024))))

  (func (export "unstake_all")
    (call $unstake (call $get (i32.const 115) (i64.const 2048) (call $predecessor))))

  (func (export "transfer")
    (local $len i64) (local $left i64)
    (local.set $len (call $read_input))
    (local.set $left (call $debit (i64.load (i32.const 1024))))
    (if (i64.eqz (local.get $left))
      (then (call $set_count (i64.sub (call $count) (i64.const 1)))))
    (call $credit (i64.const 1032) (i64.sub (local.get $len) (i64.const 8))
      (i64.load (i32.const 1024))))

  (func (export "pay"))

  (func (export "on_unstake")
    (local $c i64)
    (local.set $c (call $count))
    (if (i64.eqz (local.get $c))
      (then (call $panic_utf8 (i64.const 22) (i64.const 32))))
    (call $set_count (i64.sub (local.get $c) (i64.const 1))))

  (func (export "ping") (call $log_utf8 (i64.const 6) (i64.const 96)))
  (func (export "claim") (call $log_utf8 (i64.const 6) (i64.const 96)))

  (func (export "holders")
    (i64.store (i32.const 600) (call $count))
    (call $value_return (i64.const 8) (i64.const 600))))
