# The simulator

Use this page to set up the crate, build and share a world, and read or
patch contract state. `Sim` is a single-shard NEAR chain in process, run
by nearcore's own `Runtime::apply` at 2.14.0 (protocol 88), so
fees, gas, receipts, refunds, callbacks and yields are a node's
([fidelity.md](fidelity.md)). Calls are synchronous and ids are strings.

## Set up

The test crate's layout and dependency block are in
[quickstart.md](quickstart.md). Beyond them:

- **System packages** for nearcore's C dependencies, which build from
  source: a C/C++ compiler, libclang, perl and make (Ubuntu:
  `build-essential clang libclang-dev perl`; macOS: the Xcode Command
  Line Tools).
- **The test crate's own `Cargo.lock`**, never a copy of the
  simulator's: commit it, so a rerun builds the same nearcore and
  findings replay.
- **The toolchain pin** is read from the directory cargo runs in: `cargo
  test --manifest-path` from elsewhere ignores it.
- **A local checkout** of the simulator:
  `near-contract-sim = { path = "../near-contract-sim" }`.
- **Imports.** `near_contract_sim::prelude::*` has the everyday names,
  `key` included; the rest are at the root (`near_contract_sim::Token`,
  `ActionErrorKind`). `Gas` is nearcore's: near-sdk's
  `Gas::from_tgas(30)` is `Gas::from_teragas(30)`.
- **nearcore's types come from this crate**, at the root and in
  `near_contract_sim::nearcore`. A nearcore dependency of your own is a
  second copy of every type.
- A test crate that also depends on near-sdk needs its
  `non-contract-usage` feature.

## A world by hand

`Sim::new()` starts an empty chain. `create_account(id, balance)` writes
an account directly, like a genesis record, so any id works without a
parent. `deploy(id, wasm)` deploys with a real transaction, creating the
account with 100 NEAR if it is missing. `view(id, method, args)` runs a
view; `.json::<T>()` decodes it, and `.assert_failure()` returns why it
failed. `balance(id)` and `set_balance(id, amount)` read and overwrite
an account's liquid balance. `auction_wasm()` is the README's.

```rust
let mut sim = Sim::new();
let alice = sim.create_account("alice.near", Balance::from_near(100));
sim.create_account("bob.near", Balance::from_near(100));
sim.deploy("auction.near", auction_wasm());
```

`Balance` has no `+` or `-`: use `saturating_*` and `checked_*`, or
`as_yoctonear()` and `u128` arithmetic.

## A contract without source

For a prebuilt `.wasm` or a `.wat`, skip the quickstart's build: read
the file, or compile a `.wat` with the `wat` crate (`wat = "1"`). Its
interface is its exports: `wasm-objdump -x -j Export`, or call a method
it lacks, whose failure lists them. `.args(bytes)` sends raw input, with
no JSON; `out.value()` and `sim.view(..).bytes()` are raw returns. With
no views, assert on storage (`tests/docs.rs`):

```rust
let wat = format!("{}/tests/res/guestbook.wat", env!("CARGO_MANIFEST_DIR"));
sim.deploy("guestbook.near", wat::parse_file(wat).unwrap());
sim.call("alice.near", "guestbook.near", "sign").args("hello").transact().assert_success();
assert_eq!(sim.state("guestbook.near", b"galice.near"), Some(b"hello".to_vec()));
```

## A world as a Fixture

`Fixture` writes the setup as data and builds it once per test process.
Steps: `account(id, balance)`; `contract(id, wasm)` (a missing account
gets 100 NEAR, as every deploy's does: put `.account(id, balance)` first
to fund it more); `init(method)` on the last contract, signed by the
contract, so its gas refund lands after `method` (`call` from another
account avoids that, unless it's `#[private]`); `call(signer, receiver,
method)`; `args_json(v)`, `args_borsh(v)` or `args(bytes)` (raw, never
JSON), `deposit(d)` and `gas(g)` for the last `init` or `call`, which
sends no args without one; `ft_register(token, accounts)`, paid by the
token (`call` for another payer); `ft_transfer(token, from, to,
amount)`; `stub(id, Stub)` ([stubs.md](stubs.md)); `token(Token)`
([outcomes-and-effects.md](outcomes-and-effects.md)); `setup(name, |sim|
..)` for anything else; `config(|c| ..)` for the `Sim`'s `Config`.
[quickstart.md](quickstart.md)'s `world()` is one.

- `build()` settles and returns a clone. The first build in a process
  runs the steps or loads a snapshot an earlier run of the same test
  binary saved; later builds clone from memory.
- The key is every step, wasm bytes and arguments included, so a rebuilt
  contract gets a fresh setup.
- `setup(name, ..)` is keyed by its name and where it is written, not by
  what the closure captures. A captured value that differs between two
  fixtures built from the same line goes in `name` too.

## Clones and the cache

A clone is a snapshot. It shares the insert-only trie store and the
compiled-contract cache, so it costs about 2 ms. Branch a test with
`sim.clone()`. Without a `Fixture`, a `static LazyLock<Sim>` holds the
setup. Across test processes (nextest runs each test in its own),
`Sim::cached(key, setup)` saves the setup to disk. Put in `key` what
`setup` reads that the binary doesn't contain, usually the wasm.

Compiled wasm and snapshots go in the first of `NCS_CACHE_DIR`,
`near-contract-sim/` in cargo's target directory, or
`~/.cache/near-contract-sim`. The cache holds native code: point
`NCS_CACHE_DIR` only at a private directory.

## Contract state

`state(id, key)` reads one key and `view_state(id, prefix)` every entry
under a prefix. `set_state(id, key, value)` writes one and keeps
`storage_usage` right. `account(id)` is an `Option` (`None`: no such
account) with the balance, `locked` and `storage_usage`.

`contract_state::<T>(id)` borsh-decodes a near-sdk contract's root
struct (its `STATE` key) as a type of the test's own, the contract's
fields in order. A contract with no `#[init]` has no `STATE` until its
first `&mut self` call, and `contract_state` panics until then. Each
collection is the fields near-sdk stores for it:

| contract field | mirror field |
|---|---|
| `LookupMap`, `LookupSet` (`store` or `collections`) | `Vec<u8>`: the prefix |
| `Vector` | `(u32, Vec<u8>)`: length, prefix (a `u64` length in `collections`) |
| `store::IterableMap`, `IterableSet` | `(u32, Vec<u8>, Vec<u8>)` |
| `store::UnorderedMap`, `UnorderedSet` | `(Option<u32>, u32, u32, Vec<u8>, Vec<u8>)` |
| `collections::UnorderedMap` | `(Vec<u8>, (u64, Vec<u8>), (u64, Vec<u8>))`: prefix, keys, values |
| a plain `HashMap`, `HashSet` | the same type, or a `BTreeMap`: borsh writes it sorted by key |
| an enum | the same variants in the same order: borsh writes the index |
| near-sdk `PublicKey` | `Vec<u8>`: a `u32` length, the curve byte, the key; the prelude's `PublicKey` has no length and mis-decodes |

`update_contract_state::<T>(id, f)` edits it in place. Both panic,
naming the type and stored length, when it doesn't decode
(`tests/vault.rs`):

```rust
sim.update_contract_state::<VaultState>(VAULT, |s| s.total_shares += 1);
```

That is how a check is made to fail on purpose ([audit.md](audit.md)). A
collection entry is a key of its own: `LookupMap` keeps each under its
prefix and the borsh-encoded key (`tests/patch.rs`):

```rust
let key = [b"s".as_slice(), &borsh::to_vec("bob.near").unwrap()].concat();
sim.set_state(VAULT, &key, Some(&borsh::to_vec(&500u128).unwrap()));
```

`key::borsh(b"s", &id)` builds the same key, and `key::borsh_keccak`
and `key::borsh_sha256` build one for a map with that hasher.
The root's totals don't move with it. `apply_state_records(records)`
writes nearcore `StateRecord`s, and `state_records(ids)` reads them back.

## Environment and CI

`NO_COLOR` turns off color in receipt trees; the `NEAR_SIM_*` variables
are a search's ([explore.md](explore.md)). In CI, run `cargo test
--locked` in the test crate, after building any wasm it reads from
`tests/res/` with the contract's toolchain.
