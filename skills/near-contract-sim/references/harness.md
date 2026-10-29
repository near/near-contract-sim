# The harness: ops, checks, books and plans

A `Harness` is a fixture, ops, and what must hold (checks or `Books`).
The verbs of [explore.md](explore.md) run it on your plans or generated
ones. `explore` is experimental: its API changes between minor versions.

## Ops

An op is one value-moving operation, as a variant of an enum that is
`Clone + Debug + Serialize + Deserialize`. Each field is one a caller
chooses, and `Op::submit` sends its transactions. The examples on these
pages are the vault's, [tests/vault.rs](https://github.com/near/near-contract-sim/blob/main/tests/vault.rs):
the quickstart's world and helpers with bob.near too, `ft(sim, who)` and
`shares(sim, who)` reading the token and the vault, and
`VaultOp::{Deposit, Withdraw, Claim}`, each with a `who: User` (the
test's own enum of accounts), the first two with an `amount: Amount`.

Rules for `submit`:

- It only submits (`Tx::submit`). The plan decides when blocks happen.
  Calling `transact`, `settle` or `step` here is a harness error.
- Return `Err(Skip)` when the op doesn't apply in the state it finds.
  The case goes on, and no per-op check judges the skipped op. A panic
  in `submit` is a harness error that prints the panic, never a skip.
- Read the world to choose arguments. `Amount` (`Zero`, `One`, `All`,
  `AllPlus(n)`, `AllMinus(n)`, `Fraction(a, b)`, `Exact(n)`) resolves
  against a balance with `amount.of(balance)?`; `Exact(n)` is `n`
  whatever the balance. To grow an amount along a curve, make it a
  plain number field.
- Leave gas unpinned unless the contract needs it. Exploration sets the
  caller's gas itself, and a pinned call is never explored.
- Make every id, list, memo and counterparty answer a field, holding the
  data, never an index into a table built at run time (`Op`'s rustdoc).
  Exploration moves ops by editing their serde form, so a constant inside
  `submit` is never explored. Each number is an edge for `scan` and an
  axis for `curves`; so is each list's length and each string's bytes. An
  enum, data-carrying too, is a choice a scan tries. Draw them in a search
  from `explore::input` (`id`, `text`, `list`, `pick`, `gas`, `answer`).
- An op consuming what an earlier step made (an id) resolves a `Pick` at
  submit. What only an earlier op saw (a return value) it records in a
  field of your world, and `enumerate` then needs a `.key` reading it
  (`Op::submit`'s rustdoc).

An op's name in findings is serde's variant name (`Withdraw`); a struct
op names every case alike (`Op::name`). For a world that is more than a
`Sim` (an intents verifier with its signing keys), implement
`AsRef<Sim> + AsMut<Sim> + Clone` on your type and use `Op<YourWorld>`.

## Checks

Pick where each property is judged:

| registration | judged | use it for |
|---|---|---|
| `always(name, f)` | after every block, and after an op that queued nothing, nothing else in flight | what holds even mid-flow (a sum of shares) |
| `at_rest(name, f)` | once nothing is in flight: each idle block, epoch gaps too, and after an op that queued nothing, nothing else in flight (a costly one goes in `at_end`) | totals that hold only when flows settle |
| `at_rest_ops(name, f)` | at rest, with every op submitted so far | shared state under overlap: what the case's ops add up to |
| `per_op(name, f)` / `per_op_for(pred, name, f)` | when the op's own receipts are done | what this one op did (`OpDone::effects()`) |
| `must_not_fail(site)` | every receipt at `site` | a callback that commits or reverses value |

A check returns `Result<(), E>` with `E: Into<Fail>` (a `String` works);
one walking holders returns `Fail::all(..)`, a finding per failure.

- One that reads what it can't observe returns `Unobserved`: the case
  counts as not judged, never as a pass. A `Books` check with nothing
  owed yet counts as idle.
- One that panics, or fails on the fixture itself, is a harness error.
- Checks also run on shrink candidates and a fault's targeting run:
  never count coverage in one; `audit.reached("vault.near::on_withdraw")`
  counts the cases that ran a site.
- A per-op check of ops in flight together judges each on `effects()`,
  the op's own receipts' moves. `before()` and `world()` are the shared
  world: a check that reads them for an op not `isolated()` is not
  judged, its reason naming the read.

```rust
/// per-op: what this withdraw's own receipts did.
fn withdraw_delivers(d: &OpDone<VaultOp>) -> Result<(), Fail> {
    let VaultOp::Withdraw { who, .. } = *d.op() else { unreachable!("withdraws only") };
    let taken = -d.effects().token_net(VAULT, who.id())?;
    let paid = d.effects().token_net(TOKEN, who.id())?;
    if taken != paid {
        return Err(
            format!("{} gave up {taken} shares and received {paid} tokens", who.id()).into()
        );
    }
    Ok(())
}
```

The vault registers it with `.per_op_for(is_withdraw, "withdraw
delivers what it debits", withdraw_delivers)` beside its `always` and
`at_rest` checks (`harness` in `tests/vault.rs`); a predicate can be
`|op| matches!(op, VaultOp::Claim { .. })`.

Shared state under overlap goes in `at_rest_ops`, judged at every rest
point with the world and every op (no raw call) submitted so far, each
complete: the overlap rule is for per-op checks. The ops in flight
together run from the last whose `rest()` is true: judge from its
`before()`; with ops but none at rest, not judged. A raw call before
any op is in no window (`OpDone::rest`); `Books` sees raw calls. Bob's
tips (`tests/invariants.rs`):

```rust
    Harness::new(sim)
        .at_rest_ops("bob has every tip", |sim: &Sim, tips: &[OpDone<TipBob>]| {
            bob_has_every_tip(sim, tips.len())
        })
```

A staked account's NEAR also moves by its epoch rewards: subtract
`sim.rewarded(account)`. A model of the contract can sit in an
`at_rest` check too: `sim.receipts_in(heights)` lists every receipt in
execution order, refunds and failures included, to replay through it
(`shares_follow_receipts` in `tests/vault.rs`).

## Books

For a contract that owes and holds value, declare what it owes (a
liability, per holder) and what backs it (an asset), once. The checks
follow from that, judged at every rest point (`.at_end()`: once a case).

```rust
fn vault_books() -> Books<VaultOp> {
    let view = |sim: &Sim, m: &str| -> u128 { sim.view(VAULT, m, json!({})).u128() };
    Books::new()
        .liability("vault shares", move |sim: &Sim| {
            // Reward shares are unbacked by design.
            BTreeMap::from([("all".into(), view(sim, "total_shares") - view(sim, "total_rewards"))])
        })
        .asset("vault tokens", |sim: &Sim| ft(sim, VAULT))
        .backs("vault shares", "vault tokens")
}
```

| check | fails when |
|---|---|
| `L is backed by A` | the liabilities exceed the asset by more than its tolerance |
| `A backs what it owes` | over the case, the liabilities grew more than the asset |
| `A is kept, owed to no one` | over the case, the asset grew more than the liabilities: stranded value (not for an asset only priced) |
| `L falls only for its holder` | a holder's amount fell below its highest reading, and it authorized no op but those that raised it (`owned`; its rustdoc: blind spots) |
| `L's supply is the sum of its holders` | the stated supply differs from the holders' sum (`supply`) |
| `L's price never falls` | the asset per share fell from its first reading with shares outstanding (`price`) |
| `K at most once` | two complete ops, a repeat's copies too, consumed one key (`once`) |
| `C is redeemable` | a claim can't be redeemed in full at the end (`claim`) |
| `outcome some serial order gives` | up to 3 ops in flight together leave readings no serial order of them gives, or one is refused only when interleaved |

The value checks compare the world at rest with the world before the
case's first op (an `at_rest_ops` check, above), so ops in flight
together are judged exactly.

- `Books::near(name, account)` books an account's NEAR, staked
  included, as an asset. Its change in a case is what moved plus the
  epoch rewards its stake was paid, never gas (rewards not synced yet are
  a liability: its rustdoc). Kept and backing allow 1 yocto either way
  per proof move (wrap.near pays `amount + 1`): a call that moves a proof
  hides a 1-yocto leak. `keeps_no_near(account)` is that asset with nothing
  owed against it: `<account> NEAR is kept` fires on any it keeps.
  `watch_near(account)` only reads it, for the serial-order check: a
  plain account an interleaving can pay twice.
- An asset that reads zero, backing nothing owed, in every case is an
  `unjudged:` to-do.
- A liability can come from a view that takes an internal id
  (`mt_balance_of(account, "nep141:wrap.near")`).
- Liveness (`claim`) advances a clone of the case's end to each claim's
  maturity, at most `LIVENESS_EPOCHS` epochs, and plays its redeem plan.
  A claim that matures later is not judged, never a pass.
- An op is authorized by the account its transactions came from; for a
  relayer or a signed payload, `authorized(|d: &OpDone<MyOp>|
  Some(vec![d.op().holder()]))` names the holder.
- `tolerance(name, units)` lets one reading's rounding pass. `once`
  judges a repeat's raw-call copy as its op: key on what was sent,
  `d.entries().first()?.args_json(..).ok()`, `None` for a failed outcome.

A contract prepaying storage from a ledger stakes records' bytes from
the NEAR backing it (`near` reads a debit as kept): book its balance less
its storage stake as an `asset` backing the ledger (`prepaid_books` in
`tests/books.rs`): bytes it stakes for itself are `backs` findings.

Storage a caller pays for takes three `per_op` checks on kept
(`near_net`) against cost (the storage recipe, [outcomes-and-effects.md](outcomes-and-effects.md)):
a refused op keeps nothing; kept ≤ cost times your factor; kept ≥ cost.
Blind spots: storage freed and bought in one op, a factor that is policy.

## Plans

A `Plan` is one exact case: steps, each an op and what happens after it.

- `Plan::new().op(a).op(b)`: `b` submits after `a` settles.
- `.blocks(0)` puts the next step in the same block (repeat it to
  overlap n ops). `.blocks(n)` waits n blocks; `.epochs(n)` crosses
  epoch starts, the next op submitting in the block before: an op that
  reads maturity at submit needs `epochs(n + 1)`.
- `.gas(g)` sets the gas of the last step's unpinned calls: a `Gas`, or
  `StepGas::LeastPlus(extra)`, the least that succeeds plus `extra`.
- `.inject(Receipts::callback("on_withdraw"), Fault::PromiseFailed)`
  forces a fault on the last step's receipts ([faults.md](faults.md)).
  A step may force several.
- `plan.ops()` lists each step's op (`None` for a raw `Call`).

`harness.run(&plan)` runs one exact case and saves no replay. Its `Run`
has `verdict()` (`Clean`, `Findings`, `Incomplete`, `HarnessError`; it
prints), `reason()` for why it isn't clean, `findings()`, `outcomes()`,
`trees()` (every step's receipt trees) and `world::<W>()` (`None` for
another `W`). `assert_clean()` passes only a clean run. An audit's `assert_clean` judges what ran, not open items: a
campaign is done only when `done()` is `Ok`. A plan with mistakes (an
injection naming a method no fixture contract exports, a gap set twice)
is refused before anything runs: the run is a `HarnessError` naming
them, and a scan lists them in `audit.errors()`.

The types: `Harness<MyOp>`, or `Harness<MyOp, MyWorld>` with
`OpDone<MyOp, MyWorld>` in its per-op checks; `Run<O>`, `Audit<O>` and
`Plan<O>` take the op type only.

## Excuses

`.excuse(todo_id, why)`, chained on the harness as checks are, closes an open item of the audit's to-do
list by its id, or by a glob over the whole id (`unreached:token.near::*`
closes every method of the family in one call; `*nft_*` for
`do_nft_withdraw`). The summary's
header names every excuse and what it closed. Prefer closing the item:
a witness for an unreached method, a longer curve for an open row.

It is the only excuse: a finding never is. A regression test asserts
on the findings it expects (`f.finding.check`, `f.finding.site`), and
on nothing else showing.
