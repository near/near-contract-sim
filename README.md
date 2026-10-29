<div align="center">

# near-contract-sim

⚡ A fast, in-process NEAR chain for Rust tests.

[![CI](https://github.com/near/near-contract-sim/actions/workflows/ci.yml/badge.svg?branch=main)](https://github.com/near/near-contract-sim/actions/workflows/ci.yml)
![Rust 1.95](https://img.shields.io/badge/rust-1.95-orange)

[Quickstart](skills/near-contract-sim/references/quickstart.md) · [Migrating](skills/near-contract-sim/references/migrating.md) · [Fidelity](skills/near-contract-sim/references/fidelity.md) · [Audits (experimental)](skills/near-contract-sim/references/audit.md)

</div>

near-contract-sim is for testing NEAR smart contracts from Rust. It runs
nearcore's own runtime inside `#[test]`, so a test deploys the
contract's wasm, sends signed transactions and follows every receipt
they spawn, in milliseconds, and cross-contract bugs show up in `cargo
test`, not on a sandbox. One tool replaces a contract's near-sdk unit
tests (`testing_env!`) and its near-workspaces sandbox tests.

- **Fast:** a call takes milliseconds, with no node to start
- **Self-contained:** no node, no network, no sandbox binary
- **Cross-contract calls:** every promise, callback, refund and yield
  runs, nothing mocked
- **Real gas:** matches a `neard` sandbox, receipt by receipt
  ([fidelity](skills/near-contract-sim/references/fidelity.md))
- **Your clock:** set the time and height, the past included; yields time
  out on cue
- **Who paid whom:** each call's own NEAR and token moves, exact while
  other calls run in the same blocks
- **Stubs and faults:** make a callee answer as the test says, fail any
  spawned call, or land a receipt late
- **State in and out:** typed contract state, and state records as
  `sandbox_patch_state` takes them

Everyday tests are the core, and most users need only them. The audit
layer on top, explore, is experimental: its API changes more often, and
the [CHANGELOG](CHANGELOG.md) lists each break.

## 🔗 Test cross-contract calls

A unit test can call one method, but it can't follow the promise that
method returns. Here every receipt a call spawns runs, in the blocks a
single-shard chain runs it, with the gas a node charges. near-sdk's
cross-contract example computes a factorial by calling itself
(`tests/docs.rs`):

```rust
let out =
    sim.call("alice.near", "factorial.near", "factorial").args_json(json!({"n": 3})).transact();
assert_eq!(out.json::<u32>(), 6);
// factorial(3) calls factorial(2), which calls factorial(1); callbacks multiply on the way back.
let callbacks = out.receipts.iter().filter(|r| r.method_names == ["factorial_mult"]);
assert!(callbacks.count() == 2 && out.receipts.iter().all(|r| r.is_success()), "{out}");
```

When an assertion fails, it prints the receipt tree and a gas table, so
you see which hop broke and why. Here, a call that ran out of gas:

```text
tx 29HYHB… signed by alice.near at #3 (conversion 0.31 Tgas) -> FAILURE: out of gas (Exceeded the prepaid gas)
└─ #4 status.near::set_status  attached 0.20 Tgas, burnt 1.09 Tgas (0.20 + 0.89 exec fees)  FAILED: out of gas (Exceeded the prepaid gas)
      log: alice.near set_status with message hi
```

## ⏰ Control the clock

Contracts that read `env::block_timestamp()` are hard to test on a node,
since its clock is the wall clock. Here the clock is a value you set, and
blocks happen when you ask for them. `advance_to` skips ahead cheaply, so
a week of 1 s blocks costs little, and a yield that nobody resumes times
out on cue (`tests/docs.rs`):

```rust
sim.set_timestamp_ns(1_577_836_800_000_000_000); // 2020-01-01, or any time, the past included
sim.advance_to(sim.height() + 7 * 86_400); // a week of 1 s blocks: 604,800 blocks
assert_eq!(sim.timestamp_ns(), 1_577_836_800_000_000_000 + 7 * 86_400 * 1_000_000_000);
sim.call("alice.near", "probe.near", "start_yield").transact().assert_success();
sim.advance_to(sim.yields()[0].expires_at); // the yield times out
sim.settle();
assert_eq!(sim.view("probe.near", "last", ()).json::<String>(), "timeout");
```

## 💥 Fail the branch nobody tests

A callback runs whether or not the call it waited on succeeded, and must
refund or roll back when it didn't. That branch is where funds go
missing, and it rarely runs in a test, since every callee succeeds. Here
any spawned call can fail, run short of gas or land late, and
`out.effects()` reads only that transaction's moves, so it stays exact
while other calls overlap it. A vault withdraw whose payout fails
(`tests/vault.rs`):

```rust
let w = withdraw(&mut sim, "alice.near", 40)
    .inject(Receipts::callback("on_withdraw"), Fault::PromiseFailed)
    .transact();
assert_eq!(w.u128(), 0);
assert_eq!(w.effects().token_net(TOKEN, "alice.near"), Ok(Delta::ZERO));
assert_eq!(w.effects().token_net(VAULT, "alice.near"), Ok(Delta::ZERO), "{w}");
// PromiseFailed at the callback starves the call it waits on: that call shows OutOfGas.
assert_eq!(w.expect_receipt("ft_transfer").fault, Some(Fault::OutOfGas));
```

The same branch without a fault: `sim.deploy_stub(TOKEN,
Stub::new().on("ft_transfer", Reply::panic("the token is paused")))`
replaces the token with one that refuses.

## ⛽ Know your gas margin

A callback with a fixed gas budget can work at launch and run out of gas
once the state it touches grows, which can leave a contract stuck halfway
through an operation. Since the gas here is nearcore's, a test can
measure the margin. A receipt's headroom is the attached gas it left
unused:

```rust
out.assert_headroom("factorial_mult", Gas::from_teragas(1));
```

## 🚀 Install and a first test

near-contract-sim isn't on crates.io yet: it builds on nearcore's
runtime crates, which aren't published there, so it is a git
dependency. Pin it to a commit with `rev`; releases will be git tags.
It pins nearcore 2.14.0 (protocol 88) and re-exports nearcore's types,
in `near_contract_sim::nearcore`: a nearcore dependency of your own
would be a second copy of every type. A test crate that also depends on
near-sdk needs near-sdk's `non-contract-usage` feature.

The test host needs:

- Rust 1.95.0 with the wasm32 target:
  `rustup toolchain install 1.95.0 --target wasm32-unknown-unknown`;
- [cargo-near](https://github.com/near/cargo-near) 0.22, which the
  test runs to build the contract:
  `curl --proto '=https' --tlsv1.2 -LsSf https://github.com/near/cargo-near/releases/download/cargo-near-v0.22.0/cargo-near-installer.sh | sh`;
- a C/C++ compiler, libclang, perl and make for nearcore's C
  dependencies: on Ubuntu, `sudo apt-get install build-essential clang
  libclang-dev perl`; on macOS, `xcode-select --install`.

The first build clones nearcore (several hundred MB), compiles about 500
crates, takes 10 to 30 minutes and uses several GB of disk; later builds
reuse it. The tests live in a crate of their own beside the contract's
(if the contract's directory is a Cargo workspace, add the test crate to
its `members` or `exclude`). For the auction `cargo near new auction`
generates:

```text
auction/                  the contract's crate, untouched
auction-tests/
├── .cargo/config.toml    the test-thread limit, below
├── Cargo.toml            a [package] and the block below
├── rust-toolchain.toml   the toolchain, below
├── src/lib.rs            empty: cargo refuses a package with only tests
└── tests/sim.rs
```

`auction-tests/Cargo.toml`, pinned to a commit of this repo (any later one works too):

```toml
[package]
name = "auction-tests"
version = "0.0.0"
edition = "2024"
publish = false

[dev-dependencies]
near-contract-sim = { git = "https://github.com/near/near-contract-sim", rev = "c40ce151f8b00dcc114bcd34bf58534593fef24a" }
cargo-near-build = "0.11"
serde_json = "1"
serde = { version = "1", features = ["derive"] }
borsh = { version = "1", features = ["derive"] }

[profile.dev.package."*"]
opt-level = 2
debug = false
```

`auction-tests/rust-toolchain.toml`. Inside `cargo test` the contract's
build uses this toolchain, not the one the contract pins, so it needs
the wasm32 target:

```toml
[toolchain]
channel = "1.95.0"
targets = ["wasm32-unknown-unknown"]
```

`auction-tests/.cargo/config.toml`. nearcore's VM allows 1000 wasm
instances per process, so cap the test threads:

```toml
[env]
RUST_TEST_THREADS = "4"
```

`tests/sim.rs` builds the wasm with cargo-near's library, as
near-workspaces' `compile_project` does, so it never tests a stale one:

```rust
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
```

In `auction-tests/`, write the lockfile once and commit it, so a rerun
builds the same nearcore; then run the test:

```sh
cargo generate-lockfile
cargo test --locked --test sim
```

The test deploys the wasm on a fresh chain, sends each call as a signed
transaction, and checks the bid and Alice's payment, gas included. Its
first run also builds the contract, which takes a little while; an
unchanged contract then costs about half a second.
`sim.findings().assert_none()` adds the built-in checks: no failure that
nobody learned of, no failed callback, no NEAR created or destroyed. The
[quickstart](skills/near-contract-sim/references/quickstart.md) goes on
from here, on a token and a vault: a `Fixture` built once, the
prelude's names, typed helpers (`serde` and `borsh` above are for its
views and state), faults and the clock.

## 🔍 Audit a contract (explore, experimental)

Tests written by hand cover the orders someone thought of; NEAR's hard
bugs are in the others. For an audit, write the contract's value-moving
operations as an enum and the properties they must keep, or declare its
books. The harness scans each flow's faults and edges, measures what each
receipt uses as inputs grow, and searches random plans with overlapping
calls, block gaps, explored gas and faults. It shrinks each finding and
saves it as a replay file (`tests/vault.rs`):

```rust
#[test]
fn audit_search() {
    let report = vault().search(plans(ops()).len(1..=6)).run();
    report.assert_reached("vault.near::on_withdraw");
    only_forced(&report);
}
```

A clean search means no property failed in the cases tried. It is never
proof that the contract is correct.
[audit.md](skills/near-contract-sim/references/audit.md) is the method.
A search draws its cases with proptest: add `proptest = "1"` to the
`[dev-dependencies]`.

## Why it matters

near-sdk's `MockedBlockchain` is a second model of the chain, and keeping
it in step with nearcore is recurring work. This crate calls nearcore's
code, so there is one model, and moving to a new nearcore is a version
bump. Because a test runs in milliseconds, checks on callbacks, gas and
timeouts can run on every save instead of at the end of the pipeline.
Runs are deterministic and each `Sim` is independent, so tests run in
parallel.

## Limits

One shard, a static gas price, and ids, block hashes and timestamps that
aren't a real chain's. Epochs have one validator and flat rewards.
[fidelity.md](skills/near-contract-sim/references/fidelity.md) lists
every difference.

## Versions

Releases will be git tags; until the first, pin a `rev`. While the
version is 0.x, a breaking change or a new nearcore pin moves the minor
version; additions and fixes move the patch. nearcore's types are re-exported as they are: a nearcore bump that
changes one changes this crate's API with it.
[CHANGELOG.md](CHANGELOG.md) lists what each release changes.

## Where to go next

- **Everyday tests:** the
  [quickstart](skills/near-contract-sim/references/quickstart.md) (the
  test crate, a fixture built once, typed helpers, a callback with a
  fault, the clock, a build that can't go stale), then
  [migrating.md](skills/near-contract-sim/references/migrating.md) to port
  `testing_env!` and near-workspaces tests idiom by idiom.
- **Audits (experimental):**
  [audit.md](skills/near-contract-sim/references/audit.md), the method,
  with a first harness in one file.
- **Everything else:** the guides are an agent skill,
  [skills/near-contract-sim](skills/near-contract-sim/SKILL.md), whose
  table maps each concept to its page. Install it with
  `npx skills add near/near-contract-sim --skill near-contract-sim`, or
  in Claude Code, `/plugin marketplace add near/near-contract-sim` then
  `/plugin install near-contract-sim@near-contract-sim`.
- **The API:** rustdoc. From your crate, run
  `cargo +1.95.0 doc -p near-contract-sim --no-deps --examples --open`
  (without `--examples`, cargo fails to document a dev-dependency); it
  writes `target/doc/near_contract_sim/index.html`. In a clone of
  this repo, `cargo run --example tour` walks through the API, and
  [CONTRIBUTING.md](CONTRIBUTING.md) is for contributors.
