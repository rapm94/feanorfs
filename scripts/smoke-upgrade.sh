#!/bin/sh
# Smoke: source-level workspace compatibility across an in-place CLI upgrade.
#
# Creates an embedded workspace with the PREVIOUS release bytes, replaces the
# CLI at the same executable path, resumes with the NEW release, and proves
# that workspace identity, encrypted configuration, files, and reachable
# snapshot history remain intact.
#
# This source-level smoke deliberately does not claim service-restart coverage:
# an unmanaged `service run` process is not part of an OS login manager's
# inventory and cannot prove that an installer refreshes registered jobs. A
# separate hub, workspace worker, tray, native installer, and real login
# manager remain required installed-product evidence in TODO AI-1.
#
# Usage:
#   smoke-upgrade.sh OLD_BIN NEW_BIN
#     OLD_BIN  path to the previous release's `feanorfs` binary
#     NEW_BIN  path to the new release's `feanorfs` binary
#
# The previous release binary is provided by CI (built from the previous tag).
# Both binaries may also point at the same file when only the restart
# mechanics are under test; the coherence assertions must hold either way.
set -eu

OLD_BIN=${1:-}
NEW_BIN=${2:-}
if [ -z "$OLD_BIN" ] || [ -z "$NEW_BIN" ]; then
  echo "usage: $0 OLD_BIN NEW_BIN" >&2
  exit 2
fi
# Resolve to absolute paths: the worker subshell chdirs into the workspace.
OLD_BIN=$(cd "$(dirname "$OLD_BIN")" && pwd)/$(basename "$OLD_BIN")
NEW_BIN=$(cd "$(dirname "$NEW_BIN")" && pwd)/$(basename "$NEW_BIN")

ROOT=$(mktemp -d)
INSTALLED=
cleanup() {
  rm -rf "$ROOT"
}
trap 'cleanup' EXIT INT TERM

FEANORFS_TEST_HOME="$ROOT/state"
export FEANORFS_HOME="$FEANORFS_TEST_HOME"
mkdir -p "$FEANORFS_HOME"
WS="$ROOT/ws"
mkdir -p "$WS"
echo "seed content" > "$WS/seed.txt"
FAILURES=0

INSTALL_DIR="$ROOT/install"
mkdir -p "$INSTALL_DIR"
INSTALLED="$INSTALL_DIR/feanorfs"
cp "$OLD_BIN" "$INSTALLED"

fail() {
  echo "FAIL: $1" >&2
  FAILURES=$((FAILURES + 1))
}

json_string_values() {
  field=$1
  file=$2
  sed -n 's/.*"'"$field"'"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' "$file"
}

# --- Phase 1: previous release creates the workspace ------------------------
OLD_VERSION=$("$INSTALLED" --version | awk '{print $2}')
NEW_VERSION=$("$NEW_BIN" --version | awk '{print $2}')
echo "old release: $OLD_VERSION  new release: $NEW_VERSION"

(cd "$WS" && "$INSTALLED" start --local --workspace upgrade-ws --no-watch) >/dev/null 2>&1

STATE_DIR=
for candidate in "$FEANORFS_HOME"/workspaces/*; do
  if [ -d "$candidate" ]; then
    STATE_DIR=$candidate
    break
  fi
done
[ -n "$STATE_DIR" ] || {
  fail "workspace state directory missing"
  exit 1
}
WS_ID_BEFORE=$(json_string_values workspace_id "$STATE_DIR/config.json" | head -1)
CONFIG_HASH_BEFORE=$(cksum "$STATE_DIR/config.json" | awk '{print $1 $2}')
SEED_HASH_BEFORE=$(cksum "$WS/seed.txt" | awk '{print $1 $2}')
LOG_BEFORE="$ROOT/log-before.json"
(cd "$WS" && "$INSTALLED" --json log --limit 1000) > "$LOG_BEFORE"
grep -q '"snapshot_id"' "$LOG_BEFORE" || {
  fail "previous release did not return readable snapshot history"
  exit 1
}
json_string_values snapshot_id "$LOG_BEFORE" > "$ROOT/snapshots-before.txt"

# --- Phase 2: install the new release at the same path ----------------------
# Replace the old binary with the new one at the SAME path so the
# executable bytes change while the consumer-facing path remains stable.
cp "$NEW_BIN" "$INSTALLED.tmp"
mv "$INSTALLED.tmp" "$INSTALLED"

# --- Phase 3: resume with the new release -----------------------------------
(cd "$WS" && "$INSTALLED" start --no-watch -- "$WS") >/dev/null 2>&1 ||
  fail "new release could not resume the workspace"

# --- Assertions -------------------------------------------------------------
NEW_RUNNING="$("$INSTALLED" --version | awk '{print $2}')"
INSTALLED_HASH=$(cksum "$INSTALLED" | awk '{print $1 $2}')
NEW_HASH=$(cksum "$NEW_BIN" | awk '{print $1 $2}')
[ "$INSTALLED_HASH" = "$NEW_HASH" ] || fail "installed path does not contain the new CLI bytes"

# Workspace identity and encryption state are preserved.
WS_ID_AFTER=$(json_string_values workspace_id "$STATE_DIR/config.json" | head -1)
[ "$WS_ID_BEFORE" = "$WS_ID_AFTER" ] || fail "workspace identity changed: $WS_ID_BEFORE -> $WS_ID_AFTER"
CONFIG_HASH_AFTER=$(cksum "$STATE_DIR/config.json" | awk '{print $1 $2}')
[ "$CONFIG_HASH_BEFORE" = "$CONFIG_HASH_AFTER" ] || fail "encrypted workspace configuration changed across upgrade"

# Files are preserved byte-for-byte.
SEED_HASH_AFTER=$(cksum "$WS/seed.txt" | awk '{print $1 $2}')
[ "$SEED_HASH_BEFORE" = "$SEED_HASH_AFTER" ] || fail "file contents changed across upgrade"

# E2EE access remains usable through the new release.
(cd "$WS" && "$INSTALLED" cat seed.txt) > "$ROOT/seed-via-new-cli.txt" ||
  fail "new release could not decrypt the existing file"
cmp -s "$WS/seed.txt" "$ROOT/seed-via-new-cli.txt" ||
  fail "new release returned different decrypted file bytes"

# Reachable snapshot identity is preserved. Compare immutable IDs instead of
# complete JSON so additive presentation fields do not create false failures.
LOG_AFTER="$ROOT/log-after.json"
(cd "$WS" && "$INSTALLED" --json log --limit 1000) > "$LOG_AFTER" ||
  fail "new release did not return readable snapshot history"
json_string_values snapshot_id "$LOG_AFTER" > "$ROOT/snapshots-after.txt"
cmp -s "$ROOT/snapshots-before.txt" "$ROOT/snapshots-after.txt" ||
  fail "reachable snapshot history changed across upgrade"

if [ "$FAILURES" -eq 0 ]; then
  echo "smoke-upgrade-state: PASS (${OLD_VERSION} -> ${NEW_RUNNING})"
else
  echo "smoke-upgrade-state: FAIL ($FAILURES assertion(s))"
  exit 1
fi
