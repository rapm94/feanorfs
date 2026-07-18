#!/bin/sh
# Prove the native Linux desktop packages install and start on clean,
# currently supported Debian, Fedora, and Arch systems.

set -eu

debian_image="${FEANORFS_DEBIAN_SMOKE_IMAGE:-docker.io/library/debian:13-slim@sha256:020c0d20b9880058cbe785a9db107156c3c75c2ac944a6aa7ab59f2add76a7bd}"
fedora_image="${FEANORFS_FEDORA_SMOKE_IMAGE:-docker.io/library/fedora:44@sha256:6c75d5bf57cb0fa5aa4b92c6a83c86c791644496d9ac230de7711f5b8ec3b898}"
archlinux_image="${FEANORFS_ARCH_SMOKE_IMAGE:-docker.io/library/archlinux:base@sha256:fe6972d4dc1f660c0c10f4c41b2de8986bab89e7e2955378f8beadb8ebcd7433}"

if [ "$#" -ne 3 ]; then
    echo "Usage: $0 FEANORFS_DEB FEANORFS_RPM FEANORFS_ARCH_PACKAGE" >&2
    exit 2
fi
container_runtime="${FEANORFS_CONTAINER_RUNTIME:-}"
if [ -z "$container_runtime" ]; then
    if command -v docker >/dev/null 2>&1; then
        container_runtime=docker
    elif command -v podman >/dev/null 2>&1; then
        container_runtime=podman
    else
        echo "error: docker or podman is required for clean Linux package smoke" >&2
        exit 1
    fi
fi
command -v "$container_runtime" >/dev/null 2>&1 || {
    echo "error: container runtime '$container_runtime' is unavailable" >&2
    exit 1
}
"$container_runtime" info >/dev/null 2>&1 || {
    echo "error: container runtime '$container_runtime' is not running" >&2
    exit 1
}

absolute_file() {
    directory=$(cd "$(dirname "$1")" && pwd -P)
    printf '%s/%s\n' "$directory" "$(basename "$1")"
}

deb=$(absolute_file "$1")
rpm=$(absolute_file "$2")
arch_package=$(absolute_file "$3")
[ -f "$deb" ] || { echo "error: missing $deb" >&2; exit 1; }
[ -f "$rpm" ] || { echo "error: missing $rpm" >&2; exit 1; }
[ -f "$arch_package" ] || { echo "error: missing $arch_package" >&2; exit 1; }

echo "Smoke: clean Debian 13 desktop package install"
"$container_runtime" run --rm --pull=always \
    --mount "type=bind,source=$deb,target=/tmp/feanorfs.deb,readonly" \
    "$debian_image" sh -s <<'DEBIAN_SMOKE'
set -eu
export DEBIAN_FRONTEND=noninteractive
apt-get -qq update
apt-get -qq install -y --no-install-recommends \
    /tmp/feanorfs.deb dbus-x11 jq xvfb

feanorfs --version | grep -E '^feanorfs [0-9]'
test -x /usr/bin/feanorfs-tray
test -f /usr/share/applications/com.feanorfs.tray.desktop
test -f /usr/share/icons/hicolor/scalable/apps/com.feanorfs.tray.svg

home=$(mktemp -d)
workspace="$home/workspace"
mkdir -p "$workspace"
printf 'Debian package smoke\n' > "$workspace/smoke.txt"
HOME="$home" FEANORFS_CREDENTIAL_STORE=file \
    /usr/bin/feanorfs start --local --no-watch "$workspace" >/dev/null
status_json=$(
    cd "$workspace"
    HOME="$home" FEANORFS_CREDENTIAL_STORE=file /usr/bin/feanorfs --json status
)
printf '%s\n' "$status_json" | jq -e '
    .mirror_state == "idle" and
    .local_file_count == 1 and
    (.upload_required | length) == 0 and
    (.download_required | length) == 0 and
    (.pending_conflicts | length) == 0
' >/dev/null
test "$(stat -c '%a' "$workspace/.feanorfs/config.json")" = 600
grep -q '"format_version": 3' "$workspace/.feanorfs/config.json"
grep -q '"hub_local": true' "$workspace/.feanorfs/config.json"
test -s "$workspace/.feanorfs/refs/last-synced"
find "$workspace/.feanorfs/objects" -type f -print -quit | grep -q .

Xvfb :99 -screen 0 1024x768x24 >/tmp/feanorfs-xvfb.log 2>&1 &
xvfb_pid=$!
trap 'kill "$xvfb_pid" 2>/dev/null || true; rm -rf "$home"' EXIT HUP INT TERM
sleep 1
kill -0 "$xvfb_pid"

status=0
HOME="$home" DISPLAY=:99 timeout --signal=TERM --kill-after=2s 3s \
    dbus-run-session -- /usr/bin/feanorfs-tray \
    >/tmp/feanorfs-tray.log 2>&1 || status=$?
if [ "$status" -ne 124 ]; then
    echo "error: Debian tray exited during startup (status $status)" >&2
    cat /tmp/feanorfs-tray.log >&2
    exit 1
fi
DEBIAN_SMOKE

echo "Smoke: clean Fedora 44 desktop package install"
"$container_runtime" run --rm --pull=always \
    --mount "type=bind,source=$rpm,target=/tmp/feanorfs.rpm,readonly" \
    "$fedora_image" sh -s <<'FEDORA_SMOKE'
set -eu
dnf -q install -y --setopt=install_weak_deps=False \
    /tmp/feanorfs.rpm dbus-daemon jq xorg-x11-server-Xvfb

feanorfs --version | grep -E '^feanorfs [0-9]'
test -x /usr/bin/feanorfs-tray
test -f /usr/share/applications/com.feanorfs.tray.desktop
test -f /usr/share/icons/hicolor/scalable/apps/com.feanorfs.tray.svg

home=$(mktemp -d)
workspace="$home/workspace"
mkdir -p "$workspace"
printf 'Fedora package smoke\n' > "$workspace/smoke.txt"
HOME="$home" FEANORFS_CREDENTIAL_STORE=file \
    /usr/bin/feanorfs start --local --no-watch "$workspace" >/dev/null
status_json=$(
    cd "$workspace"
    HOME="$home" FEANORFS_CREDENTIAL_STORE=file /usr/bin/feanorfs --json status
)
printf '%s\n' "$status_json" | jq -e '
    .mirror_state == "idle" and
    .local_file_count == 1 and
    (.upload_required | length) == 0 and
    (.download_required | length) == 0 and
    (.pending_conflicts | length) == 0
' >/dev/null
test "$(stat -c '%a' "$workspace/.feanorfs/config.json")" = 600
grep -q '"format_version": 3' "$workspace/.feanorfs/config.json"
grep -q '"hub_local": true' "$workspace/.feanorfs/config.json"
test -s "$workspace/.feanorfs/refs/last-synced"
find "$workspace/.feanorfs/objects" -type f -print -quit | grep -q .

Xvfb :99 -screen 0 1024x768x24 >/tmp/feanorfs-xvfb.log 2>&1 &
xvfb_pid=$!
trap 'kill "$xvfb_pid" 2>/dev/null || true; rm -rf "$home"' EXIT HUP INT TERM
sleep 1
kill -0 "$xvfb_pid"

status=0
HOME="$home" DISPLAY=:99 timeout --signal=TERM --kill-after=2s 3s \
    dbus-run-session -- /usr/bin/feanorfs-tray \
    >/tmp/feanorfs-tray.log 2>&1 || status=$?
if [ "$status" -ne 124 ]; then
    echo "error: Fedora tray exited during startup (status $status)" >&2
    cat /tmp/feanorfs-tray.log >&2
    exit 1
fi
FEDORA_SMOKE

case "$(uname -m)" in
x86_64|amd64)
    echo "Smoke: clean Arch Linux desktop package install"
    "$container_runtime" run --rm --pull=always \
    --mount "type=bind,source=$arch_package,target=/tmp/feanorfs.pkg.tar.zst,readonly" \
    "$archlinux_image" sh -s <<'ARCH_SMOKE'
set -eu
pacman -Syu --noconfirm --needed dbus jq xorg-server-xvfb
pacman -U --noconfirm /tmp/feanorfs.pkg.tar.zst

feanorfs --version | grep -E '^feanorfs [0-9]'
test -x /usr/bin/feanorfs-tray
test -f /usr/share/applications/com.feanorfs.tray.desktop
test -f /usr/share/icons/hicolor/scalable/apps/com.feanorfs.tray.svg

home=$(mktemp -d)
workspace="$home/workspace"
mkdir -p "$workspace"
printf 'Arch package smoke\n' > "$workspace/smoke.txt"
HOME="$home" FEANORFS_CREDENTIAL_STORE=file \
    /usr/bin/feanorfs start --local --no-watch "$workspace" >/dev/null
status_json=$(
    cd "$workspace"
    HOME="$home" FEANORFS_CREDENTIAL_STORE=file /usr/bin/feanorfs --json status
)
printf '%s\n' "$status_json" | jq -e '
    .mirror_state == "idle" and
    .local_file_count == 1 and
    (.upload_required | length) == 0 and
    (.download_required | length) == 0 and
    (.pending_conflicts | length) == 0
' >/dev/null
test "$(stat -c '%a' "$workspace/.feanorfs/config.json")" = 600
grep -q '"format_version": 3' "$workspace/.feanorfs/config.json"
grep -q '"hub_local": true' "$workspace/.feanorfs/config.json"
test -s "$workspace/.feanorfs/refs/last-synced"
find "$workspace/.feanorfs/objects" -type f -print -quit | grep -q .

Xvfb :99 -screen 0 1024x768x24 >/tmp/feanorfs-xvfb.log 2>&1 &
xvfb_pid=$!
trap 'kill "$xvfb_pid" 2>/dev/null || true; rm -rf "$home"' EXIT HUP INT TERM
sleep 1
kill -0 "$xvfb_pid"

status=0
HOME="$home" DISPLAY=:99 timeout --signal=TERM --kill-after=2s 3s \
    dbus-run-session -- /usr/bin/feanorfs-tray \
    >/tmp/feanorfs-tray.log 2>&1 || status=$?
if [ "$status" -ne 124 ]; then
    echo "error: Arch tray exited during startup (status $status)" >&2
    cat /tmp/feanorfs-tray.log >&2
    exit 1
fi
ARCH_SMOKE
    ;;
aarch64|arm64)
    # Docker's official Arch Linux image is x86-64-only. The native ARM64
    # binary has already executed above in Debian and Fedora; package-linux.sh
    # independently verifies the Arch package's architecture, dependencies,
    # and exact payload.
    echo "Arch Linux has no official ARM64 container; verified ARM64 package metadata/payload and Debian/Fedora native execution."
    ;;
*)
    echo "error: unsupported Linux smoke architecture: $(uname -m)" >&2
    exit 1
    ;;
esac

echo "Clean Debian, Fedora, and Arch package smoke passed."
