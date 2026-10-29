# Exploring: scan, curves, search, replays, diff

Each verb runs cases on a [harness](harness.md) and returns an `Audit`:
findings, curve rows, coverage and a to-do list of what is still open.
`explore` is experimental: its API and files change between minor versions.

| verb | runs | use it to |
|---|---|---|
| `harness.run(&plan)` | one exact case | pin a regression, debug a finding (`run.trees()`) |
| `harness.scan(&plans).run()` | each witness, then every fault at each receipt it fits and every edge | cover each flow's failure branches and boundaries |
| `harness.curves(&plans).run()` | each axis of each witness up a ladder, measured | find where a budget or limit runs out as inputs grow |
| `harness.search(plans(ops)).run()` | generated cases, each finding shrunk | find what you didn't think of |
| `harness.enumerate(&ops).run()` | every sequence to depth 3, each next op at each block of the last's flight, epoch edges, `.faults(..)` (with `Faults::edges()`, a step placed at rest also runs at the scan's per-step edges, each a leaf); states deduped (`.key`: the default ignores height within an epoch); `.first(&xs)` draws step 1 from `xs` only (split processes by it, then `union`) | find the interleaving that breaks it; a bounded campaign finds bugs, proves nothing |
| `harness.regressions()` | every saved replay | keep findings as regression tests |
| `a.diff(&b, &plans)` | the same plans on two harnesses | see what a fix changed, and only that |

## Scan

A scan runs each witness as written. Then, for each witness, it runs
one variant per:

- **fault** of its `Faults` at each receipt the fault fits;
- **edge of the caller's gas**: the least that succeeds, and each gas
  constant of the fixture's wasm, plus or minus 1. Where the transaction
  succeeds at the least but one of its receipts fails, the report gives
  the band (`Report::bands`): up to the least gas with which every
  receipt succeeds, and the receipt that failed;
- **edge of each number the op signs**: 0, 1, its type's maximum, and the
  gas constants plus or minus 1. Each runs at the least caller gas and at
  the ceiling. A field the op leaves unset (`None`) gets 0 and the gas
  edges too, as a number or a decimal string, whichever its type takes;
- **choice**: each other variant of an enum the op holds, at the step's
  own gas, and each inside a held data one (`Returns(_)` tries each
  `Shape`). A payload neither the witness's nor plain (a tuple such as
  `Fraction(u32, u32)`) is skipped unlisted. Hold `Returns(_)`, not
  `Panic`; an enum left `None`, or a fixture's reply, isn't;
- **adversary answer**: where a stub answered a call with an amount (or
  a list) and the call carried what was sent, a setup copy of the steps
  where it keeps all, then the step where it answers twice what was
  sent, so a refund capped by what the receiver holds, not what it was
  sent, fires. Nothing is forced: a counterparty may answer anything.

Scans with edges send each call the witnesses made into an audited
contract from `axis.near` (`ft_register` it for tokens) after that
witness's earlier steps (`<site> sent directly from axis.near`), varying
deposit and account-id args; curves send only other contracts' calls.
`SELF_ONLY`: a self-only method ran for `axis.near` and changed state
(`anyone_may_call("*")` opts out); a callback reading a promise result
fails sent without one, so this can't tell if it's open: test by hand.

`Faults::default()` fails a call to another account, holds a receipt or
a result 1 to 3 blocks, starves a receipt of gas, and tries the edges
(`Faults::edges()`). `Faults::tolerable()` leaves out starvation, for a
regression a fixed build must survive; `Faults::none()` runs the
witnesses only. Those and `Faults::new` try no edges (the scan says so):
`.and(Faults::edges())` adds them. `Faults::new` refuses
`Receipts::in_tx`: hold across transactions in a plan step.

A witness whose transaction fails is a harness error in a scan, curves
or `Harness::run`, naming it and the checks that fired, unless a check of
yours (`must_not_fail`) fired at the failed receipt: then that finding
is the failure. Mark one meant to be refused `Plan::refused()`.
`tests/vault_books.rs` scans a buggy and a fixed vault: `assert_found`
on one, `found_at(..).is_none()` on the other (and see Diff, below).

A variant whose receipts differ from the witness's where its fault is
meant is an error: an op must submit the same transactions each time.
Name a gas floor that isn't a wasm constant in `Op::axes`:
`Axes::auto().floor("/Withdraw/min_gas", floor)`.

## Curves

`curves` measures what each receipt uses as an axis of a witness moves.
The axes:

- each number, each list's length and each string's bytes;
- the caller's gas;
- **repeat**: how often the step, or a call its receipts made into an
  audited contract, runs before it, each copy settled in turn;
- **fresh callers of**: the step's entry call from that many new
  signers, the accounts the op names fresh too;
- **tail**: how many such calls land, two blocks apart, while its callback waits.

A copy is the op itself unless `Op::copy` gives it a fresh nonce or
payload (a repeated call carries what copy `k` sent on a scratch world,
so a nonce `submit` takes from a counter in the world repeats: "nonce
already used"); an op reading a balance at submit (an `Amount`) reads what the last copy
left. Each axis climbs from the witness's value to 2048 times
it (a repeat or tail to 64, the caller's gas to the ceiling): every
64-fold, then the fourfold rungs between two that differ in a use, a
receipt or an outcome, halving where outcomes change.

A list grows by copies of its last item made fresh (`alice.near`,
`alice-1.near`, ...), or plain copies. A string that doesn't deserialize
once changed (a hash), a tuple's length, a choice and an unset field
aren't climbed. A list a receipt's call receives that stays as long with
each of the op's lists doubled is a `list:` item: no axis can grow it.

Each row is a (site, resource, axis) triple. The resources are:

- `ReceiptGas`: the gas a receipt burnt, against what it was attached;
- `PrepaidGas`: what the whole transaction burnt of its attached gas
  (not the refunds, nor the entry's prepaid exec fees), against the
  ceiling;
- `LogBytes`: bytes logged, against the host's 16 KiB;
- `StoragePaid`: storage its own account staked, against its free
  balance. Along `fresh callers of`, the site ends `(fresh callers)`: what
  the contract staked for the new callers, less their deposits, against
  10 times what they paid (gas, deposits). The row shows bytes per new
  caller and the ratio; past it, `storage paid by the contract` is a
  finding.

A row's `reach` says how it stands:

| reach | meaning | open? |
|---|---|---|
| `Crossed { at }` | a run at `at` hit the limit: a finding, with its replay, if a check fired in that run; else listed as harmless | finding, or harmless |
| `Below` | under the limit as far as `moved_to` | closed only that far |
| `EntryCap { cap, .. }` | the contract refuses the axis from `cap`, with evidence | no |
| `Stopped(why)` | the axis couldn't move further, and what to supply when the world refused a copy or a longer list. A refused copy reads "at N copies, copy k failed at `<site>`: <why>", naming the copy's caller and the witness's: a guard (a caller check, a lock while held, a used nonce) is excused by the item's own id, `.excuse("axis:step N:<axis>", "<the guard>")`; else give the copy's caller what it lacks in the fixture, or return a fresh value from `Op::copy` | open |
| `AtBound(why)` | the next rung's step, or a fresh caller of an op naming no account, was refused whole (a balance, an id's length): `stopped at a bound: <why>` | no |
| `NoEffect` | moving the axis didn't change this site's receipt | no |
| `Exhausted(why)` | a copy skipped for what the world had left: `world exhausted at n`, with the balance | no |

A row whose axis moved no further than 8 is open. Points a scan or a
search saw, passed in with `.with(&audit)` (or `.with(&report)`),
combine with the curve's own, coverage too. A point is crossed only if
every case at that value crossed. A row prints its reach first, then its
measured slope and largest use with the axis value it was at
(`at most 90 (at 1) of 100`); nothing is extrapolated.

```rust
let audit = harness().curves(&[batches(&[1])]).run();
let row = audit.assert_curve(WORK, Resource::ReceiptGas, "/Batch");
assert_eq!(row.reach, Reach::Crossed { at: 8 }, "{row}");
assert_eq!(row.limit, u128::from(Gas::from_teragas(20).as_gas()), "{row}");
assert!(row.points.iter().any(|p| p.0 == 7), "refined to 7: {row}");
```

A callback with a budget fixed when it's scheduled is a margin target
if a forced `OutOfGas` there shows a finding besides its own failure, or
`must_not_fail` names it. Its rows give its margin at each point
(`CurveRow::margins`: attached less used less forwarded). The curves
then move the axes along which it falls together (16 runs at most), and
plant trie keys beside each key the target wrote, keeping a point that
lowers it. An axis along which it keeps falling by over 1% of the
budget, or a margin under 25% of it ([fidelity.md](fidelity.md)), is a
`gas margin` finding with a replay.

`harness.curve(&plan, step, "/Withdraw/ids")` climbs one axis only, and
`.rungs(n)` to 2^(n-1) times the start. `Op::axes` returning
`Axes::only(["/Deposit", "repeat fresh callers"])` narrows a large op;
the summary lists the copies, not arguments, it drops (`left out by Axes::only`). A crossed row's finding is
shown, or forced when the curve held a receipt to land calls during it.

## Search

```rust
vault_on(wasm).search(plans(vault_ops()).len(1..=6)).seed(0x5EED_0000).seeds(2).run();
```

`plans(ops)` draws 1 to 6 steps, with gaps of 0 to 2 blocks or settling
(or to an epoch start, on short epochs), mixed caller gas (just enough
in one draw in sixteen), and a fault in a quarter of cases. Tune it with
`.len(..)`, `.gaps(..)`, `.gas(..)`, `.faults(..)` and `.repeats(..)`.
About one plan in eight repeats a step in one block whatever `.gaps(..)`
says; only `.repeats(false)` with `.gaps(Just(Gap::Settle))` keeps ops
apart. Judge shared state under overlap with `at_rest_ops`
([harness.md](harness.md)). A search runs 256 cases a seed; use
`.seeds(2)`: one can miss what two find. `NEAR_SIM_SEED` repeats a
search, and `NEAR_SIM_CASES` lengthens it. A list field an op sent at
one length in every case is a `length:` to-do item.

Each new finding, shown ones first, is shrunk for at most 15 minutes (a
cut shrink says so). One cause is one row: its other cases and checks
are in its `also`, where `assert_found` finds them.

Two ops that must meet (a request and its redeem, a sign and its
respond) pair in one of two ways. The second draws a `Pick`
(`input::pick()`), not a name, and resolves it when it submits against
what the contract or world holds then, skipping if there is none
(`tests/live_keys.rs`):

```rust
            QueueOp::Redeem(pick) => *pick.of(&queued(sim))?,
```

Or both derive what they share from one small key field (`k: u8` in
`tests/mpc.rs`), so a search with a few keys pairs them often. A drawn
id almost never matches a live one; either way does, and the plan stays
data that replays. A curve moves one step's fields, so it varies one
partner only: grow what both share in the witness itself.

## Findings

A finding's identity is its check, its site, the kinds of op that broke
it, its first failed receipt that moves value (method and failure
class), and the faults forced. Two causes at one check and one site are two findings,
each with its own replay; a forced finding of a check of the world stays
its own row (its fault is its cause). Each finding has a label:

- `shown` (`Label::Shown`): nothing forced, in the plan as written;
- `shown at <edge>` (`ShownAt`): nothing forced, at an edge a scan or a
  curve set (`/Withdraw/min_gas = 0, caller gas least + 0`);
- `forced (<fault> at <site>)` (`Forced`): after a fault the test forced.

Findings rank by consequence, each row's `kind`: books
and conservation, stranded, claim stuck, check, gas margin, storage
paid, failed receipt. A margin finding over 25% is an axis whose cost
grows faster than its budget; its legs say where they would meet
("cost grows, budget flat, meets it near 912").

## Replays

Each row a campaign reports is saved as `<check>@<site>-<hash>.json`:
its case as reported (shrunk, unforced, faults by receiver, method and
occurrence), with each folded finding its case shows; another cause
keeps its own file. The directory is the test's, under
`NEAR_SIM_FOUND_DIR`, else `$CARGO_TARGET_DIR/near-contract-sim/found`,
else `target/...`; `Harness::store(dir)` names another (give a spawned
thread or a binary's `main` its own: they share `found/unnamed`). The
hash covers the plan, the fixture and the checks, so a cited replay
keeps reproducing what it printed. Only a finished campaign writes;
nothing deletes. `Plan::load(path)` reads a replay's plan
(`Plan::from_json` returns why it can't). Commit the replays, and replay
them all:

```rust
fn forgetful_payout_stays_fixed() {
    let replays = vault().store(found_dir()).regressions();
    assert_eq!(replays.len(), 1, "{replays:#?}");
    for r in &replays {
        assert!(r.fixture_changed, "{r:#?}");
        match &r.status {
            Status::Gone(run) => run.assert_clean(),
            other => panic!("{other:#?}"),
        }
    }
}
```

`Shows`: one of its identities shows. `Gone`: the plan and its breaking
receipts' sites ran and none showed (another cause may still break the
check: `Gone(run) => run.assert_clean()`). `NotReached`: a site never
ran (or its method is gone), an op skipped, or a forced fault fit no
receipt: not a fix. `Stale`: its plan no longer reads, or an older format
wrote it: an open item. A harness lacking one of its checks skips it;
stderr counts what it read. Without `.store(dir)`, a missing directory
panics. `harness_changed()`: `Gone` or `NotReached` on its own fixture,
so an op or a check changed; regenerate it.

## Diff

```rust
let (fixed, forgetful) = (harness("vault.wasm"), harness("vault_forgetful.wasm"));
let found = forgetful.scan(&[deposit_then_withdraw()]).faults(Faults::tolerable()).run();
let kept = found.assert_found(KEPT, "world").run.plan().clone();

let diff = fixed.diff(&forgetful, &[deposit_then_withdraw(), kept]);
```

The diff reports only what differs: the books' readings on the two
fixtures (`diff.fixture`), once; per plan, each method's outcome
classes, the findings, and how far each reading moved from step 0.

## The summary and the to-do list

`audit.to_string()` (its `Display`) prints at most 60 lines: a headline
counting findings (shown and forced) and open items
(`audit.with_open_gaps(why)` puts `Coverage not achieved (why)` above
it), coverage, `not judged:` (each check's skipped judgments, and why:
never passes), faults, the findings ranked, the notable curve rows, and
the to-do items, each list cut short ("N more"; `{:#?}` prints all).

`audit.todo()` lists the open items, each with a stable id (`report()`
is the whole `Report`, as JSON too). The summary prints the
`unreached:` ones on one line, the `incomplete:` ones, and one `stopped:`
line per site and cause (a trap's frame, `an unnamed trap`, a refusal's
message) for the `axis:` ones a copy stopped, with a glob closing them.

| id | open because |
|---|---|
| `unreached:<account>::<method>` | no case, setup or view ran this exported method of an audited contract (a view a check reads ran; excuse the rest, `unreached:<account>::get_*`); a report that ran it closes it in `harness.union` |
| `axis:step N:<axis>` | a curve row stopped, or moved too little |
| `list:<site>` | a call receives lists, and refuses them sent directly; an excused one still prints in the headline |
| `unjudged:<check>` | no case judged this check (nothing ever owed; a book that never held anything) |
| `stale:<path>` | a replay can't be judged now |
| `replays:<dir>` | replays couldn't be written there |
| `truncated:depth N`, `unexplored:depth N` | an enumeration's `.cases` ran out at depth N, or never reached it (its notes say what it didn't vary) |
| `incomplete:<op>, <axis>` | cases not tried to their end, one item an op and axis (an op skipped at its edges): count, first case and why; `incomplete:<op>*` closes an op's, but a blanket `incomplete:*` hides those skips. Where checks not judging were a case's only gap, the family is the check: `incomplete:<check> not judged (overlapped)` (read `effects()`, or judge in `at_rest_ops`), or `incomplete:<check> not judged` |
| `errors` | harness errors, an op's `submit` panic among them (its message printed) |

`audit.done()` returns `Ok(summary)` only when nothing in that audit is
open ([harness.md](harness.md), "Excuses"): a scan's says nothing of
curves or a search not yet run. A gas band and `left out of the audit:`
(contracts `Harness::audits` didn't name) are information, not items. Saved reports join with
`harness.union(&[a, b])`, each run and seed once: a method no report
reached is unreached, every report's excuses apply, and `Report::done()`
says what's left.

## Cost

Each verb prints its case count and the time left (`Report::timings`
per variant), and each phase outside the count as it starts. A case
takes milliseconds on a WAT specimen, up to a second on near-sdk
contracts with tokens. A scan runs a few dozen variants per witness
step; curves at most 38 runs per axis. Narrow curves with `curve(..)`
or `Axes::only` before you cut seeds.

- A case past a minute stops and is a `timeout:` to-do item. A costly
  at-rest check can run once per case with `Harness::at_end` or
  `Books::at_end()`.
- `.saves(path, every)` saves a report as it goes and resumes on the same
  fixture, checks, binary and enumeration ops and faults; else it starts over.
