;; A pool that books rewards as whatever its balance holds beyond its books
;; (tests/identity.rs): a liquid staking pool's in-flight double count, with
;; nothing failing. Amounts are u128 little-endian, account ids raw.
;;   init        -> books the pool's balance as it stands
;;   deposit     -> syncs, then calls its own on_deposit(amount, who) to
;;                  book the attached deposit
;;   on_deposit  -> books the amount and credits who
;;   ping        -> calls its own helper, detached, and returns
;;   helper      -> panics: a value-neutral receipt that always fails (no
;;                  deposit, no tokens, no callback)
;;   booked      -> (view) the books, a u128
;; The sync books as a reward the balance less the attached deposit less
;; the books. A deposit that has arrived but whose on_deposit hasn't run
;; yet is in the balance, not in the books: a second deposit in the same
;; block syncs it as a reward, then on_deposit books it again. Each
;; deposit alone, or two settled apart, keep the books at the balance.
(module
  (import "env" "input" (func $input (param i64)))
  (import "env" "register_len" (func $register_len (param i64) (result i64)))
  (import "env" "read_register" (func $read_register (param i64 i64)))
  (import "env" "current_account_id" (func $current_account_id (param i64)))
  (import "env" "predecessor_account_id" (func $predecessor_account_id (param i64)))
  (import "env" "attached_deposit" (func $attached_deposit (param i64)))
  (import "env" "account_balance" (func $account_balance (param i64)))
  (import "env" "storage_write" (func $storage_write (param i64 i64 i64 i64 i64) (result i64)))
  (import "env" "storage_read" (func $storage_read (param i64 i64 i64) (result i64)))
  (import "env" "promise_create"
    (func $promise_create (param i64 i64 i64 i64 i64 i64 i64 i64) (result i64)))
  (import "env" "value_return" (func $value_return (param i64 i64)))
  (import "env" "panic_utf8" (func $panic_utf8 (param i64 i64)))
  (memory 1)
  (export "memory" (memory 0))
  (data (i32.const 0) "*booked")
  (data (i32.const 16) "on_deposit")
  (data (i32.const 32) "helper")
  (data (i32.const 48) "helper failed")
  ;; 64: u128 zero. 128: the books. 144: the balance. 160: the deposit.
  ;; 256: on_deposit's args (amount, then who). 512: the own id.
  ;; 768: a credit key ("c" + id). 1024: input.

  (func $load_books
    (i64.store (i32.const 128) (i64.const 0))
    (i64.store (i32.const 136) (i64.const 0))
    (if (i64.eq (call $storage_read (i64.const 7) (i64.const 0) (i64.const 0)) (i64.const 1))
      (then (call $read_register (i64.const 0) (i64.const 128)))))

  (func $save_books
    (drop (call $storage_write (i64.const 7) (i64.const 0) (i64.const 16) (i64.const 128)
                               (i64.const 0))))

  ;; `len` bytes from `src` to `dst`.
  (func $copy (param $dst i32) (param $src i32) (param $len i32)
    (local $i i32)
    (block $done (loop $byte
      (br_if $done (i32.ge_u (local.get $i) (local.get $len)))
      (i32.store8 (i32.add (local.get $dst) (local.get $i))
                  (i32.load8_u (i32.add (local.get $src) (local.get $i))))
      (local.set $i (i32.add (local.get $i) (i32.const 1)))
      (br $byte))))

  ;; *a += *b
  (func $add (param $a i32) (param $b i32)
    (local $lo i64)
    (local.set $lo (i64.add (i64.load (local.get $a)) (i64.load (local.get $b))))
    (i64.store offset=8 (local.get $a)
      (i64.add (i64.add (i64.load offset=8 (local.get $a)) (i64.load offset=8 (local.get $b)))
        (i64.extend_i32_u (i64.lt_u (local.get $lo) (i64.load (local.get $b))))))
    (i64.store (local.get $a) (local.get $lo)))

  ;; *a -= *b, if *a >= *b; else *a = 0.
  (func $sub (param $a i32) (param $b i32)
    (local $alo i64) (local $ahi i64) (local $blo i64) (local $bhi i64)
    (local.set $alo (i64.load (local.get $a)))
    (local.set $ahi (i64.load offset=8 (local.get $a)))
    (local.set $blo (i64.load (local.get $b)))
    (local.set $bhi (i64.load offset=8 (local.get $b)))
    (if (i32.or (i64.lt_u (local.get $ahi) (local.get $bhi))
                (i32.and (i64.eq (local.get $ahi) (local.get $bhi))
                         (i64.lt_u (local.get $alo) (local.get $blo))))
      (then
        (i64.store (local.get $a) (i64.const 0))
        (i64.store offset=8 (local.get $a) (i64.const 0))
        (return)))
    (i64.store offset=8 (local.get $a)
      (i64.sub (i64.sub (local.get $ahi) (local.get $bhi))
        (i64.extend_i32_u (i64.lt_u (local.get $alo) (local.get $blo)))))
    (i64.store (local.get $a) (i64.sub (local.get $alo) (local.get $blo))))

  (func (export "init")
    (call $account_balance (i64.const 128))
    (call $save_books))

  ;; The balance less the attached deposit less the books, booked.
  (func $sync
    (call $load_books)
    (call $account_balance (i64.const 144))
    (call $attached_deposit (i64.const 160))
    (call $sub (i32.const 144) (i32.const 160))
    (call $sub (i32.const 144) (i32.const 128))
    (call $add (i32.const 128) (i32.const 144))
    (call $save_books))

  (func (export "deposit")
    (local $n i64)
    (call $sync)
    (call $attached_deposit (i64.const 256))
    (call $predecessor_account_id (i64.const 1))
    (local.set $n (call $register_len (i64.const 1)))
    (call $read_register (i64.const 1) (i64.const 272))
    (call $current_account_id (i64.const 2))
    (call $read_register (i64.const 2) (i64.const 512))
    (drop (call $promise_create
      (call $register_len (i64.const 2)) (i64.const 512)
      (i64.const 10) (i64.const 16)
      (i64.add (i64.const 16) (local.get $n)) (i64.const 256)
      (i64.const 64) (i64.const 10000000000000))))

  (func (export "on_deposit")
    (local $n i64)
    (local.set $n (i64.sub (block (result i64)
      (call $input (i64.const 0))
      (call $read_register (i64.const 0) (i64.const 1024))
      (call $register_len (i64.const 0))) (i64.const 16)))
    (call $load_books)
    (call $add (i32.const 128) (i32.const 1024))
    (call $save_books)
    ;; The credit: "c" + who.
    (i32.store8 (i32.const 768) (i32.const 99))
    (call $copy (i32.const 769) (i32.const 1040) (i32.wrap_i64 (local.get $n)))
    (drop (call $storage_write (i64.add (local.get $n) (i64.const 1)) (i64.const 768)
                               (i64.const 16) (i64.const 1024) (i64.const 0))))

  (func (export "ping")
    (call $current_account_id (i64.const 2))
    (call $read_register (i64.const 2) (i64.const 512))
    (drop (call $promise_create
      (call $register_len (i64.const 2)) (i64.const 512)
      (i64.const 6) (i64.const 32)
      (i64.const 0) (i64.const 0)
      (i64.const 64) (i64.const 5000000000000))))

  (func (export "helper")
    (call $panic_utf8 (i64.const 13) (i64.const 48)))

  (func (export "booked")
    (call $load_books)
    (call $value_return (i64.const 16) (i64.const 128))))
