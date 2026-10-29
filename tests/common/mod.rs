#![allow(dead_code)]
use near_contract_sim::{Balance, FinalExecutionStatus, Reply, Sim, Stub, TxOutcome};
use serde_json::json;
use std::path::PathBuf;

/// The checkout, read when the test runs rather than baked in by `env!`,
/// so the test binary is the same in every checkout.
pub fn manifest_dir() -> PathBuf {
    std::env::var_os("CARGO_MANIFEST_DIR")
        .expect("CARGO_MANIFEST_DIR unset: run under cargo")
        .into()
}

pub fn res(name: &str) -> Vec<u8> {
    let p = manifest_dir().join("tests/res").join(name);
    std::fs::read(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display()))
}

// The four wasm below were built with cargo-near on a host whose Rust
// toolchain went unrecorded (`rustc` commit 2d8144b7). Each embedded the
// build machine's home directory in its panic-location strings; every
// `$HOME/` prefix was replaced in place with a same-length `/build/xxx…/`
// placeholder, so only data-segment bytes changed and the code and gas are
// the original build's. A rebuild should pass `--env "RUSTFLAGS=-C
// link-arg=-s --remap-path-prefix=$HOME=/build
// --remap-path-prefix=$HOME/.cargo/registry/src=/cargo"` to `cargo near
// build`.

/// near-sdk 5.29.0's status-message example (licence: `tests/res/NOTICE.md`).
pub fn status_wasm() -> Vec<u8> {
    res("status_message.wasm")
}
/// near-sdk 5.29.0's cross-contract-calls example, its high-level contract.
pub fn xcc_wasm() -> Vec<u8> {
    res("cross_contract_high_level.wasm")
}
/// near-sdk 5.29.0's fungible-token example.
pub fn ft_wasm() -> Vec<u8> {
    res("fungible_token.wasm")
}
/// Built from `tests/res/probe_src.rs.txt`.
pub fn probe_wasm() -> Vec<u8> {
    res("probe.wasm")
}
pub fn tip_jar_wasm() -> Vec<u8> {
    wat::parse_file(manifest_dir().join("tests/res/tip_jar.wat")).unwrap()
}
pub fn ledger_pair_wasm() -> Vec<u8> {
    wat::parse_file(manifest_dir().join("tests/res/ledger_pair.wat")).unwrap()
}
pub fn staking_pool_wasm() -> Vec<u8> {
    wat::parse_file(manifest_dir().join("tests/res/staking_pool.wat")).unwrap()
}
pub fn failure_probe_wasm() -> Vec<u8> {
    wat::parse_file(manifest_dir().join("tests/res/failure_probe.wat")).unwrap()
}

pub fn near(n: u128) -> Balance {
    Balance::from_near(n)
}

/// The raw error of an outcome that must have failed, debug-formatted.
pub fn err(out: &TxOutcome) -> String {
    match &out.status {
        FinalExecutionStatus::Failure(e) => format!("{e:?}"),
        s => panic!("expected a failure, got {s:?}"),
    }
}

/// The message of a caught panic, without color.
pub fn panic_message(r: Result<impl Sized, Box<dyn std::any::Any + Send>>) -> String {
    let msg = match r {
        Ok(_) => panic!("expected a panic"),
        Err(p) => p
            .downcast_ref::<String>()
            .cloned()
            .or_else(|| p.downcast_ref::<&str>().map(|s| s.to_string()))
            .unwrap_or_default(),
    };
    let mut parts = msg.split("\x1b[");
    let mut plain = parts.next().unwrap_or_default().to_string();
    for p in parts {
        plain.push_str(p.split_once('m').map_or(p, |(_, rest)| rest));
    }
    plain
}

/// `got` must be `want` exactly. On a mismatch it prints `got` as is,
/// ready to paste into the test.
#[track_caller]
pub fn assert_rendered(got: &str, want: &str) {
    assert!(got == want, "rendering changed; got:\n{got}<<<");
}

/// alice.near and bob.near, with 1000 NEAR each.
pub fn sim_with_users() -> Sim {
    let mut sim = Sim::new();
    for id in ["alice.near", "bob.near"] {
        sim.create_account(id, near(1_000));
    }
    sim
}

/// `sim_with_users()`, and probe.near running `tests/res/probe.wasm`.
pub fn probe_sim() -> Sim {
    let mut sim = sim_with_users();
    sim.deploy("probe.near", probe_wasm());
    sim
}

/// alice.near holds 1000 of ft.near; r.near is a stub whose
/// `ft_on_transfer` keeps everything (`"0"`): `Sim::stub_reply` changes it.
pub fn ft_sim() -> Sim {
    let mut sim = Sim::new();
    sim.create_account("alice.near", Balance::from_near(100));
    sim.deploy("ft.near", ft_wasm());
    sim.call("ft.near", "ft.near", "new_default_meta")
        .args_json(json!({"owner_id": "alice.near", "total_supply": "1000"}))
        .transact()
        .assert_success();
    sim.deploy_stub("r.near", Stub::new().on("ft_on_transfer", Reply::json("0")));
    sim.call("alice.near", "ft.near", "storage_deposit")
        .args_json(json!({"account_id": "r.near"}))
        .deposit(Balance::from_millinear(125))
        .transact()
        .assert_success();
    sim
}

/// `id`'s balance on ft.near.
pub fn ft_balance(sim: &Sim, id: &str) -> String {
    sim.view("ft.near", "ft_balance_of", json!({"account_id": id})).json()
}

/// Minimal yield contract:
///   yield_return      -> promise_yield_create("cb") and return that promise
///                        (the callback is on the result path)
///   yield_and_resume  -> yield and resume in the same call, payload "\"now\""
///   cb                -> returns "\"resumed\"" / "\"timeout\"" from promise_result(0)
pub fn yield_wasm() -> Vec<u8> {
    wat::parse_str(
        r#"(module
  (import "env" "promise_yield_create" (func $yc (param i64 i64 i64 i64 i64 i64 i64) (result i64)))
  (import "env" "promise_yield_resume" (func $yr (param i64 i64 i64 i64) (result i32)))
  (import "env" "promise_return" (func $ret (param i64)))
  (import "env" "promise_result" (func $pr (param i64 i64) (result i64)))
  (import "env" "read_register" (func $rr (param i64 i64)))
  (import "env" "value_return" (func $vr (param i64 i64)))
  (memory 1)
  (export "memory" (memory 0))
  (data (i32.const 0) "cb")
  (data (i32.const 16) "\"resumed\"")
  (data (i32.const 32) "\"timeout\"")
  (data (i32.const 48) "\"now\"")
  (func $yield (result i64)
    (call $yc (i64.const 2) (i64.const 0) (i64.const 0) (i64.const 0)
              (i64.const 10000000000000) (i64.const 0) (i64.const 1)))
  (func (export "yield_return") (call $ret (call $yield)))
  (func (export "yield_and_resume")
    (drop (call $yield))
    (call $rr (i64.const 1) (i64.const 256))
    (drop (call $yr (i64.const 32) (i64.const 256) (i64.const 5) (i64.const 48))))
  (func (export "cb")
    (if (i64.eq (call $pr (i64.const 0) (i64.const 0)) (i64.const 1))
      (then (call $vr (i64.const 9) (i64.const 16)))
      (else (call $vr (i64.const 9) (i64.const 32))))))"#,
    )
    .unwrap()
}

/// Environment probe: each method returns a host-function value.
///   chain_id               -> env::chain_id() bytes
///   validator_stake        -> u128 LE stake of the account id passed as raw args
///   validator_total_stake  -> u128 LE
///   echo                   -> returns its raw input
///   random_seed            -> env::random_seed() bytes
pub fn env_wasm() -> Vec<u8> {
    wat::parse_str(
        r#"(module
  (import "env" "chain_id" (func $chain_id (param i64)))
  (import "env" "input" (func $input (param i64)))
  (import "env" "register_len" (func $rlen (param i64) (result i64)))
  (import "env" "read_register" (func $rr (param i64 i64)))
  (import "env" "validator_stake" (func $vs (param i64 i64 i64)))
  (import "env" "validator_total_stake" (func $vts (param i64)))
  (import "env" "value_return" (func $vr (param i64 i64)))
  (import "env" "random_seed" (func $random_seed (param i64)))
  (memory 1)
  (export "memory" (memory 0))
  (func $ret_reg
    (call $rr (i64.const 0) (i64.const 1024))
    (call $vr (call $rlen (i64.const 0)) (i64.const 1024)))
  (func (export "chain_id") (call $chain_id (i64.const 0)) (call $ret_reg))
  (func (export "echo") (call $input (i64.const 0)) (call $ret_reg))
  (func (export "random_seed") (call $random_seed (i64.const 0)) (call $ret_reg))
  (func (export "validator_stake")
    (call $input (i64.const 0))
    (call $rr (i64.const 0) (i64.const 1024))
    (call $vs (call $rlen (i64.const 0)) (i64.const 1024) (i64.const 0))
    (call $vr (i64.const 16) (i64.const 0)))
  (func (export "validator_total_stake")
    (call $vts (i64.const 0))
    (call $vr (i64.const 16) (i64.const 0))))"#,
    )
    .unwrap()
}
