#!/usr/bin/env bash
# Dev runner: build, then run the daemon + web chat + Gmail listener as
# foreground children of this script. Ctrl-C stops all of them. Not for real
# deployments (no restart-on-failure) — see README "Running as systemd Services".
set -euo pipefail
cd "$(dirname "$0")"

cargo build --release
BIN=target/release/openferris

pids=()
"$BIN" daemon & pids+=($!)
"$BIN" web & pids+=($!)
"$BIN" gmail & pids+=($!)

trap 'kill "${pids[@]}" 2>/dev/null || true' INT TERM
wait
