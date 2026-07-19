#!/usr/bin/env bash
# SDK-2: opencode-style driver — full agent loop via feanorfs --json only.
# Runs in a non-git directory with in-process local hub (no network daemon).
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
DEMO="${TMPDIR:-/tmp}/feanorfs-sdk-$$"
WS="$DEMO/workspace"

cleanup() { rm -rf "$DEMO"; }
trap cleanup EXIT

FEANORFS="${FEANORFS_BIN:-$ROOT/target/debug/feanorfs}"

build_if_needed() {
  if [[ ! -x "$FEANORFS" ]]; then
    cargo build -q --manifest-path "$ROOT/Cargo.toml" -p feanorfs-client
  fi
}

json() { "$FEANORFS" --json "$@"; }

build_if_needed
mkdir -p "$WS"
cd "$WS"

# Local hub workspace — no git, no server process
"$FEANORFS" start --local --workspace sdk-demo --no-watch
echo "seed" > seed.txt
"$FEANORFS" sync --no-watch

echo "== spawn agent =="
SPAWN_JSON="$(json agent spawn worker)"
printf '%s\n' "$SPAWN_JSON" >&2
grep -q '"files_copied"' <<<"$SPAWN_JSON"

"$FEANORFS" agent run worker -- sh -c 'printf "%s\n" "agent edit" > task.txt'

echo "== land (expect clean or conflicts JSON) =="
LAND_JSON="$(json agent land worker)"
echo "$LAND_JSON"
echo "$LAND_JSON" | grep -qE '"landed"|"message"'
LAND_ID="$(echo "$LAND_JSON" | python3 -c "import json,sys; print(json.load(sys.stdin)['snapshot_id'])")"

echo "== log landed snapshot =="
LOG_JSON="$(json log --limit 10)"
echo "$LOG_JSON" | python3 -c "import json,sys; d=json.load(sys.stdin); target='$LAND_ID'; assert any(e['snapshot_id']==target for e in d['entries'])"
PRE_LAND_ID="$(echo "$LOG_JSON" | python3 -c "import json,sys; d=json.load(sys.stdin); target='$LAND_ID'; e=next(e for e in d['entries'] if e['snapshot_id']==target); print(e['parents'][-1])")"

echo "== undo landed snapshot =="
UNDO_JSON="$(json undo "$PRE_LAND_ID")"
UNDO_ID="$(echo "$UNDO_JSON" | python3 -c "import json,sys; print(json.load(sys.stdin)['snapshot_id'])")"
json log --limit 10 | python3 -c "import json,sys; d=json.load(sys.stdin); target='$UNDO_ID'; assert d['entries'][0]['snapshot_id']==target"

if echo "$LAND_JSON" | grep -q '"conflicts":\[\]'; then
  echo "== clean land — done =="
elif echo "$LAND_JSON" | grep -q '"conflicts":\['; then
  echo "== reconcile via conflicts keep --file =="
  CONFLICT_PATH="$(echo "$LAND_JSON" | python3 -c "import json,sys; d=json.load(sys.stdin); print(d['conflicts'][0]['path'] if d.get('conflicts') else '')" 2>/dev/null || true)"
  if [[ -n "$CONFLICT_PATH" ]]; then
    LOCAL_ART="$(echo "$LAND_JSON" | python3 -c "import json,sys; print(json.load(sys.stdin)['conflicts'][0].get('local_file',''))")"
    if [[ -n "$LOCAL_ART" && -f "$LOCAL_ART" ]]; then
      "$FEANORFS" conflicts keep "$CONFLICT_PATH" --file "$LOCAL_ART"
    else
      "$FEANORFS" conflicts keep "$CONFLICT_PATH" --local
    fi
  fi
fi

json agent clean worker
echo "SDK-2 loop OK"
