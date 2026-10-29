#!/usr/bin/env bash
# Rebuild the fuzz contract fixture (needs cargo-near 0.22). The remap
# keeps the build machine's home path out of the wasm's panic locations.
set -euo pipefail
contract_dir=$(cd "$(dirname "$0")" && pwd)
output_dir=${1:-"$contract_dir/../../res"}
mkdir -p "$output_dir"
output_dir=$(cd "$output_dir" && pwd)
cd "$contract_dir"
cargo near build non-reproducible-wasm --no-abi --locked \
  --env "RUSTFLAGS=-C link-arg=-s --remap-path-prefix=$HOME=/build --remap-path-prefix=$HOME/.cargo/registry/src=/cargo" \
  --out-dir "$output_dir"
