# Migrating from testing_env! and near-workspaces

Use this page to port a near-sdk unit test (`testing_env!`,
`VMContextBuilder`) or a near-workspaces test, one idiom a row. Every
call is a real transaction: a value the mock took as input is what the
chain holds when the call runs.

## testing_env! and VMContextBuilder

| unit test | simulator |
|---|---|
| `Contract::new(..)`, then a method on the struct | deploy, `init`, then `sim.call(..).transact()` ([quickstart.md](quickstart.md)) |
| reading a private field | `sim.contract_state::<T>(id)` with a mirror struct ([sim.md](sim.md)) |
| `current_account_id` | the account you deployed to |
| `predecessor_account_id` | the signer of `sim.call(signer, ..)`. A predecessor that isn't a signer: `.predecessor("token.near")` delivers the call as a receipt from that account ([transactions.md](transactions.md)) |
| `signer_account_id` | the signer, except in a relayed call: `.relay(relayer)` runs the actions with the sender as predecessor and the **relayer** as `signer_account_id` ([transactions.md](transactions.md)) |
| `signer_account_pk` | `sim.secret_key(id).public_key()`; another key with `.sign_with(key)` |
| `attached_deposit` | `.deposit(balance)` |
| `prepaid_gas` | `.gas(gas)`; 300 Tgas by default |
| `block_timestamp`, `block_index` | `sim.set_timestamp_ns(t)`, `sim.set_height(h)`: the next call's receipt runs at exactly `t` and `h`, and a view sees them. After `advance_by` or `advance_to`, a call runs a block or two later ([blocks-and-clock.md](blocks-and-clock.md)) |
| `epoch_height` | `sim.advance_epochs(n)`; `sim.epoch_of(h)` is block `h`'s |
| `account_balance` | the contract's balance: `sim.set_balance(id, amount)`, read with `sim.balance(id)` |
| `storage_usage` | `sim.account(id).unwrap().storage_usage` |
| `random_seed` | derived from `Config::seed` and the height: `Fixture::new().config(\|c\| c.seed = 7)` |
| `is_view(true)` | `sim.view(id, method, args)` |
| `promise_results` | `Tx::promise_results`, below |
| `get_logs()` | `out.logs()`; a view's, `sim.view(..).logs()` |
| `#[should_panic(expected = ..)]` | `out.assert_panic_contains(fragment)`: a substring, as `expected` is, and it returns the message. An exact one: `Failure::Panic`, below |

A callback on results you pick, the `promise_results` idiom. It runs as
the contract calling itself, so `#[private]` passes; sign it as any other
account ([transactions.md](transactions.md)).

```rust
let out = sim
    .call(ALICE, VAULT, "on_withdraw")
    .args_json(json!({ "account_id": ALICE, "amount": "40" }))
    .promise_results([PromiseResult::Failed])
    .transact();
assert_eq!(out.u128(), 0);
assert_eq!(u128_view(&sim, VAULT, "shares_of", ALICE), 40);
```

Access control on the predecessor, and a panic matched by a fragment
(`tests/quickstart.rs`):

```rust
let args = json!({ "sender_id": ALICE, "amount": "10", "msg": "" });
let out = sim.call(ALICE, VAULT, "ft_on_transfer").args_json(&args).transact();
out.assert_panic_contains("only the vault's token");
let out =
    sim.call(ALICE, VAULT, "ft_on_transfer").args_json(&args).predecessor(TOKEN).transact();
assert_eq!(out.u128(), 0, "{out}"); // the vault keeps all 10
```

### What stays a unit test

- Pure helpers, private methods and math without `env::`: keep them as
  plain `#[test]`s in the contract's crate. The simulator reaches only
  what the wasm exports.
- `Contract::default()` panicking before init: call any method before
  `init` and assert its panic.
- A private field: read it with `sim.contract_state::<T>(id)`, not a
  getter added for the test.

## near-workspaces

| near-workspaces | simulator |
|---|---|
| `near_workspaces::sandbox().await?` | `Sim::new()`, or a `Fixture` built once and cloned per test |
| `compile_project("./")` | the same call: `cargo_near_build::build_with_cli` in the test ([quickstart.md](quickstart.md)) |
| `worker.dev_deploy(&wasm)` | `sim.deploy(id, wasm)` at an id you pick, or `Fixture::contract` in a fixture |
| `dev_create_account()`, `account.create_subaccount("alice")` | `sim.create_account("alice.<parent id>", balance)`: any id, no parent needed. `sim.create_subaccount("parent.near", "alice", balance)` sends the parent's real transaction, and the new account signs straight away. To see the `CreateAccount` action's rules refuse one: `sim.tx("parent.near", "sub.parent.near").create_account().transfer(..)` |
| `account.call(id, m).args_json(..).deposit(..).transact().await?` | `sim.call(&account, id, m).args_json(..).deposit(..).transact()`, with no `await` |
| positional arguments, `.args_json(("hi", 1))` | the same, or `json!(["hi", 1])`: near-sdk reads an array in the order the method declares its arguments |
| `.max_gas()` | `.gas(max)` with `let max = sim.gas_ceiling();` (1 PGas, as near-workspaces 0.22.4 and later attach; older ones attach 300 Tgas, the default here) |
| `contract.view(m).args_json(..).await?.json()?` | `sim.view(id, m, args).json()` |
| `account.view_account()`, `.balance` | `sim.account(id)`, `sim.balance(id)` |
| `account.transfer_near(to, amount)` | `sim.tx(from, to).transfer(amount).transact()` |
| `contract.batch()` | `sim.tx(signer, receiver)` with several actions ([transactions.md](transactions.md)) |
| `patch_state` | `sim.set_state`, `sim.update_contract_state` |
| `worker.fast_forward(n)` | `sim.advance_to(sim.height() + n + 1)`: a sandbox lands `n` blocks past the next one |
| `outcome.is_success()` | `out.is_success()` |
| `outcome.logs()` | `out.all_logs()`. The simulator's `logs()` leaves out failed receipts' logs, whose state rolled back; workspaces' includes them |
| `outcome.receipt_outcomes()`, `failures()` | `out.receipts`; a failed receipt has `is_success() == false` |
| `outcome.into_result().unwrap_err()` and a substring | `out.assert_failure()`, a `Failure` to compare exactly, below |
| `total_gas_burnt` | `out.gas_burnt()`, refund receipts' uncharged gas included; what the signer paid is `out.effects().gas_paid()` |

An exact error. `require!` and `env::panic_str` give `Failure::Panic`
with the message alone, without nearcore's `Smart contract panicked:`.
`assert!`, `assert_eq!` and `panic!` go through near-sdk's panic hook,
so theirs starts with `panicked at src/lib.rs:L:C:` and a newline:
match those with `assert_panic_contains`.

```rust
let out = withdraw(&mut sim, ALICE, 1).transact();
assert_eq!(out.assert_failure(), Failure::Panic("not enough shares".into()));
```

The other `Failure`s are in [outcomes-and-effects.md](outcomes-and-effects.md);
`Action(..)` holds a `near_contract_sim::ActionErrorKind`.

## What else differs

- A `Tx` borrows the `Sim` mutably until it's sent, so
  `sim.call(..).args_json(json!({ "at": sim.timestamp_ns() }))` doesn't
  compile: read what you need first, `let now = sim.timestamp_ns();`.

- `Fixture::init` signs as the contract (its gas refund lands after
  `init`) ([sim.md](sim.md)).
- A refused top-level call isn't a finding: assert on the outcome.
- Self-moves are left out of `near_net`, `near_paid` and
  `near_deposited` ([outcomes-and-effects.md](outcomes-and-effects.md)).
- Queued calls prepay 300 Tgas each ([blocks-and-clock.md](blocks-and-clock.md)).
