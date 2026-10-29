;; A ledger whose `spend` debits the holder its args name, not its caller,
;; as a payload signed in the args does (a relayer submits it):
;;   set       -> input: an amount (u64 little-endian), then the holder id.
;;                Sets the holder's credit
;;   spend     -> input: an amount, then the holder id. Debits the holder;
;;                panics "not enough credit" past its credit
;;   credit_of -> (view) input: the holder id. Its credit, u64 little-endian
(module
  (import "env" "input" (func $input (param i64)))
  (import "env" "register_len" (func $register_len (param i64) (result i64)))
  (import "env" "read_register" (func $read_register (param i64 i64)))
  (import "env" "storage_write" (func $storage_write (param i64 i64 i64 i64 i64) (result i64)))
  (import "env" "storage_read" (func $storage_read (param i64 i64 i64) (result i64)))
  (import "env" "value_return" (func $value_return (param i64 i64)))
  (import "env" "panic_utf8" (func $panic_utf8 (param i64 i64)))
  (memory 1)
  (export "memory" (memory 0))
  (data (i32.const 0) "not enough credit")
  ;; 1024: input (amount, then holder). 512: a credit.

  ;; The credit of the holder of `len` bytes at `ptr`.
  (func $get (param $len i64) (param $ptr i64) (result i64)
    (if (result i64) (i64.eq (call $storage_read (local.get $len) (local.get $ptr) (i64.const 1))
                             (i64.const 1))
      (then (call $read_register (i64.const 1) (i64.const 512)) (i64.load (i32.const 512)))
      (else (i64.const 0))))

  (func $put (param $len i64) (param $ptr i64) (param $v i64)
    (i64.store (i32.const 512) (local.get $v))
    (drop (call $storage_write (local.get $len) (local.get $ptr) (i64.const 8) (i64.const 512)
                               (i64.const 2))))

  ;; Reads the input at 1024: the holder's length.
  (func $args (result i64)
    (call $input (i64.const 0))
    (call $read_register (i64.const 0) (i64.const 1024))
    (i64.sub (call $register_len (i64.const 0)) (i64.const 8)))

  (func (export "set")
    (local $len i64)
    (local.set $len (call $args))
    (call $put (local.get $len) (i64.const 1032) (i64.load (i32.const 1024))))

  (func (export "spend")
    (local $len i64) (local $have i64) (local $n i64)
    (local.set $len (call $args))
    (local.set $n (i64.load (i32.const 1024)))
    (local.set $have (call $get (local.get $len) (i64.const 1032)))
    (if (i64.lt_u (local.get $have) (local.get $n))
      (then (call $panic_utf8 (i64.const 17) (i64.const 0))))
    (call $put (local.get $len) (i64.const 1032) (i64.sub (local.get $have) (local.get $n))))

  (func (export "credit_of")
    (call $input (i64.const 0))
    (call $read_register (i64.const 0) (i64.const 1024))
    (i64.store (i32.const 512) (call $get (call $register_len (i64.const 0)) (i64.const 1024)))
    (call $value_return (i64.const 8) (i64.const 512)))
)
