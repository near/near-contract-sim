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

## 0.2.1 (2026-11-11)

### Fixed

- Docs: a failed callback's deposit is refunded to the contract (`NearKind::DepositRefund`, transactions.md); sim.md lists every `NEAR_SIM_*` variable ([#28](https://github.com/near/near-contract-sim/pull/28))
- explore: Measure a batch receipt's gas margin, judge a resume by its own excuses, no axis on a wrapped call's written args ([#29](https://github.com/near/near-contract-sim/pull/29))

## 0.2.0 (2026-11-11)

### Breaking

- explore: Drop the replay formats no release wrote ([#7](https://github.com/near/near-contract-sim/pull/7))
- explore: Judge a resumed audit with its saved part in is_clean, todo, done and found_at, count skipped edges as incomplete ([#17](https://github.com/near/near-contract-sim/pull/17))
- explore: Send a saved call's args as written, widen Inject.tx to u32 ([#20](https://github.com/near/near-contract-sim/pull/20))

### Added

- explore: Run per-step edges on new enumerate steps placed at rest ([#5](https://github.com/near/near-contract-sim/pull/5))
- explore: `NEAR_SIM_CASE_TIMEOUT` sets a case's wall-clock timeout in whole seconds (0: none; unset: a minute) ([#19](https://github.com/near/near-contract-sim/pull/19))

### Fixed

- Accept nearcore's newest actions, dedupe report lists, correct stale docs ([#6](https://github.com/near/near-contract-sim/pull/6))
- Find the cache in a target dir without CACHEDIR.TAG, and delete week-old snapshots ([#11](https://github.com/near/near-contract-sim/pull/11))
- explore: Judge every claim and epoch-crossing overlap, count incomplete cases once, cap the summary at 60 lines ([#14](https://github.com/near/near-contract-sim/pull/14))
- explore: Cap margin ascents at the ladder's top, count a curve's first rung, keep digit map keys as axes ([#13](https://github.com/near/near-contract-sim/pull/13))
- explore: Shrink rows a search shows while shrinking, size raw args, count escaped bytes ([#15](https://github.com/near/near-contract-sim/pull/15))
- Delivered deposits, per-build view coverage, refused retries, forced windows, u128 supply ([#16](https://github.com/near/near-contract-sim/pull/16))
- Name curve rows by the receipt tree's sites, assert_reached counts a resumed audit's saved part ([#18](https://github.com/near/near-contract-sim/pull/18))
- explore: A search's determinism check skips a run that hit the case timeout ([#21](https://github.com/near/near-contract-sim/pull/21))

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
