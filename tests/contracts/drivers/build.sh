#!/usr/bin/env bash
# Rebuild the drivers specimen (needs cargo-near 0.22).
#   build.sh [out]   drivers_funded.wasm (the `funded` feature) and
#                    drivers_capped.wasm (`capped`)
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

build() { # features, out name
  ( cd "$here"
    cargo near build non-reproducible-wasm --no-abi --locked --env "$flags" \
      ${1:+--features "$1"} --out-dir "$tmp" )
  mv "$tmp/drivers.wasm" "$out/$2"
  chmod 644 "$out/$2"
}

build funded drivers_funded.wasm
build capped drivers_capped.wasm
echo "built: drivers_funded.wasm, drivers_capped.wasm -> $out"
