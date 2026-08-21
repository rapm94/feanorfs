#!/usr/bin/env bash
# Prints local-evidence status for the acceptance matrix in
# docs/acceptance-evidence.md. Local suites only: no network, no installs.
set -uo pipefail

pass() { printf 'PASS %s\n' "$1"; }
fail() { printf 'FAIL %s\n' "$1"; }
skip() { printf 'SKIP %s (%s)\n' "$1" "$2"; }

run() {
  local name=$1
  shift
  if "$@" >/dev/null 2>&1; then pass "$name"; else fail "$name"; fi
}

run ai5-state-identity \
  cargo test -p feanorfs-agent-core --locked workspace_state
run ai5-layout-retirement \
  cargo test -p feanorfs-agent-core --locked workspace_layout
run ai2-ffwork1-reducer \
  cargo test -p feanorfs-agent-core --locked work::
run ai2-ffres1-protocol \
  cargo test -p feanorfs-agent-core --locked resolution_protocol
run ai2-wire-contract-snapshots \
  cargo test -p feanorfs-client --locked --test contract_snapshots
run ai2-work-engine-cli \
  cargo test -p feanorfs-client --locked --test work_engine

skip ai2-mixed-version-peers "requires released products (AI-1)"
skip ai6-process-ownership-field "requires installed products"
skip ai6-two-agent-soak "requires two machines and instrumentation"
skip ai6-lan-convergence-p95 "requires timing harness on LAN"
