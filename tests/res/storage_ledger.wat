;; A contract that prepays storage from a ledger, as Omni Bridge does.
;;   deposit: records what was attached under "D" (u128 little-endian).
;;   write: stores 100 bytes under "U", paid from the ledger: it owes what
;;     was deposited less the storage "U" takes.
;;   pause / unpause: writes / removes 69 bytes of its own under "P", paid
;;     from its own NEAR.
(module
  (import "env" "attached_deposit" (func $attached (param i64)))
  (import "env" "storage_write" (func $write (param i64 i64 i64 i64 i64) (result i64)))
  (import "env" "storage_remove" (func $remove (param i64 i64 i64) (result i64)))
  (memory 1)
  (export "memory" (memory 0))
  (data (i32.const 0) "DUP")
  (func (export "deposit")
    (call $attached (i64.const 16))
    (drop (call $write (i64.const 1) (i64.const 0) (i64.const 16) (i64.const 16) (i64.const 0))))
  (func (export "write")
    (drop (call $write (i64.const 1) (i64.const 1) (i64.const 100) (i64.const 256) (i64.const 0))))
  (func (export "pause")
    (drop (call $write (i64.const 1) (i64.const 2) (i64.const 69) (i64.const 256) (i64.const 0))))
  (func (export "unpause")
    (drop (call $remove (i64.const 1) (i64.const 2) (i64.const 0))))
)
