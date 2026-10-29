;; A 1:1 NEAR wrapper, as wNEAR is (tests/books.rs):
;;   near_deposit   -> payable: credits the predecessor the deposit
;;   near_withdraw  -> input: an amount, u64 little-endian. Debits the
;;                     predecessor and transfers it that NEAR
;;   ft_balance_of  -> (view) input: `{"account_id":"<id>"}`. Its balance,
;;                     a JSON string of digits
;; A balance sits under the view's own JSON input, as a u64 (so deposits
;; stay below 2^64 yocto), and the key is built from the predecessor.
(module
  (import "env" "input" (func $input (param i64)))
  (import "env" "register_len" (func $register_len (param i64) (result i64)))
  (import "env" "read_register" (func $read_register (param i64 i64)))
  (import "env" "attached_deposit" (func $attached_deposit (param i64)))
  (import "env" "predecessor_account_id" (func $predecessor_account_id (param i64)))
  (import "env" "storage_write" (func $storage_write (param i64 i64 i64 i64 i64) (result i64)))
  (import "env" "storage_read" (func $storage_read (param i64 i64 i64) (result i64)))
  (import "env" "promise_batch_create" (func $batch (param i64 i64) (result i64)))
  (import "env" "promise_batch_action_transfer" (func $transfer (param i64 i64)))
  (import "env" "value_return" (func $value_return (param i64 i64)))
  (import "env" "panic_utf8" (func $panic_utf8 (param i64 i64)))
  (memory 1)
  (export "memory" (memory 0))
  (data (i32.const 256) "{\"account_id\":\"")
  (data (i32.const 32) "insufficient balance")
  ;; 256: the key (its prefix stays from the data segment). 512: a u64 value. 600: u128 amount. 1024: input.
  ;; 2048: the view's digits.

  ;; Builds the predecessor's key at 256; its length.
  (func $key (result i64)
    (local $len i64)
    (call $predecessor_account_id (i64.const 0))
    (local.set $len (call $register_len (i64.const 0)))
    (call $read_register (i64.const 0) (i64.const 271))
    (i32.store8 (i32.add (i32.const 271) (i32.wrap_i64 (local.get $len))) (i32.const 34))
    (i32.store8 (i32.add (i32.const 272) (i32.wrap_i64 (local.get $len))) (i32.const 125))
    (i64.add (local.get $len) (i64.const 17)))

  ;; The balance under the key at `ptr`, `len` bytes.
  (func $get (param $len i64) (param $ptr i64) (result i64)
    (if (result i64) (i64.eq (call $storage_read (local.get $len) (local.get $ptr) (i64.const 1))
                             (i64.const 1))
      (then (call $read_register (i64.const 1) (i64.const 512)) (i64.load (i32.const 512)))
      (else (i64.const 0))))

  (func $set (param $len i64) (param $v i64)
    (i64.store (i32.const 512) (local.get $v))
    (drop (call $storage_write (local.get $len) (i64.const 256) (i64.const 8) (i64.const 512)
                               (i64.const 2))))

  (func (export "near_deposit")
    (local $len i64)
    (call $attached_deposit (i64.const 600))
    (local.set $len (call $key))
    (call $set (local.get $len)
      (i64.add (call $get (local.get $len) (i64.const 256)) (i64.load (i32.const 600)))))

  (func (export "near_withdraw")
    (local $len i64) (local $n i64) (local $have i64)
    (call $input (i64.const 0))
    (call $read_register (i64.const 0) (i64.const 1024))
    (local.set $n (i64.load (i32.const 1024)))
    (local.set $len (call $key))
    (local.set $have (call $get (local.get $len) (i64.const 256)))
    (if (i64.lt_u (local.get $have) (local.get $n))
      (then (call $panic_utf8 (i64.const 20) (i64.const 32))))
    (call $set (local.get $len) (i64.sub (local.get $have) (local.get $n)))
    (i64.store (i32.const 600) (local.get $n))
    (i64.store (i32.const 608) (i64.const 0))
    (call $predecessor_account_id (i64.const 0))
    (call $read_register (i64.const 0) (i64.const 1024))
    (call $transfer (call $batch (call $register_len (i64.const 0)) (i64.const 1024))
                    (i64.const 600)))

  (func (export "ft_balance_of")
    (local $v i64) (local $at i32)
    (call $input (i64.const 0))
    (call $read_register (i64.const 0) (i64.const 1024))
    (local.set $v (call $get (call $register_len (i64.const 0)) (i64.const 1024)))
    ;; Digits backwards from 2100, then the quotes.
    (local.set $at (i32.const 2100))
    (i32.store8 (local.get $at) (i32.const 34))
    (loop $digit
      (local.set $at (i32.sub (local.get $at) (i32.const 1)))
      (i32.store8 (local.get $at)
        (i32.add (i32.const 48) (i32.wrap_i64 (i64.rem_u (local.get $v) (i64.const 10)))))
      (local.set $v (i64.div_u (local.get $v) (i64.const 10)))
      (br_if $digit (i64.ne (local.get $v) (i64.const 0))))
    (local.set $at (i32.sub (local.get $at) (i32.const 1)))
    (i32.store8 (local.get $at) (i32.const 34))
    (call $value_return (i64.extend_i32_u (i32.sub (i32.const 2101) (local.get $at)))
                        (i64.extend_i32_u (local.get $at))))
)
