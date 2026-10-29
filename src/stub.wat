;; The template of every `near_contract_sim::Stub` (src/stub.rs). For each
;; declared method, in order, the stub's wasm replaces the methods marker
;; below with the storage key `reply/<i>` (as data, from offset 0) and an
;; export that calls `$reply` with that key. A method the stub doesn't
;; declare isn't exported, so calling it fails with `MethodResolveError`.
;;
;; `$reply` first looks for a reply bound to this call (`Tx::stub_reply`):
;; under its key, `/`, and the call's `env::random_seed()`, which the
;; simulator writes before the block the call runs in. Without one it reads
;; the stub-wide reply, under its key alone.
;;
;; A view runs it too: it reads the stub-wide reply (no bound one matches a
;; view's seed), and only a reply that burns gas fails there.
;;
;; It does what the value says (`Reply`, little endian):
;;   u8 kind (0: return the payload, 1: panic with it), u64 gas to leave
;;   (u64::MAX: burn nothing), u64 gas to burn (0: none), u64 least gas
;;   attached (0: any), u32 length to pad the payload to with spaces (0:
;;   none), and if the least gas isn't 0 the panic message's text before
;;   and after the attached gas, each as u32 length and bytes; then u32 log
;;   count, each log as u32 length and bytes, then u8 1 if it makes a call
;;   first (`Reply::call`: the receiver, the method and the args, each as
;;   u32 length and bytes, then a u128 deposit) or 0, then the payload to
;;   the end. In that order: the least attached gas, logs, the call (with
;;   all unused gas, weight 1), burn, burn to leave, then panic or return.
;;   The value is read to 65536, past the keys, and a message is built past
;;   its end; nearcore gives the contract its own memory, far larger than
;;   this one page.
(module
  (import "env" "storage_read" (func $storage_read (param i64 i64 i64) (result i64)))
  (import "env" "read_register" (func $read_register (param i64 i64)))
  (import "env" "register_len" (func $register_len (param i64) (result i64)))
  (import "env" "log_utf8" (func $log_utf8 (param i64 i64)))
  (import "env" "prepaid_gas" (func $prepaid_gas (result i64)))
  (import "env" "used_gas" (func $used_gas (result i64)))
  (import "env" "panic_utf8" (func $panic_utf8 (param i64 i64)))
  (import "env" "value_return" (func $value_return (param i64 i64)))
  (import "env" "random_seed" (func $random_seed (param i64)))
  (import "env" "promise_batch_create" (func $promise_batch_create (param i64 i64) (result i64)))
  (import "env" "promise_batch_action_function_call_weight"
    (func $call_weight (param i64 i64 i64 i64 i64 i64 i64 i64)))
  (memory 1)
  (export "memory" (memory 0))
  ;; {methods}

  (func $reply (param $key_len i64) (param $key i64)
    (local $p i32) (local $end i32) (local $n i32) (local $len i32)
    (local $to i32) (local $to_len i32) (local $m i32) (local $m_len i32) (local $a i32)
    (local $a_len i32) (local $burn i64) (local $start i64)
    ;; The bound key, built at 65536: the key, `/`, the 32-byte seed.
    (local.set $len (i32.wrap_i64 (local.get $key_len)))
    (local.set $p (i32.const 0))
    (block $copied (loop $copy
      (br_if $copied (i32.ge_u (local.get $p) (local.get $len)))
      (i32.store8 (i32.add (i32.const 65536) (local.get $p))
                  (i32.load8_u (i32.add (i32.wrap_i64 (local.get $key)) (local.get $p))))
      (local.set $p (i32.add (local.get $p) (i32.const 1)))
      (br $copy)))
    (i32.store8 (i32.add (i32.const 65536) (local.get $len)) (i32.const 47))
    (call $random_seed (i64.const 1))
    (call $read_register (i64.const 1)
                         (i64.extend_i32_u (i32.add (i32.const 65537) (local.get $len))))
    (if (i64.ne (call $storage_read (i64.add (local.get $key_len) (i64.const 33))
                                    (i64.const 65536) (i64.const 0))
                (i64.const 1))
      (then
        (if (i64.ne (call $storage_read (local.get $key_len) (local.get $key) (i64.const 0))
                    (i64.const 1))
          (then (call $panic_utf8 (local.get $key_len) (local.get $key))))))
    (call $read_register (i64.const 0) (i64.const 65536))
    (local.set $end
      (i32.add (i32.const 65536) (i32.wrap_i64 (call $register_len (i64.const 0)))))
    (local.set $burn (i64.load (i32.const 65545)))
    (local.set $p (i32.const 65565))
    ;; The least attached gas: under it, panic naming what was attached.
    (if (i64.ne (i64.load (i32.const 65553)) (i64.const 0))
      (then
        (local.set $m_len (i32.load (local.get $p)))
        (local.set $m (i32.add (local.get $p) (i32.const 4)))
        (local.set $a_len (i32.load (i32.add (local.get $m) (local.get $m_len))))
        (local.set $a (i32.add (i32.add (local.get $m) (local.get $m_len)) (i32.const 4)))
        (local.set $p (i32.add (local.get $a) (local.get $a_len)))
        (if (i64.lt_u (call $prepaid_gas) (i64.load (i32.const 65553)))
          (then
            (call $copy (local.get $end) (local.get $m) (local.get $m_len))
            (local.set $to (call $digits (call $prepaid_gas)
                                         (i32.add (local.get $end) (local.get $m_len))))
            (call $copy (local.get $to) (local.get $a) (local.get $a_len))
            (local.set $to (i32.add (local.get $to) (local.get $a_len)))
            (call $panic_utf8 (i64.extend_i32_u (i32.sub (local.get $to) (local.get $end)))
                              (i64.extend_i32_u (local.get $end)))))))
    ;; Logs.
    (local.set $n (i32.load (local.get $p)))
    (local.set $p (i32.add (local.get $p) (i32.const 4)))
    (block $logged (loop $log
      (br_if $logged (i32.eqz (local.get $n)))
      (local.set $len (i32.load (local.get $p)))
      (call $log_utf8 (i64.extend_i32_u (local.get $len))
                      (i64.extend_i32_u (i32.add (local.get $p) (i32.const 4))))
      (local.set $p (i32.add (local.get $p) (i32.add (local.get $len) (i32.const 4))))
      (local.set $n (i32.sub (local.get $n) (i32.const 1)))
      (br $log)))
    ;; The call to make first, if any: each part a u32 length then bytes,
    ;; then the u128 deposit, which $p is left at.
    (if (i32.eq (i32.load8_u (local.get $p)) (i32.const 1))
      (then
        (local.set $to_len (i32.load (i32.add (local.get $p) (i32.const 1))))
        (local.set $to (i32.add (local.get $p) (i32.const 5)))
        (local.set $p (i32.add (local.get $to) (local.get $to_len)))
        (local.set $m_len (i32.load (local.get $p)))
        (local.set $m (i32.add (local.get $p) (i32.const 4)))
        (local.set $p (i32.add (local.get $m) (local.get $m_len)))
        (local.set $a_len (i32.load (local.get $p)))
        (local.set $a (i32.add (local.get $p) (i32.const 4)))
        (local.set $p (i32.add (local.get $a) (local.get $a_len)))
        (call $call_weight
          (call $promise_batch_create (i64.extend_i32_u (local.get $to_len))
                                      (i64.extend_i32_u (local.get $to)))
          (i64.extend_i32_u (local.get $m_len)) (i64.extend_i32_u (local.get $m))
          (i64.extend_i32_u (local.get $a_len)) (i64.extend_i32_u (local.get $a))
          (i64.extend_i32_u (local.get $p)) (i64.const 0) (i64.const 1))
        (local.set $p (i32.add (local.get $p) (i32.const 16))))
      (else (local.set $p (i32.add (local.get $p) (i32.const 1)))))
    ;; Burn this much gas.
    (if (i64.ne (local.get $burn) (i64.const 0))
      (then
        (local.set $start (call $used_gas))
        (loop $burning
          (br_if $burning (i64.lt_u (i64.sub (call $used_gas) (local.get $start))
                                    (local.get $burn))))))
    ;; Burn gas until at most this much of the attached gas is left. A
    ;; reply that burns nothing reads no gas, so it serves views too, where
    ;; `prepaid_gas` and `used_gas` are not allowed.
    (if (i64.ne (i64.load (i32.const 65537)) (i64.const -1))
      (then
        (loop $burn
          (br_if $burn (i64.gt_u (i64.sub (call $prepaid_gas) (call $used_gas))
                                 (i64.load (i32.const 65537)))))))
    ;; Pad the payload, 8 spaces at a time, to the length asked.
    (local.set $len (i32.add (local.get $p) (i32.load (i32.const 65561))))
    (if (i32.lt_u (local.get $end) (local.get $len))
      (then
        (block $padded (loop $pad
          (br_if $padded (i32.ge_u (local.get $end) (local.get $len)))
          (i64.store (local.get $end) (i64.const 0x2020202020202020))
          (local.set $end (i32.add (local.get $end) (i32.const 8)))
          (br $pad)))
        (local.set $end (local.get $len))))
    (if (i32.eq (i32.load8_u (i32.const 65536)) (i32.const 1))
      (then (call $panic_utf8 (i64.extend_i32_u (i32.sub (local.get $end) (local.get $p)))
                              (i64.extend_i32_u (local.get $p)))))
    (call $value_return (i64.extend_i32_u (i32.sub (local.get $end) (local.get $p)))
                        (i64.extend_i32_u (local.get $p))))

  ;; Copies `n` bytes from `src` to `dst`, front to back.
  (func $copy (param $dst i32) (param $src i32) (param $n i32)
    (block $done (loop $byte
      (br_if $done (i32.eqz (local.get $n)))
      (i32.store8 (local.get $dst) (i32.load8_u (local.get $src)))
      (local.set $dst (i32.add (local.get $dst) (i32.const 1)))
      (local.set $src (i32.add (local.get $src) (i32.const 1)))
      (local.set $n (i32.sub (local.get $n) (i32.const 1)))
      (br $byte))))

  ;; Writes `v` in decimal at `at`; returns where it ends.
  (func $digits (param $v i64) (param $at i32) (result i32)
    (local $q i32)
    (local.set $q (i32.add (local.get $at) (i32.const 40)))
    (loop $digit
      (local.set $q (i32.sub (local.get $q) (i32.const 1)))
      (i32.store8 (local.get $q)
                  (i32.add (i32.const 48) (i32.wrap_i64 (i64.rem_u (local.get $v) (i64.const 10)))))
      (local.set $v (i64.div_u (local.get $v) (i64.const 10)))
      (br_if $digit (i64.ne (local.get $v) (i64.const 0))))
    (call $copy (local.get $at) (local.get $q)
                (i32.sub (i32.add (local.get $at) (i32.const 40)) (local.get $q)))
    (i32.add (local.get $at) (i32.sub (i32.add (local.get $at) (i32.const 40)) (local.get $q)))))
