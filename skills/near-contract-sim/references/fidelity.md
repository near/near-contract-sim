# Fidelity

Use this page to decide how far a result carries to a real node: what
matches a node exactly, what differs, and what the simulator doesn't
model.

What a contract observes comes from nearcore's own code at 2.14.0
(protocol 88): `Runtime::apply` validates and converts transactions and
runs receipts with nearcore's fees, limits and wasm config, and views go
through nearcore's `TrieViewer`. The simulator decides what goes into
each block and when blocks happen ([blocks-and-clock.md](blocks-and-clock.md)).
The same inputs give byte-identical outcomes, receipt ids, gas and state
root (`tests/prop_invariants.rs`, over random programs).

## How it is checked

Gas, every gas-profile entry and every refund match a recorded `neard`
2.14.0 sandbox run at protocol 88 (`tests/gas_vs_sandbox.rs`);
random programs match a 2.14.0 sandbox receipt by receipt, with a new
seed in CI (`tests/differential.rs`); near/mpc's TEE verifier matches
its native build (`tests/mpc_tee.rs`). An older `protocol_version` gets
nearcore's config for it, unchecked against a sandbox. A margin here is
a sandbox's margin: near/mpc#4118 measured one call on testnet at 1.34
times its sandbox gas fixed, and 1.21 times per entry.

## Read a trap correctly

`unreachable` (a wasm trap) where you expected a panic message means
the contract was built with `panic = "abort"`, which drops the message.
Rebuild with `panic = "unwind"` or a debug profile to read it. A
finding's run names the functions it trapped in ("executed in `f`,
called from `g`") if the wasm keeps its name section and imports
`log_utf8`.

## Receipt order

nearcore guarantees one order only: a callback runs once every result it
waits on has arrived. A receipt can overtake another through a route's
outgoing buffer, which carries only so many bytes a block and drains in
order. The simulator runs that buffer on its one shard's route to
itself, so receipts run in the order sent unless large ones fill it
(`tests/order.rs`).

`Fault::Hold` keeps a receipt back, as a full buffer would, in an order
a node allows; the length is the test's choice ([faults.md](faults.md)).
A hold has no cause (the write-up says what would delay it), held
receipts share one queue where a node keeps one per route, and a hold
can't reach a transaction's first receipt, gas refunds, a yield's
callback or the resume that wakes it.

## Known differences

- **Chain-specific values.** Receipt, data and yield ids, block hashes,
  heights, timestamps, random seeds and epoch heights differ from a
  node's.
- **Trie-node gas depends on every key in the trie**, so it matches a
  node once both tries hold the same keys.
- **Gas margins.** A callback left under 25% of its attached gas is a
  `gas margin` finding though it never ran out, since trie-node pricing
  can differ from a network's. The 25% allows for that; it isn't a
  measured bound.
- **Block gaps.** A node produces blocks on the wall clock, the simulator
  when asked, so a yield's callback runs a different number of blocks
  after its parent. A receipt that waits on nothing runs one block after
  its parent on both.
- **Congested chunks pack differently.** A burst lands in one block here,
  in submission order; a node spreads it and orders signers by hash. Trie
  gas and the gas price (up to 1% a block while chunks are over half
  full) then differ. Statuses, logs and receipt trees still match.
- **Invalid transactions are recorded**, as `Failure(InvalidTx(..))`
  with no receipts, where a node's chunk producer leaves them out.
- **A strict transaction behind a nonce gap** waits in a node's pool; the
  simulator refuses it at conversion.
- **No expiry by age.** The validity period is the chain layer's, which
  the simulator doesn't run.
- **A callback `Tx::promise_results` delivers has no parent.** What it
  sees and its gas match the same callback reached through
  `promise_then` (`tests/callbacks.rs`).
- **A fault rewrites a receipt between blocks** ([faults.md](faults.md)).
- **`apply_state_records` leaves `storage_usage` alone**, where
  `Sim::set_state` keeps it right.

## Limits

- **One shard.** No resharding and no cross-shard delays except holds. A
  finding that needs receipts of different accounts to line up in
  particular blocks is not shown to happen on a multi-shard network:
  say so on its reachability line ([audit.md](audit.md)).
- **A static gas price**, `config.gas_price`. **No gas reward**: from
  protocol 87 a contract earns nothing from the gas its calls burn, on a
  node too.
- **Epochs, simplified** ([blocks-and-clock.md](blocks-and-clock.md)).
  One validator, `validator.near`; a positive stake under `config.minimum_stake`
  (0 by default) fails; no seat price, kickouts or slashing. Rewards are
  a flat `config.epoch_reward` per staked account per epoch, 0 by default:
  test who gets rewards, not share prices. A stake proposed in epoch `e`
  is a validator's, and first rewarded, from `e + 2`, as on a node.
- **Time can go backwards** (`set_timestamp_ns`): handy for a fixture's
  validity window, but a state no chain reaches.
- **`Sim::create_account` writes the account directly**, like a genesis
  record, so the `CreateAccount` action's rules (a parent for
  `a.b.near`) don't apply. `Tx::create_account` is the action.
