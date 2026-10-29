#!/usr/bin/env bash
# Rebuild the keeper specimen (needs cargo-near 0.22).
#   build.sh [out]          tests/res/keeper.wasm, keeper_closed.wasm
#                           (the `closed` feature) and mtoken.wasm
#   build.sh --events [out] mtoken_events.wasm (the `events` feature) only
#   build.sh --ping [out]   keeper_ping.wasm (the `ping` feature) and
#                           keeper_ping_hashed.wasm (`ping` and `hashed`)
#                           only
# The remap keeps the build machine's home path out of the wasm's panic
# locations.
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
events=false
ping=false
if [ "${1:-}" = "--events" ]; then events=true; shift; fi
if [ "${1:-}" = "--ping" ]; then ping=true; shift; fi
out=${1:-"$here/../../res"}
mkdir -p "$out"
out=$(cd "$out" && pwd)
export CARGO_BUILD_JOBS=${CARGO_BUILD_JOBS:-2}
flags="RUSTFLAGS=-C link-arg=-s --remap-path-prefix=$HOME=/build --remap-path-prefix=$HOME/.cargo/registry/src=/cargo"
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

build() { # dir, features, built name, out name
  ( cd "$here/$1"
    cargo near build non-reproducible-wasm --no-abi --locked --env "$flags" \
      ${2:+--features "$2"} --out-dir "$tmp" )
  mv "$tmp/$3" "$out/$4"
  chmod 644 "$out/$4"
}

if $ping; then
  build . ping keeper.wasm keeper_ping.wasm
  build . ping,hashed keeper.wasm keeper_ping_hashed.wasm
  echo "built: keeper_ping.wasm, keeper_ping_hashed.wasm -> $out"
  exit 0
fi
if $events; then
  build mtoken events mtoken.wasm mtoken_events.wasm
  echo "built: mtoken_events.wasm -> $out"
  exit 0
fi
build . "" keeper.wasm keeper.wasm
build . closed keeper.wasm keeper_closed.wasm
build mtoken "" mtoken.wasm mtoken.wasm
echo "built: keeper.wasm, keeper_closed.wasm, mtoken.wasm -> $out"
