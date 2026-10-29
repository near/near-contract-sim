;; A standard NEP-141 receiver, answering `ft_on_transfer` as near-sdk's
;; fungible-token test-contract-defi does (tests/token_effects.rs). It reads
;; the JSON the token sends, {"sender_id":..,"amount":"N","msg":..}, and
;; returns the amount it didn't use, which the token's `ft_resolve_transfer`
;; refunds to the sender:
;;   msg "take-my-money" -> "0": it keeps all N;
;;   msg "<digits>"      -> that number, verbatim, as the unused amount: "40"
;;                          on 100 keeps 60, "100" refunds all;
;;   any other msg       -> panics "Not an integer", and the token refunds
;;                          all N.
;; The defi contract returns a numeric msg's amount through a call to its
;; own `value_please`; this one returns it at once, in the same receipt.
(module
  (import "env" "input" (func $input (param i64)))
  (import "env" "register_len" (func $register_len (param i64) (result i64)))
  (import "env" "read_register" (func $read_register (param i64 i64)))
  (import "env" "value_return" (func $value_return (param i64 i64)))
  (import "env" "panic_utf8" (func $panic_utf8 (param i64 i64)))
  (memory 1)
  (export "memory" (memory 0))
  (data (i32.const 0) "\"msg\":\"")
  (data (i32.const 16) "\"msg\":\"take-my-money\"")
  (data (i32.const 80) "Not an integer")
  (data (i32.const 96) "\"0\"")
  ;; 1024: the input.

  ;; Where the `$nlen` bytes at `$n` start in the input's first `$len`
  ;; bytes, or -1.
  (func $find (param $len i32) (param $n i32) (param $nlen i32) (result i32)
    (local $i i32) (local $j i32)
    (block $none
      (loop $at
        (br_if $none (i32.gt_s (i32.add (local.get $i) (local.get $nlen)) (local.get $len)))
        (local.set $j (i32.const 0))
        (block $miss
          (loop $byte
            (if (i32.eq (local.get $j) (local.get $nlen)) (then (return (local.get $i))))
            (br_if $miss
              (i32.ne
                (i32.load8_u (i32.add (i32.const 1024) (i32.add (local.get $i) (local.get $j))))
                (i32.load8_u (i32.add (local.get $n) (local.get $j)))))
            (local.set $j (i32.add (local.get $j) (i32.const 1)))
            (br $byte)))
        (local.set $i (i32.add (local.get $i) (i32.const 1)))
        (br $at)))
    (i32.const -1))

  (func $not_an_integer
    (call $panic_utf8 (i64.const 14) (i64.const 80))
    (unreachable))

  (func (export "ft_on_transfer")
    (local $len i32) (local $at i32) (local $start i32) (local $end i32) (local $c i32)
    (call $input (i64.const 0))
    (local.set $len (i32.wrap_i64 (call $register_len (i64.const 0))))
    (call $read_register (i64.const 0) (i64.const 1024))
    (if (i32.ge_s (call $find (local.get $len) (i32.const 16) (i32.const 21)) (i32.const 0))
      (then (call $value_return (i64.const 3) (i64.const 96)) (return)))
    (local.set $at (call $find (local.get $len) (i32.const 0) (i32.const 7)))
    (if (i32.lt_s (local.get $at) (i32.const 0)) (then (call $not_an_integer)))
    ;; The msg's opening quote, then its digits up to the closing one.
    (local.set $start (i32.add (local.get $at) (i32.const 6)))
    (local.set $end (i32.add (local.get $start) (i32.const 1)))
    (block $done
      (loop $digit
        (br_if $done (i32.ge_s (local.get $end) (local.get $len)))
        (local.set $c (i32.load8_u (i32.add (i32.const 1024) (local.get $end))))
        (br_if $done (i32.eq (local.get $c) (i32.const 34)))
        (if (i32.or (i32.lt_u (local.get $c) (i32.const 48)) (i32.gt_u (local.get $c) (i32.const 57)))
          (then (call $not_an_integer)))
        (local.set $end (i32.add (local.get $end) (i32.const 1)))
        (br $digit)))
    ;; No digits, or no closing quote.
    (if (i32.or
          (i32.eq (local.get $end) (i32.add (local.get $start) (i32.const 1)))
          (i32.ge_s (local.get $end) (local.get $len)))
      (then (call $not_an_integer)))
    (call $value_return
      (i64.extend_i32_u (i32.add (i32.sub (local.get $end) (local.get $start)) (i32.const 1)))
      (i64.extend_i32_u (i32.add (i32.const 1024) (local.get $start))))))
