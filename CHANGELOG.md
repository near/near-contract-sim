# Changelog

While the version is 0.x, a breaking change or a new nearcore pin moves
the minor version (0.1 to 0.2); additions and fixes move the patch. A
change is breaking when a consumer's test stops compiling, or a saved
replay or report file stops loading as it did. `tests/api/public.txt`
lists the public API, and `tests/docs.rs` fails when it drifts.

release-plz writes each entry from the Conventional Commit titles merged
since the last tag: **Breaking** for a `!` title, **Added** for `feat`,
**Fixed** for `fix`. A change to what a run reports for the same code
(findings, coverage, to-do items, the summary's lines) says so in its
title. `explore` is experimental and changes more often than the rest;
its changes follow the same rules.

## 0.1.1 (2026-10-29)

### Added

- explore: Run per-step edges on new enumerate steps placed at rest ([#5](https://github.com/near/near-contract-sim/pull/5))

## 0.1.0 (2026-10-28)

The first release. It runs NEAR contracts on nearcore
2.14.0's runtime (protocol 88) inside `cargo test`.

### Added

- `Sim`, a single-shard chain in process: accounts and keys, deploys,
  signed transactions with every receipt, callback, refund and yield
  they spawn, views, contract state read and patched, and clones as
  cheap snapshots. `Fixture` builds a setup once per test process.
- `Tx`: function calls and any action batch, gas, deposits, other keys,
  function-call keys, subaccounts, meta transactions, global contracts,
  a callback on picked promise results (`promise_results`) and a call
  from another predecessor (`predecessor`).
- `TxOutcome`: the status, return value, logs, events and each
  receipt's outcome; a receipt tree and gas table on failure, headed by
  the first failed receipt; `assert_panic_contains` and gas headroom
  asserts. `Effects`: each transaction's own NEAR and token moves.
- `sim.findings()`: built-in checks for unhandled failures, failed
  callbacks and NEAR conservation.
- The clock: a pinned time and height for the next call, advancing
  blocks, time and epochs, yields and their timeouts.
- Faults (`Tx::inject`) and stubs (`Stub`, `Reply`) for callees that
  fail, starve, answer oddly or land late.
- With the `near-kit` feature, `SimTransport` answers near-kit's
  JSON-RPC in process.
- `explore`, experimental: a harness of ops and checks or books, with
  scans of faults and edges, curves of resource use as inputs grow,
  searches of overlapping calls, enumeration, shrunk findings saved as
  replays, and diffs between builds.
- An agent skill (`skills/near-contract-sim`) holding the guides.
