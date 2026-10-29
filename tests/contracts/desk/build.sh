#!/usr/bin/env bash
# Rebuild the desk specimen fixtures (needs cargo-near 0.22):
#   tests/res/desk.wasm       (on_issued's budget grows with the batch)
# With --band, only tests/res/desk_band.wasm (feature band, a gas band).
#
# The remap keeps the build machine's home path out of the wasm's panic
# locations.
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
if [ "${1:-}" = "--band" ]; then BAND=1; shift; fi
out=${1:-"$here/../../res"}
mkdir -p "$out"
out=$(cd "$out" && pwd)
export CARGO_BUILD_JOBS=${CARGO_BUILD_JOBS:-2}
flags="RUSTFLAGS=-C link-arg=-s --remap-path-prefix=$HOME=/build --remap-path-prefix=$HOME/.cargo/registry/src=/cargo"
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

build() { # <features> <dest>
  local features=$1 dest=$2
  ( cd "$here"
    cargo near build non-reproducible-wasm --no-abi --locked --env "$flags" \
      ${features:+--features "$features"} --out-dir "$tmp" )
  mv "$tmp/desk.wasm" "$out/$dest"
  chmod 644 "$out/$dest"
}

if [ "${BAND:-}" = 1 ]; then
  build "band" desk_band.wasm
  echo "built: desk_band.wasm -> $out"
  exit 0
fi
build "" desk.wasm
echo "built: desk.wasm -> $out"
