use cargo_near_build::BuildOpts;
use std::sync::OnceLock;

fn wasm() -> &'static [u8] {
    static WASM: OnceLock<Vec<u8>> = OnceLock::new();
    WASM.get_or_init(|| {
        let manifest = format!("{CONTRACT_DIR}/Cargo.toml");
        let opts = BuildOpts::builder().manifest_path(manifest).no_locked(true).build();
        let path = cargo_near_build::build_with_cli(opts).expect("cargo near build");
        std::fs::read(path).unwrap()
    })
}

/// The contract's crate, beside this test crate.
const CONTRACT_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../auction");

#[test]
fn a_fresh_build() {
    use near_contract_sim::prelude::*;
    let mut sim = Sim::new();
    sim.deploy("auction.near", wasm());
    let code_hash = sim.account("auction.near").unwrap().code_hash;
    assert_eq!(code_hash, CryptoHash::hash_bytes(wasm()));
    let end_time = (sim.timestamp_ns() + 60_000_000_000).to_string();
    sim.call("auction.near", "auction.near", "init")
        .args_json(serde_json::json!({ "end_time": end_time, "auctioneer": "bob.near" }))
        .transact()
        .assert_success();
}
