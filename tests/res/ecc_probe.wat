;; An external contract call probe. `__external_call` and the ordinary
;; method `call` run the same dispatcher on the input, by its first byte:
;;
;;   (empty)  accept, log "accepted"
;;   0 rest   accept, log "accepted", return rest
;;   1        write "w" = "1", then panic "refused"
;;   2 rest   write "k" = rest
;;   3        signer_account_pk
;;   4        predecessor_account_id
;;   5        prepaid_gas
;;   6        random_seed
;;   7        promise_yield_create
;;   8        promise_yield_create_with_id
;;   9        return signer_account_id
;;   10       return refund_to_account_id
;;   11       return attached_deposit (u128 LE)
;;   12       return account_balance (u128 LE)
;;   13 gas:u64 weight:u64 deposit:u128 mlen:u8 method target
;;            call `method` of `target` with `gas`, `weight` and
;;            `deposit`, and return the promise
;;   14 n:u64 loop n times
;;   15 n:u32 read keys 0..n (u32 LE)
;;   16 n:u32 remove keys 0..n
;;   17       return block_index, block_timestamp (u64 LE each)
;;   18 n:u32 s:u32 write keys 0..n, each a value of s bytes: the key, then zeros
;;   19       log "accepted", then panic "refused"
;;
;; `ping` logs "ping" and returns "pong"; `pk` returns signer_account_pk;
;; `fwd` calls its own `pk` with 10 Tgas and returns that promise.
(module
  (import "env" "input" (func $input (param i64)))
  (import "env" "register_len" (func $register_len (param i64) (result i64)))
  (import "env" "read_register" (func $read_register (param i64 i64)))
  (import "env" "value_return" (func $value_return (param i64 i64)))
  (import "env" "log_utf8" (func $log (param i64 i64)))
  (import "env" "panic_utf8" (func $panic (param i64 i64)))
  (import "env" "storage_write" (func $storage_write (param i64 i64 i64 i64 i64) (result i64)))
  (import "env" "storage_read" (func $storage_read (param i64 i64 i64) (result i64)))
  (import "env" "storage_remove" (func $storage_remove (param i64 i64 i64) (result i64)))
  (import "env" "signer_account_pk" (func $signer_account_pk (param i64)))
  (import "env" "signer_account_id" (func $signer_account_id (param i64)))
  (import "env" "predecessor_account_id" (func $predecessor_account_id (param i64)))
  (import "env" "refund_to_account_id" (func $refund_to_account_id (param i64)))
  (import "env" "current_account_id" (func $current_account_id (param i64)))
  (import "env" "prepaid_gas" (func $prepaid_gas (result i64)))
  (import "env" "random_seed" (func $random_seed (param i64)))
  (import "env" "attached_deposit" (func $attached_deposit (param i64)))
  (import "env" "account_balance" (func $account_balance (param i64)))
  (import "env" "block_index" (func $block_index (result i64)))
  (import "env" "block_timestamp" (func $block_timestamp (result i64)))
  (import "env" "promise_yield_create"
    (func $yield_create (param i64 i64 i64 i64 i64 i64 i64) (result i64)))
  (import "env" "promise_yield_create_with_id"
    (func $yield_create_with_id (param i64 i64 i64 i64 i64 i64 i64 i64 i64) (result i64)))
  (import "env" "promise_batch_create" (func $batch_create (param i64 i64) (result i64)))
  (import "env" "promise_batch_action_function_call_weight"
    (func $call_weight (param i64 i64 i64 i64 i64 i64 i64 i64)))
  (import "env" "promise_return" (func $promise_return (param i64)))
  (memory 1)
  (export "memory" (memory 0))
  (data (i32.const 0) "accepted")
  (data (i32.const 16) "refused")
  (data (i32.const 32) "w1k")
  (data (i32.const 48) "ping")
  (data (i32.const 64) "pong")
  (data (i32.const 80) "pk")
  (data (i32.const 96) "cb")

  ;; Returns register `r` as the call's value, through memory at 8192.
  (func $return_register (param $r i64)
    (call $read_register (local.get $r) (i64.const 8192))
    (call $value_return (call $register_len (local.get $r)) (i64.const 8192)))

  (func $dispatch (local $len i64) (local $op i32) (local $i i64) (local $n i64)
    (local $mlen i64) (local $p i64)
    (call $input (i64.const 0))
    (local.set $len (call $register_len (i64.const 0)))
    (if (i64.eqz (local.get $len)) (then
      (call $log (i64.const 8) (i64.const 0))
      (return)))
    (call $read_register (i64.const 0) (i64.const 1024))
    (local.set $op (i32.load8_u (i32.const 1024)))
    (if (i32.eq (local.get $op) (i32.const 0)) (then
      (call $log (i64.const 8) (i64.const 0))
      (call $value_return (i64.sub (local.get $len) (i64.const 1)) (i64.const 1025))
      (return)))
    (if (i32.eq (local.get $op) (i32.const 1)) (then
      (drop (call $storage_write (i64.const 1) (i64.const 32) (i64.const 1) (i64.const 33)
        (i64.const 1)))
      (call $panic (i64.const 7) (i64.const 16))))
    (if (i32.eq (local.get $op) (i32.const 2)) (then
      (drop (call $storage_write (i64.const 1) (i64.const 34)
        (i64.sub (local.get $len) (i64.const 1)) (i64.const 1025) (i64.const 1)))
      (return)))
    (if (i32.eq (local.get $op) (i32.const 3)) (then
      (call $signer_account_pk (i64.const 1)) (return)))
    (if (i32.eq (local.get $op) (i32.const 4)) (then
      (call $predecessor_account_id (i64.const 1)) (return)))
    (if (i32.eq (local.get $op) (i32.const 5)) (then
      (drop (call $prepaid_gas)) (return)))
    (if (i32.eq (local.get $op) (i32.const 6)) (then
      (call $random_seed (i64.const 1)) (return)))
    (if (i32.eq (local.get $op) (i32.const 7)) (then
      (drop (call $yield_create (i64.const 2) (i64.const 96) (i64.const 0) (i64.const 0)
        (i64.const 5000000000000) (i64.const 0) (i64.const 1)))
      (return)))
    (if (i32.eq (local.get $op) (i32.const 8)) (then
      (drop (call $yield_create_with_id (i64.const 2) (i64.const 96) (i64.const 0)
        (i64.const 0) (i64.const 4096) (i64.const 5000000000000) (i64.const 0)
        (i64.const 32) (i64.const 4096)))
      (return)))
    (if (i32.eq (local.get $op) (i32.const 9)) (then
      (call $signer_account_id (i64.const 1))
      (call $return_register (i64.const 1))
      (return)))
    (if (i32.eq (local.get $op) (i32.const 10)) (then
      (call $refund_to_account_id (i64.const 1))
      (call $return_register (i64.const 1))
      (return)))
    (if (i32.eq (local.get $op) (i32.const 11)) (then
      (call $attached_deposit (i64.const 4096))
      (call $value_return (i64.const 16) (i64.const 4096))
      (return)))
    (if (i32.eq (local.get $op) (i32.const 12)) (then
      (call $account_balance (i64.const 4096))
      (call $value_return (i64.const 16) (i64.const 4096))
      (return)))
    (if (i32.eq (local.get $op) (i32.const 13)) (then
      (local.set $mlen (i64.extend_i32_u (i32.load8_u (i32.const 1057))))
      (local.set $p (call $batch_create
        (i64.sub (local.get $len) (i64.add (i64.const 34) (local.get $mlen)))
        (i64.add (i64.const 1058) (local.get $mlen))))
      (call $call_weight (local.get $p) (local.get $mlen) (i64.const 1058)
        (i64.const 0) (i64.const 0) (i64.const 1041)
        (i64.load (i32.const 1025)) (i64.load (i32.const 1033)))
      (call $promise_return (local.get $p))
      (return)))
    (if (i32.eq (local.get $op) (i32.const 14)) (then
      (local.set $n (i64.load (i32.const 1025)))
      (block $out (loop $again
        (br_if $out (i64.ge_u (local.get $i) (local.get $n)))
        (local.set $i (i64.add (local.get $i) (i64.const 1)))
        (br $again)))
      (return)))
    (if (i32.eq (local.get $op) (i32.const 15)) (then
      (local.set $n (i64.extend_i32_u (i32.load (i32.const 1025))))
      (block $out (loop $again
        (br_if $out (i64.ge_u (local.get $i) (local.get $n)))
        (i64.store32 (i32.const 4096) (local.get $i))
        (drop (call $storage_read (i64.const 4) (i64.const 4096) (i64.const 1)))
        (local.set $i (i64.add (local.get $i) (i64.const 1)))
        (br $again)))
      (return)))
    (if (i32.eq (local.get $op) (i32.const 16)) (then
      (local.set $n (i64.extend_i32_u (i32.load (i32.const 1025))))
      (block $out (loop $again
        (br_if $out (i64.ge_u (local.get $i) (local.get $n)))
        (i64.store32 (i32.const 4096) (local.get $i))
        (drop (call $storage_remove (i64.const 4) (i64.const 4096) (i64.const 1)))
        (local.set $i (i64.add (local.get $i) (i64.const 1)))
        (br $again)))
      (return)))
    (if (i32.eq (local.get $op) (i32.const 17)) (then
      (i64.store (i32.const 4096) (call $block_index))
      (i64.store (i32.const 4104) (call $block_timestamp))
      (call $value_return (i64.const 16) (i64.const 4096))
      (return)))
    (if (i32.eq (local.get $op) (i32.const 18)) (then
      (local.set $n (i64.extend_i32_u (i32.load (i32.const 1025))))
      (block $out (loop $again
        (br_if $out (i64.ge_u (local.get $i) (local.get $n)))
        (i64.store32 (i32.const 4096) (local.get $i))
        (i64.store32 (i32.const 16384) (local.get $i))
        (drop (call $storage_write (i64.const 4) (i64.const 4096)
          (i64.extend_i32_u (i32.load (i32.const 1029))) (i64.const 16384) (i64.const 1)))
        (local.set $i (i64.add (local.get $i) (i64.const 1)))
        (br $again)))
      (return)))
    (if (i32.eq (local.get $op) (i32.const 19)) (then
      (call $log (i64.const 8) (i64.const 0))
      (call $panic (i64.const 7) (i64.const 16))))
    (call $panic (i64.const 7) (i64.const 16)))

  (func (export "__external_call") (call $dispatch))
  (func (export "call") (call $dispatch))
  (func (export "ping")
    (call $log (i64.const 4) (i64.const 48))
    (call $value_return (i64.const 4) (i64.const 64)))
  (func (export "pk")
    (call $signer_account_pk (i64.const 1))
    (call $return_register (i64.const 1)))
  (func (export "fwd") (local $p i64)
    (call $current_account_id (i64.const 1))
    (call $read_register (i64.const 1) (i64.const 8192))
    (local.set $p (call $batch_create (call $register_len (i64.const 1)) (i64.const 8192)))
    (call $call_weight (local.get $p) (i64.const 2) (i64.const 80) (i64.const 0) (i64.const 0)
      (i64.const 4096) (i64.const 10000000000000) (i64.const 0))
    (call $promise_return (local.get $p))))
