;; A book whose resolver pays per distinct entry it removes, on a budget
;; fixed per id at schedule time (tests/margin.rs).
;;
;;   deposit -> input: `{"ids":["<id>", ...],"sent":["<amount>", ...]}`,
;;              each id 64 bytes at most.
;;              Credits "b" + each id, then calls sink.near's on_deposit
;;              with the same input (20 Tgas) and its own `resolve` with
;;              it on a budget of 3 Tgas + 0.2 Tgas an id.
;;   resolve  -> for each id whose amount the sink's answer (a list of
;;              decimal strings) gives back (not "0"): removes "b" + id
;;              and, if it was there, logs the id and writes "r" and "s" + id. A
;;              second copy of one id finds nothing to remove, so its
;;              cost grows per distinct id, not per copy.
;;   held     -> (view) "b" + the input's bytes: 1 if credited, else 0.
(module
  (import "env" "input" (func $input (param i64)))
  (import "env" "register_len" (func $register_len (param i64) (result i64)))
  (import "env" "read_register" (func $read_register (param i64 i64)))
  (import "env" "current_account_id" (func $current_account_id (param i64)))
  (import "env" "storage_write" (func $storage_write (param i64 i64 i64 i64 i64) (result i64)))
  (import "env" "storage_remove" (func $storage_remove (param i64 i64 i64) (result i64)))
  (import "env" "storage_has_key" (func $storage_has_key (param i64 i64) (result i64)))
  (import "env" "promise_create"
    (func $promise_create (param i64 i64 i64 i64 i64 i64 i64 i64) (result i64)))
  (import "env" "promise_then"
    (func $promise_then (param i64 i64 i64 i64 i64 i64 i64 i64 i64) (result i64)))
  (import "env" "promise_result" (func $promise_result (param i64 i64) (result i64)))
  (import "env" "promise_return" (func $promise_return (param i64)))
  (import "env" "value_return" (func $value_return (param i64 i64)))
  (import "env" "log_utf8" (func $log (param i64 i64)))
  (import "env" "panic_utf8" (func $panic_utf8 (param i64 i64)))
  (memory 2)
  (export "memory" (memory 0))
  (data (i32.const 0) "sink.near")
  (data (i32.const 16) "on_deposit")
  (data (i32.const 32) "resolve")
  (data (i32.const 48) "input too long")
  (data (i32.const 1008) "id too long")
  ;; 64: a u64. 96: u128 zero. 128: the own id. 72000: a key.
  ;; 1024: the input. 40960: the sink's answer.
  (global $rc (mut i64) (i64.const 0))
  (global $rend (mut i64) (i64.const 0))

  (func $read (result i64)
    (local $len i64)
    (call $input (i64.const 0))
    (local.set $len (call $register_len (i64.const 0)))
    (if (i64.gt_u (local.get $len) (i64.const 30000))
      (then (call $panic_utf8 (i64.const 14) (i64.const 48))))
    (call $read_register (i64.const 0) (i64.const 1024))
    (local.get $len))

  ;; The key `tag` + the `len` bytes at `at`, at 72000; its length.
  (func $key (param $tag i32) (param $at i64) (param $len i64) (result i64)
    (local $i i64)
    (i32.store8 (i32.const 72000) (local.get $tag))
    (block $done
      (loop $copy
        (br_if $done (i64.ge_u (local.get $i) (local.get $len)))
        (i32.store8 (i32.wrap_i64 (i64.add (i64.const 72001) (local.get $i)))
          (i32.load8_u (i32.wrap_i64 (i64.add (local.get $at) (local.get $i)))))
        (local.set $i (i64.add (local.get $i) (i64.const 1)))
        (br $copy)))
    (i64.add (local.get $len) (i64.const 1)))

  ;; Whether the answer's next string gives something back (isn't "0").
  (func $gives (result i32)
    (local $from i64)
    (block $none
      (loop $find
        (br_if $none (i64.ge_u (global.get $rc) (global.get $rend)))
        (if (i32.eq (i32.load8_u (i32.wrap_i64 (global.get $rc))) (i32.const 34))
          (then
            (if (i64.eqz (local.get $from))
              (then (local.set $from (i64.add (global.get $rc) (i64.const 1))))
              (else
                (global.set $rc (i64.add (global.get $rc) (i64.const 1)))
                (return (i32.eqz (i32.and
                  (i64.eq (i64.sub (global.get $rc) (local.get $from)) (i64.const 2))
                  (i32.eq (i32.load8_u (i32.wrap_i64 (local.get $from))) (i32.const 48)))))))))
        (global.set $rc (i64.add (global.get $rc) (i64.const 1)))
        (br $find)))
    (i32.const 0))

  ;; Each id of the input (the strings between its first '[' and ']'):
  ;; credits it (`resolve` 0) or settles it (1); the count.
  (func $each (param $len i64) (param $resolve i32) (result i64)
    (local $i i64) (local $from i64) (local $n i64) (local $in i32) (local $c i32) (local $k i64)
    (block $done
      (loop $scan
        (br_if $done (i64.ge_u (local.get $i) (local.get $len)))
        (local.set $c (i32.load8_u (i32.wrap_i64 (i64.add (i64.const 1024) (local.get $i)))))
        (br_if $done (i32.and (local.get $in) (i32.eq (local.get $c) (i32.const 93))))
        (if (i32.eq (local.get $c) (i32.const 91)) (then (local.set $in (i32.const 1))))
        (if (i32.and (local.get $in) (i32.eq (local.get $c) (i32.const 34)))
          (then
            (if (i64.eqz (local.get $from))
              (then (local.set $from (i64.add (local.get $i) (i64.const 1))))
              (else
                (if (i64.gt_u (i64.sub (local.get $i) (local.get $from)) (i64.const 64))
                  (then (call $panic_utf8 (i64.const 11) (i64.const 1008))))
                (local.set $n (i64.add (local.get $n) (i64.const 1)))
                (local.set $k (call $key (i32.const 98) (i64.add (i64.const 1024) (local.get $from))
                                         (i64.sub (local.get $i) (local.get $from))))
                (if (local.get $resolve)
                  (then
                    (if (i32.and (call $gives)
                          (i64.eq (call $storage_remove (local.get $k) (i64.const 72000) (i64.const 3))
                                  (i64.const 1)))
                      (then
                        (call $log (i64.sub (local.get $k) (i64.const 1)) (i64.const 72001))
                        (i32.store8 (i32.const 72000) (i32.const 114))
                        (drop (call $storage_write (local.get $k) (i64.const 72000)
                                                   (i64.const 16) (i64.const 96) (i64.const 3)))
                        (i32.store8 (i32.const 72000) (i32.const 115))
                        (drop (call $storage_write (local.get $k) (i64.const 72000)
                                                   (i64.const 16) (i64.const 96) (i64.const 3))))))
                  (else
                    (drop (call $storage_write (local.get $k) (i64.const 72000)
                                               (i64.const 16) (i64.const 96) (i64.const 3)))))
                (local.set $from (i64.const 0))))))
        (local.set $i (i64.add (local.get $i) (i64.const 1)))
        (br $scan)))
    (local.get $n))

  (func (export "deposit")
    (local $len i64) (local $n i64) (local $own i64) (local $p i64)
    (local.set $len (call $read))
    (local.set $n (call $each (local.get $len) (i32.const 0)))
    (call $current_account_id (i64.const 1))
    (local.set $own (call $register_len (i64.const 1)))
    (call $read_register (i64.const 1) (i64.const 128))
    (local.set $p
      (call $promise_create (i64.const 9) (i64.const 0) (i64.const 10) (i64.const 16)
                            (local.get $len) (i64.const 1024) (i64.const 96)
                            (i64.const 20000000000000)))
    (call $promise_return
      (call $promise_then (local.get $p) (local.get $own) (i64.const 128) (i64.const 7)
                          (i64.const 32) (local.get $len) (i64.const 1024) (i64.const 96)
                          (i64.add (i64.const 3000000000000)
                                   (i64.mul (local.get $n) (i64.const 200000000000))))))

  (func (export "resolve")
    (local $len i64)
    (local.set $len (call $read))
    (if (i64.ne (call $promise_result (i64.const 0) (i64.const 2)) (i64.const 1)) (then (return)))
    (global.set $rend (i64.add (i64.const 40960) (call $register_len (i64.const 2))))
    (if (i64.gt_u (global.get $rend) (i64.const 71000))
      (then (call $panic_utf8 (i64.const 14) (i64.const 48))))
    (call $read_register (i64.const 2) (i64.const 40960))
    (global.set $rc (i64.const 40960))
    (drop (call $each (local.get $len) (i32.const 1))))

  (func (export "held")
    (local $len i64)
    (local.set $len (call $read))
    (i64.store (i32.const 64)
      (call $storage_has_key (call $key (i32.const 98) (i64.const 1024) (local.get $len))
                             (i64.const 72000)))
    (call $value_return (i64.const 8) (i64.const 64)))
)
