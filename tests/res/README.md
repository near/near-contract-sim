# Test fixtures

The contracts the tests deploy, and what each is for. A `.wat` file's
header comment lists its methods and their arguments; the tests compile
it with the `wat` crate. A `.wasm` built here has its source in
[tests/contracts](../contracts), and near-sdk's prebuilt examples are
credited in [NOTICE.md](NOTICE.md). `intents/` holds near/intents'
contracts ([intents/README.md](intents/README.md)) and `mpc/` near/mpc's
([mpc/README.md](mpc/README.md)).

"Used by" names the test files (under `tests/`, or in `src/` for unit
tests) that load each one, directly or through a helper.

| file | what it is | used by |
|---|---|---|
| `abort_ledger.wat` | A ledger that panics as a `panic = "abort"` build does (`unreachable`): `withdraw` traps above 100; `relay` calls its own `withdraw`, detached. | `explore_scan.rs`, `trap_site.rs` |
| `batch.wat` | A keeper and worker in one module: `batch(n)` gives `work(n)` a fixed 20 Tgas, which runs out at 8 items; `register`, `touch`, an `items` view. | `batch.rs`, `custody_batch.rs`, `outsider.rs` |
| `budget_ledger.wat` | `put` appends ids, then runs `on_put` on 4 Tgas plus 2 Tgas an id, a budget that misses the cost of moving the list's last entry. | `margin.rs` |
| `callback_probe.wat` | `start` and `start_and` chain calls into `cb`, which returns its signer, predecessor, deposit, gas and each promise result: for `Tx::promise_results`. | `src/explore/hook.rs`, `src/outcome.rs`, `callbacks.rs` |
| `claim_queue.wat` | `request` queues a claim under the next id; `redeem(id)` removes it or panics "no such claim"; a `count` view. | `src/explore/memo_tests.rs`, `explore_enumerate.rs`, `live_keys.rs` |
| `cross_contract_high_level.wasm` | near-sdk 5.29.0's cross-contract example: `factorial(n)` calls itself and multiplies in `factorial_mult` callbacks ([NOTICE.md](NOTICE.md)). | `examples/tour.rs`, `calls.rs`, `docs.rs`, `gas_vs_sandbox.rs`, `outcomes.rs` |
| `custody.wasm` | An NEP-141 custody ([tests/contracts/custody](../contracts/custody)): deposits through `ft_on_transfer`, an optional `notify` and refund, `withdraw` re-crediting a failed payout. | `curves.rs`, `intents_like.rs`, `report.rs` |
| `custody_batch.wat` | A custody and token pair: `withdraw` debits, then the token's `work` runs on 20 Tgas and runs out at 8 items; `on_withdraw` never re-credits. | `custody_batch.rs` |
| `custody_gas_bug.wasm` | custody.wasm with its `bug_gas` feature: `resolve_deposit` gets a flat gas budget, so many positions starve it and leave credit unbacked. | `curves.rs`, `intents_like.rs`, `report.rs` |
| `custody_log_bug.wasm` | custody.wasm with its `bug_log` feature: the refund log isn't size-checked, so long ids or memos pass 16 KiB and trap the resolver. | `curves.rs`, `intents_like.rs` |
| `desk.wasm` | A ticket desk ([tests/contracts/desk](../contracts/desk)): `redeem` calls the issuer's `issue`, then `on_issued`, whose budget is sized to the event bytes. | `explore_scan.rs`, `report.rs` |
| `desk_band.wasm` | desk.wasm with its `band` feature: inside a band of caller gas, `release` runs out and the pending amount stays locked. | `curves.rs` |
| `drivers_capped.wasm` | drivers_funded.wasm with `capped`: `pledge` refuses 8 or more ids. | `curves.rs` |
| `drivers_funded.wasm` | A holdings contract ([tests/contracts/drivers](../contracts/drivers)): `settle` then `on_settled`, whose cost per id grows with the entries after it; `fund`, `issue`, `pledge`. | `curves.rs` |
| `failure_probe.wat` | `hello`, `write`, `write_then_panic` ("boom"), and `send_to_missing`, which sends 1 NEAR to a missing account and logs the callback's result. | `accounts.rs`, `calls.rs`, `chain.rs`, `color.rs`, `effects.rs` and 2 more |
| `flight_lock.wat` | `start` sets a busy flag until `finish`; `sync` panics and `try_sync` returns false while busy; `work` always ends in a failing call; epoch checks. | `src/explore/enumerate.rs`, `src/explore/memo_tests.rs`, `explore_enumerate.rs`, `explore_run.rs` |
| `floor.wasm` | floor.wat compiled (a test checks they match); the specimen of audit.md's first harness. | `first_harness.rs`, `floor.rs` |
| `floor.wat` | A keeper and token pair: `withdraw` pays through the token's `transfer_call`, forwarding a gas floor the caller signs; at the floor the payout fails and the amount is lost. | `first_harness.rs`, `floor.rs` |
| `forwarder.wat` | `forward` (payable) calls the receiver's `take` with the deposit, then `on_forward`; the bug build keeps the NEAR of a failed `take`. | `books.rs` |
| `ft_receiver.wat` | A standard NEP-141 receiver answering as near-sdk's test-contract-defi: `ft_on_transfer` keeps all on msg `take-my-money`, returns a numeric msg as the unused amount (`"40"` on 100 keeps 60), and panics "Not an integer" on any other msg. | `token_effects.rs` |
| `fungible_token.wasm` | near-sdk 5.29.0's fungible-token example, an NEP-141 token with storage management ([NOTICE.md](NOTICE.md)). | `src/explore/memo_tests.rs`, `src/fixture.rs`, `calls.rs`, `curves.rs`, `effects.rs` and 10 more |
| `fuzz_contract.wasm` | A programmable contract ([tests/contracts/fuzz-contract](../contracts/fuzz-contract)): `run` executes a borsh list of storage, env, log and promise ops; a `digest` view. | `chain.rs`, `congestion.rs`, `delegate.rs`, `differential.rs`, `differential/` and 12 more |
| `gas_split.wat` | `split` keeps a reserve and gives a detached `child` a quarter of the rest; it fails only in a 4-gas window where the child would get 0. | `gas_edges.rs` |
| `gasmax.wasm` | A journal ([tests/contracts/gasmax](../contracts/gasmax)): `put`, `remove`, and `settle` then `on_settled`, whose cost depends on the last entry's size. | `curves.rs` |
| `guestbook.wat` | `sign` stores a raw message under the caller's id with no deposit, so the contract pays for the storage; `sign_paid` asks for about 0.01 NEAR. | `curves.rs`, `docs.rs` |
| `holder_counter.wat` | A staking pool's holder count: `stake`, `unstake`, `transfer`, a `holders` view; every partial unstake lowers the count, which underflows. | `holder_counter.rs` |
| `host_probe.wat` | Views that call host functions directly: `keccak256`, `ecrecover`, `ed25519_verify`, `bls12381_p1_sum`, `alt_bn128_g1_sum`. | `docs.rs` |
| `keeper.wasm` | A multi-token keeper ([tests/contracts/keeper](../contracts/keeper)): `on_transfer` credits ids from any token; `notify` calls the receiver, then `on_resolve` refunds. | `curves.rs`, `keeper.rs` |
| `keeper_closed.wasm` | keeper.wasm with `closed`: `on_transfer` accepts only the token it was initialised with. | `keeper.rs` |
| `keeper_ping.wasm` | keeper.wasm with `ping`: `ping` credits ids, then `on_ping` hashes rounds per id on a fixed budget. | `curves.rs` |
| `keeper_ping_hashed.wasm` | keeper_ping.wasm with `hashed`: every storage key is hashed, and `on_transfer` takes `previous_owner_ids`. | `curves.rs` |
| `ledger_pair.wat` | A token and receiver in one module: `send` writes, calls `on_transfer`, then `resolve`; the receiver's `settle` callback panics after its write commits. | `invariants.rs` |
| `margin_registry.wat` | `put` forwards ids to `on_receive`, which queues them and calls `resolve` on a small budget that misses the cost of settling later entries. | `margin.rs` |
| `mtoken.wasm` | A minimal multi-token ([tests/contracts/keeper/mtoken](../contracts/keeper/mtoken)): `create`, `give`, `batch_transfer_call` to a receiver, then `resolve`. | `curves.rs`, `keeper.rs` |
| `mtoken_events.wasm` | mtoken.wasm with `events`: NEP-245 `mt_mint` and `mt_transfer` logs, and `mt_balance_of` and `mt_supply` views. | `src/sim.rs`, `token_effects.rs` |
| `near_wrapper.wat` | A wNEAR-like 1:1 wrapper: `near_deposit` (payable), `near_withdraw`, an `ft_balance_of` view. | `books.rs` |
| `nonce_box.wat` | `use(nonce)` stores the nonce and panics on one it has seen. | `axes.rs`, `trie_gas.rs` |
| `open_registry.wat` | `register` (payable) stores a 1000-byte profile paid from the registry's own balance; the paying build requires 0.011 NEAR. | `curves.rs`, `report.rs` |
| `payload_ledger.wat` | `set` and `spend` act on a holder named in the args, not the caller, as a relayed signed payload does; `spend` panics "not enough credit". | `axes.rs` |
| `pending.wat` | Self-callbacks: `request`, `process` then `on_withdraw` then `on_result`, and `batch`, `keyed` and `pair`, each a multi-action receipt to itself. | `src/tx.rs`, `explore_replay.rs`, `explore_run.rs`, `explore_search.rs`, `outsider.rs` |
| `pool_sync.wat` | A pool that books its balance less deposits as reward: two `deposit`s in one block double-count the one in flight. | `src/explore/memo_tests.rs`, `explore_enumerate.rs`, `faults.rs`, `identity.rs` |
| `probe.wasm` | A near-sdk probe built from [probe_src.rs.txt](probe_src.rs.txt): views of the seed, key, time and height; `start_yield` and `resume`; `call_missing`; `last`. | `examples/tour.rs`, `src/sim.rs`, `accounts.rs`, `callbacks.rs`, `calls.rs` and 17 more |
| `probe_src.rs.txt` | probe.wasm's source. | `calls.rs`, `docs.rs`, `effects.rs`, `token_effects.rs`, `trie_gas.rs` and 1 more |
| `receiver.wasm` | custody.wasm's notify receiver ([tests/contracts/receiver](../contracts/receiver)): `on_notify` returns half of each position to refund. Not an NEP-141 receiver: that is ft_receiver.wat. | `intents_like.rs` |
| `refund_book.wat` | `deposit` credits ids, calls a sink, then `resolve` on 3 Tgas plus 0.2 Tgas an id, while its cost is per distinct id it removes. | `margin.rs` |
| `refund_ledger.wat` | `send` calls the receiver's `on_receive`, then `resolve`; the refund is capped by the receiver's credit, not by what was sent, so a hostile reply drains it. | `axes.rs`, `books.rs`, `explore_search.rs`, `fault_address.rs` |
| `sandbox_gas_2.14.0.json` | Data, not a contract: per-scenario gas, profiles, logs and statuses recorded from a neard 2.14.0 sandbox at protocol 88. | `gas_vs_sandbox.rs` |
| `self_only.wat` | `start` calls its own callbacks; `on_done` lacks a predecessor check (a missing `#[private]`), `on_done_guarded` has one. | `outsider.rs` |
| `self_only_peer.wat` | Deployed twice: `forward` calls the peer's `land`, which writes, and `picky`, which panics unless the signer's id is 10 bytes. | `outsider.rs` |
| `settle_starves.wat` | `pay` calls `ping`, then `on_pay` with the leftover gas, and succeeds even when `on_pay` starves. | `explore_scan.rs`, `explore_search.rs`, `gas_edges.rs`, `scan_cost.rs` |
| `staking_pool.wat` | A minimal staking pool: `new(key)`, `deposit_and_stake`, `unstake` (withdrawable 4 epochs later), `withdraw`, an `epoch_height` view. | `src/explore/books.rs`, `books.rs`, `epochs.rs`, `explore_enumerate.rs`, `fixture.rs` |
| `status_message.wasm` | near-sdk 5.29.0's status-message example: `set_status` and `get_status` ([NOTICE.md](NOTICE.md)). | `examples/tour.rs`, `accounts.rs`, `calls.rs`, `chain.rs`, `differential/` and 11 more |
| `storage_ledger.wat` | Prepays storage from a ledger: `deposit` records a deposit, `write` stores 100 bytes paid from it; `pause` and `unpause` pay from the contract's own NEAR. | `books.rs` |
| `ticket_desk.wat` | Two planted bugs: `buy` refuses only a zero deposit, and `claim` credits a beneficiary named in its args without checking the caller. | `outsider.rs` |
| `tip_jar.wat` | `tip` sends 1 NEAR detached; `tip_checked` sends 1 NEAR, then `on_tip` logs whether it arrived. | `examples/tour.rs`, `explore_run.rs`, `faults.rs`, `invariants.rs`, `snapshots.rs` |
| `unwrapper.wat` | Holds wNEAR: `withdraw` calls the wrapper's `near_withdraw`, then `on_withdraw` pays the signer the amount less `$keeps` (a test sets it for the bug). | `books.rs` |
| `vault.wasm` | The quickstart's vault ([tests/contracts/vault](../contracts/vault)): deposits through `ft_on_transfer`, `withdraw` pays with `ft_transfer`, `on_withdraw` restores shares if it failed; `claim_rewards` once a day. | `src/explore/memo_tests.rs`, `callbacks.rs`, `diff.rs`, `explore_replay.rs`, `explore_run.rs` and 11 more |
| `vault_forgetful.wasm` | vault.wasm with `forgetful`, a planted bug: no backing guard on `withdraw`, and `on_withdraw` doesn't restore shares. | `diff.rs`, `explore_replay.rs`, `explore_scan.rs`, `explore_search.rs`, `vault.rs` and 1 more |
| `vault_pair.wat` | A token and vault for fault injection: `transfer_call` to the vault, then `verify`, `resolve_deposit` and `resolve_transfer`; a failed `resolve_deposit` leaves credit unbacked. | `src/explore/check.rs`, `explore_scan.rs`, `faults.rs` |
| `yield_pay.wat` | `request` yields to its own `cb`; `respond` resumes it; `cb`, once resumed, pays 1 NEAR; a timeout does nothing. | `src/explore/memo_tests.rs`, `explore_enumerate.rs` |
