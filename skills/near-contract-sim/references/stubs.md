# Stubs

Use this page when a test needs another contract to answer a certain way:
a token that refuses a transfer, a verifier that rejects a quote, an
oracle with a fixed price, a receiver that returns a bad amount. Also to
see every call an account received.

## Stand in for a contract

A `Stub` is a contract with declared methods and a `Reply` for each:

- `Reply::json(v)`, `Reply::bytes(b)` (malformed JSON too) or
  `Reply::panic(msg)`. `Reply::json(())` returns nothing, as a near-sdk
  method returning `()` does;
- `.logs(lines)`, `.burn(gas)`, `.burn_leaving(gas)`, `.fail_below(gas)`
  (panics when attached less), `.at_limit()` (the largest reply the host
  returns) and `.pad_to(n)`.

`sim.deploy_stub(id, stub)` deploys it with a real transaction, creating
`id` or replacing its code. It produces blocks: deploy every stub before
you queue the transactions that call it, never between `Tx::submit` and
settling. The vault's token, paused (`tests/vault.rs`; `shares` reads `shares_of`):

```rust
#[test]
fn a_token_that_refuses_the_payout() {
    let mut sim = world();
    deposit(&mut sim, "alice.near", 100).transact().assert_success();
    sim.deploy_stub(TOKEN, Stub::new().on("ft_transfer", Reply::panic("the token is paused")));
    let w = withdraw(&mut sim, "alice.near", 40).transact();
    assert_eq!(w.u128(), 0);
    assert_eq!(shares(&sim, "alice.near"), 100);
    let calls = sim.calls_to(TOKEN);
    let payout = calls.last().unwrap();
    assert_eq!((payout.method_name.as_str(), payout.succeeded), ("ft_transfer", false));
    assert_eq!(payout.args_json::<serde_json::Value>().unwrap()["amount"], "40");
}
```

- `sim.stub_reply(id, method, reply)` changes one reply for every caller.
  Replies live in the stub's storage, so a clone keeps its own.
- `tx.stub_reply(id, method, reply)` sets the reply only for calls that
  descend from that transaction. This is how an op makes the stub's
  answer one of its fields, so a search draws hostile answers and a
  finding replays with its answer ([harness.md](harness.md)). Binding one
  method several times queues the replies in the order calls arrive. The
  logs, call, gas and padding a bound reply leaves unset come from the
  stub-wide reply, so `sim.stub_reply(.., Reply::json("0").burn(gas))` keeps
  burning under an op's own answer.
- A view of a declared method answers with the stub-wide reply, so a
  stubbed token works with balance checks.
- An undeclared method fails as on chain, with `MethodResolveError`.
- `Fixture::stub(id, stub)` deploys one as a setup step.
- A hostile NEP-141 receiver answers `ft_on_transfer` with
  `Reply::json("999")`, or panics. A standard one, keeping or refunding
  by `msg`, is the repo's `tests/res/ft_receiver.wat`.

## A counterparty that acts

`Reply::call(receiver, method, args).then(answer)` makes the stub call
`receiver` (`.deposit(d)` attaches NEAR from the stub's balance) and then
answer. The call leaves before the answer, so it runs before the callback
that reads the answer: a receiver that calls back into the contract
mid-flow. From `tests/stubs.rs`, a stub that calls `bob.near` with 1
yoctoNEAR before it answers:

```rust
let reply = Reply::call("bob.near", "ping", json!({ "n": 1 }))
    .deposit(Balance::from_yoctonear(1))
    .then(Reply::json("answered").logs(["called first"]));
```

A counterparty can only spend what it holds: in the fixture, register a
hostile receiver with every token the contract can send it, and fund it,
so the contract, not a missing registration, is what stops it.

## See the calls an account got

`sim.calls_to(id)` lists every function call `id` received, failed ones
included: `method_name`, `args` (`args_json::<T>()`), `predecessor_id`, `signer_id`,
`deposit`, `gas`, `block_height`, `succeeded` and `receipt_id`.
It works on any account, not only stubs. Views aren't listed. In
`tests/mpc.rs` a stub verifier rejects the quote, and the test checks
the one call it got and the gas MPC attached ([mpc.md](mpc.md)).

## A stub, a fault, or the real counterparty?

A stub really answers that way, so its findings are natural; a fault is
forced, labelled `forced (..)` ([faults.md](faults.md)). Stub a callee
that is untrusted or can change (a token, an oracle, a receiver); fault
one you deploy yourself. A stub's gas, refunds and logs are invented:
for a token standard the contract moves value through, test against
real code too (`tests/invariants_intents.rs` runs near/intents as its
own NEP-245 counterparty), and keep stubs for answers no honest
contract gives.

A token stub declares the NEP-145 methods the contract calls
(`storage_deposit`, `storage_balance_of`, `storage_balance_bounds`)
beside `ft_transfer` and `ft_transfer_call`: an undeclared one fails,
and a scan only holds or fails declared ones.

For a callee with no methods to declare, the `wat` crate compiles a
module in a line (`tests/docs.rs`): an exported `unreachable` fails with
`Failure::Host`, and `(loop (br 0))` runs out of gas.
