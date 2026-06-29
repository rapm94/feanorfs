#!/usr/bin/env bash
# Full smoke test: CI checks + live server E2E.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

TARGET="$(cargo metadata --no-deps --format-version 1 | python3 -c "import json,sys; print(json.load(sys.stdin)['target_directory'])")"
FEANORFS="$TARGET/release/feanorfs"
FEANORFS_SERVER="$TARGET/release/feanorfs-server"

SMOKE_ROOT="$(mktemp -d /tmp/feanorfs-smoke-XXXXXX)"
SMOKE_HOME="$SMOKE_ROOT/home"
SERVER_DATA="$SMOKE_ROOT/server-data"
CLIENT_A="$SMOKE_ROOT/client-a"
CLIENT_B="$SMOKE_ROOT/client-b"
PORT=13030
TOKEN="smoke-server-token"
E2EE="smoke-e2ee-0123456789abcdef0123456789abcdef0123456789abcdef01"
WS="smoke-ws"
SERVER_PID=""

cleanup() {
  if [[ -n "${SERVER_PID}" ]] && kill -0 "${SERVER_PID}" 2>/dev/null; then
    kill "${SERVER_PID}" 2>/dev/null || true
    wait "${SERVER_PID}" 2>/dev/null || true
  fi
}
trap cleanup EXIT

pass() { echo "  ✓ $*"; }
fail() { echo "  ✗ $*" >&2; exit 1; }
step() { echo ""; echo "== $* =="; }

step "Build release binaries"
cargo build --release -q
[[ -x "$FEANORFS" ]] || fail "missing feanorfs binary"
[[ -x "$FEANORFS_SERVER" ]] || fail "missing feanorfs-server binary"
pass "binaries built"

step "cargo fmt --check"
cargo fmt --all -- --check
pass "fmt"

step "cargo clippy"
cargo clippy --workspace --all-targets -- -D warnings
pass "clippy"

step "cargo test --workspace"
cargo test --workspace -q
pass "tests"

step "cargo doc (strict)"
RUSTDOCFLAGS="-D warnings" cargo doc --no-deps --workspace -q
pass "docs"

step "Start feanorfs-server"
mkdir -p "$SMOKE_HOME" "$SERVER_DATA" "$CLIENT_A" "$CLIENT_B"
export HOME="$SMOKE_HOME"
FEANORFS_PORT=$PORT FEANORFS_TOKEN=$TOKEN FEANORFS_DATA_DIR="$SERVER_DATA" \
  "$FEANORFS_SERVER" --port "$PORT" --data-dir "$SERVER_DATA" --token "$TOKEN" \
  >"$SMOKE_ROOT/server.log" 2>&1 &
SERVER_PID=$!
sleep 1
kill -0 "$SERVER_PID" || { cat "$SMOKE_ROOT/server.log"; fail "server failed to start"; }
pass "server pid $SERVER_PID on :$PORT"

SERVER_URL="http://127.0.0.1:$PORT"

step "Client A: init + push"
cd "$CLIENT_A"
mkdir -p src
echo "hello from machine A" > hello.txt
echo "nested content" > src/nested.rs
"$FEANORFS" init "$SERVER_URL" --workspace "$WS" --encryption-key "$E2EE" --server-token "$TOKEN" >/dev/null
"$FEANORFS" doctor >/dev/null
PUSH_JSON=$("$FEANORFS" --json push)
echo "$PUSH_JSON" | python3 -c "import json,sys; d=json.load(sys.stdin); assert d['uploads']==2, d"
pass "init + doctor + push (2 uploads)"

step "Client B: join + lazy pull + hydrate + cat"
cd "$CLIENT_B"
"$FEANORFS" attach "$WS" --encryption-key "$E2EE" --server-url "$SERVER_URL" --server-token "$TOKEN" >/dev/null
PULL_JSON=$("$FEANORFS" --json pull --lazy)
echo "$PULL_JSON" | python3 -c "import json,sys; d=json.load(sys.stdin); assert d['placeholders']==2, d"
[[ -f hello.txt ]] && [[ ! -s hello.txt ]] || fail "lazy placeholder missing or non-empty"
"$FEANORFS" hydrate hello.txt >/dev/null
CAT_OUT=$("$FEANORFS" cat hello.txt)
[[ "$CAT_OUT" == "hello from machine A" ]] || fail "cat mismatch: $CAT_OUT"
pass "join + lazy pull + hydrate + cat"

step "Client B: sync idempotent"
SYNC_JSON=$("$FEANORFS" --json sync --no-watch)
echo "$SYNC_JSON" | python3 -c "import json,sys; d=json.load(sys.stdin); assert d['uploads']==0 and d['downloads']==0, d"
pass "sync --no-watch idempotent"

step "Client A: agent spawn + run"
cd "$CLIENT_A"
"$FEANORFS" agent spawn ci1 >/dev/null
AGENT_LIST=$("$FEANORFS" agent list)
echo "$AGENT_LIST" | grep -q ci1 || fail "agent not listed"
"$FEANORFS" agent run ci1 -- sh -c 'echo agent-ok' | grep -q agent-ok || fail "agent run failed"
pass "agent spawn + list + run"

step "Client A: summary session marker"
SUM1=$("$FEANORFS" --json summary)
echo "$SUM1" | python3 -c "import json,sys; json.load(sys.stdin)"
echo "smoke edit" >> hello.txt
SUM2=$("$FEANORFS" --json summary)
echo "$SUM2" | python3 -c "import json,sys; d=json.load(sys.stdin); assert 'hello.txt' in d.get('files_modified',[]), d"
pass "summary detects modification"

step "Client A: push update + workspaces"
"$FEANORFS" push >/dev/null
WS_LIST=$("$FEANORFS" workspaces)
echo "$WS_LIST" | grep -q "$WS" || fail "workspace not listed on server"
pass "push update + workspaces"

step "Client A: status --json"
STATUS_JSON=$("$FEANORFS" --json status)
echo "$STATUS_JSON" | python3 -c "import json,sys; d=json.load(sys.stdin); assert d['mirror_state']=='idle', d; assert 'local_files' in d, d"
pass "status --json"

step "Client B: pull full file after A update"
cd "$CLIENT_B"
"$FEANORFS" pull >/dev/null
grep -q "smoke edit" hello.txt || fail "client B did not receive update"
pass "client B pull"

step "Cleanup smoke temp dir"
cd "$ROOT"
rm -rf "$SMOKE_ROOT"
trap - EXIT
pass "temp dirs removed"

echo ""
echo "All smoke checks passed."
