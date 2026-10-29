# Outcomes and effects

Use this page to assert on what a call did: its status and return value,
one receipt, logs and events, gas, and who paid whom, in NEAR and in
tokens, while other calls run in the same blocks.

## The outcome

`transact()` and `sim.outcome(hash)` return a `TxOutcome`, every receipt
included:

- `is_success()`, `is_failure()`, `is_pending()`; `is_complete()` once
  every receipt has run, refunds and detached calls included.
  `is_success()` is the status RPC reports, which follows the result
  path only: a detached call that failed leaves it true. Check
  `receipts.iter().all(|r| r.is_success())` or `sim.findings()` for those;
- `assert_success()`, `assert_failure() -> Failure`, `json::<T>()`: each
  panics with the receipt tree and gas table when it doesn't hold;
- `value()`, `failure()`, `tokens_burnt()`, `gas_burnt()` (refund
  receipts' uncharged gas included: for money, `effects().gas_paid()`); `logs()`
  (succeeded receipts only; `all_logs()` adds failed ones, whose state
  rolled back);
- `receipts`: every `ReceiptOutcome`, in execution order, refunds
  included. `first_call()` is the transaction's own call (an `Option`:
  `None` while queued or refused);
  `receipt(method)` is the first that ran `method`, and
  `expect_receipt(method)` panics if none did;
- `pending`: the receipts still waiting, and why.

`Failure` is `Panic(msg)`, `OutOfGas`, `GasLimitExceeded`,
`AccountDoesNotExist(id)`, `InvalidTx(..)`, `Action(..)` or `Host(msg)`.
`println!("{out}")` draws the receipt tree with each receipt's gas and
logs, and a gas table, cut to about 100 lines (repeats collapse; a long
tree keeps its ends and first failure); `{out:#}` prints all of it,
refunds and full logs too. Put `"{out}"` in assert messages. An
assert's panic opens with `first failure: #h receiver::method: why`,
which can be a hop before the receipt that failed the transaction.

A `ReceiptOutcome` has `receipt_id`, `receiver_id`, `predecessor_id`, `method_names`,
`args_json::<T>()`, `deposit`, `attached_gas`, `gas_burnt`,
`headroom()`, `block_height`, `logs`, `status`, `fault` and `held` (what
a test forced on it), `token_moves` (watched token moves) and `unhandled`.
Its `args_json` is a `Result<T, serde_json::Error>`, `Err` for a refund or a
batch, so a loop can skip those; a stub's `IncomingCall::args_json` is one too.

Check what the operation did, not only its status. A resolver can catch a
receiver's panic, refund the tokens and return success. A later receipt's
failure doesn't undo what an earlier one committed. `out.events()` parses
the NEP-297 `EVENT_JSON:` logs of receipts that succeeded, in order
(`tests/calls.rs`):

```rust
assert_eq!(out.events()[0]["event"], "ft_transfer", "{out}");
```

## Gas

Gas is nearcore's, to the unit ([fidelity.md](fidelity.md)). A receipt's
`headroom()` is the gas it attached and didn't use.
`out.assert_headroom(method, min)` panics unless the first receipt that
ran `method` succeeded with at least `min` left: the margin of a
fixed-budget callback. A receipt that forwarded all its unused gas to its
calls (near-sdk's weighted forwarding) has no headroom of its own: assert
on its calls instead (`tests/docs.rs`):

```rust
    out.assert_headroom("factorial_mult", Gas::from_teragas(1));
```

A receipt's line can read "attached 5.00 Tgas, burnt 5.91 Tgas (5.00 +
0.91 exec fees)": its own static execution fees (the receipt and each
action) were prepaid by the sender outside its attached gas, as on a
node.

Compare `out.gas_burnt()` with a number within a tolerance, not exactly:
a contract or nearcore upgrade moves it. The least gas a call needs is `Tx::least_gas`
([transactions.md](transactions.md)).

## Effects: who paid whom

`out.effects()` reads only the transaction's own receipts, so it stays
exact while other calls overlap it. A balance diff mixes every call in
those blocks, and gas.

- `near_net(who) -> Delta` (compares with an `i128`): NEAR in minus out, gas excluded.
- `near_paid(from, to) -> Balance`.
- `gas_paid()`: the signer's gas. The signer's balance changes by
  `near_net(signer)` less `gas_paid()`, exactly.
- `near`: each `NearMove`, as `Deposit`, `DepositRefund` or
  `AccountDeletion`. Gas refunds are never value.
- `token_net(token, who)` and `token_supply_net(token)`, for watched
  tokens.
- `storage_bytes(who) -> i64`: the `storage_usage` the transaction
  added to `who` (negative: freed); `near_deposited(who)`: NEAR that
  reached `who` in receipts that succeeded. When the bytes cost more
  than the deposit, the contract paid for its caller's storage. Judge
  by the transaction's signer: `ft_on_transfer` runs with the token as
  predecessor (`tests/effects.rs`):

```rust
    let bytes: i64 = e.storage_bytes(TOKEN);
    assert!(bytes > 0, "{out}");
    let cost = sim.storage_cost(u64::try_from(bytes).unwrap_or(0));
```

`near_net`, `near_paid` and `near_deposited` leave two things out:

- **Self-moves**: a deposit or refund from an account to itself (a
  contract paying itself, a self-signed call's deposit). `near_net`
  reads 0 for them and `near_paid(a, a)` is 0, so an assert on them can
  pass vacuously. `effects().near` lists every move, self-moves
  included.
- **Burnt charges**: gas (`gas_paid()`), and NEAR the runtime burns,
  such as a global contract's storage ([transactions.md](transactions.md)),
  which is in `out.tokens_burnt()`.

## Tokens

Declare the tokens you care about with `sim.watch(token)` or
`Fixture::token(token)`, a `near_contract_sim::Token`:

- `Token::nep141(id)` reads `ft_balance_of` and `ft_total_supply`;
- `Token::nep245(id, token_id)` reads `mt_balance_of` and `mt_supply` for
  one token id; name it as a pair, `token_net((INTENTS,
  "nep141:wrap.near"), who)`. `Token::nep245_ids(id, [a, b])` watches
  several ids; `.discover()` also watches ids minted mid-case;
- `Token::nep171(id)` reads per-owner NFT counts and the total supply;
- `Token::view(id, method)` reads any balance view returning a `U128`
  string or a number; `.arg(name)` renames its `account_id` argument,
  `.supply(method)` adds a supply view;
- `.pure()` says the views read only the contract's storage, which
  halves their cost (not for a rebasing token).

Holders are every account in state, and each id a standard event of the
token's own receipts names, from that receipt on. `Token::holders(ids)`
adds ids no account or event gives: implicit ids, users a contract keeps
inside itself (`tests/invariants_intents.rs`):

```rust
    world.sim.watch(Token::nep245_ids(INTENTS, ids).holders(&users));
```

Around each receipt that writes a watched token's storage, the
simulator runs the holders' balance views and the supply view, so a
silent mint or burn is measured too. A watched case runs twice as long.

## Delta and Unobserved

`token_net` and `token_supply_net` return a `Delta`, a signed change
over all of `u128`: compare it with `Delta::gain(amount)`,
`Delta::loss(amount)` or an `i128` literal (`d == -5`), never `amount as
i128`, which wraps; `i128::try_from(d)` is `Err` out of range.

`token_net` returns `Result<Delta, Unobserved>`. It is `Err`, never
`Ok(0)`, when the token isn't watched, when tokens went to an id nobody
watches (with a supply view, the holders' changes don't add up), or when
a view failed. A check that gets `Unobserved` counts as not judged, never
as a pass. Make it measurable: watch the token, or add the missing
holder with `holders`.

## Attribution

Effects follow receipts: a move belongs to the transaction whose receipt
made it. A resumed callback belongs to the transaction that yielded; an
op that settles others' claims carries their payouts (filter by
recipient); a relayed deposit shows from the sender, though the relayer
paid it; stake returns and epoch rewards come from no receipt.

## Findings

`sim.findings()` judges every receipt so far with three built-in checks:
an unhandled failure (a failed receipt whose failure reached nobody), a
failed callback, and conservation (NEAR that appears or vanishes). A
failure on code deployed since its transaction began says so.
`assert_none()` panics listing them; each `Finding` has `check`, `site`,
`message`, `block`, `tx` and `receipt_id` (`tests/vault.rs`):

```rust
let findings = sim.findings();
assert_eq!(findings.len(), 2, "{findings}");
assert!(findings.iter().all(|f| f.check == builtin::UNHANDLED_FAILURE));
assert!(findings.iter().all(|f| f.site.to_string() == "rewards.near::on_claim"));
```

A failure that reaches the caller isn't unhandled: a refused top-level
call, or a callback that fails as the transaction's result (a starved
resolver, say). The outcome shows it, and `findings()` stays empty, so
assert on the outcome. A failure a fault forced is excused
([faults.md](faults.md)).

The built-ins don't know your books: the vault's planted bug passes them
all. That takes checks of your own ([harness.md](harness.md)).
