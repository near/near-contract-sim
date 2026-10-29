//! Real-world check: near/mpc's tee-verifier doing full Intel TDX DCAP quote
//! verification on-chain, and the MPC contract's `submit_participant_info`
//! flow that calls it, with the NORMAL block-clock build (no pinned-now
//! feature). The simulator moves the block clock into the fixture's
//! collateral validity window instead.
//!
//! Fixtures in tests/res/mpc/ (provenance):
//! - near/mpc @ 1d514bc4001028b8c4a8af0190d84167216aa818, built with the
//!   repo's Makefile.toml commands (default features => `block-clock`):
//!   `cargo near build non-reproducible-wasm --no-abi --profile=release-contract --locked`
//!   - mpc_contract.wasm  sha256 as built 5aa22b0ab076398d023d741cdc9df00db5a1c21c595de59dbdf3bf7cdd295c97,
//!     this file 2eecdc26aed2d1ea61362dea0bac8589e77447d755a5ee12e215c892416bef88
//!   - tee_verifier.wasm  sha256 as built 749ecdc201897c1838713ab6ca96fbabe567fef4be7ba51caf838da0af629387,
//!     this file f8056005ed2d122ebf74ce9401cfd4fd7b8f58ba9678d0e2712818d17e0cb94c
//!
//!   Both builds (Rust 1.97.1) embedded the build machine's home directory
//!   in their panic-location strings. Each `$HOME/` prefix was replaced in
//!   place with a same-length `/build/xxx…/` placeholder, so only
//!   data-segment bytes changed and the code and gas are the original
//!   build's. A fresh build should add
//!   `--env "RUSTFLAGS=-C link-arg=-s --remap-path-prefix=$HOME=/build
//!   --remap-path-prefix=$HOME/.cargo/registry/src=/cargo"`.
//! - verify_quote_args.borsh: byte-identical to the repo's
//!   crates/tee-verifier/tests/fixtures/verify_quote_args.borsh
//! - verify_quote_expected_result.borsh: `VerificationResult::Verified(..)`
//!   produced by the repo's own native verifier at the pinned time.
//! - mpc_*.json / mpc_state_patched.borsh / near_account_secret_key: built
//!   from the repo's test fixtures (crates/test-utils/assets) and types. The
//!   STATE patch allowlists the fixture's image/launcher-compose hashes, the
//!   same trick the repo's sandbox tests use (`whitelist_dstack_in_state`).
//!
//! Time window: tests pin now = 1_786_622_400 s (2026-08-13T12:00Z); the
//! collateral is valid 2026-08-13T10:45:38Z .. 2026-09-11T23:57:11Z.

mod common;
use common::*;
use near_contract_sim::{Balance, Gas, Sim};
use std::path::{Path, PathBuf};

const PINNED_NOW_NS: u64 = 1_786_622_400_000_000_000;
const TODAY_NS: u64 = 1_790_121_600_000_000_000; // 2026-09-23, after the window

fn mpc(name: &str) -> Vec<u8> {
    res(&format!("mpc/{name}"))
}

fn sim_at(ts: u64) -> Sim {
    let mut sim = Sim::new();
    sim.set_timestamp_ns(ts);
    sim.create_account("alice.near", near(1_000));
    sim.deploy("tee-verifier.near", mpc("tee_verifier.wasm"));
    sim
}

#[test]
fn tee_verifier_verify_quote_direct() {
    let mut sim = sim_at(PINNED_NOW_NS);
    let out = sim
        .call("alice.near", "tee-verifier.near", "verify_quote")
        .args(mpc("verify_quote_args.borsh"))
        .gas(Gas::from_teragas(300))
        .transact();
    out.assert_success();
    let r = out.receipt("verify_quote").unwrap();
    println!(
        "verify_quote: gas_burnt = {} ({:.3} Tgas), block_ts = {}",
        r.gas_burnt.as_gas(),
        r.gas_burnt.as_gas() as f64 / 1e12,
        r.block_timestamp_ns
    );
    let mut top: Vec<_> = r.gas_profile.iter().collect();
    top.sort_by_key(|(_, g)| std::cmp::Reverse(**g));
    for (k, g) in top.iter().take(6) {
        println!("    {k:<28} {:>8.3} Tgas", **g as f64 / 1e12);
    }
    // Byte-exact against the repo's native verifier.
    assert_eq!(out.value().unwrap(), mpc("verify_quote_expected_result.borsh").as_slice());
    assert_eq!(out.value().unwrap()[0], 0, "VerificationResult::Verified");
    // The MPC contract gives verify_quote exactly 200 Tgas; its sandbox test
    // asserts <= 180 Tgas.
    assert!(r.gas_burnt < Gas::from_teragas(180), "{}", r.gas_burnt);
}

#[test]
fn tee_verifier_rejects_outside_validity_window() {
    let mut sim = sim_at(TODAY_NS);
    let out = sim
        .call("alice.near", "tee-verifier.near", "verify_quote")
        .args(mpc("verify_quote_args.borsh"))
        .gas(Gas::from_teragas(300))
        .transact();
    // A rejection is a *successful* receipt carrying VerificationResult::Rejected.
    out.assert_success();
    let v = out.value().unwrap();
    assert_eq!(v[0], 1, "VerificationResult::Rejected");
    let msg: String =
        String::from_utf8_lossy(&v[1..]).chars().filter(|c| !c.is_control()).collect();
    println!("rejected at 2026-09-23: {msg}");
    assert!(msg.to_lowercase().contains("expired"), "{msg}");
}

/// This binary's `Sim::cached` snapshot directory in each place
/// skills/near-contract-sim/references/sim.md lists for the cache, once each; the one in use is among
/// them.
fn snapshot_dirs() -> Vec<PathBuf> {
    let exe = std::env::current_exe().unwrap();
    let target = std::env::var_os("CARGO_TARGET_DIR").map(PathBuf::from);
    let tagged = exe.ancestors().find(|d| d.join("CACHEDIR.TAG").is_file()).map(Path::to_path_buf);
    let roots = std::env::var_os("NCS_CACHE_DIR").map(PathBuf::from).into_iter().chain(
        target
            .filter(|d| d.is_absolute())
            .into_iter()
            .chain(tagged)
            .map(|d| d.join("near-contract-sim")),
    );
    let roots = roots.chain(std::env::home_dir().map(|h| h.join(".cache/near-contract-sim")));
    let mut dirs: Vec<_> =
        roots.map(|r| r.join("snapshots").join(exe.file_name().unwrap())).collect();
    // CARGO_TARGET_DIR is often the tagged ancestor too.
    dirs.sort();
    dirs.dedup();
    dirs
}

/// The snapshot files (not locks) in this binary's snapshot directories.
fn snapshot_files() -> Vec<PathBuf> {
    let entries = snapshot_dirs().into_iter().filter_map(|d| std::fs::read_dir(d).ok()).flatten();
    entries.map(|e| e.unwrap().path()).filter(|p| p.extension().is_none()).collect()
}

/// An MPC world (the tee verifier and MPC contracts deployed, one node
/// attested) saves to under 5 MB, although its deployed code is stored
/// twice: in the trie, and in the transactions the block log keeps.
#[test]
fn an_mpc_world_snapshot_stays_under_5_mb() {
    // The only `Sim::cached` in this binary: with its snapshots gone, the
    // call below saves exactly one.
    snapshot_files().iter().for_each(|p| std::fs::remove_file(p).unwrap());
    Sim::cached("mpc world", || {
        let mut sim = sim_at(PINNED_NOW_NS);
        sim.deploy("mpc.near", mpc("mpc_contract.wasm"));
        let init = sim.call("alice.near", "mpc.near", "init").args(mpc("mpc_init_args.json"));
        init.transact().assert_success();
        sim.set_state("mpc.near", b"STATE", Some(&mpc("mpc_state_patched.borsh")));
        let sk = String::from_utf8(mpc("near_account_secret_key")).unwrap();
        let node = "fixture-node-a.near";
        sim.create_account_with_key(node, near(10), &sk.trim().parse().unwrap());
        sim.call("alice.near", "mpc.near", "prepay_attestation_storage")
            .args(mpc("mpc_prepay_attestation_storage_args.json"))
            .deposit(Balance::from_millinear(20))
            .transact()
            .assert_success();
        sim.call(node, "mpc.near", "submit_participant_info")
            .args(mpc("mpc_submit_participant_info_args.json"))
            .gas(Gas::from_teragas(300))
            .transact()
            .assert_success();
        sim
    });
    let [path] = &snapshot_files()[..] else { panic!("{:?}", snapshot_files()) };
    let bytes = std::fs::metadata(path).unwrap().len();
    std::fs::remove_file(path).unwrap();
    assert!(bytes < 5_000_000, "{bytes} bytes");
}
