#!/usr/bin/env bash
# Rebuild the custody specimen fixtures (needs cargo-near 0.22):
#   tests/res/custody.wasm          (fixed)
#   tests/res/custody_gas_bug.wasm  (feature bug_gas: flat resolver gas)
#   tests/res/custody_log_bug.wasm  (feature bug_log: unbounded refund log)
#   tests/res/receiver.wasm         (the notify receiver)
#
# The remap keeps the build machine's home path out of the wasm's panic
# locations.
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
out=${1:-"$here/../../res"}
mkdir -p "$out"
out=$(cd "$out" && pwd)
export CARGO_BUILD_JOBS=${CARGO_BUILD_JOBS:-2}
flags="RUSTFLAGS=-C link-arg=-s --remap-path-prefix=$HOME=/build --remap-path-prefix=$HOME/.cargo/registry/src=/cargo"
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

build() { # <contract_dir> <features> <artifact> <dest>
  local dir=$1 features=$2 artifact=$3 dest=$4
  ( cd "$dir"
    cargo near build non-reproducible-wasm --no-abi --locked --env "$flags" \
      ${features:+--features "$features"} --out-dir "$tmp" )
  mv "$tmp/$artifact" "$out/$dest"
  chmod 644 "$out/$dest"
}

build "$here"               ""        custody.wasm  custody.wasm
build "$here"               "bug_gas" custody.wasm  custody_gas_bug.wasm
build "$here"               "bug_log" custody.wasm  custody_log_bug.wasm
build "$here/../receiver"   ""        receiver.wasm receiver.wasm

echo "built: custody.wasm custody_gas_bug.wasm custody_log_bug.wasm receiver.wasm -> $out"
