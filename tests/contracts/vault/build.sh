#!/usr/bin/env bash
# Rebuild both vault fixtures (needs cargo-near 0.22): vault.wasm (fixed)
# and vault_forgetful.wasm (feature `forgetful`, the planted bug). The
# remap keeps the build machine's home path out of the wasm's panic
# locations.
set -euo pipefail
contract_dir=$(cd "$(dirname "$0")" && pwd)
output_dir=${1:-"$contract_dir/../../res"}
mkdir -p "$output_dir"
output_dir=$(cd "$output_dir" && pwd)
cd "$contract_dir"
flags="RUSTFLAGS=-C link-arg=-s --remap-path-prefix=$HOME=/build --remap-path-prefix=$HOME/.cargo/registry/src=/cargo"
cargo near build non-reproducible-wasm --no-abi --locked --env "$flags" --out-dir "$output_dir"
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
cargo near build non-reproducible-wasm --no-abi --locked --env "$flags" --features forgetful --out-dir "$tmp"
mv "$tmp/vault.wasm" "$output_dir/vault_forgetful.wasm"
chmod 644 "$output_dir/vault.wasm" "$output_dir/vault_forgetful.wasm"
