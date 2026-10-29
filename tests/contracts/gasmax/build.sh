#!/usr/bin/env bash
# Rebuild the gasmax specimen (needs cargo-near 0.22): tests/res/gasmax.wasm.
# The remap keeps the build machine's home path out of the wasm's panic
# locations.
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
out=${1:-"$here/../../res"}
mkdir -p "$out"
out=$(cd "$out" && pwd)
export CARGO_BUILD_JOBS=${CARGO_BUILD_JOBS:-2}
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

( cd "$here"
  cargo near build non-reproducible-wasm --no-abi --locked \
    --env "RUSTFLAGS=-C link-arg=-s --remap-path-prefix=$HOME=/build --remap-path-prefix=$HOME/.cargo/registry/src=/cargo" \
    --out-dir "$tmp" )
mv "$tmp/gasmax.wasm" "$out/gasmax.wasm"
chmod 644 "$out/gasmax.wasm"
echo "built: gasmax.wasm -> $out"
