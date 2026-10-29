;; Two contracts in one module, for the failed-callback check
;; (tests/invariants.rs): a token-like sender and a receiver, each keeping
;; its own side of a transfer.
;;   send      -> (token) input: the raw receiver id. Writes "sent", calls
;;                receiver.on_transfer, then self.resolve with its result,
;;                and returns that promise
;;   resolve   -> (token) if on_transfer failed, removes "sent" (the refund)
;;   on_transfer -> (receiver) writes "credited", then calls self.noop and
;;                returns noop.then(self.settle): settle's result is
;;                on_transfer's
;;   noop      -> does nothing
;;   settle    -> panics: the receiver's callback fails, after its own
;;                on_transfer committed "credited"
(module
  (import "env" "input" (func $input (param i64)))
  (import "env" "register_len" (func $register_len (param i64) (result i64)))
  (import "env" "read_register" (func $read_register (param i64 i64)))
  (import "env" "current_account_id" (func $current_account_id (param i64)))
  (import "env" "storage_write" (func $storage_write (param i64 i64 i64 i64 i64) (result i64)))
  (import "env" "storage_remove" (func $storage_remove (param i64 i64 i64) (result i64)))
  (import "env" "promise_create" (func $promise_create (param i64 i64 i64 i64 i64 i64 i64 i64) (result i64)))
  (import "env" "promise_then" (func $promise_then (param i64 i64 i64 i64 i64 i64 i64 i64 i64) (result i64)))
  (import "env" "promise_return" (func $promise_return (param i64)))
  (import "env" "promise_result" (func $promise_result (param i64 i64) (result i64)))
  (import "env" "panic_utf8" (func $panic_utf8 (param i64 i64)))
  (memory 1)
  (export "memory" (memory 0))
  ;; 0: u128 zero
  (data (i32.const 16) "sent")
  (data (i32.const 32) "credited")
  (data (i32.const 48) "on_transfer")
  (data (i32.const 64) "resolve")
  (data (i32.const 80) "noop")
  (data (i32.const 96) "settle")
  (data (i32.const 112) "1")
  (data (i32.const 128) "settle failed")
  ;; 256: receiver id, 512: current account id

  ;; Calls method `m` on the account at 256 (`len` bytes), then callback `c`
  ;; on self with its result, each with `gas`, and returns that promise.
  (func $call_then (param $len i64) (param $m i64) (param $mp i64) (param $c i64) (param $cp i64)
    (param $gas i64)
    (local $self i64)
    (call $current_account_id (i64.const 1))
    (local.set $self (call $register_len (i64.const 1)))
    (call $read_register (i64.const 1) (i64.const 512))
    (call $promise_return
      (call $promise_then
        (call $promise_create (local.get $len) (i64.const 256)
          (local.get $m) (local.get $mp) (i64.const 0) (i64.const 0)
          (i64.const 0) (local.get $gas))
        (local.get $self) (i64.const 512)
        (local.get $c) (local.get $cp) (i64.const 0) (i64.const 0)
        (i64.const 0) (local.get $gas))))

  (func (export "send")
    (call $input (i64.const 0))
    (call $read_register (i64.const 0) (i64.const 256))
    (drop (call $storage_write (i64.const 4) (i64.const 16) (i64.const 1) (i64.const 112) (i64.const 2)))
    (call $call_then (call $register_len (i64.const 0))
      (i64.const 11) (i64.const 48) (i64.const 7) (i64.const 64)
      (i64.const 30000000000000)))                       ;; 30 Tgas

  (func (export "resolve")
    (if (i64.ne (call $promise_result (i64.const 0) (i64.const 0)) (i64.const 1))
      (then (drop (call $storage_remove (i64.const 4) (i64.const 16) (i64.const 2))))))

  (func (export "on_transfer")
    (drop (call $storage_write (i64.const 8) (i64.const 32) (i64.const 1) (i64.const 112) (i64.const 2)))
    (call $current_account_id (i64.const 0))
    (call $read_register (i64.const 0) (i64.const 256))
    (call $call_then (call $register_len (i64.const 0))
      (i64.const 4) (i64.const 80) (i64.const 6) (i64.const 96)
      (i64.const 5000000000000)))                        ;; 5 Tgas

  (func (export "noop"))

  (func (export "settle")
    (call $panic_utf8 (i64.const 13) (i64.const 128))))
