# Transactions

Use this page to call a contract, send several actions, pick gas, sign
with other keys, relay a meta transaction, sign a payload a contract
verifies, publish and use global contracts, or deliver a call as a
callback or from another predecessor.

## Build and send

`sim.call(signer, receiver, method)` builds a function call;
`sim.tx(signer, receiver)` builds a transaction for any actions. Both
return a `Tx`:

- `.args_json(value)`, `.args_borsh(value)` or `.args(bytes)`. A
  near-sdk `U128` travels as a JSON string: send `amount.to_string()`.
- `.deposit(balance)`: a `Balance` (`from_near`, `from_millinear`,
  `from_yoctonear`, `as_yoctonear()`), near-sdk's `NearToken` by another name.
- `.gas(gas)`: a `Gas` (`Gas::from_teragas(30)`, `as_gas()`). In JSON
  args it is a number, where near-sdk's `Gas` writes a string. It also
  pins the call's gas, so exploration leaves it alone.
- `.transfer`, `.deploy`, `.create_account`, `.add_key`,
  `.add_function_call_key`, `.delete_key`, `.deploy_global`,
  `.use_global`, `.transfer_to_gas_key`, and
  `.action(a)` for any nearcore `Action` (a NEP-616 state init, say).

Then one of:

- `.transact()` submits, produces blocks until idle and returns the
  `TxOutcome`. A plain call takes 2 or 3 blocks. It never panics on a
  failed call: assert on the outcome
  ([outcomes-and-effects.md](outcomes-and-effects.md)).
- `.submit()` queues it for the next block and returns its hash, with no
  block produced. Read it later with `sim.outcome(hash)`. To land many
  calls in one block, submit them all, then settle
  ([blocks-and-clock.md](blocks-and-clock.md)).

## Gas

A call attaches 300 Tgas by default, what wallets and SDKs attach. The
ceiling is higher: `sim.gas_ceiling()`, the protocol's
`max_total_prepaid_gas`, 1 PGas for one transaction's calls together.
One gas more fails at conversion with `TotalPrepaidGasExceeded`.
`config.gas_limits = GasLimits::Legacy300` (or
`NEAR_SIM_GAS_LIMITS=legacy300`) applies the old 300 Tgas limits.

`tx.least_gas()` bisects, on copies of the simulator, for the least gas
the transaction's unpinned calls can attach and still succeed, to 1 gas
(it panics if `.gas(..)` pins them all). Nothing is submitted; `Err`
holds the outcome at the ceiling (`tests/floor.rs`):

```rust
        let least = withdraw(&mut sim, 10, min_gas).least_gas().expect("it succeeds at the most");
```

A detached call or a callback off the result path can still starve just
above it: assert its margin with `out.assert_headroom(method, min)`
([outcomes-and-effects.md](outcomes-and-effects.md)).
`tx.gas_edges(range, step, explore::OutcomeShape::of)` splits the caller
gas into regions of one outcome shape, each edge bisected to 1 gas (a
`bool` key, `|out| ..`, gives windows), probing around each edge and its
gas constants; `explore::edges` does the same over any `u64`. A scan
doesn't run it: call it where gas matters.

## Keys and signers

`create_account` derives each account's key, and `sim.secret_key(id)`
returns it. `create_account_with_key(id, balance, &key)` and
`set_key(id, &key)` use yours. `create_subaccount(parent, name,
balance)` sends the parent's real transaction.
`.add_function_call_key(pk, receiver, methods, allowance)` adds a
function-call key (`&[]`: any method; `None`: no limit); `.sign_with(key)`
signs with it. A deleted signing key panics naming it: `set_key`
another. A `Tx` holds the `Sim` mutably, so bind a key the `Sim` holds
first: `let key = sim.secret_key("bob.near");`. The nonce is one above
the key's on chain and its queued transactions, a key a contract added
included; `.nonce(n)` overrides it.

`.relay(relayer)` wraps the transaction in a meta transaction the
relayer sends and pays for. The actions run with the sender as
predecessor, but the relayer as `signer_account_id`: a contract that
keys on the signer sees the relayer (`tests/delegate.rs`; `near(n)` is
`Balance::from_near(n)`):

```rust
    let out = sim.tx("alice.near", "bob.near").transfer(near(2)).relay("relayer.near");
```

`relay(relayer)` returns the relayer's outcome, which succeeds even when
the inner call fails: look in its `receipts`, or at `sim.findings()`.
`sign_delegate(max_block_height)` returns the signed delegate, to tamper
with or send yourself.

## Signed payloads and the clock

A contract that verifies what a user signed off-chain (a NEP-413
message, an intent, a permit) takes a signature by the user's key:
`sim.secret_key(id)`, or `SecretKey::from_seed(KeyType::ED25519, seed)`
so a replay signs the same bytes. For NEP-413, near-kit's
`nep413::serialize_message` computes the hash ([near-kit.md](near-kit.md))
(`tests/accounts.rs`):

```rust
    let hash = nep413::serialize_message(&params);
    let key = sim.secret_key("bob.near");
    let signature = key.sign(hash.as_ref());
    assert!(signature.verify(hash.as_ref(), &key.public_key()));
```

`near_contract_sim::Signature::ED25519(s)` (near-crypto's) holds the 64
raw bytes, `s.to_bytes()`, for a contract that takes them. A key prints
as `ed25519:<base58>`. near/intents' `raw_ed25519` payload is its JSON
text signed as bytes (`tests/invariants_intents.rs`).

- **Deadlines** come from the chain's clock, `sim.timestamp_ns()`, never
  the host's: `SystemTime::now()` makes a case replay differently, and
  the contract compares against the block's time. The clock starts at
  2020-09-13T12:26:40Z, a second a block ([blocks-and-clock.md](blocks-and-clock.md)).
  Sign a deadline far ahead, or the block time plus the window under test.
- **Nonces**: keep a counter in the world or the op, or use the
  timestamp, so each payload gets a fresh one and a replay rebuilds the
  same ones. A nonce-reuse test signs one twice on purpose.
- **Argument types.** The contract's `AccountId`, `U128` or `PublicKey`
  aren't nearcore's: send JSON ([quickstart.md](quickstart.md)).

## Global contracts

`.deploy_global(wasm, mode)` publishes code once: by its hash
(`GlobalContractDeployMode::CodeHash`, immutable) or under the
publisher's account (`AccountId`, upgradable: users follow each new
publish). The publisher pays by burning NEAR at the global storage
price per byte, so the charge is in `tokens_burnt()`, not in `near_net`.
`.use_global(GlobalContractIdentifier::..)` points an account at it
(both types are at the crate root, `near_contract_sim::`);
`sim.global_contract_code(id)` reads what was published, and
`sim.account(id)` has `global_contract_hash` or
`global_contract_account_id` (`tests/global_contracts.rs`):

```rust
    sim.tx("alice.near", "sub.alice.near")
        .create_account()
        .transfer(near(5))
        .use_global(GlobalContractIdentifier::AccountId("owner.near".parse().unwrap()))
        .transact()
        .assert_success();
```

`sim.global_storage_cost(wasm.len() as u64)` is that charge, as
`sim.storage_cost(bytes)` is an account's storage stake.

## A callback on results you pick

`.promise_results(results)` delivers the call as the contract's callback
to itself, on `PromiseResult::Successful(bytes)`,
`PromiseResult::json(v)` or `PromiseResult::Failed`, as `promise_and`
would join them: the `testing_env!` idiom ([migrating.md](migrating.md)
has an example). It runs with the contract as predecessor, so
`#[private]` passes. The signer must be another account: a call to its
own signer runs before it could be rewritten, and `promise_results`
panics saying so. The deposit comes from the signer. Its failure is the
transaction's result, so `sim.findings()` doesn't flag it: assert on the
outcome. To fail a real callee inside the whole flow, use `Tx::inject`
([faults.md](faults.md)).

## A call from another predecessor

`.predecessor(id)` delivers the call as a receipt from `id`, as if
`id`'s contract sent it, with no contract at `id`: "only the token may
call `ft_on_transfer`" in one line ([migrating.md](migrating.md) has an
example). The signer still signs and pays; a failed deposit goes back
to it. Its failure is the transaction's result, and the tree marks the
receipt `[delivered from <id>]`. Panics if the signer is the receiver,
with `promise_results`, or in a delegate.
