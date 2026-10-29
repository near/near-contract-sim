# Working on near-contract-sim

A Rust crate: an in-process, single-shard NEAR chain for contract tests,
run by nearcore 2.14.0's own `Runtime::apply` over an in-memory trie. Its
users are mostly coding agents writing contract tests, through the skill
in `skills/near-contract-sim/`.

Setup (Rust 1.95.0 with wasm32, cargo-near 0.22, libclang, Python 3) is in
[CONTRIBUTING.md](CONTRIBUTING.md).

## Commands

```sh
cargo fmt --check && rustfmt --check --edition 2024 tests/consumer/*.rs
cargo clippy --all-targets --all-features -- -D warnings
cargo nextest run --all-targets --all-features   # or cargo test --all-targets --all-features
cargo test --doc
cargo test --test <name>                         # one file of tests/
tests/consumer/run.sh                            # the docs' code in a fresh consumer crate
```

All of these must pass before a change is done. `RUSTDOCFLAGS="-D
warnings" cargo doc --no-deps --all-features` must too, since CI runs it.
The first build compiles nearcore from git and is slow; later ones are
incremental. Run a single test file while iterating and the full suite
before finishing.

## Where things live

- `src/sim.rs`: `Sim`, the chain's public face. `src/chain/`: how a block
  runs (`apply.rs`), the block log (`log.rs`), the one path that writes
  state (`state.rs`), epochs, callbacks, injected faults and holds
  (`fault.rs`, `order.rs`).
- `src/tx.rs`, `src/outcome.rs`, `src/effects.rs`, `src/findings.rs`:
  transactions, their outcomes, who paid whom, and what went wrong
  unnoticed. `src/fixture.rs`, `src/stub.rs`, `src/patch.rs`,
  `src/transport.rs` (feature `near-kit`).
- `src/explore/`: scans, searches, curves, books and replays over a
  contract's operations. **Experimental**: its API and files change
  between minor versions.
- `tests/*.rs`: integration tests, one topic per file. Helpers are in
  `tests/common/`, `tests/fuzz_common/` and `tests/mpc_fixture/`.
- `tests/res/`: checked-in wasm and `.wat` fixtures, with READMEs for the
  third-party ones. `tests/contracts/<name>/`: the sources and `build.sh`
  of this repo's own fixtures.
- `tests/api/public.txt`: the public API, one line per item.
- `README.md`, `skills/near-contract-sim/SKILL.md` and its `references/`:
  the user docs. Their code blocks are compiled (`tests/docs.rs`,
  `tests/consumer/run.sh`).
- `CHANGELOG.md`: the versioning policy and the Unreleased entry.

## Rules

- **Test first.** Reproduce a bug, or pin down new behaviour, with a
  failing test against the actual simulator, then make it pass. A change
  to scheduling, holds, faults or replay needs a regression test of that
  behaviour.
- **No backward-compatibility shims before 1.0.** Change or remove the
  API outright, update every caller, test and doc, and record it under
  **Breaking** in the CHANGELOG. Don't keep deprecated aliases or old
  signatures alongside the new ones.
- **Keep the public API list in sync.** When `pub` items change,
  regenerate the list with `NCS_API_WRITE=1 cargo test --test docs
  the_public_api_is_the_committed_list`, check its diff is what you meant,
  and add a CHANGELOG line (Breaking, Added, Fixed or Verdicts, as the
  CHANGELOG's header defines them).
- **Keep the docs compiling.** A changed example changes its copy under
  `src/`, `tests/` or `examples/` as well, which `tests/docs.rs` checks.
- **One nearcore.** Every nearcore crate comes from the same git tag
  (`Cargo.toml`); don't add one from another source.
- **No build-machine paths in fixtures.** Build a wasm with its
  `build.sh`, which remaps them; CI rejects a tracked file that holds a
  home or temporary directory path.
- **Keep the ownership boundaries.** nearcore's runtime owns execution;
  the simulator owns the schedule, the clock and receipt rewrites; a
  contract's tests own its properties. Say which one a semantic change
  affects. A search or scan checks the executions it ran: never claim a
  bounded campaign proves a contract correct, or that a forced fault or
  hold can happen on the real network.
