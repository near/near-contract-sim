;; A minimal staking pool for the epoch tests (tests/epochs.rs): it stakes
;; what accounts deposit, with its own balance, and pays an unstaked amount
;; back once the cooldown is over. Amounts are u128 little-endian, the key
;; and account ids raw; each account's stake, unstaked amount and unlock
;; epoch (a u64) sit under its id, prefixed "s", "u" and "e".
;;   new               -> input: the staking key, borsh-serialized
;;   deposit_and_stake -> stakes the attached deposit for the predecessor
;;   unstake           -> unstakes all the predecessor's stake, to withdraw
;;                        4 epochs on (blocks-and-clock.md, "Epochs")
;;   withdraw          -> sends the predecessor its unstaked amount; panics
;;                        before its unlock epoch
;;   epoch_height      -> returns env::epoch_height(), a u64
;; Each change stakes the pool's total on itself (`Promise::stake`), then
;; calls its epoch_height in the same batch, so a fault can fail it.
(module
  (import "env" "input" (func $input (param i64)))
  (import "env" "register_len" (func $register_len (param i64) (result i64)))
  (import "env" "read_register" (func $read_register (param i64 i64)))
  (import "env" "current_account_id" (func $current_account_id (param i64)))
  (import "env" "predecessor_account_id" (func $predecessor_account_id (param i64)))
  (import "env" "attached_deposit" (func $attached_deposit (param i64)))
  (import "env" "epoch_height" (func $epoch_height (result i64)))
  (import "env" "storage_write" (func $storage_write (param i64 i64 i64 i64 i64) (result i64)))
  (import "env" "storage_read" (func $storage_read (param i64 i64 i64) (result i64)))
  (import "env" "promise_batch_create" (func $promise_batch_create (param i64 i64) (result i64)))
  (import "env" "promise_batch_action_stake" (func $stake (param i64 i64 i64 i64)))
  (import "env" "promise_batch_action_transfer" (func $transfer (param i64 i64)))
  (import "env" "promise_batch_action_function_call" (func $call (param i64 i64 i64 i64 i64 i64 i64)))
  (import "env" "value_return" (func $value_return (param i64 i64)))
  (import "env" "panic_utf8" (func $panic_utf8 (param i64 i64)))
  (memory 1)
  (export "memory" (memory 0))
  (data (i32.const 0) "*key")
  (data (i32.const 16) "*total")
  (data (i32.const 32) "not unlocked")
  (data (i32.const 48) "epoch_height")
  ;; 64: u128 zero
  ;; 256: the input, 512: the own id, 1024: a key (a prefix, then the
  ;; predecessor's id), 2048: a stored value, 2064: the amount moved,
  ;; 2080: the total, 2096: an epoch, 2112: the staking key

  ;; The value under the key at `ptr` to `dst`, zeros if there is none.
  (func $get (param $len i64) (param $ptr i64) (param $dst i32)
    (i64.store (local.get $dst) (i64.const 0))
    (i64.store offset=8 (local.get $dst) (i64.const 0))
    (if (i64.eq (call $storage_read (local.get $len) (local.get $ptr) (i64.const 2)) (i64.const 1))
      (then (call $read_register (i64.const 2) (i64.extend_i32_u (local.get $dst))))))

  (func $put (param $len i64) (param $ptr i64) (param $src i32) (param $n i64)
    (drop (call $storage_write (local.get $len) (local.get $ptr)
      (local.get $n) (i64.extend_i32_u (local.get $src)) (i64.const 2))))

  ;; *a += *b
  (func $add (param $a i32) (param $b i32)
    (local $lo i64)
    (local.set $lo (i64.add (i64.load (local.get $a)) (i64.load (local.get $b))))
    (i64.store offset=8 (local.get $a)
      (i64.add (i64.add (i64.load offset=8 (local.get $a)) (i64.load offset=8 (local.get $b)))
        (i64.extend_i32_u (i64.lt_u (local.get $lo) (i64.load (local.get $b))))))
    (i64.store (local.get $a) (local.get $lo)))

  ;; *a -= *b, with *b at most *a
  (func $sub (param $a i32) (param $b i32)
    (local $lo i64)
    (local.set $lo (i64.load (local.get $a)))
    (i64.store offset=8 (local.get $a)
      (i64.sub (i64.sub (i64.load offset=8 (local.get $a)) (i64.load offset=8 (local.get $b)))
        (i64.extend_i32_u (i64.lt_u (local.get $lo) (i64.load (local.get $b))))))
    (i64.store (local.get $a) (i64.sub (local.get $lo) (i64.load (local.get $b)))))

  ;; `prefix`, then the predecessor's id, at 1024; returns its length.
  (func $key (param $prefix i32) (result i64)
    (i32.store8 (i32.const 1024) (local.get $prefix))
    (call $predecessor_account_id (i64.const 1))
    (call $read_register (i64.const 1) (i64.const 1025))
    (i64.add (call $register_len (i64.const 1)) (i64.const 1)))

  ;; Zeroes the predecessor's value under `prefix`, its amount kept at 2064.
  (func $take (param $prefix i32) (result i64)
    (local $len i64)
    (local.set $len (call $key (local.get $prefix)))
    (call $get (local.get $len) (i64.const 1024) (i32.const 2064))
    (i64.store (i32.const 2048) (i64.const 0))
    (i64.store (i32.const 2056) (i64.const 0))
    (call $put (local.get $len) (i64.const 1024) (i32.const 2048) (i64.const 16))
    (local.get $len))

  ;; Adds the amount at 2064 to the predecessor's value under `prefix`.
  (func $credit (param $prefix i32)
    (local $len i64)
    (local.set $len (call $key (local.get $prefix)))
    (call $get (local.get $len) (i64.const 1024) (i32.const 2048))
    (call $add (i32.const 2048) (i32.const 2064))
    (call $put (local.get $len) (i64.const 1024) (i32.const 2048) (i64.const 16)))

  ;; Stakes the total with the pool's key.
  (func $restake
    (local $p i64)
    (call $current_account_id (i64.const 1))
    (call $read_register (i64.const 1) (i64.const 512))
    (local.set $p (call $promise_batch_create (call $register_len (i64.const 1)) (i64.const 512)))
    (drop (call $storage_read (i64.const 4) (i64.const 0) (i64.const 2)))
    (call $read_register (i64.const 2) (i64.const 2112))
    (call $stake (local.get $p) (i64.const 2080) (call $register_len (i64.const 2)) (i64.const 2112))
    (call $call (local.get $p) (i64.const 12) (i64.const 48) (i64.const 0) (i64.const 0)
      (i64.const 64) (i64.const 5000000000000)))

  (func (export "new")
    (call $input (i64.const 0))
    (call $read_register (i64.const 0) (i64.const 256))
    (drop (call $storage_write (i64.const 4) (i64.const 0)
      (call $register_len (i64.const 0)) (i64.const 256) (i64.const 2))))

  (func (export "deposit_and_stake")
    (call $attached_deposit (i64.const 2064))
    (call $credit (i32.const 115)) ;; "s"
    (call $get (i64.const 6) (i64.const 16) (i32.const 2080))
    (call $add (i32.const 2080) (i32.const 2064))
    (call $put (i64.const 6) (i64.const 16) (i32.const 2080) (i64.const 16))
    (call $restake))

  (func (export "unstake")
    (drop (call $take (i32.const 115))) ;; "s"
    (call $get (i64.const 6) (i64.const 16) (i32.const 2080))
    (call $sub (i32.const 2080) (i32.const 2064))
    (call $put (i64.const 6) (i64.const 16) (i32.const 2080) (i64.const 16))
    (call $credit (i32.const 117)) ;; "u"
    (i64.store (i32.const 2096) (i64.add (call $epoch_height) (i64.const 4))) ;; cooldown
    (call $put (call $key (i32.const 101)) (i64.const 1024) (i32.const 2096) (i64.const 8)) ;; "e"
    (call $restake))

  (func (export "withdraw")
    (local $len i64)
    (call $get (call $key (i32.const 101)) (i64.const 1024) (i32.const 2096)) ;; "e"
    (if (i64.lt_u (call $epoch_height) (i64.load (i32.const 2096)))
      (then (call $panic_utf8 (i64.const 12) (i64.const 32))))
    (local.set $len (call $take (i32.const 117))) ;; "u"
    (call $transfer
      (call $promise_batch_create (i64.sub (local.get $len) (i64.const 1)) (i64.const 1025))
      (i64.const 2064)))

  (func (export "epoch_height")
    (i64.store (i32.const 2096) (call $epoch_height))
    (call $value_return (i64.const 8) (i64.const 2096))))
