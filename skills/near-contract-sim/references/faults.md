# Faults

Use this page to reach the branches a happy path never takes: a callee
that fails, a callback short of gas, a callee that returns odd bytes, or
receipts that land late or out of order. One fault, in one transaction.
To try every fault of a flow, see [explore.md](explore.md).

A fault changes what nearcore's runtime is given, never what it reports.
The failure, the refunds and the promise results are nearcore's.

## Inject one

`tx.inject(Receipts, Fault)` picks receipts the transaction spawns and
forces a fault on them. [quickstart.md](quickstart.md) fails the vault's
payout with `Receipts::callback("on_withdraw"), Fault::PromiseFailed`,
and `on_withdraw` must give the shares back. `PromiseFailed` at a
callback fails the call it waits on, so that call shows `OutOfGas` as
its fault (`tests/vault.rs`):

```rust
assert_eq!(w.expect_receipt("ft_transfer").fault, Some(Fault::OutOfGas));
```

A selector that matches nothing changes nothing, so assert the fault
landed: some receipt's `fault` is set (`held` for a hold). Several
`inject` calls chain; a receipt can be starved and held, but not starved
twice.

## Faults and findings

`sim.findings()` excuses the failure a fault forced: the starved receipt
is no unhandled failure. The tree marks every forced receipt `[fault:
..]`, and one whose failure reached nobody also `<-- FORCED (excused)`;
one whose failure a callback read gets no second mark. What it caused is
judged as any receipt is, and marked `injected`: a failure downstream
nobody learns of is a finding, and so is a failed callback once an
earlier receipt of the transaction changed its contract's state. A
harness audit keeps every finding that follows a fault, labeled `forced
(..)` ([audit.md](audit.md)).

## Pick the receipts

`Receipts` never picks the transaction's first receipt (use `Tx::gas`),
refunds or data receipts.

| selector | picks |
|---|---|
| `Receipts::all()` | every spawned receipt |
| `Receipts::callbacks()` | every receipt that waits on a promise result |
| `Receipts::cross_contract_callbacks()` | callbacks whose calls all go to other accounts |
| `Receipts::callback(m)` | a callback running `m` |
| `Receipts::method(m)` | any call of `m` |
| `Receipts::call(receiver, m)` | `m` on `receiver` |
| `Receipts::receiver(id)` | any receipt to `id` |
| `Receipts::action(kind)` | a receipt with an action of that kind (`"Transfer"`, `"Stake"`) |
| `Receipts::index(n)` | the `n`th receipt spawned |
| `Receipts::occurrence(receiver, m, k)` | the `k`th call of `m` on `receiver` the fault fits: what scans and replays write |

`.nth(k)` keeps only the `k`th (from 0) receipt it selects that the fault
fits, in spawn order. To fail the result a callback reads, put
`PromiseFailed` on the callback (`Receipts::callback(m)`); to starve the
callee itself, put `OutOfGas` or `Gas(g)` on `Receipts::call(receiver, m)`.

## What each fault does

| fault | on | what the runtime does |
|---|---|---|
| `PromiseFailed` | a callback | each call it waits on runs with 1 gas and fails at load; its deposit is refunded; the callback reads `PromiseResult::Failed` |
| `OutOfGas` | a call | it runs with 1 gas and fails with `Failure::OutOfGas` before changing anything |
| `Gas(g)` | one call | it runs with `g`; below what it needs it fails where a node's would |
| `ReturnValue(bytes)` | a callback | each successful result it reads is `bytes`; a failed one stays failed |
| `Hold(n)` | any receipt | it leaves `n` blocks late; receipts made meanwhile go ahead of it |
| `HoldResult(n)` | a call | its result reaches the callback `n` blocks late |
| `HoldUntil(receipts)` | any receipt | it leaves after the receipt `receipts` names runs (this transaction's, or with `.in_tx(t1)` an earlier one's), behind earlier holds |

`transact()` runs until everything settles, held receipts included. To
land another call while a receipt is held, `submit()` the transaction
and `run_until` the hold is in place (the quickstart's world):

```rust
    let first = withdraw(&mut sim, ALICE, 40)
        .inject(Receipts::method("ft_transfer"), Fault::Hold(5))
        .submit();
    sim.run_until(10, |s| s.outcome(first).receipt("withdraw").is_some()).unwrap();
    let second = withdraw(&mut sim, ALICE, 60).transact(); // settles the held one too
```

A failed receipt commits nothing: no write, promise or log. So faults
reach only receipts after one that committed (continuations,
callbacks), and `OutOfGas` at load leaves what running out at any later
host call does, but for gas burnt (refunded to the creator; `Gas(g)` is
exact). For a single-receipt op's gas limits, use a search's gas draws
and curves on the caller's gas, not a hand-written gas walk.

No fault fails a receipt that makes no function call, such as a pool's
`Stake` to itself: `Receipts::action("Stake")` takes only holds. To see
a failed stake's callback, make the stake really fail (stake more than
the account holds).

## Which faults are realistic

A forced fault shows its consequences; whether a network can cause it
is a separate question. An audit's `forced (..)` label names each fault
and its receipt, and the write-up says in a line why such a failure is
plausible ([audit.md](audit.md)).

| fault | a network can cause it when |
|---|---|
| `PromiseFailed` at a call to another account | the callee panics, runs out of gas, or doesn't exist |
| `PromiseFailed` at a contract's call to itself | the contract's own code fails; rarely a caller's choice |
| `Hold(n)`, `HoldResult(n)`, `HoldUntil` | the order is one nearcore allows; the length is the test's choice ([fidelity.md](fidelity.md)) |
| `Gas(g)` on a call | a caller can attach `g`, or the contract forwards a share that small |
| `OutOfGas` at a callback or cross-contract call | its fixed budget runs out as input size, logs or storage grow |
| `ReturnValue(bytes)` | the callee is untrusted or upgradable; prefer a stub ([stubs.md](stubs.md)) |

To test a callback's branches without the flow around it,
`Tx::promise_results` delivers results you pick
([transactions.md](transactions.md)).
