;; Two contracts in one module, for fault injection (tests/faults.rs): a
;; token that takes back what its receiver doesn't keep, and a vault that
;; credits a deposit, then confirms it in a callback. Amounts are u64
;; little-endian and account ids raw; each contract keeps one u64 per
;; account, under the account's id.
;;   mint             -> (token) input: an amount. Credits the predecessor
;;   transfer_call    -> (token) input: an amount, then the receiver's id.
;;                       Moves the amount from the predecessor to the
;;                       receiver, calls receiver.on_transfer(amount, sender)
;;                       with 50 Tgas, then self.resolve_transfer with 20
;;                       Tgas, and returns that promise
;;   resolve_transfer -> (token) moves back what on_transfer's result says
;;                       the receiver didn't keep (a u64, at most the
;;                       amount), the whole amount if it failed; returns it
;;   on_transfer      -> (vault) credits the sender's shares, then calls
;;                       self.verify with 5 Tgas and self.resolve_deposit
;;                       with the gas stored under "*gas" (10 Tgas without
;;                       it), and returns that promise: its result is the
;;                       amount not kept. With "*safe" set, credits nothing
;;   verify           -> (vault) panics if "*reject" is set
;;   resolve_deposit  -> (vault) if verify failed, takes the credit back and
;;                       returns the whole amount, else returns 0. With
;;                       "*safe" set, credits on success instead; with
;;                       "*strict" set, panics if verify failed
;; The bug: if resolve_deposit fails, the credit stays, and the token, which
;; sees on_transfer fail, takes the whole amount back. With "*safe", the
;; callback is the only credit, so its failure leaves the books whole.
(module
  (import "env" "input" (func $input (param i64)))
  (import "env" "register_len" (func $register_len (param i64) (result i64)))
  (import "env" "read_register" (func $read_register (param i64 i64)))
  (import "env" "current_account_id" (func $current_account_id (param i64)))
  (import "env" "predecessor_account_id" (func $predecessor_account_id (param i64)))
  (import "env" "storage_write" (func $storage_write (param i64 i64 i64 i64 i64) (result i64)))
  (import "env" "storage_read" (func $storage_read (param i64 i64 i64) (result i64)))
  (import "env" "storage_has_key" (func $storage_has_key (param i64 i64) (result i64)))
  (import "env" "promise_create" (func $promise_create (param i64 i64 i64 i64 i64 i64 i64 i64) (result i64)))
  (import "env" "promise_then" (func $promise_then (param i64 i64 i64 i64 i64 i64 i64 i64 i64) (result i64)))
  (import "env" "promise_return" (func $promise_return (param i64)))
  (import "env" "promise_result" (func $promise_result (param i64 i64) (result i64)))
  (import "env" "value_return" (func $value_return (param i64 i64)))
  (import "env" "panic_utf8" (func $panic_utf8 (param i64 i64)))
  (memory 1)
  (export "memory" (memory 0))
  ;; 0: u128 zero
  (data (i32.const 16) "on_transfer")
  (data (i32.const 32) "resolve_transfer")
  (data (i32.const 64) "verify")
  (data (i32.const 80) "resolve_deposit")
  (data (i32.const 96) "*safe")
  (data (i32.const 112) "*reject")
  (data (i32.const 128) "*gas")
  (data (i32.const 144) "rejected")
  (data (i32.const 160) "insufficient balance")
  (data (i32.const 192) "*strict")
  (data (i32.const 208) "verify failed")
  ;; 256: the input, 1024 and 1536: a call's args, 2048: the own id,
  ;; 3072: a stored u64, 3584: a promise result, 3600: the value returned

  ;; The u64 under the key at `ptr`, 0 if there is none.
  (func $get (param $len i64) (param $ptr i64) (result i64)
    (if (result i64)
      (i64.eq (call $storage_read (local.get $len) (local.get $ptr) (i64.const 2)) (i64.const 1))
      (then
        (call $read_register (i64.const 2) (i64.const 3072))
        (i64.load (i32.const 3072)))
      (else (i64.const 0))))

  (func $put (param $len i64) (param $ptr i64) (param $v i64)
    (i64.store (i32.const 3072) (local.get $v))
    (drop (call $storage_write (local.get $len) (local.get $ptr)
      (i64.const 8) (i64.const 3072) (i64.const 2))))

  (func $add (param $len i64) (param $ptr i64) (param $v i64)
    (call $put (local.get $len) (local.get $ptr)
      (i64.add (call $get (local.get $len) (local.get $ptr)) (local.get $v))))

  ;; Panics if the balance is short.
  (func $sub (param $len i64) (param $ptr i64) (param $v i64)
    (local $had i64)
    (local.set $had (call $get (local.get $len) (local.get $ptr)))
    (if (i64.lt_u (local.get $had) (local.get $v))
      (then (call $panic_utf8 (i64.const 20) (i64.const 160))))
    (call $put (local.get $len) (local.get $ptr) (i64.sub (local.get $had) (local.get $v))))

  ;; Reads the input to 256 (register 0 keeps it); returns its length.
  (func $read_input (result i64)
    (call $input (i64.const 0))
    (call $read_register (i64.const 0) (i64.const 256))
    (call $register_len (i64.const 0)))

  ;; Reads the own id to 2048; returns its length.
  (func $self (result i64)
    (call $current_account_id (i64.const 1))
    (call $read_register (i64.const 1) (i64.const 2048))
    (call $register_len (i64.const 1)))

  (func $return_u64 (param $v i64)
    (i64.store (i32.const 3600) (local.get $v))
    (call $value_return (i64.const 8) (i64.const 3600)))

  (func (export "mint")
    (drop (call $read_input))
    (call $predecessor_account_id (i64.const 1))
    (call $read_register (i64.const 1) (i64.const 1024))
    (call $add (call $register_len (i64.const 1)) (i64.const 1024) (i64.load (i32.const 256))))

  (func (export "transfer_call")
    (local $rlen i64) (local $slen i64) (local $amount i64) (local $p i64) (local $me i64)
    (local.set $rlen (i64.sub (call $read_input) (i64.const 8)))
    (local.set $amount (i64.load (i32.const 256)))
    ;; on_transfer's args at 1024: the amount, then the sender.
    (i64.store (i32.const 1024) (local.get $amount))
    (call $predecessor_account_id (i64.const 1))
    (call $read_register (i64.const 1) (i64.const 1032))
    (local.set $slen (call $register_len (i64.const 1)))
    (call $sub (local.get $slen) (i64.const 1032) (local.get $amount))
    (call $add (local.get $rlen) (i64.const 264) (local.get $amount))
    (local.set $p
      (call $promise_create (local.get $rlen) (i64.const 264) (i64.const 11) (i64.const 16)
        (i64.add (i64.const 8) (local.get $slen)) (i64.const 1024)
        (i64.const 0) (i64.const 50000000000000)))
    ;; resolve_transfer's args at 1536: the amount, the receiver's id's
    ;; length, the receiver, the sender.
    (call $read_register (i64.const 0) (i64.const 1544))
    (i64.store (i32.const 1536) (local.get $amount))
    (i64.store (i32.const 1544) (local.get $rlen))
    (call $read_register (i64.const 1) (i64.add (i64.const 1552) (local.get $rlen)))
    (local.set $me (call $self))
    (call $promise_return
      (call $promise_then (local.get $p) (local.get $me) (i64.const 2048)
        (i64.const 16) (i64.const 32)
        (i64.add (i64.const 16) (i64.add (local.get $rlen) (local.get $slen))) (i64.const 1536)
        (i64.const 0) (i64.const 20000000000000))))

  (func (export "resolve_transfer")
    (local $len i64) (local $amount i64) (local $rlen i64) (local $back i64)
    (local.set $len (call $read_input))
    (local.set $amount (i64.load (i32.const 256)))
    (local.set $rlen (i64.load (i32.const 264)))
    (local.set $back (local.get $amount))
    (if (i64.eq (call $promise_result (i64.const 0) (i64.const 3)) (i64.const 1))
      (then
        (call $read_register (i64.const 3) (i64.const 3584))
        (local.set $back (i64.load (i32.const 3584)))))
    (if (i64.gt_u (local.get $back) (local.get $amount))
      (then (local.set $back (local.get $amount))))
    (if (i64.ne (local.get $back) (i64.const 0))
      (then
        (call $sub (local.get $rlen) (i64.const 272) (local.get $back))
        (call $add (i64.sub (local.get $len) (i64.add (i64.const 16) (local.get $rlen)))
          (i64.add (i64.const 272) (local.get $rlen)) (local.get $back))))
    (call $return_u64 (local.get $back)))

  (func (export "on_transfer")
    (local $len i64) (local $me i64) (local $gas i64)
    (local.set $len (call $read_input))
    (if (i64.eqz (call $storage_has_key (i64.const 5) (i64.const 96)))
      (then
        (call $add (i64.sub (local.get $len) (i64.const 8)) (i64.const 264)
          (i64.load (i32.const 256)))))
    (local.set $gas (i64.const 10000000000000))
    (if (i64.ne (call $storage_has_key (i64.const 4) (i64.const 128)) (i64.const 0))
      (then (local.set $gas (call $get (i64.const 4) (i64.const 128)))))
    (local.set $me (call $self))
    (call $promise_return
      (call $promise_then
        (call $promise_create (local.get $me) (i64.const 2048) (i64.const 6) (i64.const 64)
          (i64.const 0) (i64.const 0) (i64.const 0) (i64.const 5000000000000))
        (local.get $me) (i64.const 2048) (i64.const 15) (i64.const 80)
        (local.get $len) (i64.const 256) (i64.const 0) (local.get $gas))))

  (func (export "verify")
    (if (i64.ne (call $storage_has_key (i64.const 7) (i64.const 112)) (i64.const 0))
      (then (call $panic_utf8 (i64.const 8) (i64.const 144)))))

  (func (export "resolve_deposit")
    (local $slen i64) (local $amount i64) (local $ok i32) (local $safe i32)
    (local.set $slen (i64.sub (call $read_input) (i64.const 8)))
    (local.set $amount (i64.load (i32.const 256)))
    (local.set $ok (i64.eq (call $promise_result (i64.const 0) (i64.const 3)) (i64.const 1)))
    (local.set $safe (i64.ne (call $storage_has_key (i64.const 5) (i64.const 96)) (i64.const 0)))
    (if (local.get $ok)
      (then
        (if (local.get $safe)
          (then (call $add (local.get $slen) (i64.const 264) (local.get $amount))))
        (call $return_u64 (i64.const 0)))
      (else
        (if (i64.ne (call $storage_has_key (i64.const 7) (i64.const 192)) (i64.const 0))
          (then (call $panic_utf8 (i64.const 13) (i64.const 208))))
        (if (i32.eqz (local.get $safe))
          (then (call $sub (local.get $slen) (i64.const 264) (local.get $amount))))
        (call $return_u64 (local.get $amount))))))
