# Auditing a contract

Use this page to hunt for bugs in a contract that holds value, with
`explore`, which is experimental: its API and files change between
minor versions. It is a checklist of seven steps, each with what it produces and when it is
done. Do the steps in order. Before your first harness, read up to the
end of step 2; read the later steps when you reach them. The tools are
in [harness.md](harness.md) and [explore.md](explore.md).

| step | produces | done when |
|---|---|---|
| 0. Pick the build | the wasm and its sha256 | the harness asserts which build it runs |
| 1. Inventory | the flow list and the callback worksheet | every callback is in a flow or out of scope, with why |
| 2. Witnesses | ops, `Books` or checks, one plan per flow and branch | each witness runs as signed; each check failed once on purpose |
| 3. Scan | `scan(&witnesses)` | its findings are read; nothing `unreached` you can't explain |
| 4. Curves | `curves(&witnesses).with(&scan)` | no curve row open |
| 5. Search | `search(plans(ops)).seeds(2)` | findings read, barely reached ops weighted up |
| 6. Triage | a label and a class for each finding | every finding is labeled; by-design ones say why |
| 7. Report | the write-up | `done()` is `Ok`, or the write-up opens with "Coverage not achieved" |

## What a clean audit claims

A clean audit means no check failed in the cases tried (these ops,
witnesses, sizes, faults, seeds and gas), as every summary's first line
says. It is no proof of correctness, and says nothing of a property you
didn't write, a flow you didn't map, or a branch no witness reached. A
crossed curve is a finding; a row below its limit is safe only as far
as the axis moved. Every report names the build, the ops, the checks,
the campaign settings, and what you didn't model.

## A first harness in one file

The shape of steps 2 and 3, on a specimen (`tests/res/floor.wasm`, a
keeper that pays out through a token and forwards a gas floor the caller
signs; [floor.wat](https://github.com/near/near-contract-sim/blob/main/tests/res/floor.wat)'s
header lists its methods and their argument bytes). [tests/first_harness.rs](https://github.com/near/near-contract-sim/blob/main/tests/first_harness.rs)
holds the world (a `Fixture`, as in [quickstart.md](quickstart.md)), a
`read` view helper and the constants, then what follows. It imports
`near_contract_sim::explore::prelude::*`, which includes the everyday
prelude:

```rust
/// The contract's value-moving operations, one variant each, every field
/// one a caller chooses.
#[derive(Clone, Debug, Serialize, Deserialize)]
enum KeeperOp {
    /// Withdraws `amount`, signing `min_gas` for the token's call.
    Withdraw { amount: u64, min_gas: Gas },
}

impl Op for KeeperOp {
    fn submit(&self, sim: &mut Sim) -> Result<(), Skip> {
        let KeeperOp::Withdraw { amount, min_gas } = *self;
        if amount == 0 || read(sim, KEEPER, "credit_of") < amount {
            return Err("nothing to withdraw".into());
        }
        let args = [amount.to_le_bytes(), min_gas.as_gas().to_le_bytes()].concat();
        sim.call(ALICE, KEEPER, "withdraw").args(args).submit();
        Ok(())
    }
}

/// The property: a withdraw moves credit into tokens, never loses it.
fn credit_and_tokens_add_up(sim: &Sim) -> Result<(), String> {
    let (credit, tokens) = (read(sim, KEEPER, "credit_of"), read(sim, TOKEN, "balance_of"));
    match credit + tokens {
        HELD => Ok(()),
        n => Err(format!("alice holds {credit} credit and {tokens} tokens, {n} of {HELD}")),
    }
}

fn harness() -> Harness<KeeperOp> {
    Harness::new(world())
        .audits([KEEPER])
        .at_rest("alice's credit and tokens add up", credit_and_tokens_add_up)
}

#[test]
fn first_harness() {
    let harness = harness();
    // One witness per flow, as a wallet signs it.
    let witnesses = [
        Plan::new().op(KeeperOp::Withdraw { amount: 10, min_gas: Gas::from_teragas(40) }),
        Plan::new().op(KeeperOp::Withdraw { amount: 10, min_gas: Gas::from_teragas(30) }),
    ];
    // Each witness runs clean as signed: a positive control.
    harness.run(&witnesses[0]).assert_clean();
    // Every fault at every receipt, and every edge, of each witness.
    let audit = harness.scan(&witnesses).run();
    println!("{}", audit.with_open_gaps("first harness: no curves or search yet"));
    // A finding with nothing forced, at an edge: the caller's least gas.
    assert!(audit.found().iter().any(|f| matches!(f.label, Label::ShownAt(_))), "{audit}");
}
```

Run it with `cargo test --test first_harness -- --nocapture`. It prints:

- **`shown at step 0, /Withdraw/min_gas = 0, caller gas least + 0
  gas`**: at the caller's least gas the token's call fails while the
  keeper keeps the debit. A real bug, shown with nothing forced.
- **Forced** findings (`PromiseFailed`, `OutOfGas`): what the keeper
  does when its callee fails or starves. Rate them; never drop them.
- **`unreached: 6 exported methods never ran: ...`**: add a witness, or
  excuse the family with one glob and a reason.

Each row is saved as a replay ([explore.md](explore.md)). Scan and
curves need no proptest; a search needs `proptest = "1"` (dev-dependency).

## 0. Pick the build

- Audit what is live, by code hash, and the latest main. Say which build
  each finding holds on.
- Build the source the repo's own way and record the wasm's sha256
  ([sim.md](sim.md)). With only a deployed wasm, its exports are the
  surface.
- Deploy it as production does: through its factory, or as a global
  contract plus a state init, with the real counterparties where you
  have their source. Replace untrusted counterparties with stubs
  ([stubs.md](stubs.md)).
- Make the harness, a file of the test crate, assert which build it
  runs. A fallback to a default wasm silently tests the wrong build.

## 1. Inventory

Read the source before writing any code. Grep for what schedules,
continues and limits work:

```sh
grep -rn '#\[private\]' src/          # continuations, migrations, admin hooks
grep -rn '\.then(' src/               # what each call continues with
grep -rn 'Promise::new\|ext(' src/    # calls out, detached ones included
grep -rn 'Gas::from_tgas\|_GAS\b\|with_static_gas\|prepaid_gas' src/
grep -rn 'Vec<\|msg\|memo\|_ids\|amounts' src/   # size drivers in the arguments
grep -rn 'LookupMap\|UnorderedMap\|IterableMap\|Vector\|insert(' src/   # state that grows
```

Then write three things down.

**The flows.** Every entry point that moves NEAR or tokens, or changes
what others can withdraw, followed to its last receipt:

| flow | receipts, in order | commits before value leaves | value out | if a callee fails |
|---|---|---|---|---|
| deposit | `ft_transfer_call` → `vault.ft_on_transfer` → `token.ft_resolve_transfer` | shares credited | none | the token refunds what the vault returns |
| withdraw | `withdraw` → `token.ft_transfer` → `vault.on_withdraw` | shares debited | tokens to the user | `on_withdraw` must re-credit |

The last three columns find bugs. State committed before value leaves
needs a branch that undoes it when the call fails. Value that leaves
needs a debit that can't be skipped. List each promise branch that
changes value flow as its own row: with and without a notify, a batch
of one and of many. Mark each detached call, each yield, and each place
two flows write the same state.

To see a flow before you write its row, submit it and step, printing
what the books read after each block (`tests/vault.rs`, run with
`--nocapture`; its helpers are in [harness.md](harness.md)):

```rust
let tx = withdraw(&mut sim, "alice.near", 40).submit();
let mut mid_flow = false;
while !sim.is_idle() {
    sim.step();
    let (owed, held) = (sim.view(VAULT, "total_shares", ()).u128(), ft(&sim, VAULT));
    println!("#{}: the vault owes {owed} shares, holds {held} tokens", sim.height());
    mid_flow |= owed < held;
}
println!("{}", sim.outcome(tx));
assert!(mid_flow);
```

It shows the shares debited a block before the tokens leave (so the
backing check is `at_rest`, not `always`), then the receipt tree.

**The callback worksheet.** One row per `#[private]` method and per
method a `.then(` schedules. A callback has three inputs, and a test
controls each:

- **the promise result**: what a counterparty can answer, including a
  panic and a huge or malformed reply. Make it an op field
  (`input::answer`, [stubs.md](stubs.md));
- **the scheduling arguments**: they come from the entry, so the entry's
  sizes (a list of ids, a memo) meet the callback's fixed gas budget.
  Make every free-form argument (`msg`, `memo`, JSON in a string) an op
  field, so curves grow it; give a structured string a malformed variant;
  an enum is a choice a scan tries. `forward(msg)` scheduling
  `on_forward` with a flat 10 Tgas, which stores `msg`, pays per byte of
  `msg`: curves show where that cost meets the budget;
- **the state it reads**: build it in earlier steps, or change it while
  the callback is in flight (a second op in the same block). Work
  backward from each `require!`, `unwrap`, overflow and host limit.

**The world.** Every token standard and interface the contract calls,
each with a real counterparty (the audited contract itself, an older
release, the standard's reference build) or a stub with a written
reason. Every gas amount a caller signs (`min_gas`, a callback budget in
an intent) is an op field. If the contract stakes or pays rewards, run
with epochs passing (`Plan::epochs(n)`). The sim's genesis is in 2020,
so sign explicit deadlines and draw nonces from the world
([transactions.md](transactions.md)).

**Done when** every callback is in some flow's row or out of scope with
why, every flow has its branches listed, and every counterparty and
signed gas field is named.

## 2. Witnesses

**The ops** ([harness.md](harness.md)). One variant per value-moving
entry, every choice a field, gas unpinned. Pre-fund counterparties in
the fixture.

**The witness plans.** One plan per flow and per branch, as a wallet
would sign it. Read every `#[serde(default)]` and `Option` on the
signed input types: write one plan per field whose omission changes
value flow. A list field gets a witness of one item and one of several.

**The properties.** If the contract owes and holds value, declare
`Books`: each liability (per holder), each asset, which backs which,
`owned` for balances only their holder may spend, `price` for shares,
`once` for keys consumed once, `claim` for what must stay redeemable.
The backing, conservation, stranded-value and liveness checks follow
from that. Then add what the books don't say, placed by when it holds:
`always` (holds after every block), `at_rest` (holds once flows settle),
`per_op_for` (what one op did, from its own receipts). Use
`must_not_fail(site)` on the callbacks that commit or reverse value.
Name the audited contract: `Harness::audits([account])`.

**Controls.** Run each witness with `harness.run(&plan)` and
`assert_clean()`. Then make each check fail once on purpose: a check
that never fails tests nothing. Three routes, cheapest first:

- **Break a clone.** Build the world, damage it (`set_balance`,
  `set_state`, `update_contract_state`, [sim.md](sim.md)), and call the
  check function itself (`tests/vault.rs`):

  ```rust
  let mut sim = world();
  deposit(&mut sim, "alice.near", 100).transact().assert_success();
  assert!(vault_holds_what_it_owes(&sim).is_ok());
  sim.update_contract_state::<VaultState>(VAULT, |s| s.total_shares += 1);
  assert!(vault_holds_what_it_owes(&sim).is_err());
  ```

  A harness on the damaged world reports a harness error at the
  fixture, not a finding: no case can tell anything about a world that
  fails before it. Damage it in an op instead, and the check judges it.

- **Force a fault**: a plan whose `.inject(..)` reaches the branch the
  check guards ([faults.md](faults.md)).
- **Patch the build**: plant the bug the check is for, and scan both.

**Done when** each witness runs clean as signed, each check has failed
once on purpose, and every flow row has its witness.

## 3. Scan every witness

`harness.scan(&witnesses).run()` with the default faults
([explore.md](explore.md)).

Read its findings and its to-do list. Each `unreached:` item is an
exported method nothing ran: add a witness, or excuse a family with one
glob and a reason. A harness error stops everything else: fix it first.

## 4. Curves

`harness.curves(&witnesses).with(&scan).run()` measures what each receipt
uses (gas, logs, storage) against its limit as each axis grows. A
`Crossed` row is a finding with a replay. A `Stopped` or barely moved row
is open: excuse a copy a guard refuses (caller check, lock while held,
used nonce), naming the guard; else supply what its to-do item names, or
start the witness larger. One
refused whole at a balance or a length is closed ("stopped at a bound").
State a gas finding "at the protocol max (1000 Tgas)" if it holds there,
never "at 300 Tgas" (what wallets attach).

## 5. Search

`harness.search(plans(ops)).seeds(2).run()` with the default plans, one
op strategy per variant, weighted toward the flows that move value.
Read its findings and its coverage. Weight up any method that few
cases reached, and search again.

## 6. Triage

Label every finding with its impact, its reachability (shown, shown at
an edge, or forced, from its label), and its threat model, then classify
it. A finding by design stays a finding: say why it is by design and
what it leaves behind (a credit, value stranded, a usable payload, a
lock, or nothing).

- A `panic = "abort"` build fails with `unreachable`: read the trap by
  its site ([fidelity.md](fidelity.md)).
- A row's `broke` (`breaking step 2 of 3: Skew`) names the op after
  which a world check first fails.

## 7. Report

Findings first, ordered by impact, most severe first. Reachability is a
label on each, never a section. A finding that follows a forced fault
sits with the natural ones, labeled "forced", and is never "held".

**Every finding of the audit is a numbered finding.** Ones sharing a
root cause may be merged, saying which; none is dropped. A forced one
may be rated low, but has its own fix line. No hardening table. Per
issue:

- **Covers:** the audit findings it numbers.
- **Where:** file and line in the tree you name.
- **Problem:** what the code does, against what it documents.
- **Impact:** the worst effect on the books, and at what scale.
- **Reachability:** shown with normal inputs, shown at an edge (which),
  shown with a hostile counterparty (answering what), or forced: the
  fault, and one line on why such a failure is plausible.
- **Threat model:** who has to misbehave.
- **Evidence:** the replay file and the test that reproduces it.
- **Fix:** a direction.
- **Status:** on the live build and on main.

Then, in this order:

1. **The curves** that matter: each crossed or near-limit row, its site,
   its limit, and the input that drives it.
2. **Coverage**, as the summaries print it: exported methods that ran, and
   every excuse with its reason, copied verbatim.
3. **What held**, with the settings behind it. Only what no check failed
   on goes there.
4. **What you didn't model**: flows, properties, sizes or counterparties.

**Done when** each audit's `done()` returns `Ok` without
`with_open_gaps` (or the write-up opens with "Coverage not achieved",
its reason and every open item), and
every finding of the audit appears as a numbered finding or is
explicitly merged into one. Keep each replay as a regression
(`harness.regressions()`, [explore.md](explore.md)).

## Lenses

Properties by contract shape, to start from:

- **Vault or custody**: the vault's tokens back its shares
  (`Books::backs`); a deposit credits what it takes; a withdraw delivers
  what it debits; nothing is kept and owed to no one.
- **Token or ledger**: the supply is the sum of balances; a balance falls
  only for its holder (`owned`); a refund never exceeds what was sent.
- **Staking pool**: the price per share never falls (`price`); an
  unstaked claim is redeemable after its cooldown (`claim`).
- **Intents or signed messages**: a nonce is used at most once (`once`);
  an expired deadline is refused; a failed intent leaves no usable
  signature.
- **Yield-based (MPC, oracles)**: every request resolves or times out,
  and a timeout refunds ([mpc.md](mpc.md)).
