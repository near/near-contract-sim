# Blocks and the clock

Use this page when a test depends on when things happen: deadlines,
cooldowns, epochs, yields and their timeouts, stopping between two
receipts, or many calls landing in one block.

Blocks happen only when the test asks. `height()` and `timestamp_ns()`
are the last block's, and views see them. Each block moves the clock by
`config.block_time`, 1 s by default.

## Produce blocks

- `step()`: one block.
- `settle()`: blocks until idle, at most `config.max_blocks` (10,000).
  Past that it panics and lists the receipts still pending: a receipt loop
  shows up as one call that keeps coming back. Parked yields don't count;
  they stay parked.
- `run_until(max, |sim| cond)`: checks `cond` before each block and stops
  on the block it turns true, so a test can stop between two receipts.
  Past `max` blocks it returns `Err(Busy { blocks, idle })`
  (`near_contract_sim::Busy`);
  `run_until(max, Sim::is_idle)` is a `settle` that doesn't panic.
- `advance_to(h)`, `advance_by(duration)`: run to a height, or let time
  pass. Both skip idle heights.
- `is_idle()`: nothing queued or in flight.

`run_until` returns `Ok` with how many blocks it produced. In `tests/clock.rs`,
loop.near's `ping` calls itself with all its gas, so it never settles:

```rust
let tx = sim.call("alice.near", "loop.near", "ping").submit();
let h = sim.height();
assert!(matches!(sim.run_until(5, Sim::is_idle), Err(Busy { blocks: 5, idle: false, .. })));
assert_eq!(sim.height(), h + 5);
```

## Land many calls in one block

`transact()` runs each call to the end, so 30 calls in a row take 60
blocks or more. To land them in one block, submit them all, then settle
(`tests/chain.rs`):

```rust
let next = sim.height() + 1;
let hs: Vec<CryptoHash> = users
    .iter()
    .map(|u| sim.call(u, "status.near", "set_status").args_json(json!({"message": u})).submit())
    .collect();
sim.settle();
```

Each converts in block `next`, unless the chunk's gas or size budget
pushes the tail to the block after. An `Op` does the same: it submits,
and the harness decides when blocks happen ([harness.md](harness.md)).

Each queued transaction prepays all the gas it attaches when it
converts, 300 Tgas a call by default (about 0.03 NEAR), refunded only
later. So 500 calls from one account in one block need about 15 NEAR up
front; the rest are refused with `InvalidTx(NotEnoughBalance)` or
`LackBalanceForState`, naming the prepaid gas. Attach less, or fund it.

## Set the clock

`set_timestamp_ns(ns)` and `set_height(h)` take any value, the past
included, and produce no block. The next transaction's first receipt
runs at exactly `ns` and `h` (views see them until then): submitting it
moves the last block back one block for a call to its own signer, two
for another account. A block produced first spends the pin. A rate
limit, tested at exact times (`tests/vault.rs`; `HOUR` is in ns):

```rust
    let mut sim = world();
    let t0 = sim.timestamp_ns();
    let claim = |sim: &mut Sim| sim.call("alice.near", VAULT, "claim_rewards").transact();
    sim.set_timestamp_ns(t0);
    claim(&mut sim).assert_success();
    sim.set_timestamp_ns(t0 + HOUR);
    assert!(claim(&mut sim).is_failure());
    sim.set_timestamp_ns(t0 + 24 * HOUR + 1_000_000_000);
    claim(&mut sim).assert_success();
    assert_eq!(shares(&sim, "alice.near"), 20);
```

`advance_to(h)` and `advance_by(d)` produce blocks (`d` rounded up to
whole blocks), so a call after them runs a block or two past them.
`pause_clock_at(ns)` keeps every block from the next one at `ns`, until
`resume_clock()`; call it again to move it. Heights go on, and
`advance_by` panics while paused. A fixture valid only for a while, such
as a TEE quote's collateral, sets the clock before anything runs
([mpc.md](mpc.md)).

## Yields

A callback waiting on a yield stays parked after `transact()`, listed in
`sim.yields()` as a `PendingYield`:

- `expires_at`: the height its timeout fires at (a height, not a time);
- `callback_methods` and `callback_args`: what the contract passed to
  `promise_yield_create`, to match a yield to its request;
- `tx`, `receipt_id`, `data_id` and `account_id`.

`expire(&y)` advances to `y.expires_at`, settles and returns
`outcome(y.tx)`. To race a resume against the timeout, play each side
from a clone (`tests/mpc.rs`; `mpc_fixture::respond` sends the answer):

```rust
let [y] = <[_; 1]>::try_from(sim.yields()).expect("one parked yield");
assert_eq!(y.callback_methods, ["return_signature_and_clean_state_on_success"]);
assert!(!sim.outcome(tx).is_complete());

// One block before expiry, a respond wins the race.
let mut early = sim.clone();
early.advance_to(y.expires_at - 1);
mpc_fixture::respond(&mut early, &y).assert_success();
early.settle();
early.outcome(tx).assert_success();

// Otherwise the timeout wins: no sleeps, no polling.
let out = sim.expire(&y);
assert_eq!(out.assert_failure(), Failure::Panic("Request has timed out.".into()));
```

At the edge, a resume submitted at `expires_at - 1` wins, and one at
`expires_at` loses, unless the contract resumes itself (`tests/yields.rs`).

A harness case times out every parked yield at its end
([harness.md](harness.md)). [mpc.md](mpc.md) is the whole recipe.

## Epochs

Epoch `e` starts at `sim.epoch_start(e)`; `sim.epoch_height()` is the
last block's, `sim.epoch_of(h)` block `h`'s: a call submitted in an
epoch's last two blocks runs in the next, so read a receipt's with
`sim.epoch_of(r.block_height)`. `advance_epochs(n)` lands on the first
block of the epoch `n` after. A `Stake` that lowers a stake keeps it
locked until the first block of the third epoch after the one it counts
in, as on a node. Set `config.epoch_length` (at least 3) before the
first block (`tests/epochs.rs`).

Wait with `advance_epochs` or `advance_to`. Don't patch the contract's
stored epoch or rewind with `set_height`: that tests the contract's clock
without the chain's. Epochs are simplified (one validator, flat rewards
of `config.epoch_reward`, 0 by default): see [fidelity.md](fidelity.md).
