#!/usr/bin/env bash
# Differential test against a fresh near sandbox in Docker (see
# tests/differential.rs).
#
#   tests/differential/run.sh [cases] [seed]
#
# Starts nearprotocol/sandbox:$NCS_SANDBOX_IMAGE_TAG (default 2.14.0, the
# pinned nearcore) on a localhost port, runs the test, removes the container.
# The chain must be fresh: the simulator replays the same account set.
set -euo pipefail
cd "$(dirname "$0")/../.."
TAG="${NCS_SANDBOX_IMAGE_TAG:-2.14.0}"
PORT="${NCS_SANDBOX_PORT:-3930}"
NAME="ncs-diff-sandbox-$$"
docker run -d --rm --name "$NAME" -p "127.0.0.1:$PORT:3030" "nearprotocol/sandbox:$TAG" >/dev/null
trap 'docker rm -f "$NAME" >/dev/null 2>&1 || true' EXIT
wait_for_sandbox() {
  for _ in $(seq 1 120); do
    curl -sf -m 2 "http://127.0.0.1:$PORT/status" >/dev/null && return 0
    sleep 1
  done
  return 1
}
wait_for_sandbox
tests/differential/retain-history.sh "$NAME"
wait_for_sandbox
export NCS_SANDBOX_RPC="http://127.0.0.1:$PORT"
export NCS_DIFF_CASES="${1:-20}" NCS_DIFF_SEED="${2:-1}"
cargo test --test differential -- --ignored --nocapture
