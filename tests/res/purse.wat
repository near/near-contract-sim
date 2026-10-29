;; A purse that checks only that it isn't empty: `withdraw` takes any
;; amount from a balance above 0, so one over the balance wraps it.
;;   deposit  -> input: the amount, u64 little-endian. Adds it to "b"
;;               (the balance) and "t" (all ever deposited).
;;   withdraw -> input: the amount, u64 little-endian. Panics "empty" at
;;               a balance of 0; else takes it from "b", wrapping.
;; "b" and "t" are u64 little-endian; a missing one reads 0.
(module
  (import "env" "input" (func $input (param i64)))
  (import "env" "read_register" (func $read_register (param i64 i64)))
  (import "env" "storage_read" (func $storage_read (param i64 i64 i64) (result i64)))
  (import "env" "storage_write" (func $storage_write (param i64 i64 i64 i64 i64) (result i64)))
  (import "env" "panic_utf8" (func $panic_utf8 (param i64 i64)))
  (memory 1)
  (export "memory" (memory 0))
  (data (i32.const 0) "empty")
  (data (i32.const 16) "bt")
  ;; 32: the amount; 40: a value read or written.
  (func $load (param $key i64) (result i64)
    (if (result i64)
      (i64.eq (call $storage_read (i64.const 1) (local.get $key) (i64.const 1)) (i64.const 1))
      (then
        (call $read_register (i64.const 1) (i64.const 40))
        (i64.load (i32.const 40)))
      (else (i64.const 0))))
  (func $store (param $key i64) (param $v i64)
    (i64.store (i32.const 40) (local.get $v))
    (drop (call $storage_write (i64.const 1) (local.get $key) (i64.const 8) (i64.const 40)
                               (i64.const 2))))
  (func $amount (result i64)
    (call $input (i64.const 0))
    (call $read_register (i64.const 0) (i64.const 32))
    (i64.load (i32.const 32)))
  (func (export "deposit") (local $n i64)
    (local.set $n (call $amount))
    (call $store (i64.const 16) (i64.add (call $load (i64.const 16)) (local.get $n)))
    (call $store (i64.const 17) (i64.add (call $load (i64.const 17)) (local.get $n))))
  (func (export "withdraw") (local $n i64) (local $b i64)
    (local.set $n (call $amount))
    (local.set $b (call $load (i64.const 16)))
    (if (i64.eqz (local.get $b))
      (then (call $panic_utf8 (i64.const 5) (i64.const 0))))
    (call $store (i64.const 16) (i64.sub (local.get $b) (local.get $n))))
)
