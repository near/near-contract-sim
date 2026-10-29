# MPC test fixtures

These fixtures are used by `tests/mpc_tee.rs`, its snapshot-size regression included. Their source commit, build command, derived-state notes, hashes and attestation validity window are recorded in [the test header](../../mpc_tee.rs).

`near_account_secret_key` is checked-in test signing material, used only for `fixture-node-a.near` by `tests/mpc_tee.rs` and `tests/mpc.rs`. It comes from near/mpc's public test assets (`crates/test-utils/assets`). Treat it as disclosed: never use this key for production, import it into a real wallet, or fund an account using it.

The fixtures include quote collateral and derived MPC state. They come from https://github.com/near/mpc, MIT licensed: see [LICENSE](LICENSE), copied from that repository, and [THIRD-PARTY-NOTICES.md](THIRD-PARTY-NOTICES.md) for the dependencies the wasm links.

## `mpc_contract_8397c0ae.wasm` (sign/respond fixture)

Used by `tests/mpc_fixture` and `tests/mpc.rs`. The DCAP fixtures above stay at `1d514bc4`, because `mpc_state_patched.borsh` is that build's STATE layout.

- Source: near/mpc @ `8397c0aeb67a207f28f0f5d757e304eea993f018` (2026-10-09), `crates/contract` with default features (`block-clock`, production clock).
- sha256: `b377d4aae54b99934ea7c4b60813a6c0762c27c4a8f069b1627594a0eaf4c8fc` as built; this file is `4b9865c738512a4f727c8ad83fc7f57c7ebd1092f3e99031cf1ce008c28ef128` (see below).
- Build: Rust 1.97.1 (the clone's `rust-toolchain.toml`), cargo-near 0.22.0, the command from the repo's `Makefile.toml` (`build-contract-optimized`):

  ```
  AR_wasm32_unknown_unknown=<path to llvm-ar> \
    cargo near build non-reproducible-wasm --no-abi --profile=release-contract \
    --manifest-path crates/contract/Cargo.toml --locked
  ```

  A fresh build should add `--env "RUSTFLAGS=-C link-arg=-s --remap-path-prefix=$HOME=/build --remap-path-prefix=$HOME/.cargo/registry/src=/cargo"`, so no local path lands in the wasm. The checked-in build predates that: it embedded the build machine's home directory in its panic-location strings, and each `$HOME/` prefix was replaced in place with a same-length `/build/xxx…/` placeholder. Only data-segment bytes changed; the code and gas are the original build's.

  On macOS, `AR_wasm32_unknown_unknown` must name an `llvm-ar`. Apple's `ar` silently builds a `libblst.a` whose wasm objects aren't indexed, so the link fails with `undefined symbol: blst_hash_to_g1`. The rustup `llvm-tools` component has one, at `~/.rustup/toolchains/<toolchain>/lib/rustlib/<host>/bin/llvm-ar`.

The fixture's domain key is test material: the root secret key is a constant in `tests/mpc_fixture/mod.rs`, so tests can sign valid responses.
