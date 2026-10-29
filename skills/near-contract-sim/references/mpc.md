# MPC-style tests

Use this page to test a contract built on yields: a request parks a
callback, and an off-chain party answers with a resume, or the request
times out. near/mpc's chain-signatures contract is the example; the
recipe fits any oracle or request-and-response contract.

The example is `tests/mpc.rs`. Its helpers in `tests/mpc_fixture/mod.rs`
(`world`, `sign_request`, `request_of`, `signature_for`, `respond`,
`PARTICIPANTS`) are the test's own, a recipe to copy, not crate API.

## 1. Build the world

Reach a state where requests are served the way the contract's own tests
do, with real calls, and patch what can't be reached that way.
`mpc_fixture::world()` runs `init_running` with a domain whose root key
the test knows, so `respond` can sign what the contract checks.
`dcap_world()` deploys MPC with its real TEE verifier, sets the clock
inside the DCAP quote's collateral window before anything runs, and
patches the contract's `STATE` with `set_state` to allow the quote's
image hashes.

## 2. A request, answered or timed out

Submit the request and settle: the yield stays parked. Race the answer
and the timeout from clones; the clock moves only when you say, so there
are no sleeps and no polling. The whole test is in
[blocks-and-clock.md](blocks-and-clock.md), "Yields": one yield parked
with the callback you expect, the request incomplete while parked, an
answer winning one block before `expires_at` and the timeout at it, and
the bill, `effects().near_net(USER)` and `gas_paid()`.

## 3. A verifier that says no, or never answers

Replace the counterparty with a stub that rejects, or fail the real one's
receipt with a fault. Either way, assert what the failed request
consumed: no attestation grant (`tests/mpc.rs`):

```rust
    let out = submit_quote(&mut sim)
        .inject(Receipts::call(VERIFIER, "verify_quote"), Fault::OutOfGas)
        .transact();
    assert!(out.is_failure());
    assert_eq!(out.expect_receipt("verify_quote").fault, Some(Fault::OutOfGas));
    assert_eq!(grants(&sim), 1);
```

The stub version uses `deploy_stub` and `calls_to` ([stubs.md](stubs.md)).
On the happy path, `out.assert_headroom("resolve_verification",
Gas::from_teragas(10))` checks the callback's margin: the real verifier
burns about 173 Tgas, so a budget near it is worth watching.

## 4. A harness: requests and answers in any order

Requests overlap, answers come late or not at all. Write two ops:
`Sign`, and `Respond { k }`, which answers the `k`th parked request and
skips when none is ([harness.md](harness.md)). The per-op check runs once
the sign's whole tree is complete, so a parked request is judged after it
is answered or timed out; the callback's moves count in the sign's
effects, not the respond's. A case times out every parked yield at its
end.

`tests/mpc.rs` adds an `at_rest` check that no sign stays parked and
searches; its only finding is forced (a starved timeout handler).

## What this doesn't show

The test plays the nodes with a key it knows, so it checks the
contract's side of the protocol, not the network's. A timeout is a block
count, not wall time. Measure gas on the real verifier, not a stub.
