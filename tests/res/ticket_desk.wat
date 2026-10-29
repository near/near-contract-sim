;; A ticket desk with two planted bugs (tests/outsider.rs).
;;   buy   -> payable: sells one ticket. Adds 1 to "s" (u64 LE) and the
;;            deposit to "p" (u128 LE). It only refuses a zero deposit, where
;;            it should require the 1 NEAR price.
;;   claim -> {"beneficiary":"<id>"} exactly: writes "c" + id (a credit for
;;            the beneficiary), trusting the argument rather than the
;;            predecessor or a member list.
(module
  (import "env" "input" (func $input (param i64)))
  (import "env" "register_len" (func $register_len (param i64) (result i64)))
  (import "env" "read_register" (func $read_register (param i64 i64)))
  (import "env" "attached_deposit" (func $attached_deposit (param i64)))
  (import "env" "storage_read" (func $storage_read (param i64 i64 i64) (result i64)))
  (import "env" "storage_write" (func $storage_write (param i64 i64 i64 i64 i64) (result i64)))
  (import "env" "panic_utf8" (func $panic_utf8 (param i64 i64)))
  (memory 1)
  (export "memory" (memory 0))
  (data (i32.const 0) "s")
  (data (i32.const 8) "p")
  (data (i32.const 16) "attach a deposit")
  ;; 64: the deposit (u128). 128: "s". 256: "p" (u128). 1024: the input.

  (func (export "buy")
    (local $lo i64)
    (call $attached_deposit (i64.const 64))
    (if (i64.eqz (i64.or (i64.load (i32.const 64)) (i64.load (i32.const 72))))
      (then (call $panic_utf8 (i64.const 16) (i64.const 16))))
    ;; s += 1
    (i64.store (i32.const 128) (i64.const 0))
    (if (i64.eq (call $storage_read (i64.const 1) (i64.const 0) (i64.const 1)) (i64.const 1))
      (then (call $read_register (i64.const 1) (i64.const 128))))
    (i64.store (i32.const 128) (i64.add (i64.load (i32.const 128)) (i64.const 1)))
    (drop (call $storage_write (i64.const 1) (i64.const 0) (i64.const 8) (i64.const 128)
                               (i64.const 2)))
    ;; p += deposit, as u128
    (i64.store (i32.const 256) (i64.const 0))
    (i64.store (i32.const 264) (i64.const 0))
    (if (i64.eq (call $storage_read (i64.const 1) (i64.const 8) (i64.const 1)) (i64.const 1))
      (then (call $read_register (i64.const 1) (i64.const 256))))
    (local.set $lo (i64.add (i64.load (i32.const 256)) (i64.load (i32.const 64))))
    (i64.store (i32.const 264)
      (i64.add (i64.add (i64.load (i32.const 264)) (i64.load (i32.const 72)))
               (i64.extend_i32_u (i64.lt_u (local.get $lo) (i64.load (i32.const 64))))))
    (i64.store (i32.const 256) (local.get $lo))
    (drop (call $storage_write (i64.const 1) (i64.const 8) (i64.const 16) (i64.const 256)
                               (i64.const 2))))

  ;; {"beneficiary":" is 16 bytes and "} 2: the id sits between. Its key is
  ;; "c" + id, written over the quote before it.
  (func (export "claim")
    (local $len i64)
    (call $input (i64.const 0))
    (local.set $len (call $register_len (i64.const 0)))
    (call $read_register (i64.const 0) (i64.const 1024))
    (i32.store8 (i32.const 1039) (i32.const 99))
    (drop (call $storage_write (i64.sub (local.get $len) (i64.const 17)) (i64.const 1039)
                               (i64.const 1) (i64.const 16) (i64.const 2))))
)
