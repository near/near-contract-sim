;; A request answered by a yield (tests/explore_enumerate.rs): nothing
;; moves NEAR.
;;   request -> yields to its own "cb", storing the data id under "id"
;;   respond -> resumes the stored yield; panics "no yield" if none waits
;;   cb      -> if it was resumed, stores "paid" = 1 and pays alice.near 1
;;              NEAR; nothing on a timeout
(module
  (import "env" "promise_yield_create" (func $yc (param i64 i64 i64 i64 i64 i64 i64) (result i64)))
  (import "env" "promise_yield_resume" (func $yr (param i64 i64 i64 i64) (result i32)))
  (import "env" "promise_result" (func $pr (param i64 i64) (result i64)))
  (import "env" "read_register" (func $rr (param i64 i64)))
  (import "env" "storage_write" (func $sw (param i64 i64 i64 i64 i64) (result i64)))
  (import "env" "storage_read" (func $sr (param i64 i64 i64) (result i64)))
  (import "env" "panic_utf8" (func $panic (param i64 i64)))
  (import "env" "promise_batch_create" (func $pbc (param i64 i64) (result i64)))
  (import "env" "promise_batch_action_transfer" (func $pbt (param i64 i64)))
  (memory 1)
  (export "memory" (memory 0))
  (data (i32.const 0) "cb")
  (data (i32.const 16) "id")
  (data (i32.const 32) "paid")
  (data (i32.const 48) "no yield")
  (data (i32.const 64) "\01")
  (data (i32.const 80) "alice.near")
  (data (i32.const 96) "\00\00\00\a1\ed\cc\ce\1b\c2\d3\00\00\00\00\00\00")
  ;; 256: the data id.
  (func (export "request")
    (drop (call $yc (i64.const 2) (i64.const 0) (i64.const 0) (i64.const 0)
                    (i64.const 10000000000000) (i64.const 0) (i64.const 1)))
    (drop (call $sw (i64.const 2) (i64.const 16) (i64.const -1) (i64.const 1) (i64.const 2))))
  (func (export "respond")
    (if (i64.ne (call $sr (i64.const 2) (i64.const 16) (i64.const 1)) (i64.const 1))
      (then (call $panic (i64.const 8) (i64.const 48))))
    (call $rr (i64.const 1) (i64.const 256))
    (if (i32.eqz (call $yr (i64.const 32) (i64.const 256) (i64.const 0) (i64.const 0)))
      (then (call $panic (i64.const 8) (i64.const 48)))))
  (func (export "cb")
    (if (i64.eq (call $pr (i64.const 0) (i64.const 0)) (i64.const 1))
      (then
        (drop (call $sw (i64.const 4) (i64.const 32) (i64.const 1) (i64.const 64) (i64.const 2)))
        (call $pbt (call $pbc (i64.const 10) (i64.const 80)) (i64.const 96))))))
