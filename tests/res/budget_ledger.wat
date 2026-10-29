;; A ledger whose resolver has a budget fixed at schedule time
;; (tests/margin.rs). Inputs are raw: ids separated by "\n".
;;
;;   put     -> input: ids, each at most 64 bytes, at most 32 of them.
;;              Appends each to the list ("l" + its place, u64 LE; "n"
;;              is the length), then calls its own `noop` and, after it,
;;              `on_put` with the place of the first and the ids, on a
;;              budget of 4 Tgas + 2 Tgas an id: what its formula sees;
;;              returns what `on_put` does.
;;   noop    -> nothing.
;;   unsettled -> (view) entries not settled yet, u64 LE.
;;   on_put  -> settles each id, last first. Settling works over the id's
;;              bytes, and, where entries were appended after the put's
;;              own, also over the bytes of the list's last entry (it moves
;;              into the settled place): a cost the budget doesn't see.
;;              Then counts them settled ("s").
(module
  (import "env" "input" (func $input (param i64)))
  (import "env" "register_len" (func $register_len (param i64) (result i64)))
  (import "env" "read_register" (func $read_register (param i64 i64)))
  (import "env" "current_account_id" (func $current_account_id (param i64)))
  (import "env" "storage_write" (func $storage_write (param i64 i64 i64 i64 i64) (result i64)))
  (import "env" "storage_read" (func $storage_read (param i64 i64 i64) (result i64)))
  (import "env" "promise_create"
    (func $promise_create (param i64 i64 i64 i64 i64 i64 i64 i64) (result i64)))
  (import "env" "promise_then"
    (func $promise_then (param i64 i64 i64 i64 i64 i64 i64 i64 i64) (result i64)))
  (import "env" "panic_utf8" (func $panic_utf8 (param i64 i64)))
  (import "env" "promise_return" (func $promise_return (param i64)))
  (import "env" "value_return" (func $value_return (param i64 i64)))
  (memory 1)
  (export "memory" (memory 0))
  (data (i32.const 0) "noop")
  (data (i32.const 16) "on_put")
  (data (i32.const 32) "id too long")
  (data (i32.const 48) "too many ids")
  (data (i32.const 80) "n")
  (data (i32.const 81) "s")
  ;; 64: a key ("l" + u64). 96: a u64 read. 112: u128 zero. 128: the own
  ;; id. 256: on_put's args (the first place, u64, then the ids).

  ;; The u64 under the 1-byte key at `k`: "n", the list's length, or "s".
  (func $get (param $k i64) (result i64)
    (if (result i64)
        (i64.eq (call $storage_read (i64.const 1) (local.get $k) (i64.const 0)) (i64.const 1))
      (then (call $read_register (i64.const 0) (i64.const 96)) (i64.load (i32.const 96)))
      (else (i64.const 0))))

  (func $len (result i64) (call $get (i64.const 80)))

  (func (export "unsettled")
    (i64.store (i32.const 96) (i64.sub (call $len) (call $get (i64.const 81))))
    (call $value_return (i64.const 8) (i64.const 96)))

  ;; The key of place `at`, at 64.
  (func $key (param $at i64)
    (i32.store8 (i32.const 64) (i32.const 108))
    (i64.store (i32.const 65) (local.get $at)))

  ;; The end of the id starting at `from` in the ids at 264, `len` long.
  (func $end (param $from i64) (param $len i64) (result i64)
    (block $out
      (loop $scan
        (br_if $out (i64.ge_u (local.get $from) (local.get $len)))
        (br_if $out (i32.eq (i32.load8_u (i32.wrap_i64 (i64.add (i64.const 264) (local.get $from))))
                            (i32.const 10)))
        (local.set $from (i64.add (local.get $from) (i64.const 1)))
        (br $scan)))
    (local.get $from))

  ;; Work over `n` bytes.
  (func $spin (param $n i64)
    (local $i i64)
    (local.set $i (i64.mul (local.get $n) (i64.const 5000)))
    (block $out
      (loop $more
        (br_if $out (i64.eqz (local.get $i)))
        (local.set $i (i64.sub (local.get $i) (i64.const 1)))
        (br $more))))

  ;; Reads the input into 264: its length.
  (func $read (result i64)
    (local $len i64)
    (call $input (i64.const 0))
    (local.set $len (call $register_len (i64.const 0)))
    (if (i64.gt_u (local.get $len) (i64.const 3000))
      (then (call $panic_utf8 (i64.const 12) (i64.const 48))))
    (call $read_register (i64.const 0) (i64.const 264))
    (local.get $len))

  (func (export "noop"))

  (func (export "put")
    (local $len i64) (local $from i64) (local $to i64) (local $at i64) (local $first i64)
    (local $own i64) (local $p i64)
    (local.set $len (call $read))
    (local.set $first (call $len))
    (local.set $at (local.get $first))
    (block $done
      (loop $each
        (br_if $done (i64.ge_u (local.get $from) (local.get $len)))
        (local.set $to (call $end (local.get $from) (local.get $len)))
        (if (i64.gt_u (i64.sub (local.get $to) (local.get $from)) (i64.const 64))
          (then (call $panic_utf8 (i64.const 11) (i64.const 32))))
        (call $key (local.get $at))
        (drop (call $storage_write (i64.const 9) (i64.const 64)
                                   (i64.sub (local.get $to) (local.get $from))
                                   (i64.add (i64.const 264) (local.get $from)) (i64.const 1)))
        (local.set $at (i64.add (local.get $at) (i64.const 1)))
        (local.set $from (i64.add (local.get $to) (i64.const 1)))
        (br $each)))
    (if (i64.gt_u (i64.sub (local.get $at) (local.get $first)) (i64.const 32))
      (then (call $panic_utf8 (i64.const 12) (i64.const 48))))
    (i64.store (i32.const 96) (local.get $at))
    (drop (call $storage_write (i64.const 1) (i64.const 80) (i64.const 8) (i64.const 96)
                               (i64.const 1)))
    (i64.store (i32.const 256) (local.get $first))
    (call $current_account_id (i64.const 1))
    (local.set $own (call $register_len (i64.const 1)))
    (call $read_register (i64.const 1) (i64.const 128))
    (local.set $p (call $promise_create (local.get $own) (i64.const 128) (i64.const 4)
                                        (i64.const 0) (i64.const 0) (i64.const 0)
                                        (i64.const 112) (i64.const 2000000000000)))
    (call $promise_return (call $promise_then (local.get $p) (local.get $own) (i64.const 128) (i64.const 6)
                              (i64.const 16) (i64.add (local.get $len) (i64.const 8))
                              (i64.const 256) (i64.const 112)
                              (i64.add (i64.const 4000000000000)
                                       (i64.mul (i64.sub (local.get $at) (local.get $first))
                                                (i64.const 2000000000000))))))

  (func (export "on_put")
    (local $len i64) (local $from i64) (local $to i64) (local $n i64) (local $moved i64)
    (local $end i64)
    (local $first i64)
    (local.set $len (i64.sub (call $read) (i64.const 8)))
    ;; The first place, then the ids: move them to 264.
    (local.set $first (i64.load (i32.const 264)))
    (memory.copy (i32.const 264) (i32.const 272) (i32.wrap_i64 (local.get $len)))
    (block $done
      (loop $count
        (br_if $done (i64.ge_u (local.get $from) (local.get $len)))
        (local.set $n (i64.add (local.get $n) (i64.const 1)))
        (local.set $from (i64.add (call $end (local.get $from) (local.get $len)) (i64.const 1)))
        (br $count)))
    ;; Entries after the put's own: the last one moves at each settle.
    (local.set $end (call $len))
    (if (i64.gt_u (local.get $end) (i64.add (local.get $first) (local.get $n)))
      (then
        (call $key (i64.sub (local.get $end) (i64.const 1)))
        (drop (call $storage_read (i64.const 9) (i64.const 64) (i64.const 0)))
        (local.set $moved (call $register_len (i64.const 0)))))
    (local.set $from (i64.const 0))
    (block $done
      (loop $each
        (br_if $done (i64.ge_u (local.get $from) (local.get $len)))
        (local.set $to (call $end (local.get $from) (local.get $len)))
        (call $spin (i64.add (i64.const 8)
                             (i64.add (i64.sub (local.get $to) (local.get $from)) (local.get $moved))))
        (local.set $from (i64.add (local.get $to) (i64.const 1)))
        (br $each)))
    (i64.store (i32.const 96) (i64.add (call $get (i64.const 81)) (local.get $n)))
    (drop (call $storage_write (i64.const 1) (i64.const 81) (i64.const 8) (i64.const 96)
                               (i64.const 1))))
)
