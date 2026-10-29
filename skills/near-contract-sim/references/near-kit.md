# near-kit on the sim

Use this page to run client code written against near-kit (a relayer, a
node's sidecar, a deploy script) on the simulator, or one scenario on
both the simulator and a sandbox.

With the `near-kit` feature, `near_contract_sim::SimTransport` answers
[near-kit](https://crates.io/crates/near-kit)'s JSON-RPC requests in
process, from a `Sim`, with nearcore's own views and errors. Code written
against near-kit's `Near` runs on it unchanged. Your near-kit must be at
least 0.18.3, the first whose secp256k1 signatures nodes accept.
tokio's `test-util` gives tests a paused clock:

```toml
[dev-dependencies]
near-contract-sim = { git = "https://github.com/near/near-contract-sim", rev = "<commit>", features = ["near-kit"] }
near-kit = "0.18.3"
tokio = { version = "1", features = ["macros", "rt", "test-util"] }
```

From `tests/transport.rs`:

```rust
#[tokio::test(start_paused = true)] // back-offs and timed-out waits take no wall time
async fn near_kit_runs_on_the_sim() {
    let node = SimTransport::new(Sim::cached(probe_wasm(), probe_sim)); // any Sim
    let alice = node.near("alice.near"); // a near_kit::Near signing with the sim's key

    let t = 1_800_000_000_000_000_000; // 2027-01-15, in ns
    node.with(|sim| sim.set_timestamp_ns(t)); // sim-only controls go through the handle
    let seen: u64 = alice.view("probe.near", "ts").await.unwrap(); // near-kit, as production code calls it
    assert_eq!(seen, t);
    let out = alice.call("probe.near", "nope").await.unwrap();
    assert!(out.is_failure(), "{out:?}"); // near-kit answers a failed call with Ok(outcome)
    let hash = near_contract_sim::CryptoHash(out.transaction.hash.into()); // nearcore's hash type
    println!("{}", node.with(|sim| sim.outcome(hash))); // the sim's receipt tree of the call
    node.with(|sim| sim.findings()).assert_none(); // every call waited for FINAL, so conservation runs
}
```

`SimTransport`'s rustdoc has the rest: when blocks happen, how to branch
a node, and signing as an account the `Sim` holds no key for. To sign a
NEP-413 message, see [transactions.md](transactions.md).

## Types and resends

near-kit's `AccountId`, `PublicKey` and `CryptoHash` are other types than
nearcore's. Convert ids and keys through their text form, and a hash
through its bytes, as above. A transaction sent again is the one already
sent, as with nearcore's `send_tx`, so near-kit's resend after
`ShardCongested` and re-sign after `InvalidNonce` work as against a node.

## What it answers differently

- `sandbox_fast_forward(n)` runs the work in flight block by block,
  firing each yield timeout at its own height, where a sandbox runs it
  all in the landing block.
- Every block has one chunk; signatures, epoch ids and roots are
  defaults, and `status` lists no validators.
- `validators`, `block_effects`, `genesis_config` and
  `maintenance_windows` answer `METHOD_NOT_FOUND`.

## One scenario, on the sim and on a sandbox

A scenario written against a `Near` runs on both. On the sim it takes
`node.near(signer)`, or a client on the node's URL; on a sandbox, the
same builder with the sandbox's URL and no `.transport(..)`
(`tests/transport.rs`):

```rust
let carol = Near::custom(node.url(), "localnet").transport(node.clone()).signer(signer).build();
```

To move the sim's state to a sandbox, patch in `state_records(ids)`
with `sandbox_patch_state`; `tests/differential/sandbox.rs` runs whole
scenarios this way.
