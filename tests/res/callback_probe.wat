;; A callback that reports what it sees, for `Tx::promise_results`
;; (tests/callbacks.rs): reached through a real `promise_then`, or delivered
;; by the simulator.
;;   start      -> self.answer(input).then(self.cb(input)), returns that
;;   start_and  -> (self.answer(input) & self.boom()).then(self.cb(input))
;;   answer     -> returns its input
;;   boom       -> panics
;;   relay      -> returns self.answer(input): a call that schedules one
;;   cb         -> returns what it sees, each u64 little-endian:
;;                   signer id, signer key, predecessor, current account,
;;                     each as its length then its bytes
;;                   the input's length and sha256
;;                   attached deposit (u128), prepaid gas, result count
;;                   per result: promise_result's status (1 success,
;;                     2 failure), then for a success its length and sha256
(module
  (import "env" "input" (func $input (param i64)))
  (import "env" "register_len" (func $register_len (param i64) (result i64)))
  (import "env" "read_register" (func $read_register (param i64 i64)))
  (import "env" "value_return" (func $value_return (param i64 i64)))
  (import "env" "sha256" (func $sha256 (param i64 i64 i64)))
  (import "env" "current_account_id" (func $current_account_id (param i64)))
  (import "env" "signer_account_id" (func $signer_account_id (param i64)))
  (import "env" "signer_account_pk" (func $signer_account_pk (param i64)))
  (import "env" "predecessor_account_id" (func $predecessor_account_id (param i64)))
  (import "env" "attached_deposit" (func $attached_deposit (param i64)))
  (import "env" "prepaid_gas" (func $prepaid_gas (result i64)))
  (import "env" "promise_results_count" (func $promise_results_count (result i64)))
  (import "env" "promise_result" (func $promise_result (param i64 i64) (result i64)))
  (import "env" "promise_create" (func $promise_create (param i64 i64 i64 i64 i64 i64 i64 i64) (result i64)))
  (import "env" "promise_then" (func $promise_then (param i64 i64 i64 i64 i64 i64 i64 i64 i64) (result i64)))
  (import "env" "promise_and" (func $promise_and (param i64 i64) (result i64)))
  (import "env" "promise_return" (func $promise_return (param i64)))
  (import "env" "panic_utf8" (func $panic_utf8 (param i64 i64)))
  ;; Room for a result of max_length_returned_data (4 MiB) at 64 KiB.
  (memory 72)
  (export "memory" (memory 0))
  (data (i32.const 0) "answer")
  (data (i32.const 16) "cb")
  (data (i32.const 32) "boom")
  ;; 64: the two promises promise_and joins; 128: own account id;
  ;; 240: u128 zero; 256: the report; 65536: scratch
  (global $at (mut i64) (i64.const 256))
  ;; The input's length and the own id's, set by $prepare.
  (global $len (mut i64) (i64.const 0))
  (global $self (mut i64) (i64.const 0))

  (func $u64 (param $v i64)
    (i64.store (i32.wrap_i64 (global.get $at)) (local.get $v))
    (global.set $at (i64.add (global.get $at) (i64.const 8))))

  ;; Register 0's length, then its bytes.
  (func $bytes
    (call $u64 (call $register_len (i64.const 0)))
    (call $read_register (i64.const 0) (global.get $at))
    (global.set $at (i64.add (global.get $at) (call $register_len (i64.const 0)))))

  ;; Register 0's length, then its sha256.
  (func $digest
    (local $len i64)
    (local.set $len (call $register_len (i64.const 0)))
    (call $read_register (i64.const 0) (i64.const 65536))
    (call $sha256 (local.get $len) (i64.const 65536) (i64.const 1))
    (call $u64 (local.get $len))
    (call $read_register (i64.const 1) (global.get $at))
    (global.set $at (i64.add (global.get $at) (i64.const 32))))

  ;; Reads the input to 65536 and the own id to 128.
  (func $prepare
    (call $input (i64.const 0))
    (call $read_register (i64.const 0) (i64.const 65536))
    (global.set $len (call $register_len (i64.const 0)))
    (call $current_account_id (i64.const 1))
    (call $read_register (i64.const 1) (i64.const 128))
    (global.set $self (call $register_len (i64.const 1))))

  ;; self.answer(input), with 5 Tgas.
  (func $answer (result i64)
    (call $promise_create (global.get $self) (i64.const 128) (i64.const 6) (i64.const 0)
      (global.get $len) (i64.const 65536) (i64.const 240) (i64.const 5000000000000)))

  ;; `p`.then(self.cb(input)), with 20 Tgas, returned.
  (func $then_cb (param $p i64)
    (call $promise_return
      (call $promise_then (local.get $p) (global.get $self) (i64.const 128)
        (i64.const 2) (i64.const 16) (global.get $len) (i64.const 65536)
        (i64.const 240) (i64.const 20000000000000))))

  (func (export "start")
    (call $prepare)
    (call $then_cb (call $answer)))

  (func (export "start_and")
    (call $prepare)
    (i64.store (i32.const 64) (call $answer))
    (i64.store (i32.const 72)
      (call $promise_create (global.get $self) (i64.const 128) (i64.const 4) (i64.const 32)
        (i64.const 0) (i64.const 0) (i64.const 240) (i64.const 5000000000000)))
    (call $then_cb (call $promise_and (i64.const 64) (i64.const 2))))

  (func (export "answer")
    (call $input (i64.const 0))
    (call $read_register (i64.const 0) (i64.const 65536))
    (call $value_return (call $register_len (i64.const 0)) (i64.const 65536)))

  (func (export "boom")
    (call $panic_utf8 (i64.const 4) (i64.const 32)))

  (func (export "relay")
    (call $prepare)
    (call $promise_return (call $answer)))

  (func (export "cb")
    (local $i i64) (local $n i64) (local $status i64)
    (call $signer_account_id (i64.const 0))
    (call $bytes)
    (call $signer_account_pk (i64.const 0))
    (call $bytes)
    (call $predecessor_account_id (i64.const 0))
    (call $bytes)
    (call $current_account_id (i64.const 0))
    (call $bytes)
    (call $input (i64.const 0))
    (call $digest)
    (call $attached_deposit (global.get $at))
    (global.set $at (i64.add (global.get $at) (i64.const 16)))
    (call $u64 (call $prepaid_gas))
    (local.set $n (call $promise_results_count))
    (call $u64 (local.get $n))
    (block $done
      (loop $each
        (br_if $done (i64.ge_u (local.get $i) (local.get $n)))
        (local.set $status (call $promise_result (local.get $i) (i64.const 0)))
        (call $u64 (local.get $status))
        (if (i64.eq (local.get $status) (i64.const 1)) (then (call $digest)))
        (local.set $i (i64.add (local.get $i) (i64.const 1)))
        (br $each)))
    (call $value_return (i64.sub (global.get $at) (i64.const 256)) (i64.const 256))))
