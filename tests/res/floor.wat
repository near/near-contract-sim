;; A keeper that pays out through a token's transfer_call, forwarding a gas
;; floor (tests/floor.rs): the specimen for a scan's caller-gas variants
;; and for Harness::sweep. One module, two roles: deployed on keeper.near it
;; keeps credit, on token.near it is the token. Amounts are u64
;; little-endian, account ids raw; a credit sits under "c" + its id, a
;; token balance under "b" + its id, the keeper's total credit under "t".
;;
;; The token:
;;   mint           -> input: amount. Credits the predecessor's balance.
;;   transfer_call  -> input: amount, then the receiver id. Requires more
;;                     than 30 Tgas prepaid (the gas it reserves for the
;;                     receiver's notice and its own resolver, as
;;                     near-contract-standards' ft_transfer_call does), then
;;                     moves the amount from the predecessor to the receiver
;;   register       -> Requires more than 30 Tgas prepaid, as transfer_call
;;                     does; changes nothing (a storage registration).
;;   balance_of     -> (view) input: an account id. Its balance.
;; The keeper:
;;   fund           -> input: amount. Credits the predecessor.
;;   withdraw       -> input: amount, then optionally the caller's min_gas
;;                     (u64), then optionally a register flag (a nonzero
;;                     byte). Debits the predecessor's credit, calls
;;                     token.near's transfer_call to pay it, with the prepaid
;;                     gas less 20 Tgas, but at least min_gas and at least
;;                     the floor, then its own on_withdraw (10 Tgas), and
;;                     returns that promise. With the flag it registers
;;                     first: token.near's register gets that gas (the
;;                     prepaid less 80 Tgas, at least min_gas and the floor),
;;                     then its own pay (60 Tgas)
;;   pay            -> after register: if it failed, re-credits the amount;
;;                     else transfer_call with a fixed 40 Tgas, then
;;                     on_withdraw. At the floor register is refused, so
;;                     this path never sends transfer_call the floor
;;                     (tests/floor.rs: the GasField coverage gap)
;;   on_withdraw    -> a failed transfer_call counts as delivered: the
;;                     keeper can't tell a call refused up front from one
;;                     whose resolver failed after the tokens moved, so it
;;                     never re-credits (a verifier's documented choice)
;;   credit_of      -> (view) input: an account id. Its credit.
;;   total          -> (view) the total credit.
;;
;; The floor is 30 Tgas, the token's own reserve, and the token wants more
;; than that. A caller who signs a min_gas at the floor and attaches just
;; enough gas (the least that succeeds, about 43 Tgas, up to 50) makes the
;; keeper forward exactly 30 Tgas: the token refuses, and the amount is
;; gone. A min_gas above the floor (tests/floor.rs signs 40 Tgas by
;; default) forwards enough whatever the caller attaches, so the caller's
;; gas alone finds nothing. tests/floor.rs builds the fixed keeper by
;; raising the floor (the `$floor` global) to 35 Tgas.
(module
  (import "env" "input" (func $input (param i64)))
  (import "env" "register_len" (func $register_len (param i64) (result i64)))
  (import "env" "read_register" (func $read_register (param i64 i64)))
  (import "env" "predecessor_account_id" (func $predecessor_account_id (param i64)))
  (import "env" "current_account_id" (func $current_account_id (param i64)))
  (import "env" "prepaid_gas" (func $prepaid_gas (result i64)))
  (import "env" "storage_write" (func $storage_write (param i64 i64 i64 i64 i64) (result i64)))
  (import "env" "storage_read" (func $storage_read (param i64 i64 i64) (result i64)))
  (import "env" "promise_create"
    (func $promise_create (param i64 i64 i64 i64 i64 i64 i64 i64) (result i64)))
  (import "env" "promise_then"
    (func $promise_then (param i64 i64 i64 i64 i64 i64 i64 i64 i64) (result i64)))
  (import "env" "promise_return" (func $promise_return (param i64)))
  (import "env" "promise_result" (func $promise_result (param i64 i64) (result i64)))
  (import "env" "value_return" (func $value_return (param i64 i64)))
  (import "env" "log_utf8" (func $log_utf8 (param i64 i64)))
  (import "env" "panic_utf8" (func $panic_utf8 (param i64 i64)))
  (memory 1)
  (export "memory" (memory 0))
  (data (i32.const 0) "transfer_call")
  (data (i32.const 16) "on_withdraw")
  (data (i32.const 32) "more gas is required")
  (data (i32.const 96) "not enough credit")
  (data (i32.const 128) "t")
  (data (i32.const 144) "token.near")
  (data (i32.const 160) "transfer failed: counted as delivered")
  (data (i32.const 208) "register")
  (data (i32.const 224) "pay")
  (data (i32.const 232) "register failed: re-credited")
  ;; 64: u128 zero (no deposit). 512: a storage key. 600: a value.
  ;; 700: a call's args (amount, then an id). 1024: input. 2048: an id.

  ;; The least gas the keeper forwards to the token.
  (global $floor i64 (i64.const 30000000000000))

  ;; The u64 under `prefix` + the id at `ptr`, `len` bytes long; 0 if none.
  (func $get (param $prefix i32) (param $ptr i64) (param $len i64) (result i64)
    (call $key (local.get $prefix) (local.get $ptr) (local.get $len))
    (if (result i64)
        (i64.eq (call $storage_read (i64.add (local.get $len) (i64.const 1)) (i64.const 512)
                                    (i64.const 0))
                (i64.const 1))
      (then
        (call $read_register (i64.const 0) (i64.const 600))
        (i64.load (i32.const 600)))
      (else (i64.const 0))))

  (func $set (param $prefix i32) (param $ptr i64) (param $len i64) (param $v i64)
    (call $key (local.get $prefix) (local.get $ptr) (local.get $len))
    (i64.store (i32.const 600) (local.get $v))
    (drop (call $storage_write (i64.add (local.get $len) (i64.const 1)) (i64.const 512)
                               (i64.const 8) (i64.const 600) (i64.const 9))))

  ;; `prefix` + the id, at 512.
  (func $key (param $prefix i32) (param $ptr i64) (param $len i64)
    (i32.store8 (i32.const 512) (local.get $prefix))
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

  ;; The input at 1024; returns its length.
  (func $read_input (result i64)
    (call $input (i64.const 0))
    (call $read_register (i64.const 0) (i64.const 1024))
    (call $register_len (i64.const 0)))

  ;; The predecessor at 2048; returns its length.
  (func $predecessor (result i64)
    (call $predecessor_account_id (i64.const 1))
    (call $read_register (i64.const 1) (i64.const 2048))
    (call $register_len (i64.const 1)))

  ;; Adds `v` to the u64 under `prefix` + the predecessor.
  (func $credit_predecessor (param $prefix i32) (param $v i64)
    (local $n i64)
    (local.set $n (call $predecessor))
    (call $set (local.get $prefix) (i64.const 2048) (local.get $n)
      (i64.add (call $get (local.get $prefix) (i64.const 2048) (local.get $n)) (local.get $v))))

  ;; Takes `v` from the u64 under `prefix` + the predecessor, or panics.
  (func $debit_predecessor (param $prefix i32) (param $v i64)
    (local $n i64) (local $have i64)
    (local.set $n (call $predecessor))
    (local.set $have (call $get (local.get $prefix) (i64.const 2048) (local.get $n)))
    (if (i64.lt_u (local.get $have) (local.get $v))
      (then (call $panic_utf8 (i64.const 17) (i64.const 96))))
    (call $set (local.get $prefix) (i64.const 2048) (local.get $n)
      (i64.sub (local.get $have) (local.get $v))))

  ;; The u64 under `prefix` + the input id, returned.
  (func $view (param $prefix i32)
    (local $n i64)
    (local.set $n (call $read_input))
    (i64.store (i32.const 600) (call $get (local.get $prefix) (i64.const 1024) (local.get $n)))
    (call $value_return (i64.const 8) (i64.const 600)))

  ;; ---- the token ----

  (func (export "mint")
    (drop (call $read_input))
    (call $credit_predecessor (i32.const 98) (i64.load (i32.const 1024))))

  (func (export "transfer_call")
    (local $amount i64) (local $to i64)
    (if (i64.le_u (call $prepaid_gas) (i64.const 30000000000000))
      (then (call $panic_utf8 (i64.const 20) (i64.const 32))))
    (local.set $to (i64.sub (call $read_input) (i64.const 8)))
    (local.set $amount (i64.load (i32.const 1024)))
    (call $debit_predecessor (i32.const 98) (local.get $amount))
    (call $set (i32.const 98) (i64.const 1032) (local.get $to)
      (i64.add (call $get (i32.const 98) (i64.const 1032) (local.get $to)) (local.get $amount))))

  (func (export "register")
    (if (i64.le_u (call $prepaid_gas) (i64.const 30000000000000))
      (then (call $panic_utf8 (i64.const 20) (i64.const 32)))))

  (func (export "balance_of") (call $view (i32.const 98)))

  ;; ---- the keeper ----

  ;; Adds `v` to the total credit.
  (func $add_total (param $v i64)
    (call $set (i32.const 116) (i64.const 0) (i64.const 0)
      (i64.add (call $get (i32.const 116) (i64.const 0) (i64.const 0)) (local.get $v))))

  (func (export "fund")
    (local $amount i64)
    (drop (call $read_input))
    (local.set $amount (i64.load (i32.const 1024)))
    (call $credit_predecessor (i32.const 99) (local.get $amount))
    (call $add_total (local.get $amount)))

  (func (export "withdraw")
    (local $amount i64) (local $who i64) (local $gas i64) (local $p i64) (local $min i64)
    (local $len i64) (local $register i32) (local $reserve i64)
    (local.set $len (call $read_input))
    (local.set $amount (i64.load (i32.const 1024)))
    (if (i64.ge_u (local.get $len) (i64.const 16))
      (then (local.set $min (i64.load (i32.const 1032)))))
    (if (i64.ge_u (local.get $len) (i64.const 17))
      (then (local.set $register (i32.load8_u (i32.const 1040)))))
    (local.set $reserve (i64.const 20000000000000))
    (if (local.get $register) (then (local.set $reserve (i64.const 80000000000000))))
    (call $debit_predecessor (i32.const 99) (local.get $amount))
    (call $add_total (i64.sub (i64.const 0) (local.get $amount)))
    ;; The args: the amount, then the predecessor (at 2048 since the debit).
    (local.set $who (call $register_len (i64.const 1)))
    (i64.store (i32.const 700) (local.get $amount))
    (call $copy (i32.const 708) (i32.const 2048) (i32.wrap_i64 (local.get $who)))
    ;; The prepaid gas less the reserve, but at least min_gas and the floor.
    (if (i64.ge_u (call $prepaid_gas) (local.get $reserve))
      (then (local.set $gas (i64.sub (call $prepaid_gas) (local.get $reserve)))))
    (if (i64.lt_u (local.get $gas) (local.get $min))
      (then (local.set $gas (local.get $min))))
    (if (i64.lt_u (local.get $gas) (global.get $floor))
      (then (local.set $gas (global.get $floor))))
    (call $current_account_id (i64.const 2))
    (call $read_register (i64.const 2) (i64.const 300))
    (if (local.get $register)
      (then
        (local.set $p (call $promise_create (i64.const 10) (i64.const 144)
          (i64.const 8) (i64.const 208) (i64.const 0) (i64.const 700)
          (i64.const 64) (local.get $gas)))
        (call $promise_return (call $promise_then (local.get $p)
          (call $register_len (i64.const 2)) (i64.const 300)
          (i64.const 3) (i64.const 224)
          (i64.add (i64.const 8) (local.get $who)) (i64.const 700)
          (i64.const 64) (i64.const 60000000000000)))
        (return)))
    (local.set $p (call $promise_create (i64.const 10) (i64.const 144)
      (i64.const 13) (i64.const 0)
      (i64.add (i64.const 8) (local.get $who)) (i64.const 700)
      (i64.const 64) (local.get $gas)))
    (call $promise_return (call $promise_then (local.get $p)
      (call $register_len (i64.const 2)) (i64.const 300)
      (i64.const 11) (i64.const 16)
      (i64.add (i64.const 8) (local.get $who)) (i64.const 700)
      (i64.const 64) (i64.const 10000000000000))))

  ;; After register: its input is the amount, then the payee.
  (func (export "pay")
    (local $len i64) (local $amount i64) (local $who i64) (local $p i64)
    (local.set $len (call $read_input))
    (local.set $amount (i64.load (i32.const 1024)))
    (local.set $who (i64.sub (local.get $len) (i64.const 8)))
    (if (i64.ne (call $promise_result (i64.const 0) (i64.const 3)) (i64.const 1))
      (then
        (call $set (i32.const 99) (i64.const 1032) (local.get $who)
          (i64.add (call $get (i32.const 99) (i64.const 1032) (local.get $who))
                   (local.get $amount)))
        (call $add_total (local.get $amount))
        (call $log_utf8 (i64.const 28) (i64.const 232))
        (return)))
    (call $copy (i32.const 700) (i32.const 1024) (i32.wrap_i64 (local.get $len)))
    (local.set $p (call $promise_create (i64.const 10) (i64.const 144)
      (i64.const 13) (i64.const 0)
      (local.get $len) (i64.const 700)
      (i64.const 64) (i64.const 40000000000000)))
    (call $current_account_id (i64.const 2))
    (call $read_register (i64.const 2) (i64.const 300))
    (call $promise_return (call $promise_then (local.get $p)
      (call $register_len (i64.const 2)) (i64.const 300)
      (i64.const 11) (i64.const 16)
      (local.get $len) (i64.const 700)
      (i64.const 64) (i64.const 10000000000000))))

  (func (export "on_withdraw")
    (if (i64.ne (call $promise_result (i64.const 0) (i64.const 3)) (i64.const 1))
      (then (call $log_utf8 (i64.const 37) (i64.const 160)))))

  (func (export "credit_of") (call $view (i32.const 99)))

  (func (export "total")
    (i64.store (i32.const 600) (call $get (i32.const 116) (i64.const 0) (i64.const 0)))
    (call $value_return (i64.const 8) (i64.const 600))))
