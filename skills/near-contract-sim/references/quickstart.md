# Quickstart: everyday tests

From a near-sdk contract's source to tests that call it, assert what it
did, fail a callee and move the clock. For a prebuilt `.wasm` or a
`.wat`, skip the build: [sim.md](sim.md) deploys one.

## Set up

Put the tests in a crate of their own beside the contract's, which
builds for wasm32 with its own toolchain. Every page uses this layout:

```text
contract/                 the contract's crate, untouched
contract-tests/
├── .cargo/config.toml    the test-thread limit
├── Cargo.toml            a [package] (edition 2024) and the block below
├── rust-toolchain.toml   channel = "1.95.0", targets = ["wasm32-unknown-unknown"]
├── src/lib.rs            empty: cargo refuses a package with only tests
└── tests/
    ├── res/<crate_name>.wasm
    └── quickstart.rs     one file per flow
```

Build the contract's wasm with its own toolchain, from `contract/`:

```sh
cargo near build non-reproducible-wasm --out-dir ../contract-tests/tests/res
```

It writes `<crate_name>.wasm` (keep the embedded ABI: no `--no-abi`),
and a `Cargo.lock` if the contract had none, worth committing.
`contract-tests/Cargo.toml` takes the simulator under
`[dev-dependencies]`, never `[dependencies]`:

```toml
[dev-dependencies]
near-contract-sim = { git = "https://github.com/near/near-contract-sim", rev = "<commit>" }
cargo-near-build = "0.11"
serde_json = "1"
serde = { version = "1", features = ["derive"] }
borsh = { version = "1", features = ["derive"] }

[profile.dev.package."*"]
opt-level = 2
debug = false
```

The crate isn't on crates.io (nearcore's runtime crates aren't
published), so it is a git dependency: replace `<commit>` with a commit
of the repo, and keep it pinned. Set the profile before the first build:
unoptimized nearcore can take a minute for one large call.

nearcore's VM has 1000 wasm instances per process: cap the test threads
in `.cargo/config.toml`, where no one has to remember it:

```toml
[env]
RUST_TEST_THREADS = "4"
```

Then, in `contract-tests/`, start the first build in the background. It
compiles about 500 crates in 10 to 30 minutes, longer than most tool
timeouts, and fills several GB; point every test crate at one
`CARGO_TARGET_DIR` to build nearcore once.

```sh
cargo test --no-run    # in the background; then cargo test, and commit Cargo.lock
```

The system libraries, imports and CI are in [sim.md](sim.md).

## The world

`tests/quickstart.rs` tests near-sdk's fungible token and a vault that
holds it, both in the repo's
[tests/res](https://github.com/near/near-contract-sim/blob/main/tests/res) to copy (the vault's source is
`tests/contracts/vault`). `withdraw(amount)` pays out with
`ft_transfer` and returns what it paid; its callback `on_withdraw` gives
the shares back if the payout failed.

```rust
use borsh::BorshDeserialize;
use near_contract_sim::Token;
use near_contract_sim::prelude::*;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::time::Duration;

const TOKEN: &str = "token.near";
const VAULT: &str = "vault.near";
const ALICE: &str = "alice.near";

/// A wasm `cargo near build` wrote to `tests/res/`.
fn wasm(name: &str) -> Vec<u8> {
    let path = format!("{}/tests/res/{name}", env!("CARGO_MANIFEST_DIR"));
    std::fs::read(&path).unwrap_or_else(|e| panic!("{path}: {e}: build the contract first"))
}

/// Built once per test binary, cloned per test.
fn world() -> Sim {
    Fixture::new()
        .account(ALICE, Balance::from_near(10))
        .contract(TOKEN, wasm("fungible_token.wasm"))
        .init("new_default_meta")
        .args_json(json!({ "owner_id": TOKEN, "total_supply": "1000" }))
        .contract(VAULT, wasm("vault.wasm"))
        .init("new")
        .args_json(json!({ "token_id": TOKEN }))
        .ft_register(TOKEN, [VAULT, ALICE])
        .ft_transfer(TOKEN, TOKEN, ALICE, 1000)
        .token(Token::nep141(TOKEN))
        .token(Token::view(VAULT, "shares_of"))
        .build()
}
```

A `Fixture` is the setup as data; each `init` is signed by the contract
itself, and `.token(..)` watches a balance view so the effects say who
gained what ([sim.md](sim.md)).

## Helpers with types

The contract's crate can't be a dependency of the tests (it builds for
wasm), so write a short helper per method you call.

```rust
/// A near-sdk `U128` view of one account.
fn u128_view(sim: &Sim, id: &str, method: &str, account_id: &str) -> u128 {
    sim.view(id, method, json!({ "account_id": account_id })).u128()
}

/// `withdraw`'s arguments, as the contract declares them.
#[derive(Serialize)]
struct Withdraw {
    amount: String,
}

/// One helper per method. Each returns the `Tx`, so a test can add a
/// fault or gas before it sends.
fn withdraw<'a>(sim: &'a mut Sim, who: &str, amount: u128) -> Tx<'a> {
    sim.call(who, VAULT, "withdraw")
        .args_json(Withdraw { amount: amount.to_string() })
        .deposit(Balance::from_yoctonear(1))
}

/// A deposit is the token's `ft_transfer_call` to the vault.
fn deposit<'a>(sim: &'a mut Sim, who: &str, amount: u128) -> Tx<'a> {
    sim.call(who, TOKEN, "ft_transfer_call")
        .args_json(json!({ "receiver_id": VAULT, "amount": amount.to_string(), "msg": "" }))
        .deposit(Balance::from_yoctonear(1))
}
```

## Call and assert

`transact()` runs every receipt the call spawns. Assert on what the call
did (its effects and the views), not only its status
([outcomes-and-effects.md](outcomes-and-effects.md)).

```rust
#[test]
fn deposit_then_withdraw() {
    let mut sim = world();
    let d = deposit(&mut sim, ALICE, 100).transact();
    d.assert_success();
    assert_eq!(d.effects().token_net(TOKEN, ALICE), Ok(Delta::loss(100)));
    assert_eq!(d.effects().token_net(VAULT, ALICE), Ok(Delta::gain(100))); // shares

    let w = withdraw(&mut sim, ALICE, 40).transact();
    assert_eq!(w.u128(), 40, "{w}");
    assert_eq!(u128_view(&sim, TOKEN, "ft_balance_of", ALICE), 940);
    assert_eq!(u128_view(&sim, VAULT, "shares_of", ALICE), 60);
    sim.findings().assert_none();
}
```

`sim.findings().assert_none()` adds the built-in checks: no failure
nobody learned of, no failed callback, no NEAR created or destroyed.

## A callback with a fault

A unit test can't reach the branch where the payout fails. Force it:
`PromiseFailed` at the callback fails the call it waits on
([faults.md](faults.md)), or deploy a stub ([stubs.md](stubs.md)).

```rust
#[test]
fn a_failed_payout_gives_the_shares_back() {
    let mut sim = world();
    deposit(&mut sim, ALICE, 100).transact().assert_success();
    let w = withdraw(&mut sim, ALICE, 40)
        .inject(Receipts::callback("on_withdraw"), Fault::PromiseFailed)
        .transact();
    assert_eq!(w.u128(), 0, "{w}");
    assert_eq!(w.effects().token_net(TOKEN, ALICE), Ok(Delta::ZERO));
    assert_eq!(w.effects().token_net(VAULT, ALICE), Ok(Delta::ZERO));
    sim.findings().assert_none();
}
```

## The clock

Blocks happen when the test asks, a second apart. `advance_by` lets
time pass; `set_timestamp_ns` sets what the next call sees ([blocks-and-clock.md](blocks-and-clock.md)).

```rust
#[test]
fn rewards_once_a_day() {
    let mut sim = world();
    let claim = |sim: &mut Sim| sim.call(ALICE, VAULT, "claim_rewards").transact();
    claim(&mut sim).assert_success();
    assert!(claim(&mut sim).is_failure());
    sim.advance_by(Duration::from_secs(24 * 60 * 60));
    claim(&mut sim).assert_success();
    assert_eq!(u128_view(&sim, VAULT, "shares_of", ALICE), 20); // two claims of 10 shares
}
```

## Typed views and contract state

Decode a struct view into a type with only the fields you read. Read
state with `contract_state`: the root struct mirrored, fields in order,
borsh-decoded.
A `LookupMap` is its prefix; [sim.md](sim.md) has the other collections.

```rust
#[derive(Deserialize)]
struct Metadata {
    symbol: String,
    decimals: u8,
}

#[derive(BorshDeserialize)]
struct VaultState {
    token_id: String,
    _shares: Vec<u8>,
    total_shares: u128,
    _total_rewards: u128,
    _last_claim: Vec<u8>,
}

#[test]
fn typed_views_and_state() {
    let mut sim = world();
    deposit(&mut sim, ALICE, 100).transact().assert_success();
    let meta: Metadata = sim.view(TOKEN, "ft_metadata", ()).json();
    assert_eq!((meta.symbol.as_str(), meta.decimals), ("EXAMPLE", 24));
    let state: VaultState = sim.contract_state(VAULT);
    assert_eq!((state.token_id.as_str(), state.total_shares), (TOKEN, 100));
}
```

## Never test a stale wasm

A wasm in `tests/res/` is whatever was last built. Build it in the test
instead, once per test binary, with cargo-near's library (what
near-workspaces' `compile_project` calls); cargo rebuilds only what
changed, so an unchanged contract costs about half a second. Define
`const CONTRACT_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../contract");`:

```rust
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
```

The library runs the cargo-near CLI, which must be installed. Inside
`cargo test`, rustup has already picked the test's toolchain, so the
build uses it rather than the contract's pin: add
`targets = ["wasm32-unknown-unknown"]` to the test crate's
`rust-toolchain.toml`. For a wasm you didn't build, assert its code
hash, `CryptoHash::hash_bytes(&wasm)`.
