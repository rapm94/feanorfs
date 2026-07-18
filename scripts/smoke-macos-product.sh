#!/bin/bash
set -euo pipefail
umask 077
trap 'echo "macOS product smoke stopped at line $LINENO (status $?)." >&2' ERR

if [[ "$(uname -s)" != "Darwin" ]]; then
  echo "This product smoke test requires macOS." >&2
  exit 2
fi
if [[ "$#" -ne 2 ]]; then
  echo "Usage: $0 FEANORFS_BIN FEANORFS_TRAY_BIN" >&2
  exit 2
fi

FEANORFS="$(cd "$(dirname "$1")" && pwd)/$(basename "$1")"
FEANORFS_TRAY="$(cd "$(dirname "$2")" && pwd)/$(basename "$2")"
[[ -x "$FEANORFS" ]] || { echo "Missing feanorfs binary: $FEANORFS" >&2; exit 1; }
[[ -x "$FEANORFS_TRAY" ]] || { echo "Missing tray binary: $FEANORFS_TRAY" >&2; exit 1; }

ROOT="$(mktemp -d "${TMPDIR:-/tmp}/feanorfs-product-smoke.XXXXXX")"
ROOT="$(cd "$ROOT" && pwd -P)"
export HOME="$ROOT/home"
export FEANORFS_TRAY_BIN="$FEANORFS_TRAY"
WORKSPACE="$ROOT/workspace"
JOINED_WORKSPACE="$ROOT/joined-workspace"
PAIR_PID=""
JOIN_PID=""
EMPTY_TRAY_PID=""
RELAY_PID=""
mkdir -p "$HOME" "$WORKSPACE"
printf 'workspace recovery smoke\n' >"$WORKSPACE/recovery-smoke.txt"

cleanup() {
  if [[ -n "$PAIR_PID" ]] && kill -0 "$PAIR_PID" 2>/dev/null; then
    kill "$PAIR_PID" 2>/dev/null || true
    wait "$PAIR_PID" 2>/dev/null || true
  fi
  if [[ -n "$JOIN_PID" ]] && kill -0 "$JOIN_PID" 2>/dev/null; then
    kill "$JOIN_PID" 2>/dev/null || true
    wait "$JOIN_PID" 2>/dev/null || true
  fi
  if [[ -n "$EMPTY_TRAY_PID" ]] && kill -0 "$EMPTY_TRAY_PID" 2>/dev/null; then
    kill "$EMPTY_TRAY_PID" 2>/dev/null || true
    wait "$EMPTY_TRAY_PID" 2>/dev/null || true
  fi
  if [[ -n "$RELAY_PID" ]] && kill -0 "$RELAY_PID" 2>/dev/null; then
    kill "$RELAY_PID" 2>/dev/null || true
    wait "$RELAY_PID" 2>/dev/null || true
  fi
  printf '' | /usr/bin/pbcopy 2>/dev/null || true
  "$FEANORFS" service uninstall "$WORKSPACE" >/dev/null 2>&1 || true
  "$FEANORFS" service uninstall "$JOINED_WORKSPACE" >/dev/null 2>&1 || true
  for plist in "$HOME"/Library/LaunchAgents/com.feanorfs.{tray,hub}.plist; do
    if [[ -f "$plist" ]]; then
      /bin/launchctl unload "$plist" >/dev/null 2>&1 || true
    fi
  done
  rm -rf "$ROOT"
}
trap cleanup EXIT HUP INT TERM

echo "Smoke: first-run tray and automatic private hub"
(
  cd "$HOME"
  exec env -u FEANORFS_WORKSPACE \
    HOME="$HOME" \
    FEANORFS_BIN="$FEANORFS" \
    "$FEANORFS_TRAY" --first-run >"$ROOT/empty-tray.log" 2>&1
) &
EMPTY_TRAY_PID=$!
for _ in {1..20}; do
  kill -0 "$EMPTY_TRAY_PID" 2>/dev/null || break
  sleep 0.1
done
if ! kill -0 "$EMPTY_TRAY_PID" 2>/dev/null; then
  echo "Tray exited instead of presenting first-run folder setup." >&2
  exit 1
fi
sample_captured=false
first_run_choice_visible=false
for _ in {1..3}; do
  kill -0 "$EMPTY_TRAY_PID" 2>/dev/null || break
  rm -f "$ROOT/first-run-tray.sample"
  if /usr/bin/sample "$EMPTY_TRAY_PID" 1 \
    -file "$ROOT/first-run-tray.sample" >/dev/null 2>&1; then
    sample_captured=true
    if grep -Fq 'CFUserNotificationDisplayAlert' "$ROOT/first-run-tray.sample"; then
      first_run_choice_visible=true
      break
    fi
  fi
  sleep 0.25
done
if [[ "$first_run_choice_visible" != true ]]; then
  if [[ "$sample_captured" != true ]]; then
    echo "Could not sample the running tray after three bounded attempts." >&2
  else
    echo "Tray stayed alive but did not present the native first-run start-or-join choice." >&2
  fi
  exit 1
fi
kill "$EMPTY_TRAY_PID"
wait "$EMPTY_TRAY_PID" 2>/dev/null || true
EMPTY_TRAY_PID=""

if ! "$FEANORFS" start "$WORKSPACE" >"$ROOT/initial-start.log" 2>&1; then
  echo "Initial automatic-hub start failed:" >&2
  sed -E \
    -e 's/fn[hrp][12]-[A-Za-z0-9-]+/[capability redacted]/g' \
    -e 's/[0-9a-f]{64}/[secret-or-hash redacted]/g' \
    "$ROOT/initial-start.log" >&2
  exit 1
fi

HUB_PORT="$(<"$HOME/.feanorfs/hub-data/listen-port")"
if [[ ! "$HUB_PORT" =~ ^[0-9]+$ ]] || (( HUB_PORT < 1 || HUB_PORT > 65535 )); then
  echo "Automatic private hub did not persist a valid listen port." >&2
  exit 1
fi

jq -e '.workspace_id | startswith("fsw1-") and length == 37' \
  "$WORKSPACE/.feanorfs/config.json" >/dev/null
# Same-host multicast is unavailable on GitHub-hosted runners. In that case
# endpoint discovery deliberately retains the authenticated HTTPS loopback URL
# until the CA-bound mDNS name can be probed successfully.
jq -e --arg port "$HUB_PORT" \
  '.server_url | test("^https://(127\\.0\\.0\\.1|feanorfs-[0-9a-f]{16}\\.local):" + $port + "$")' \
  "$WORKSPACE/.feanorfs/config.json" >/dev/null

# Resume must hand off directly to the supervised watcher without a transient
# lock failure or launchd restart backoff.
"$FEANORFS" start "$WORKSPACE" >/dev/null
"$FEANORFS" service status "$WORKSPACE" | grep -q 'Automatic sync is running'

HUB_PLIST="$HOME/Library/LaunchAgents/com.feanorfs.hub.plist"
TRAY_PLIST="$HOME/Library/LaunchAgents/com.feanorfs.tray.plist"
SYNC_PLIST="$(find "$HOME/Library/LaunchAgents" -maxdepth 1 -name 'com.feanorfs.sync-*.plist' -print -quit)"
[[ -f "$HUB_PLIST" && -f "$TRAY_PLIST" && -n "$SYNC_PLIST" ]]

hub_json="$(/usr/bin/plutil -convert json -o - "$HUB_PLIST")"
sync_json="$(/usr/bin/plutil -convert json -o - "$SYNC_PLIST")"
tray_json="$(/usr/bin/plutil -convert json -o - "$TRAY_PLIST")"
jq -e --arg bin "$FEANORFS" --arg data "$HOME/.feanorfs/hub-data" \
  '.ProgramArguments == [$bin, "service", "hub-run", $data] and (.EnvironmentVariables == null)' \
  <<<"$hub_json" >/dev/null
jq -e --arg bin "$FEANORFS" --arg workspace "$WORKSPACE" \
  '.ProgramArguments == [$bin, "service", "run", $workspace]' \
  <<<"$sync_json" >/dev/null
jq -e --arg bin "$FEANORFS" --arg tray "$FEANORFS_TRAY" \
  '.ProgramArguments == [$tray] and .EnvironmentVariables.FEANORFS_BIN == $bin' \
  <<<"$tray_json" >/dev/null

for private_file in \
  "$HOME/.feanorfs/hub-data/auth-token" \
  "$HOME/.feanorfs/hub-data/listen-port" \
  "$HOME/.feanorfs/hub-data/tls/ca-key.pem" \
  "$HOME/.feanorfs/hub-data/tls/server-key.pem" \
  "$WORKSPACE/.feanorfs/config.json"; do
  [[ "$(/usr/bin/stat -f '%Lp' "$private_file")" == "600" ]]
done

jq -e '(length == 1) and ((.[0][1] | length) == 64)' \
  "$HOME/.feanorfs/hub-data/service-program" >/dev/null
jq -e '(length == 1) and ((.[0][1] | length) == 64)' \
  "$WORKSPACE/.feanorfs/service-program" >/dev/null
jq -e 'length == 2 and all(.[][1]; length == 64)' \
  "$HOME/.feanorfs/tray-service-program" >/dev/null

CA="$HOME/.feanorfs/hub-data/tls/ca-cert.pem"
[[ "$(curl --cacert "$CA" -o /dev/null -sS -w '%{http_code}' "https://127.0.0.1:$HUB_PORT/api/workspaces")" == "401" ]]
CONFIGURED_HOST="$(jq -r '.server_url | capture("^https://(?<host>[^:]+):[0-9]+$").host' "$WORKSPACE/.feanorfs/config.json")"
[[ "$(curl --resolve "$CONFIGURED_HOST:$HUB_PORT:127.0.0.1" --cacert "$CA" -o /dev/null -sS -w '%{http_code}' "https://$CONFIGURED_HOST:$HUB_PORT/api/workspaces")" == "401" ]]
if curl -o /dev/null -fsS "http://127.0.0.1:$HUB_PORT/api/workspaces" 2>/dev/null; then
  echo "Private hub unexpectedly accepted plaintext HTTP." >&2
  exit 1
fi

(cd "$WORKSPACE" && "$FEANORFS" --json doctor) | jq -e '
  .ok == true
  and ([.checks[].name] | sort == [
    "automatic_sync",
    "e2ee",
    "global_config",
    "local_state",
    "private_hub",
    "remote_workspace",
    "server",
    "tray_registration",
    "workspace_config",
    "workspace_format"
  ])
  and all(.checks[]; .status == "ok")
' >/dev/null
TRAY_STATUS="$ROOT/tray-status.json"
tray_ready=false
for _ in {1..20}; do
  if (cd "$WORKSPACE" && "$FEANORFS" --json tray status) >"$TRAY_STATUS" &&
    jq -e '.mirror_state == "idle" and .watching == true and .paused == false' \
      "$TRAY_STATUS" >/dev/null; then
    tray_ready=true
    break
  fi
  sleep 0.5
done
if [[ "$tray_ready" != true ]]; then
  echo "Tray did not reach idle, watching, and unpaused state within the bounded readiness window." >&2
  exit 1
fi
/bin/launchctl print "gui/$(id -u)/com.feanorfs.tray" >/dev/null

echo "Smoke: TLS, doctor, tray status, and MCP"
MCP_OUT="$ROOT/mcp.jsonl"
(
  cd "$WORKSPACE"
  printf '%s\n' \
    '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}' \
    '{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}' \
    '{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"sync_status","arguments":{}}}' |
    "$FEANORFS" mcp >"$MCP_OUT"
)
jq -s -e \
  'length == 3 and .[0].result.serverInfo.name == "feanorfs" and (.[1].result.tools | length) == 9 and .[2].result.mirror_state == "idle"' \
  "$MCP_OUT" >/dev/null

echo "Smoke: encrypted workspace recovery"
# Workspace recovery must keep the full capability encrypted at rest, reject a
# wrong passphrase before creating a destination, and restore through the same
# start/sync path without putting the passphrase in argv or the environment.
RECOVERY_KIT="$ROOT/workspace.fnrk"
RECOVERED_WORKSPACE="$ROOT/recovered"
WRONG_RECOVERY_WORKSPACE="$ROOT/wrong-recovery"
RECOVERY_PASSPHRASE="$(/usr/bin/uuidgen | /usr/bin/tr -d '-')-recovery"
WRONG_RECOVERY_PASSPHRASE="$(/usr/bin/uuidgen | /usr/bin/tr -d '-')-wrong"
(
  cd "$WORKSPACE"
  printf '%s\n' "$RECOVERY_PASSPHRASE" |
    "$FEANORFS" recovery export --passphrase-stdin -- "$RECOVERY_KIT" >/dev/null
)
[[ "$(/usr/bin/stat -f '%Lp' "$RECOVERY_KIT")" == "600" ]]
workspace_id="$(jq -r '.workspace_id' "$WORKSPACE/.feanorfs/config.json")"
if /usr/bin/grep -Fq "$workspace_id" "$RECOVERY_KIT"; then
  echo "Workspace recovery kit exposed its workspace capability in plaintext." >&2
  exit 1
fi
if printf '%s\n' "$WRONG_RECOVERY_PASSPHRASE" |
  "$FEANORFS" recovery import --passphrase-stdin --no-watch -- \
    "$RECOVERY_KIT" "$WRONG_RECOVERY_WORKSPACE" >"$ROOT/recovery-wrong.log" 2>&1; then
  echo "Workspace recovery accepted an incorrect passphrase." >&2
  exit 1
fi
[[ ! -e "$WRONG_RECOVERY_WORKSPACE" ]]
printf '%s\n' "$RECOVERY_PASSPHRASE" |
  "$FEANORFS" recovery import --passphrase-stdin --no-watch -- \
    "$RECOVERY_KIT" "$RECOVERED_WORKSPACE" >"$ROOT/recovery-import.log"
/usr/bin/cmp "$WORKSPACE/recovery-smoke.txt" "$RECOVERED_WORKSPACE/recovery-smoke.txt"
jq -e --arg workspace "$workspace_id" '.workspace_id == $workspace' \
  "$RECOVERED_WORKSPACE/.feanorfs/config.json" >/dev/null
"$FEANORFS" --json stop -- "$RECOVERED_WORKSPACE" >/dev/null

echo "Smoke: receiver-side tray-to-tray LAN join"
PAIR_LOG="$ROOT/pair.log"
chmod 600 "$MCP_OUT"
(cd "$WORKSPACE" && "$FEANORFS" pair --tray --expires 30 >"$PAIR_LOG" 2>&1) &
PAIR_PID=$!
pair_ready=false
for _ in {1..40}; do
  if head -n 1 "$PAIR_LOG" | jq -e '
    .event == "ready"
    and (.code | startswith("fnp1-") and length == 24)
    and .expires_in_seconds == 30
    and (keys | sort == ["code", "event", "expires_in_seconds"])
  ' >/dev/null 2>&1; then
    pair_ready=true
    break
  fi
  kill -0 "$PAIR_PID" 2>/dev/null || break
  sleep 0.25
done
if [[ "$pair_ready" != true ]]; then
  echo "Pairing did not become ready." >&2
  exit 1
fi
if ps -p "$PAIR_PID" -o args= | grep -q 'fnp1-'; then
  echo "Pairing code unexpectedly appeared in process arguments." >&2
  exit 1
fi
PAIR_CODE="$(head -n 1 "$PAIR_LOG" | jq -r '.code')"
if [[ "${GITHUB_ACTIONS:-}" == "true" ]]; then
  # GitHub-hosted macOS runners discard same-host multicast between the two
  # mDNS sockets. The ready event is emitted only after the publisher confirms
  # a real announcement; full discovery remains an ignored, explicitly runnable
  # LAN-host test and is exercised by this smoke outside GitHub-hosted runners.
  kill "$PAIR_PID" 2>/dev/null || true
  wait "$PAIR_PID" 2>/dev/null || true
  PAIR_PID=""
  PAIR_CODE=""
  echo "GitHub-hosted runner: verified mDNS announcement readiness; same-host discovery skipped."
else
  (
    printf '%s\n' "$PAIR_CODE" |
      "$FEANORFS" tray join -- "$JOINED_WORKSPACE" >"$ROOT/tray-join.log" 2>&1
  ) &
  JOIN_PID=$!
  sleep 0.1
  if kill -0 "$JOIN_PID" 2>/dev/null && ps -p "$JOIN_PID" -o args= | grep -q 'fnp[12]-'; then
    echo "Receiver-side tray pairing capability unexpectedly appeared in process arguments." >&2
    exit 1
  fi
  if ! wait "$JOIN_PID"; then
    echo "Receiver-side tray join failed:" >&2
    sed -E 's/fnp[12]-[A-Za-z0-9-]+/[pairing capability redacted]/g' \
      "$ROOT/tray-join.log" >&2
    exit 1
  fi
  JOIN_PID=""
  if ! wait "$PAIR_PID"; then
    echo "Sharing-side pairing process failed during tray join." >&2
    exit 1
  fi
  PAIR_PID=""
  PAIR_CODE=""
  /usr/bin/cmp "$WORKSPACE/recovery-smoke.txt" "$JOINED_WORKSPACE/recovery-smoke.txt" || {
    echo "Tray-joined workspace did not materialize the shared file." >&2
    exit 1
  }
  jq -e --arg workspace "$workspace_id" '.workspace_id == $workspace' \
    "$JOINED_WORKSPACE/.feanorfs/config.json" >/dev/null || {
    echo "Tray-joined workspace identity does not match the sharing workspace." >&2
    exit 1
  }
  "$FEANORFS" service status "$JOINED_WORKSPACE" | grep -q 'Automatic sync is running' || {
    echo "Tray-joined workspace did not install automatic sync." >&2
    exit 1
  }
  jq -s -e 'length == 2 and .[0].event == "ready" and .[1].event == "paired"' \
    "$PAIR_LOG" >/dev/null || {
    echo "Sharing-side tray pairing did not finish with one ready and one paired event." >&2
    exit 1
  }
  "$FEANORFS" --json stop -- "$JOINED_WORKSPACE" >/dev/null
fi
printf '' | /usr/bin/pbcopy 2>/dev/null || true

echo "Smoke: off-LAN opaque relay pairing readiness"
RELAY_PORT=""
for candidate in {3040..3099}; do
  if ! /usr/sbin/lsof -nP -iTCP:"$candidate" -sTCP:LISTEN >/dev/null 2>&1; then
    RELAY_PORT="$candidate"
    break
  fi
done
if [[ -z "$RELAY_PORT" ]]; then
  echo "No free local relay port was available in 3040–3099." >&2
  exit 1
fi
"$FEANORFS" serve \
  --allow-http \
  --allow-open \
  --relay \
  --port "$RELAY_PORT" \
  --data-dir "$ROOT/relay-data" \
  >"$ROOT/relay.log" 2>&1 &
RELAY_PID=$!
relay_ready=false
for _ in {1..50}; do
  if curl -fsS "http://127.0.0.1:$RELAY_PORT/api/workspaces" >/dev/null 2>&1; then
    relay_ready=true
    break
  fi
  kill -0 "$RELAY_PID" 2>/dev/null || break
  sleep 0.1
done
if [[ "$relay_ready" != true ]]; then
  echo "Opaque relay did not become ready." >&2
  exit 1
fi

"$FEANORFS" start --relay "http://127.0.0.1:$RELAY_PORT" "$WORKSPACE" >/dev/null
(cd "$WORKSPACE" && "$FEANORFS" --json doctor) | jq -e '
  .ok == true
  and any(.checks[]; .name == "relay" and .status == "ok")
' >/dev/null

PAIR_LOG="$ROOT/pair-relay.log"
(cd "$WORKSPACE" && "$FEANORFS" pair --tray --expires 30 >"$PAIR_LOG" 2>&1) &
PAIR_PID=$!
pair_ready=false
for _ in {1..40}; do
  if head -n 1 "$PAIR_LOG" | jq -e '
    .event == "ready"
    and (.code | startswith("fnp2-") and length <= 900)
    and .expires_in_seconds == 30
    and (keys | sort == ["code", "event", "expires_in_seconds"])
  ' >/dev/null 2>&1; then
    pair_ready=true
    break
  fi
  kill -0 "$PAIR_PID" 2>/dev/null || break
  sleep 0.25
done
if [[ "$pair_ready" != true ]]; then
  echo "Off-LAN pairing did not become ready through the opaque relay." >&2
  exit 1
fi
if ps -p "$PAIR_PID" -o args= | grep -q 'fnp2-'; then
  echo "Off-LAN pairing capability unexpectedly appeared in process arguments." >&2
  exit 1
fi
kill "$PAIR_PID" 2>/dev/null || true
wait "$PAIR_PID" 2>/dev/null || true
PAIR_PID=""
printf '' | /usr/bin/pbcopy 2>/dev/null || true

echo "Smoke: reversible consumer stop and resume"
# Consumer offboarding must remove only this workspace's automatic lifecycle.
# Files, encrypted setup, remote snapshots, and the shared private hub survive.
"$FEANORFS" --json stop -- "$WORKSPACE" | jq -e --arg workspace "$WORKSPACE" '
  .workspace == $workspace
  and .mirroring == false
  and .tray_registered == false
  and .files_preserved == true
  and .setup_preserved == true
  and .hub_preserved == true
' >/dev/null
[[ -f "$WORKSPACE/.feanorfs/config.json" ]]
"$FEANORFS" service status "$WORKSPACE" | grep -q 'Automatic sync is not installed'
"$FEANORFS" --json tray recent | jq -e '
  .active == null and (.workspaces | length) == 0
' >/dev/null
/bin/launchctl print "gui/$(id -u)/com.feanorfs.hub" >/dev/null
[[ "$(curl --cacert "$CA" -o /dev/null -sS -w '%{http_code}' "https://127.0.0.1:$HUB_PORT/api/workspaces")" == "401" ]]

"$FEANORFS" start "$WORKSPACE" >/dev/null
"$FEANORFS" service status "$WORKSPACE" | grep -q 'Automatic sync is running'
"$FEANORFS" --json tray recent | jq -e --arg workspace "$WORKSPACE" '
  .active == $workspace and (.workspaces | length) == 1
' >/dev/null

echo "macOS product smoke passed: one-command host, services, tray-to-tray join, TLS, MCP, encrypted workspace recovery, LAN/off-LAN pairing, opaque relay, and reversible stop/resume."
