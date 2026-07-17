#!/bin/sh
set -eu

engine="${CONTAINER_ENGINE:-}"
if [ -z "$engine" ]; then
    if command -v docker >/dev/null 2>&1; then
        engine=docker
    elif command -v podman >/dev/null 2>&1; then
        engine=podman
    else
        echo "error: Docker or Podman is required" >&2
        exit 2
    fi
fi

root="$(cd "$(dirname "$0")/.." && pwd)"
suffix="$$"
image="${FEANORFS_RELAY_SMOKE_IMAGE:-feanorfs-relay-smoke:$suffix}"
name="feanorfs-relay-smoke-$suffix"
volume="feanorfs-relay-smoke-$suffix"

cleanup() {
    "$engine" rm -f "$name" >/dev/null 2>&1 || true
    "$engine" volume rm -f "$volume" >/dev/null 2>&1 || true
    if [ "${KEEP_FEANORFS_RELAY_SMOKE_IMAGE:-0}" != 1 ]; then
        "$engine" image rm -f "$image" >/dev/null 2>&1 || true
    fi
}
trap cleanup EXIT HUP INT TERM

if [ "$(basename "$engine")" = podman ]; then
    "$engine" build --format docker --file "$root/Dockerfile.relay" --tag "$image" "$root"
else
    "$engine" build --file "$root/Dockerfile.relay" --tag "$image" "$root"
fi
"$engine" volume create "$volume" >/dev/null

start_container() {
    "$engine" run --detach --rm \
        --name "$name" \
        --read-only \
        --tmpfs /tmp:rw,noexec,nosuid,nodev \
        --cap-drop ALL \
        --security-opt no-new-privileges \
        --health-interval 1s \
        --health-start-period 1s \
        --publish 127.0.0.1::3030 \
        --volume "$volume:/var/lib/feanorfs" \
        "$image" >/dev/null
}

wait_until_healthy() {
    attempts=0
    while [ "$attempts" -lt 50 ]; do
        status="$("$engine" inspect --format '{{.State.Health.Status}}' "$name" 2>/dev/null || true)"
        if [ "$status" = healthy ]; then
            return 0
        fi
        attempts=$((attempts + 1))
        sleep 0.1
    done
    return 1
}

wait_until_ready() {
    binding="$("$engine" port "$name" 3030/tcp | head -n 1)"
    port="${binding##*:}"
    [ -n "$port" ] || return 1
    attempts=0
    while [ "$attempts" -lt 100 ]; do
        code="$(curl --silent --output /dev/null --write-out '%{http_code}' \
            "http://127.0.0.1:$port/api/workspaces" || true)"
        if [ "$code" = 401 ]; then
            return 0
        fi
        if ! "$engine" inspect "$name" >/dev/null 2>&1; then
            return 1
        fi
        attempts=$((attempts + 1))
        sleep 0.1
    done
    return 1
}

start_container
if ! wait_until_ready; then
    "$engine" logs "$name" >&2 || true
    echo "error: relay container did not become ready" >&2
    exit 1
fi
if ! wait_until_healthy; then
    echo "error: relay container health check did not pass" >&2
    exit 1
fi

relay_code="$(curl --silent --output /dev/null --write-out '%{http_code}' \
    "http://127.0.0.1:$port/api/tunnel-relay/0000000000000000000000000000000000000000000000000000000000000000/client")"
[ "$relay_code" = 400 ]

[ "$("$engine" exec "$name" id -u)" = 10001 ]
[ "$("$engine" exec "$name" stat -c %a /var/lib/feanorfs/auth-token)" = 600 ]

command_json="$("$engine" inspect --format '{{json .Config.Cmd}}' "$name")"
if printf '%s\n' "$command_json" | grep -Eq -- '--token|--allow-open'; then
    echo "error: relay container command contains an explicit credential or open hub" >&2
    exit 1
fi

logs="$("$engine" logs "$name" 2>&1)"
if printf '%s\n' "$logs" | grep -Eq 'fnh1-|fnr1-|Authorization:|Bearer '; then
    echo "error: relay container logs contain credential-bearing material" >&2
    exit 1
fi

token_before="$("$engine" exec "$name" sha256sum /var/lib/feanorfs/auth-token)"
"$engine" stop "$name" >/dev/null

start_container
if ! wait_until_ready; then
    "$engine" logs "$name" >&2 || true
    echo "error: relay container did not restart from persistent state" >&2
    exit 1
fi
if ! wait_until_healthy; then
    echo "error: restarted relay container health check did not pass" >&2
    exit 1
fi
token_after="$("$engine" exec "$name" sha256sum /var/lib/feanorfs/auth-token)"
[ "$token_before" = "$token_after" ]

echo "Relay container smoke passed: non-root, read-only, authenticated, secret-free, and restart-stable."
