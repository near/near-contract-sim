;; A registry whose resolver's budget is fixed at schedule time, per id
;; (tests/margin.rs). The same code runs as the front and the registry.
;;
;;   put        -> input: `{"ids":["<id>", ...]}`. Calls registry.near's
;;                 on_receive with the same input (150 Tgas).
;;   on_receive -> appends each id to the queue ("q" + its place, u64 LE;
;;                 "n" is the length), then calls its own `resolve` with
;;                 the place of its first and the count, on a budget of
;;                 2.5 Tgas + 1 Tgas an id.
;;   resolve    -> settles its entries and up to 8 queued after them:
;;                 writes "e" + each id. Entries others queued while it
;;                 waited are a cost its budget doesn't see; each write's
;;                 trie nodes grow with its key. "s" is the end it settled
;;                 to.
;;   unsettled  -> (view) queued entries not settled, u64 LE.
(module
  (import "env" "input" (func $input (param i64)))
  (import "env" "register_len" (func $register_len (param i64) (result i64)))
  (import "env" "read_register" (func $read_register (param i64 i64)))
  (import "env" "current_account_id" (func $current_account_id (param i64)))
  (import "env" "storage_write" (func $storage_write (param i64 i64 i64 i64 i64) (result i64)))
  (import "env" "storage_read" (func $storage_read (param i64 i64 i64) (result i64)))
  (import "env" "promise_create"
    (func $promise_create (param i64 i64 i64 i64 i64 i64 i64 i64) (result i64)))
  (import "env" "promise_return" (func $promise_return (param i64)))
  (import "env" "value_return" (func $value_return (param i64 i64)))
  (import "env" "panic_utf8" (func $panic_utf8 (param i64 i64)))
  (memory 2)
  (export "memory" (memory 0))
  (data (i32.const 0) "registry.near")
  (data (i32.const 16) "on_receive")
  (data (i32.const 32) "resolve")
  (data (i32.const 48) "input too long")
  (data (i32.const 80) "n")
  (data (i32.const 81) "s")
  ;; 64: a u64 read. 96: u128 zero. 128: the own id. 200: the first
  ;; place, then the count.
  ;; 256: a key ("q" + u64, or "e" + id). 1024: the input.

  (func $get (param $k i64) (result i64)
    (if (result i64)
        (i64.eq (call $storage_read (i64.const 1) (local.get $k) (i64.const 0)) (i64.const 1))
      (then (call $read_register (i64.const 0) (i64.const 64)) (i64.load (i32.const 64)))
      (else (i64.const 0))))

  (func $set (param $k i64) (param $v i64)
    (i64.store (i32.const 64) (local.get $v))
    (drop (call $storage_write (i64.const 1) (local.get $k) (i64.const 8) (i64.const 64)
                               (i64.const 1))))

  (func $qkey (param $at i64)
    (i32.store8 (i32.const 256) (i32.const 113))
    (i64.store (i32.const 257) (local.get $at)))

  (func $read (result i64)
    (local $len i64)
    (call $input (i64.const 0))
    (local.set $len (call $register_len (i64.const 0)))
    (if (i64.gt_u (local.get $len) (i64.const 60000))
      (then (call $panic_utf8 (i64.const 14) (i64.const 48))))
    (call $read_register (i64.const 0) (i64.const 1024))
    (local.get $len))

  (func (export "unsettled")
    (i64.store (i32.const 64) (i64.sub (call $get (i64.const 80)) (call $get (i64.const 81))))
    (call $value_return (i64.const 8) (i64.const 64)))

  (func (export "put")
    (local $len i64)
    (local.set $len (call $read))
    (call $promise_return
      (call $promise_create (i64.const 13) (i64.const 0) (i64.const 10) (i64.const 16)
                            (local.get $len) (i64.const 1024) (i64.const 96)
                            (i64.const 150000000000000))))

  (func (export "on_receive")
    (local $len i64) (local $i i64) (local $from i64) (local $at i64) (local $count i64)
    (local $own i64) (local $in i32)
    (local.set $len (call $read))
    (local.set $at (call $get (i64.const 80)))
    ;; Each string after the first '[' is an id.
    (block $done
      (loop $scan
        (br_if $done (i64.ge_u (local.get $i) (local.get $len)))
        (if (i32.eq (i32.load8_u (i32.wrap_i64 (i64.add (i64.const 1024) (local.get $i))))
                    (i32.const 91))
          (then (local.set $in (i32.const 1))))
        (if (i32.and (local.get $in)
                     (i32.eq (i32.load8_u (i32.wrap_i64 (i64.add (i64.const 1024) (local.get $i))))
                             (i32.const 34)))
          (then
            (if (i64.eqz (local.get $from))
              (then (local.set $from (i64.add (local.get $i) (i64.const 1))))
              (else
                (call $qkey (local.get $at))
                (drop (call $storage_write (i64.const 9) (i64.const 256)
                                           (i64.sub (local.get $i) (local.get $from))
                                           (i64.add (i64.const 1024) (local.get $from))
                                           (i64.const 1)))
                (local.set $at (i64.add (local.get $at) (i64.const 1)))
                (local.set $count (i64.add (local.get $count) (i64.const 1)))
                (local.set $from (i64.const 0))))))
        (local.set $i (i64.add (local.get $i) (i64.const 1)))
        (br $scan)))
    (call $set (i64.const 80) (local.get $at))
    (i64.store (i32.const 200) (i64.sub (local.get $at) (local.get $count)))
    (i64.store (i32.const 208) (local.get $count))
    (call $current_account_id (i64.const 1))
    (local.set $own (call $register_len (i64.const 1)))
    (call $read_register (i64.const 1) (i64.const 128))
    (call $promise_return
      (call $promise_create (local.get $own) (i64.const 128) (i64.const 7) (i64.const 32)
                            (i64.const 16) (i64.const 200) (i64.const 96)
                            (i64.add (i64.const 2500000000000)
                                     (i64.mul (local.get $count) (i64.const 1000000000000))))))

  (func (export "resolve")
    (local $at i64) (local $end i64) (local $len i64)
    (call $input (i64.const 0))
    (call $read_register (i64.const 0) (i64.const 200))
    (local.set $at (i64.load (i32.const 200)))
    (local.set $end (call $get (i64.const 80)))
    (local.set $len (i64.add (local.get $at) (i64.add (i64.load (i32.const 208)) (i64.const 8))))
    (if (i64.lt_u (local.get $len) (local.get $end)) (then (local.set $end (local.get $len))))
    (block $done
      (loop $each
        (br_if $done (i64.ge_u (local.get $at) (local.get $end)))
        (call $qkey (local.get $at))
        (drop (call $storage_read (i64.const 9) (i64.const 256) (i64.const 0)))
        (local.set $len (call $register_len (i64.const 0)))
        (call $read_register (i64.const 0) (i64.const 257))
        (i32.store8 (i32.const 256) (i32.const 101))
        (drop (call $storage_write (i64.add (local.get $len) (i64.const 1)) (i64.const 256)
                                   (i64.const 1) (i64.const 96) (i64.const 1)))
        (local.set $at (i64.add (local.get $at) (i64.const 1)))
        (br $each)))
    (call $set (i64.const 81) (local.get $end)))
)
