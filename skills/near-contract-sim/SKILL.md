---
name: near-contract-sim
description: Writes and runs tests for NEAR smart contracts with near-contract-sim, an in-process simulator that runs nearcore's runtime inside cargo test. Covers everyday call-and-assert tests that replace near-sdk unit tests (testing_env!) and near-workspaces sandbox tests, cross-contract calls, callbacks and refunds, who paid whom per call, the clock, epochs and yields, stub contracts, injected faults and late receipts, gas, and audit harnesses that scan faults and edges, measure resources as inputs grow, and search overlapping calls, with shrunk, replayable findings. Use when the user asks to write, add or improve tests for a NEAR contract (a near-sdk crate or a wasm), to port testing_env! or near-workspaces tests, to test callbacks, yields or cross-contract flows, to check gas margins, to property-test or fuzz a contract, to check token or accounting invariants, to audit or hunt for bugs in a contract or deployed wasm, or mentions near-contract-sim, near-workspaces or sandbox tests.
---

# Testing a NEAR contract with near-contract-sim

near-contract-sim runs a contract on nearcore's own runtime inside
`#[test]`: real signed transactions, every receipt, callback and refund,
a node's gas, and a clock the test sets. It replaces `testing_env!` unit
tests and near-workspaces sandbox tests.

## Everyday tests

1. Read [references/quickstart.md](references/quickstart.md) first. It
   builds a near-sdk crate; for a prebuilt `.wasm` or a `.wat`, see
   sim.md, "A contract without source".
2. Porting existing tests: [references/migrating.md](references/migrating.md)
   maps each `testing_env!` and near-workspaces idiom.
3. Read a concept page when a test needs it, not upfront.

| concept | what it is | page |
|---|---|---|
| `Sim`, `Fixture` | the chain, setup built once, accounts, contract state, the toolchain and CI | [references/sim.md](references/sim.md) |
| a contract without source | deploying a `.wat` or a prebuilt wasm, its exports, raw args and returns, asserting on storage | [references/sim.md](references/sim.md) |
| `Tx` | a signed transaction: calls, gas, keys, relayers, signed payloads, global contracts, a callback on picked results | [references/transactions.md](references/transactions.md) |
| `TxOutcome`, `Effects` | what a call did, receipt by receipt; logs, gas, who paid whom; the built-in findings | [references/outcomes-and-effects.md](references/outcomes-and-effects.md) |
| `Fault`, `Receipts` | a callee that fails, a callback short of gas, a late receipt | [references/faults.md](references/faults.md) |
| `Stub` | a contract that answers as the test says; the calls an account got | [references/stubs.md](references/stubs.md) |
| blocks and the clock | many calls in one block, `settle`, `advance_to`, yields, epochs | [references/blocks-and-clock.md](references/blocks-and-clock.md) |

Also: [references/mpc.md](references/mpc.md) (yield-based contracts),
[references/fidelity.md](references/fidelity.md) (what matches a node),
[references/near-kit.md](references/near-kit.md) (near-kit client code).

Rules:

- `use near_contract_sim::prelude::*` names the everyday types. Import
  `Token` on its own. An audit harness uses `explore::prelude::*`.
- `transact()` doesn't panic on a failed call: assert on its outcome,
  and put `"{out}"` in assert messages for the receipt tree. An
  assert's panic opens with the first receipt that failed; the tree and
  gas table are cut to about 100 lines, and `{out:#}` prints all of it.
- A near-sdk `U128` is a JSON string: send `n.to_string()`, and read a
  view's or a call's with `.u128()`.
- End a test with `sim.findings().assert_none()`, or assert the findings
  you expect.
- Build the wasm in the test, or assert its hash, so a stale build can't
  pass (quickstart).
- Cap test threads at 4 in `.cargo/config.toml` (quickstart), and keep
  build parallelism modest.

## Audits

For an audit, you map the value flows and declare the books or
properties; the library tries faults, edges, growing inputs,
interleavings and gas, shrinks each finding and saves its replay. A
clean run is no proof. This layer, `explore`, is experimental.

Read in this order:

1. [references/audit.md](references/audit.md): the checklist, with a
   first harness in one file. Read up to the end of step 2 before you
   write the harness, and each later step when you reach it.
2. [references/harness.md](references/harness.md): ops, checks, `Books`
   (the checks they derive), plans and excuses.
3. [references/explore.md](references/explore.md): `scan`, `curves`,
   `search`, `Pick`, replays, `diff`, and how to read the summary.

Rules (the harness's own are in harness.md):

- `Unobserved` isn't a pass. Watch every token a check reads.
- A failed operation whose signed payload or nonce stays usable is a
  free option for whoever holds it: report it, even when it's by design.
- The write-up numbers every finding: forced ones (saying why such a
  failure is plausible) and by-design ones (saying why) included.
