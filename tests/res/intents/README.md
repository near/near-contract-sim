# near/intents fixtures

Used by `tests/invariants_intents.rs`. From https://github.com/near/intents,
MIT License, Copyright (c) 2024 Defuse: see [LICENSE](LICENSE), copied from
that repository.

| file | source | sha256 (as built) | sha256 (this file) |
|---|---|---|---|
| `defuse-v0.2.5.wasm` | tag `defuse/v0.2.5` (9f91e9c3), `cargo near build` on a host, not the reproducible image | `58bfa1043187c0169c621ba346a297af97b18b419c9ab08e49c428c33af400c0` | `b1bcf45a210bd811e6fc3a11368ca286933ebd620545423a60b1617a102456ff` |
| `defuse-v0.2.6.wasm` | tag `defuse/v0.2.6` (cd5795ea), same | `41aa275241af735ec642b1ea04c38fe05222effe861d5801ded02ce0286f314b` | `135fc08be3e72be0eef76769cebd1e1e04b18c6a1b3b5595cb2dc886e6d2405e` |
| `defuse-v0.2.8.wasm` | tag `defuse/v0.2.8` (852c3472, "fix: native withdraw gas (#102)"), same | `8c58978d491e03d5488207bb672bbcd5a3d0c1c79fbb7aaac7ada67779879709` | `9d28f81f50366921f5b3be3608d949ca7050ce63eebb7ac9cde7d83bf49712d9` |
| `wnear.wasm` | `releases/wnear.wasm` (also `tests/contracts/wnear.wasm` at those tags), copied as is | `b72c26d390b23f4517d091a8aab06b0369d465356ec5fb337d5fda076da38ac0` | same |

The three `defuse` builds (Rust 1.85.0) embedded two build-machine paths
in their panic-location strings: the home directory, in front of every
dependency's source path, and the temporary directory the source was
checked out to, in front of the contract's own crates (`core/src/…`,
`crypto/src/…` and so on). Every such prefix was replaced in place with a
placeholder of the same length, `/build/xxx…/`, so only data-segment bytes
changed: the code, and so the gas, are the original build's. A fresh build
should pass `--env "RUSTFLAGS=-C link-arg=-s --remap-path-prefix=$HOME=/build
--remap-path-prefix=$HOME/.cargo/registry/src=/cargo
--remap-path-prefix=$(git rev-parse --show-toplevel)=/src"` to `cargo near
build` instead: the last matching prefix wins, and the checkout's root
covers every workspace crate.
