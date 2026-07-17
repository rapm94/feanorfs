#!/bin/bash
set -euo pipefail
umask 077

if [[ "$(uname -s)" != "Darwin" ]]; then
  echo "This signed-release credential smoke requires macOS." >&2
  exit 2
fi
if [[ "$#" -ne 1 ]]; then
  echo "Usage: $0 SIGNED_FEANORFS_BIN" >&2
  exit 2
fi

FEANORFS="$(cd "$(dirname "$1")" && pwd)/$(basename "$1")"
[[ -x "$FEANORFS" ]] || { echo "Missing feanorfs binary: $FEANORFS" >&2; exit 1; }

SIGNATURE="$(/usr/bin/codesign --display --verbose=4 "$FEANORFS" 2>&1)" || {
  echo "The credential smoke requires a valid signed FeanorFS binary." >&2
  exit 1
}
grep -Fq 'Authority=Developer ID Application:' <<<"$SIGNATURE" || {
  echo "The credential smoke requires a Developer ID Application signature." >&2
  exit 1
}
/usr/bin/codesign --verify --strict "$FEANORFS"

ROOT="$(mktemp -d "${TMPDIR:-/tmp}/feanorfs-keychain-smoke.XXXXXX")"
SMOKE_HOME="$ROOT/home"
WORKSPACE="$ROOT/workspace"
CREDENTIAL_ID=""
mkdir -p "$SMOKE_HOME" "$WORKSPACE"

cleanup() {
  if [[ -n "$CREDENTIAL_ID" ]]; then
    /usr/bin/security delete-generic-password \
      -s com.feanorfs.credentials \
      -a "$CREDENTIAL_ID" >/dev/null 2>&1 || true
  fi
  find "$ROOT" -depth -delete 2>/dev/null || true
}
trap cleanup EXIT HUP INT TERM

(
  cd "$WORKSPACE"
  HOME="$SMOKE_HOME" "$FEANORFS" start --local --workspace signed-keychain-smoke --no-watch \
    >/dev/null
)

CONFIG="$WORKSPACE/.feanorfs/config.json"
CREDENTIAL_ID="$(jq -er '
  select(.credential_store == "os")
  | select(has("encryption_password") | not)
  | select(has("server_password") | not)
  | .credential_id
  | select(startswith("fsc1-") and length == 37)
' "$CONFIG")"

/usr/bin/security find-generic-password \
  -s com.feanorfs.credentials \
  -a "$CREDENTIAL_ID" >/dev/null

(
  cd "$WORKSPACE"
  HOME="$SMOKE_HOME" "$FEANORFS" --json status | \
    jq -e '.mirror_state == "idle"' >/dev/null
)

echo "Signed macOS credential smoke passed: config is redacted and Keychain-backed."
printf 'cli_sha256=%s\n' "$(shasum -a 256 "$FEANORFS" | awk '{print $1}')"
