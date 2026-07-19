#!/usr/bin/env bash
# P-2: 2-minute agent isolation demo (no tray).
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
DEMO_DIR="${TMPDIR:-/tmp}/feanorfs-demo-$$"
SERVER_DATA="$DEMO_DIR/server-data"
WORKSPACE="$DEMO_DIR/workspace"

cleanup() {
  if [[ -n "${SERVER_PID:-}" ]]; then
    kill "$SERVER_PID" 2>/dev/null || true
  fi
}
trap cleanup EXIT

echo "== FeanorFS agent loop demo =="
echo "Workspace: $WORKSPACE"

cargo build -q --manifest-path "$ROOT/Cargo.toml" -p feanorfs-client

mkdir -p "$WORKSPACE"
cd "$WORKSPACE"

"$ROOT/target/debug/feanorfs" serve --allow-http --data-dir "$SERVER_DATA" --port 3030 --allow-open &
SERVER_PID=$!
for _ in $(seq 1 30); do
  if curl -sf "http://127.0.0.1:3030/api/workspaces" >/dev/null 2>&1; then
    break
  fi
  sleep 0.2
done

# One-flow setup (no watch — demo is scripted)
"$ROOT/target/debug/feanorfs" start http://127.0.0.1:3030 --workspace demo --no-watch

echo "doc.txt" > doc.txt
"$ROOT/target/debug/feanorfs" sync --no-watch

echo "== spawn two agents =="
"$ROOT/target/debug/feanorfs" agent spawn a
"$ROOT/target/debug/feanorfs" agent spawn b

"$ROOT/target/debug/feanorfs" agent run a -- sh -c 'printf "%s\n" "agent version" > doc.txt'
"$ROOT/target/debug/feanorfs" agent run b -- sh -c 'printf "%s\n" "other agent" > doc.txt'
echo "human edit" > doc.txt
"$ROOT/target/debug/feanorfs" sync --up --no-watch 2>/dev/null || "$ROOT/target/debug/feanorfs" push

echo "== land agent a (clean) =="
"$ROOT/target/debug/feanorfs" --json agent land a

echo "== land agent b (expect needs-attention) =="
"$ROOT/target/debug/feanorfs" --json agent land b || true

echo "== conflicts =="
"$ROOT/target/debug/feanorfs" conflicts
"$ROOT/target/debug/feanorfs" conflicts show doc.txt 2>/dev/null || true

echo "== resolve: keep local =="
"$ROOT/target/debug/feanorfs" conflicts keep doc.txt --local

echo "== done =="
"$ROOT/target/debug/feanorfs" agent clean a
"$ROOT/target/debug/feanorfs" agent clean b

echo "Demo complete."
