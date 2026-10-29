use cargo_near_build::BuildOpts;
use near_contract_sim::{Balance, Sim, SimTransport};
use near_kit::NearToken;
use serde_json::{Value, json};

#[tokio::test(start_paused = true)]
async fn bid_through_near_kit() {
    let mut sim = Sim::new();
    sim.create_account("alice.near", Balance::from_near(100));
    sim.create_account("bob.near", Balance::from_near(100));
    // tests/sim.rs built it; cargo-near finds it up to date.
    let manifest = concat!(env!("CARGO_MANIFEST_DIR"), "/../auction/Cargo.toml");
    let opts = BuildOpts::builder().manifest_path(manifest).no_locked(true).build();
    let wasm = cargo_near_build::build_with_cli(opts).expect("cargo near build");
    sim.deploy("auction.near", std::fs::read(wasm).unwrap());
    let end_time = sim.timestamp_ns() + 60_000_000_000;
    sim.call("auction.near", "auction.near", "init")
        .args_json(json!({"end_time": end_time.to_string(), "auctioneer": "bob.near"}))
        .transact()
        .assert_success();
    let node = SimTransport::new(sim);
    let alice = node.near("alice.near");
    let bid = alice.call("auction.near", "bid").deposit(NearToken::from_near(1)).await.unwrap();
    assert!(bid.is_success(), "{bid:?}");
    let highest: Value = alice.view("auction.near", "get_highest_bid").await.unwrap();
    assert_eq!(highest["bidder"], "alice.near");
    node.with(|sim| sim.findings()).assert_none();
}
