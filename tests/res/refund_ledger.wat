;; A credit ledger that sends credit to a receiver and takes back what the
;; receiver says to refund (tests/explore_search.rs, hostile replies). The
;; refund is capped by what the receiver holds, not by what it was sent: a
;; receiver that answers more than it got loses credit it held before.
;; tests/explore_search.rs builds the fixed ledger by replacing the
;; `{cap}` marker with a cap at the amount sent. Amounts are u64
;; little-endian, account ids raw; each credit sits under "c" + its id.
;;   fund       -> input: amount. Credits the predecessor.
;;   send       -> input: amount, then the receiver id. Moves the
;;                 predecessor's credit to the receiver, calls the
;;                 receiver's on_receive (10 Tgas) with
;;                 `{"amount":"<amount>"}`, then its own resolve
;;                 (20 Tgas), and returns that promise
;;   resolve    -> reads on_receive's result: a JSON string of digits (a
;;                 near-sdk U128, whitespace after it allowed) is the refund
;;                 asked; a failure or anything else asks all of it back.
;;                 Moves the refund, at most the receiver's credit, back
;;                 to the sender
;;   credit_of  -> (view) input: an account id. Its credit.
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
  (import "env" "panic_utf8" (func $panic_utf8 (param i64 i64)))
  (memory 3)
  (export "memory" (memory 0))
  (data (i32.const 0) "on_receive")
  (data (i32.const 16) "resolve")
  (data (i32.const 32) "not enough credit")
  ;; 64: u128 zero (no deposit). 512: a storage key. 600: a value.
  ;; 700: resolve's args. 1024: input, or on_receive's result. 1200:
  ;; on_receive's args.
  (data (i32.const 1200) "{\"amount\":\"")

  ;; The credit of the id at `ptr`, `len` bytes long.
  (func $get (param $ptr i64) (param $len i64) (result i64)
    (call $key (local.get $ptr) (local.get $len))
    (if (result i64)
        (i64.eq (call $storage_read (i64.add (local.get $len) (i64.const 1)) (i64.const 512)
                                    (i64.const 0))
                (i64.const 1))
      (then
        (call $read_register (i64.const 0) (i64.const 600))
        (i64.load (i32.const 600)))
      (else (i64.const 0))))

  (func $set (param $ptr i64) (param $len i64) (param $v i64)
    (call $key (local.get $ptr) (local.get $len))
    (i64.store (i32.const 600) (local.get $v))
    (drop (call $storage_write (i64.add (local.get $len) (i64.const 1)) (i64.const 512)
                               (i64.const 8) (i64.const 600) (i64.const 1))))

  ;; "c" + the id, at 512.
  (func $key (param $ptr i64) (param $len i64)
    (i32.store8 (i32.const 512) (i32.const 99))
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

  ;; on_receive's args at 1200, `amount` in decimal; returns their length.
  (func $args (param $v i64) (result i64)
    (local $n i32) (local $len i64) (local $t i64)
    (local.set $t (local.get $v))
    (local.set $n (i32.const 1))
    (block $counted (loop $count
      (br_if $counted (i64.lt_u (local.get $t) (i64.const 10)))
      (local.set $t (i64.div_u (local.get $t) (i64.const 10)))
      (local.set $n (i32.add (local.get $n) (i32.const 1)))
      (br $count)))
    (i32.store16 (i32.add (i32.const 1211) (local.get $n)) (i32.const 0x7d22)) ;; "}
    (local.set $len (i64.add (i64.const 13) (i64.extend_i32_u (local.get $n))))
    (local.set $t (local.get $v))
    (loop $digit
      (local.set $n (i32.sub (local.get $n) (i32.const 1)))
      (i32.store8 (i32.add (i32.const 1211) (local.get $n))
        (i32.add (i32.const 48) (i32.wrap_i64 (i64.rem_u (local.get $t) (i64.const 10)))))
      (local.set $t (i64.div_u (local.get $t) (i64.const 10)))
      (br_if $digit (i32.gt_u (local.get $n) (i32.const 0))))
    (local.get $len))

  (func (export "fund")
    (local $n i64)
    (call $input (i64.const 0))
    (call $read_register (i64.const 0) (i64.const 1024))
    (call $predecessor_account_id (i64.const 1))
    (local.set $n (call $register_len (i64.const 1)))
    (call $read_register (i64.const 1) (i64.const 100))
    (call $set (i64.const 100) (local.get $n)
      (i64.add (call $get (i64.const 100) (local.get $n)) (i64.load (i32.const 1024)))))

  (func (export "send")
    (local $amount i64) (local $to i64) (local $from i64) (local $have i64) (local $p i64)
    (call $input (i64.const 0))
    (call $read_register (i64.const 0) (i64.const 1024))
    (local.set $amount (i64.load (i32.const 1024)))
    (local.set $to (i64.sub (call $register_len (i64.const 0)) (i64.const 8)))
    (call $predecessor_account_id (i64.const 1))
    (local.set $from (call $register_len (i64.const 1)))
    (call $read_register (i64.const 1) (i64.const 100))
    (local.set $have (call $get (i64.const 100) (local.get $from)))
    (if (i64.lt_u (local.get $have) (local.get $amount))
      (then (call $panic_utf8 (i64.const 17) (i64.const 32))))
    (call $set (i64.const 100) (local.get $from) (i64.sub (local.get $have) (local.get $amount)))
    (call $set (i64.const 1032) (local.get $to)
      (i64.add (call $get (i64.const 1032) (local.get $to)) (local.get $amount)))
    ;; resolve's args at 700: amount, the sender's length (1 byte), the
    ;; sender, the receiver.
    (i64.store (i32.const 700) (local.get $amount))
    (i32.store8 (i32.const 708) (i32.wrap_i64 (local.get $from)))
    (call $copy (i32.const 709) (i32.const 100) (i32.wrap_i64 (local.get $from)))
    (call $copy (i32.add (i32.const 709) (i32.wrap_i64 (local.get $from))) (i32.const 1032)
                 (i32.wrap_i64 (local.get $to)))
    (local.set $p (call $promise_create (local.get $to) (i64.const 1032)
      (i64.const 10) (i64.const 0) (call $args (local.get $amount)) (i64.const 1200)
      (i64.const 64) (i64.const 10000000000000)))
    (call $current_account_id (i64.const 2))
    (call $read_register (i64.const 2) (i64.const 300))
    (call $promise_return (call $promise_then (local.get $p)
      (call $register_len (i64.const 2)) (i64.const 300)
      (i64.const 7) (i64.const 16)
      (i64.add (i64.const 9) (i64.add (local.get $from) (local.get $to))) (i64.const 700)
      (i64.const 64) (i64.const 20000000000000))))

  ;; The refund on_receive's result asks for, or `all` if it asks nothing
  ;; well-formed: `"digits"` then only whitespace, saturating at u64::MAX.
  (func $asked (param $all i64) (result i64)
    (local $len i32) (local $i i32) (local $c i32) (local $v i64)
    (if (i64.ne (call $promise_result (i64.const 0) (i64.const 3)) (i64.const 1))
      (then (return (local.get $all))))
    (local.set $len (i32.wrap_i64 (call $register_len (i64.const 3))))
    (call $read_register (i64.const 3) (i64.const 1024))
    (if (i32.or (i32.lt_u (local.get $len) (i32.const 3))
                (i32.ne (i32.load8_u (i32.const 1024)) (i32.const 34)))
      (then (return (local.get $all))))
    (local.set $i (i32.const 1))
    (block $digits (loop $digit
      (local.set $c (i32.sub (i32.load8_u (i32.add (i32.const 1024) (local.get $i))) (i32.const 48)))
      (br_if $digits (i32.or (i32.ge_u (local.get $i) (local.get $len))
                             (i32.gt_u (local.get $c) (i32.const 9))))
      (local.set $v
        (if (result i64) (i64.gt_u (local.get $v) (i64.const 1844674407370955160))
          (then (i64.const -1))
          (else (i64.add (i64.mul (local.get $v) (i64.const 10))
                         (i64.extend_i32_u (local.get $c))))))
      (if (i64.lt_u (local.get $v) (i64.extend_i32_u (local.get $c)))
        (then (local.set $v (i64.const -1))))
      (local.set $i (i32.add (local.get $i) (i32.const 1)))
      (br $digit)))
    (if (i32.or (i32.eq (local.get $i) (i32.const 1))
                (i32.or (i32.ge_u (local.get $i) (local.get $len))
                        (i32.ne (i32.load8_u (i32.add (i32.const 1024) (local.get $i)))
                                (i32.const 34))))
      (then (return (local.get $all))))
    (local.set $i (i32.add (local.get $i) (i32.const 1)))
    (block $rest (loop $space
      (br_if $rest (i32.ge_u (local.get $i) (local.get $len)))
      (local.set $c (i32.load8_u (i32.add (i32.const 1024) (local.get $i))))
      (if (i32.eqz (i32.or (i32.or (i32.eq (local.get $c) (i32.const 32))
                                   (i32.eq (local.get $c) (i32.const 10)))
                           (i32.or (i32.eq (local.get $c) (i32.const 9))
                                   (i32.eq (local.get $c) (i32.const 13)))))
        (then (return (local.get $all))))
      (local.set $i (i32.add (local.get $i) (i32.const 1)))
      (br $space)))
    (local.get $v))

  (func (export "resolve")
    (local $amount i64) (local $from i64) (local $to i64) (local $take i64) (local $have i64)
    (call $input (i64.const 0))
    (call $read_register (i64.const 0) (i64.const 700))
    (local.set $amount (i64.load (i32.const 700)))
    (local.set $from (i64.load8_u (i32.const 708)))
    (local.set $to (i64.sub (i64.sub (call $register_len (i64.const 0)) (i64.const 9))
                            (local.get $from)))
    ;; The sender at 100, the receiver at 200.
    (call $copy (i32.const 100) (i32.const 709) (i32.wrap_i64 (local.get $from)))
    (call $copy (i32.const 200) (i32.add (i32.const 709) (i32.wrap_i64 (local.get $from)))
                 (i32.wrap_i64 (local.get $to)))
    (local.set $take (call $asked (local.get $amount)))
    ;; {cap}
    (local.set $have (call $get (i64.const 200) (local.get $to)))
    (if (i64.gt_u (local.get $take) (local.get $have))
      (then (local.set $take (local.get $have))))
    (call $set (i64.const 200) (local.get $to) (i64.sub (local.get $have) (local.get $take)))
    (call $set (i64.const 100) (local.get $from)
      (i64.add (call $get (i64.const 100) (local.get $from)) (local.get $take))))

  (func (export "credit_of")
    (call $input (i64.const 0))
    (call $read_register (i64.const 0) (i64.const 1024))
    (i64.store (i32.const 600)
      (call $get (i64.const 1024) (call $register_len (i64.const 0))))
    (call $value_return (i64.const 8) (i64.const 600))))
