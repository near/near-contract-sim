use cargo_near_build::BuildOpts;
use near_contract_sim::{Balance, Sim};
use serde_json::{Value, json};

/// The auction's wasm, built from `../auction` by cargo-near, so the test
/// never runs a stale one.
fn auction_wasm() -> Vec<u8> {
    let manifest = concat!(env!("CARGO_MANIFEST_DIR"), "/../auction/Cargo.toml");
    let opts = BuildOpts::builder().manifest_path(manifest).no_locked(true).build();
    let wasm = cargo_near_build::build_with_cli(opts).expect("cargo near build");
    std::fs::read(wasm).unwrap()
}

#[test]
fn bid() {
    let mut sim = Sim::new();
    let alice = sim.create_account("alice.near", Balance::from_near(100));
    sim.create_account("bob.near", Balance::from_near(100));
    sim.deploy("auction.near", auction_wasm());
    let end_time = sim.timestamp_ns() + 60_000_000_000; // a minute on
    sim.call("auction.near", "auction.near", "init")
        .args_json(json!({"end_time": end_time.to_string(), "auctioneer": "bob.near"}))
        .transact()
        .assert_success();

    let before = sim.balance(&alice);
    let deposit = Balance::from_near(1);
    let bid = sim.call(&alice, "auction.near", "bid").deposit(deposit).transact();
    bid.assert_success();
    assert!(bid.receipts.iter().all(|r| r.is_success()), "{bid}");

    let highest: Value = sim.view("auction.near", "get_highest_bid", ()).json();
    assert_eq!(highest["bidder"], "alice.near");
    assert_eq!(highest["bid"], deposit.as_yoctonear().to_string());
    assert_eq!(
        before.as_yoctonear() - sim.balance(&alice).as_yoctonear(),
        deposit.as_yoctonear() + bid.tokens_burnt().as_yoctonear()
    );
    sim.findings().assert_none();
}
