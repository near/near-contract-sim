# Contributing

## Setup

- **Rust 1.95.0 with the wasm32 target.** `rust-toolchain.toml` selects
  1.95.0; the fixture contracts under `tests/contracts/` pin it too and
  build to wasm32:

  ```sh
  rustup toolchain install 1.95.0 --target wasm32-unknown-unknown
  ```

- **cargo-near 0.22**, which `tests/fixture_build.rs`, the consumer check
  and the fixtures' `build.sh` scripts run:

  ```sh
  curl --proto '=https' --tlsv1.2 -LsSf https://github.com/near/cargo-near/releases/download/cargo-near-v0.22.0/cargo-near-installer.sh | sh
  ```

- **A C/C++ compiler and libclang**, for the RocksDB that nearcore's store
  links: `sudo apt-get install clang libclang-dev` on Ubuntu,
  `xcode-select --install` on macOS.
- **Python 3**, which `tests/docs.rs` runs to list the public API.
- **cargo-nextest** (optional; `cargo test` runs the same tests) and
  **Docker** (only for the differential test).

The first build compiles nearcore from git, about 580 crates, so it takes
a while.

## The checks CI runs

[.github/workflows/ci.yml](.github/workflows/ci.yml) runs these on every
pull request; run them before you push:

```sh
cargo fmt --check
rustfmt --check --edition 2024 tests/consumer/*.rs
cargo clippy --all-targets --all-features -- -D warnings
RUSTDOCFLAGS="-D warnings" cargo doc --no-deps
RUSTDOCFLAGS="-D warnings" cargo doc --no-deps --all-features
cargo nextest run --all-targets --all-features   # or cargo test --all-targets --all-features
cargo test --doc
NCS_INV_CASES=128 cargo test --test prop_invariants
tests/consumer/run.sh
```

CI also runs `tests/chain.rs` and `tests/clock.rs` without debug
assertions (see the `Test` job), the differential test against a sandbox
(`tests/differential/run.sh` starts one in Docker), and a check that no
tracked file, binaries included, holds a path from the build machine. The
suite is CPU-bound: the opt-levels in CI's `env` block cut its CPU time to
about a quarter, and work the same locally.

## When the public API changes

`tests/docs.rs`'s `the_public_api_is_the_committed_list` compares the
public API, read from rustdoc's output by `tests/api/list.py`, with
`tests/api/public.txt`. After an intended change, rewrite the list and
review its diff:

```sh
NCS_API_WRITE=1 cargo test --test docs the_public_api_is_the_committed_list
```

Then add a line to [CHANGELOG.md](CHANGELOG.md)'s Unreleased entry:
**Breaking** for a removed or changed item, **Added** for a new one. A fix
goes under **Fixed**, and a change to what a run reports for the same
code (findings, coverage, the summary) under **Verdicts**.

## Commits and pull requests

Commit messages and pull request titles follow
[Conventional Commits](https://www.conventionalcommits.org/):
`feat(explore): …`, `fix: …`, `docs: …`, `test: …`, `ci: …`,
`refactor: …`, `chore: …`. Mark a breaking API change with `!` after the
type (`feat!: …`). Pull requests are squash-merged, so the title is the
commit that lands on `main`.

## Docs and fixtures

- Every `rust` block in the README and in `skills/near-contract-sim/` is
  compiled (`tests/docs.rs`, `tests/consumer/run.sh`): a change to an
  example changes its copy under `src/`, `tests/` or `examples/` too.
- The wasm fixtures in `tests/res/` are checked in. Rebuild one with its
  contract's `tests/contracts/<name>/build.sh`, which keeps the build
  machine's paths out of the wasm, and record where a new one comes from
  next to it.

## Where things are

- "How it works", below: how a block runs.
- [.github/workflows/ci.yml](.github/workflows/ci.yml): the checks every PR
  runs.
- [.github/workflows/mutants.yml](.github/workflows/mutants.yml) and
  [.cargo/mutants.toml](.cargo/mutants.toml): mutation testing.
- [tests/differential.rs](tests/differential.rs)'s header: the differential
  test against a sandbox, and its variables.
- [tests/docs.rs](tests/docs.rs): the check that keeps the docs' code
  compiled.
- [tests/consumer/run.sh](tests/consumer/run.sh): the quickstart's code,
  then the README's test crate, toml and commands verbatim beside a
  fresh cargo-near 0.22 contract, the quickstart's fresh build and
  optional near-kit transport.
- Fixtures: their loaders in [tests/common/mod.rs](tests/common/mod.rs) and
  [tests/fuzz_common/mod.rs](tests/fuzz_common/mod.rs),
  [tests/mpc_tee.rs](tests/mpc_tee.rs)'s header and
  [tests/res/intents/README.md](tests/res/intents/README.md).

## How it works

The private modules' docs cover the block log (`src/chain/log.rs`), the
one path that writes state (`src/chain/state.rs`) and what persists across
processes (`src/cache.rs`).

### A block's life

`Sim::step` applies one chunk on one shard with nearcore's
`Runtime::apply`:

- the incoming receipts are the ones the previous block produced;
- the transactions are the queued ones a chunk producer would take, in
  submission order;
- the state is an in-memory `ShardTries`
  (`near_store::test_utils::TestTriesBuilder`);
- the fees, limits and wasm config are `RuntimeConfigStore`'s for
  `config.protocol_version`;
- the epoch height derives from the block height (`config.epoch_length`);
- the `EpochInfoProvider` is the simulator's: one validator, the same in
  every epoch;
- the first block of an epoch also takes a `ValidatorAccountsUpdate`:
  the stake each account keeps locked, and its reward, from the
  simulator's own bookkeeping of nearcore's epoch manager
  (`src/chain/epoch.rs`). The runtime moves the rest of the stake to the
  balance, and the conservation ledger counts the rewards.

The block's outgoing receipts are the next block's incoming ones, which is
what a single-shard chain does (less the ones a hold keeps back, below), and its stake proposals count in the next
block's epoch, as the next block's header carries them on a node. Its
outcomes go into the block log, which `Sim::outcome`, `Sim::findings` and the
snapshots read.

### Ids

As on a chain, a transaction's receipt id comes from the transaction hash
and the block height, and every later receipt, data and yield id from its
parent's id and the height. A transaction signs the last block's hash,
which derives from `config.seed` and the number of blocks produced. So ids
stay unique when the height moves backwards, and they differ from a real
chain's for the same inputs.

### nearcore's code and the simulator's

nearcore's: transaction validation and conversion, receipt execution, fees,
gas and refunds, the delayed-receipt queue, congestion control, the
bandwidth scheduler, the state-witness limits, the yield timeout queue,
stake locking and its return (`update_validator_accounts`) and views
(`TrieViewer`). The simulator's: the clock, the epochs and what each
boundary returns and pays, which transactions go in a chunk, the block log
and the outcomes read from it, the conservation ledger, direct state
writes (`create_account`, `set_balance`, `set_state`,
`apply_state_records`), the caches and snapshots, and `SimTransport`'s
glue around nearcore's RPC types, copied from nearcore's server crate. Two
more rewrite receipts between blocks. The callbacks `Tx::promise_results`
delivers: on its way out of the conversion block, the simulator rewrites a
transaction's receipt into the one `promise_then` makes and sends a data
receipt per result behind it (`src/chain/callback.rs`). And injections
(`Tx::inject`): a receipt the transaction spawned gets its gas, or the
calls and results its callback waits on, rewritten as it leaves the block
that created it, and nearcore runs what it is given
(`src/chain/fault.rs`).
Holds (`Fault::Hold`, injected the same way) are picked there too: a
receipt a transaction spawned leaves the block
that created it as usual, but goes into a queue instead of the next
block's incoming receipts. After its blocks have passed, it goes out
oldest first, ahead of the receipts that block made
(`src/chain/order.rs`). The chain isn't settled while the queue holds
one.

### Exploration

`explore` runs on the same `Sim`. A case clones the fixture, lets each op
submit, produces the blocks its plan says, and judges checks at block
boundaries (`src/explore/run.rs`). A sampled fault is resolved against a
run of the plan without it, then the plan reruns with the fault at one
receipt, as `Receipts::index(n)`, so a case reruns exactly. A replay file
records the plan, the finding and the fixture's code hashes and state
root (`src/explore/store.rs`).

The runtime, the schedule and application properties have separate
owners. nearcore supplies execution; the simulator supplies the
single-shard schedule, the clock and receipt rewrites; the test author
supplies the properties. A search checks concrete executions. It neither
explores every schedule nor establishes that a forced fault or hold can
arise on the real network.

## License

near-contract-sim is licensed under either of the [Apache License, Version
2.0](LICENSE-APACHE) or the [MIT license](LICENSE-MIT), at your option.

Unless you explicitly state otherwise, any contribution intentionally
submitted for inclusion in the work by you, as defined in the Apache-2.0
license, shall be dual licensed as above, without any additional terms or
conditions.
