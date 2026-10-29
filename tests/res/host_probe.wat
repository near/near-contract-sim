;; nearcore's host functions as views, each named after the one it calls and
;; returning its result: what the chain computes, with no crypto crate on
;; the test's side (tests/docs.rs). Inputs are raw bytes:
;;   keccak256         -> the value
;;   ecrecover         -> hash (32), signature (64), v (1)
;;   ed25519_verify    -> signature (64), public key (32), message
;;   bls12381_p1_sum   -> (sign, point) pairs, as the host function takes them
;;   alt_bn128_g1_sum  -> the same, on alt_bn128
;; A host function that writes no result, as on an invalid input, fails the
;; view reading its register.
(module
  (import "env" "input" (func $input (param i64)))
  (import "env" "read_register" (func $read_register (param i64 i64)))
  (import "env" "register_len" (func $register_len (param i64) (result i64)))
  (import "env" "value_return" (func $value_return (param i64 i64)))
  (import "env" "keccak256" (func $keccak256 (param i64 i64 i64)))
  (import "env" "ecrecover" (func $ecrecover (param i64 i64 i64 i64 i64 i64 i64) (result i64)))
  (import "env" "ed25519_verify" (func $ed25519_verify (param i64 i64 i64 i64 i64 i64) (result i64)))
  (import "env" "bls12381_p1_sum" (func $bls12381_p1_sum (param i64 i64 i64) (result i64)))
  (import "env" "alt_bn128_g1_sum" (func $alt_bn128_g1_sum (param i64 i64 i64)))
  (memory 1)

  ;; The input, at offset 0; returns its length.
  (func $in (result i64)
    (call $input (i64.const 0))
    (call $read_register (i64.const 0) (i64.const 0))
    (call $register_len (i64.const 0)))

  ;; Returns register 1.
  (func $out
    (call $read_register (i64.const 1) (i64.const 0))
    (call $value_return (call $register_len (i64.const 1)) (i64.const 0)))

  (func (export "keccak256")
    (call $keccak256 (call $in) (i64.const 0) (i64.const 1))
    (call $out))

  (func (export "ecrecover")
    (drop (call $in))
    (drop (call $ecrecover (i64.const 32) (i64.const 0) (i64.const 64) (i64.const 32)
      (i64.load8_u (i32.const 96)) (i64.const 0) (i64.const 1)))
    (call $out))

  ;; Returns 1 byte: 1 if the signature is valid, else 0.
  (func (export "ed25519_verify")
    (local $len i64)
    (local.set $len (call $in))
    (i64.store8 (i32.const 0) (call $ed25519_verify (i64.const 64) (i64.const 0)
      (i64.sub (local.get $len) (i64.const 96)) (i64.const 96) (i64.const 32) (i64.const 64)))
    (call $value_return (i64.const 1) (i64.const 0)))

  (func (export "bls12381_p1_sum")
    (drop (call $bls12381_p1_sum (call $in) (i64.const 0) (i64.const 1)))
    (call $out))

  (func (export "alt_bn128_g1_sum")
    (call $alt_bn128_g1_sum (call $in) (i64.const 0) (i64.const 1))
    (call $out)))
