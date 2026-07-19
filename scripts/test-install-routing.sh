#!/bin/bash
set -euo pipefail

ROOT="$(mktemp -d "${TMPDIR:-/tmp}/feanorfs-install-routing.XXXXXX")"
trap 'rm -rf "$ROOT"' EXIT HUP INT TERM
STUBS="$ROOT/bin"
mkdir -p "$STUBS"

cat >"$STUBS/uname" <<'EOF'
#!/bin/sh
case "${1:-}" in
  -m) echo "${FAKE_UNAME_M:-arm64}" ;;
  *) echo "${FAKE_UNAME_S:-Darwin}" ;;
esac
EOF
chmod 755 "$STUBS/uname"

cat >"$STUBS/curl" <<'EOF'
#!/bin/sh
set -eu
url=""
output=""
while [ "$#" -gt 0 ]; do
  case "$1" in
    -o)
      output="$2"
      shift 2
      ;;
    http*)
      url="$1"
      shift
      ;;
    *)
      shift
      ;;
  esac
done

case "$url" in
  */releases/latest)
    printf '%s\n' "$FAKE_RELEASE_JSON"
    ;;
  */feanorfs-client-installer.sh)
    printf '%s\n' '#!/bin/sh' ': > "$FEANORFS_TEST_CLI_MARKER"'
    ;;
  */FeanorFS-macOS.pkg)
    printf 'not-a-signed-package' >"$output"
    ;;
  */FeanorFS-macOS.pkg.sha256)
    printf '%s  %s\n' "$FAKE_PACKAGE_SHA" 'FeanorFS-macOS.pkg' >"$output"
    ;;
  */FeanorFS-linux-x86_64.tar.xz)
    cp "$FAKE_LINUX_BUNDLE" "$output"
    ;;
  */FeanorFS-linux-x86_64.tar.xz.sha256)
    cp "$FAKE_LINUX_CHECKSUM" "$output"
    ;;
  */FeanorFS-linux-x86_64.deb)
    cp "$FAKE_LINUX_DEB" "$output"
    ;;
  */FeanorFS-linux-x86_64.deb.sha256)
    cp "$FAKE_LINUX_DEB_CHECKSUM" "$output"
    ;;
  */FeanorFS-linux-x86_64.rpm)
    cp "$FAKE_LINUX_RPM" "$output"
    ;;
  */FeanorFS-linux-x86_64.rpm.sha256)
    cp "$FAKE_LINUX_RPM_CHECKSUM" "$output"
    ;;
  */FeanorFS-linux-x86_64.pkg.tar.zst)
    cp "$FAKE_LINUX_ARCH_PACKAGE" "$output"
    ;;
  */FeanorFS-linux-x86_64.pkg.tar.zst.sha256)
    cp "$FAKE_LINUX_ARCH_CHECKSUM" "$output"
    ;;
  *)
    echo "unexpected test URL: $url" >&2
    exit 1
    ;;
esac
EOF
chmod 755 "$STUBS/curl"

cat >"$STUBS/ldd" <<'EOF'
#!/bin/sh
exit 0
EOF
chmod 755 "$STUBS/ldd"

cat >"$STUBS/ldconfig" <<'EOF'
#!/bin/sh
[ "${FAKE_LDCONFIG_MISSING:-0}" = 1 ] && exit 0
printf '%s\n' 'libayatana-appindicator3.so.1 => /usr/lib/libayatana-appindicator3.so.1'
EOF
chmod 755 "$STUBS/ldconfig"

cat >"$STUBS/pgrep" <<'EOF'
#!/bin/sh
exit 1
EOF
chmod 755 "$STUBS/pgrep"

cat >"$STUBS/id" <<'EOF'
#!/bin/sh
if [ "${1:-}" = -u ] && [ -n "${FAKE_ID_U:-}" ]; then
  echo "$FAKE_ID_U"
else
  /usr/bin/id "$@"
fi
EOF
chmod 755 "$STUBS/id"

cat >"$STUBS/sudo" <<'EOF'
#!/bin/sh
if [ "${1:-}" = -n ]; then shift; fi
exec "$@"
EOF
chmod 755 "$STUBS/sudo"

cat >"$STUBS/dpkg-deb" <<'EOF'
#!/bin/sh
case "$1:$3" in
  -f:Package) echo feanorfs ;;
  -f:Architecture) echo amd64 ;;
  --ctrl-tarfile:) tar -C "$FAKE_DEB_CONTROL" -cf - control ;;
  *) echo "unexpected dpkg-deb test arguments: $*" >&2; exit 1 ;;
esac
EOF
chmod 755 "$STUBS/dpkg-deb"

cat >"$STUBS/apt-get" <<'EOF'
#!/bin/sh
set -eu
[ "$1" = install ]
[ "$2" = -y ]
[ "$3" = --no-install-recommends ]
case "$4" in */FeanorFS-linux-x86_64.deb) ;; *) exit 1 ;; esac
: > "$FAKE_APT_MARKER"
mkdir -p "$FEANORFS_NATIVE_BIN_DIR"
cp "$FAKE_NATIVE_FEANORFS" "$FEANORFS_NATIVE_BIN_DIR/feanorfs"
cp "$FAKE_NATIVE_TRAY" "$FEANORFS_NATIVE_BIN_DIR/feanorfs-tray"
chmod 755 "$FEANORFS_NATIVE_BIN_DIR/feanorfs" "$FEANORFS_NATIVE_BIN_DIR/feanorfs-tray"
EOF
chmod 755 "$STUBS/apt-get"

cat >"$STUBS/rpm" <<'EOF'
#!/bin/sh
set -eu
[ "$1" = -qp ]
case "$2" in
  --queryformat)
    case "$3" in
      '%{NAME}') echo feanorfs ;;
      '%{ARCH}') echo x86_64 ;;
      *) echo "unexpected rpm query format: $3" >&2; exit 1 ;;
    esac
    ;;
  --scripts)
    ;;
  *)
    echo "unexpected rpm test arguments: $*" >&2
    exit 1
    ;;
esac
EOF
chmod 755 "$STUBS/rpm"

cat >"$STUBS/dnf" <<'EOF'
#!/bin/sh
set -eu
[ "$1" = install ]
[ "$2" = -y ]
[ "$3" = --setopt=install_weak_deps=False ]
case "$4" in */FeanorFS-linux-x86_64.rpm) ;; *) exit 1 ;; esac
: > "$FAKE_DNF_MARKER"
mkdir -p "$FEANORFS_NATIVE_BIN_DIR"
cp "$FAKE_NATIVE_FEANORFS" "$FEANORFS_NATIVE_BIN_DIR/feanorfs"
cp "$FAKE_NATIVE_TRAY" "$FEANORFS_NATIVE_BIN_DIR/feanorfs-tray"
chmod 755 "$FEANORFS_NATIVE_BIN_DIR/feanorfs" "$FEANORFS_NATIVE_BIN_DIR/feanorfs-tray"
EOF
chmod 755 "$STUBS/dnf"

cat >"$STUBS/bsdtar" <<'EOF'
#!/bin/sh
set -eu
case "$1" in
  -xOf)
    printf '%s\n' 'pkgname = feanorfs' 'arch = x86_64'
    ;;
  -tf)
    printf '%s\n' '.PKGINFO' 'usr/bin/feanorfs' 'usr/bin/feanorfs-tray'
    ;;
  *) echo "unexpected bsdtar test arguments: $*" >&2; exit 1 ;;
esac
EOF
chmod 755 "$STUBS/bsdtar"

cat >"$STUBS/pacman" <<'EOF'
#!/bin/sh
set -eu
[ "$1" = -U ]
[ "$2" = --noconfirm ]
case "$3" in */FeanorFS-linux-x86_64.pkg.tar.zst) ;; *) exit 1 ;; esac
: > "$FAKE_PACMAN_MARKER"
mkdir -p "$FEANORFS_NATIVE_BIN_DIR"
cp "$FAKE_NATIVE_FEANORFS" "$FEANORFS_NATIVE_BIN_DIR/feanorfs"
cp "$FAKE_NATIVE_TRAY" "$FEANORFS_NATIVE_BIN_DIR/feanorfs-tray"
chmod 755 "$FEANORFS_NATIVE_BIN_DIR/feanorfs" "$FEANORFS_NATIVE_BIN_DIR/feanorfs-tray"
EOF
chmod 755 "$STUBS/pacman"

export PATH="$STUBS:$PATH"
export FEANORFS_RELEASE_API="https://example.invalid/releases/latest"
export FEANORFS_BASE_URL="https://example.invalid/download/v9.9.9"
export FEANORFS_TEST_CLI_MARKER="$ROOT/cli-installed"
export FEANORFS_NO_LAUNCH=1
export HOME="$ROOT/home"
mkdir -p "$HOME"

export FAKE_RELEASE_JSON='{"tag_name":"v9.9.9","assets":[{"name":"feanorfs-client-installer.sh"}]}'
if sh scripts/install.sh >"$ROOT/fallback.log" 2>&1; then
  echo "CLI-only fallback unexpectedly replaced the desktop installer." >&2
  exit 1
fi
[[ ! -f "$FEANORFS_TEST_CLI_MARKER" ]]
grep -Fq 'no CLI-only fallback was installed' "$ROOT/fallback.log"

rm -f "$FEANORFS_TEST_CLI_MARKER"
export FAKE_RELEASE_JSON='{"tag_name":"v9.9.9","assets":[{"name":"FeanorFS-macOS.pkg"}]}'
if sh scripts/install.sh >"$ROOT/incomplete-package.log" 2>&1; then
  echo "macOS package without a checksum unexpectedly reached installation." >&2
  exit 1
fi
grep -Fq 'has a macOS package but no checksum' "$ROOT/incomplete-package.log"
[[ ! -f "$FEANORFS_TEST_CLI_MARKER" ]]

export FAKE_PACKAGE_SHA
FAKE_PACKAGE_SHA="$(printf 'not-a-signed-package' | shasum -a 256 | awk '{print $1}')"
export FAKE_RELEASE_JSON='{"tag_name":"v9.9.9","assets":[{"name":"FeanorFS-macOS.pkg"},{"name":"FeanorFS-macOS.pkg.sha256"}]}'
if sh scripts/install.sh >"$ROOT/package.log" 2>&1; then
  echo "Unsigned macOS package unexpectedly passed installer trust checks." >&2
  exit 1
fi
[[ ! -f "$FEANORFS_TEST_CLI_MARKER" ]]
grep -Fq 'Installing FeanorFS v9.9.9 for macOS' "$ROOT/package.log"

export FAKE_UNAME_S=Linux
export FAKE_UNAME_M=x86_64
export BINDIR="$ROOT/linux-bin"
export XDG_DATA_HOME="$ROOT/linux-data"
fixture="$ROOT/linux-fixture"
mkdir -p "$fixture"
printf '%s\n' '#!/bin/sh' 'exit 0' >"$fixture/feanorfs"
# The generated fixture expands this variable when the installed tray starts.
# shellcheck disable=SC2016
printf '%s\n' '#!/bin/sh' 'printf "%s\n" "$*" > "$FEANORFS_TEST_TRAY_MARKER"' 'sleep 5' >"$fixture/feanorfs-tray"
cp tray/assets/com.feanorfs.tray.desktop "$fixture/com.feanorfs.tray.desktop"
cp tray/assets/com.feanorfs.tray.svg "$fixture/com.feanorfs.tray.svg"
chmod 755 "$fixture/feanorfs" "$fixture/feanorfs-tray"
export FEANORFS_NATIVE_BIN_DIR="$ROOT/native-bin"
export FAKE_NATIVE_FEANORFS="$fixture/feanorfs"
export FAKE_NATIVE_TRAY="$fixture/feanorfs-tray"
export FAKE_LINUX_BUNDLE="$ROOT/FeanorFS-linux-x86_64.tar.xz"
tar -C "$fixture" -cJf "$FAKE_LINUX_BUNDLE" \
  feanorfs feanorfs-tray com.feanorfs.tray.desktop com.feanorfs.tray.svg
export FAKE_LINUX_CHECKSUM="$ROOT/FeanorFS-linux-x86_64.tar.xz.sha256"
export FEANORFS_TEST_TRAY_MARKER="$ROOT/tray-launched"
(
  cd "$ROOT"
  sha256sum "$(basename "$FAKE_LINUX_BUNDLE")" >"$FAKE_LINUX_CHECKSUM"
)

rm -f "$FEANORFS_TEST_CLI_MARKER"
export FAKE_RELEASE_JSON='{"tag_name":"v9.9.9","assets":[{"name":"FeanorFS-linux-x86_64.tar.xz"}]}'
if sh scripts/install.sh >"$ROOT/incomplete-linux.log" 2>&1; then
  echo "Linux desktop bundle without a checksum unexpectedly reached installation." >&2
  exit 1
fi
grep -Fq 'has a Linux desktop bundle but no checksum' "$ROOT/incomplete-linux.log"

export FAKE_RELEASE_JSON='{"tag_name":"v9.9.9","assets":[{"name":"FeanorFS-linux-x86_64.tar.xz"},{"name":"FeanorFS-linux-x86_64.tar.xz.sha256"}]}'
sh scripts/install.sh >"$ROOT/linux.log"
[[ -x "$BINDIR/feanorfs" && -x "$BINDIR/feanorfs-tray" ]]
[[ -f "$XDG_DATA_HOME/applications/com.feanorfs.tray.desktop" ]]
[[ -f "$XDG_DATA_HOME/icons/hicolor/scalable/apps/com.feanorfs.tray.svg" ]]
[[ ! -f "$FEANORFS_TEST_CLI_MARKER" ]]
grep -Fq 'CLI + system tray' "$ROOT/linux.log"
grep -Fq 'Headless setup: feanorfs start' "$ROOT/linux.log"
[[ ! -f "$FEANORFS_TEST_TRAY_MARKER" ]]

tray_before="$(shasum -a 256 "$BINDIR/feanorfs-tray" | awk '{print $1}')"
export FAKE_LDCONFIG_MISSING=1
if sh scripts/install.sh >"$ROOT/missing-appindicator.log" 2>&1; then
  echo "Linux bundle with a missing AppIndicator runtime unexpectedly installed." >&2
  exit 1
fi
unset FAKE_LDCONFIG_MISSING
grep -Fq 'sudo apt-get install libgtk-3-0 libayatana-appindicator3-1' "$ROOT/missing-appindicator.log"
[[ "$(shasum -a 256 "$BINDIR/feanorfs-tray" | awk '{print $1}')" = "$tray_before" ]]

unset FEANORFS_NO_LAUNCH
export DISPLAY=:99
export DBUS_SESSION_BUS_ADDRESS=unix:path=/tmp/feanorfs-test-bus
sh scripts/install.sh >"$ROOT/linux-launch.log"
[[ -f "$FEANORFS_TEST_TRAY_MARKER" ]]
grep -Fxq -- '--first-run' "$FEANORFS_TEST_TRAY_MARKER"
grep -Fq 'FeanorFS is now in your system tray' "$ROOT/linux-launch.log"
grep -Fq 'no Terminal setup is required' "$ROOT/linux-launch.log"
export FEANORFS_NO_LAUNCH=1
unset DISPLAY DBUS_SESSION_BUS_ADDRESS

unset BINDIR
unset FEANORFS_CLIENT_INSTALL_DIR || true
export FAKE_ID_U=1000
mkdir -p "$HOME/.local/bin"
printf '%s\n' '#!/bin/sh' 'exit 0' >"$HOME/.local/bin/feanorfs"
printf '%s\n' '#!/bin/sh' 'exit 0' >"$HOME/.local/bin/feanorfs-tray"
chmod 755 "$HOME/.local/bin/feanorfs" "$HOME/.local/bin/feanorfs-tray"
export FAKE_APT_MARKER="$ROOT/apt-installed"
export FAKE_LINUX_DEB="$ROOT/FeanorFS-linux-x86_64.deb"
printf 'fake-deb-payload' >"$FAKE_LINUX_DEB"
export FAKE_LINUX_DEB_CHECKSUM="$ROOT/FeanorFS-linux-x86_64.deb.sha256"
(
  cd "$ROOT"
  sha256sum "$(basename "$FAKE_LINUX_DEB")" >"$FAKE_LINUX_DEB_CHECKSUM"
)
export FAKE_DEB_CONTROL="$ROOT/deb-control"
mkdir -p "$FAKE_DEB_CONTROL"
printf 'Package: feanorfs\nArchitecture: amd64\n' >"$FAKE_DEB_CONTROL/control"

export FAKE_RELEASE_JSON='{"tag_name":"v9.9.9","assets":[{"name":"FeanorFS-linux-x86_64.deb"}]}'
if sh scripts/install.sh >"$ROOT/incomplete-deb.log" 2>&1; then
  echo "Linux deb package without a checksum unexpectedly reached installation." >&2
  exit 1
fi
grep -Fq 'has a Linux deb package but no checksum' "$ROOT/incomplete-deb.log"
[[ ! -f "$FAKE_APT_MARKER" ]]

export FAKE_RELEASE_JSON='{"tag_name":"v9.9.9","assets":[{"name":"FeanorFS-linux-x86_64.deb"},{"name":"FeanorFS-linux-x86_64.deb.sha256"},{"name":"FeanorFS-linux-x86_64.tar.xz"},{"name":"FeanorFS-linux-x86_64.tar.xz.sha256"}]}'
sh scripts/install.sh >"$ROOT/deb.log"
[[ -f "$FAKE_APT_MARKER" ]]
[[ ! -f "$FEANORFS_TEST_CLI_MARKER" ]]
grep -Fq 'automatic desktop dependencies' "$ROOT/deb.log"
[[ ! -e "$HOME/.local/bin/feanorfs" && ! -e "$HOME/.local/bin/feanorfs-tray" ]]
[[ -x "$XDG_DATA_HOME/feanorfs/legacy-bin-backup/v9.9.9/feanorfs" ]]
[[ -x "$XDG_DATA_HOME/feanorfs/legacy-bin-backup/v9.9.9/feanorfs-tray" ]]

rm -f "$FAKE_APT_MARKER"
export FAKE_DNF_MARKER="$ROOT/dnf-installed"
export FAKE_LINUX_RPM="$ROOT/FeanorFS-linux-x86_64.rpm"
printf 'fake-rpm-payload' >"$FAKE_LINUX_RPM"
export FAKE_LINUX_RPM_CHECKSUM="$ROOT/FeanorFS-linux-x86_64.rpm.sha256"
(
  cd "$ROOT"
  sha256sum "$(basename "$FAKE_LINUX_RPM")" >"$FAKE_LINUX_RPM_CHECKSUM"
)

export FAKE_RELEASE_JSON='{"tag_name":"v9.9.9","assets":[{"name":"FeanorFS-linux-x86_64.rpm"}]}'
if sh scripts/install.sh >"$ROOT/incomplete-rpm.log" 2>&1; then
  echo "Linux rpm package without a checksum unexpectedly reached installation." >&2
  exit 1
fi
grep -Fq 'has a Linux rpm package but no checksum' "$ROOT/incomplete-rpm.log"
[[ ! -f "$FAKE_DNF_MARKER" ]]

export FAKE_RELEASE_JSON='{"tag_name":"v9.9.9","assets":[{"name":"FeanorFS-linux-x86_64.rpm"},{"name":"FeanorFS-linux-x86_64.rpm.sha256"},{"name":"FeanorFS-linux-x86_64.tar.xz"},{"name":"FeanorFS-linux-x86_64.tar.xz.sha256"}]}'
sh scripts/install.sh >"$ROOT/rpm.log"
[[ -f "$FAKE_DNF_MARKER" ]]
[[ ! -f "$FAKE_APT_MARKER" ]]
[[ ! -f "$FEANORFS_TEST_CLI_MARKER" ]]
grep -Fq 'automatic desktop dependencies' "$ROOT/rpm.log"

rm -f "$FAKE_DNF_MARKER"
export FAKE_PACMAN_MARKER="$ROOT/pacman-installed"
export FAKE_LINUX_ARCH_PACKAGE="$ROOT/FeanorFS-linux-x86_64.pkg.tar.zst"
printf 'fake-arch-payload' >"$FAKE_LINUX_ARCH_PACKAGE"
export FAKE_LINUX_ARCH_CHECKSUM="$ROOT/FeanorFS-linux-x86_64.pkg.tar.zst.sha256"
(
  cd "$ROOT"
  sha256sum "$(basename "$FAKE_LINUX_ARCH_PACKAGE")" >"$FAKE_LINUX_ARCH_CHECKSUM"
)

export FAKE_RELEASE_JSON='{"tag_name":"v9.9.9","assets":[{"name":"FeanorFS-linux-x86_64.pkg.tar.zst"}]}'
if sh scripts/install.sh >"$ROOT/incomplete-arch.log" 2>&1; then
  echo "Linux Arch package without a checksum unexpectedly reached installation." >&2
  exit 1
fi
grep -Fq 'has a Linux arch package but no checksum' "$ROOT/incomplete-arch.log"
[[ ! -f "$FAKE_PACMAN_MARKER" ]]

export FAKE_RELEASE_JSON='{"tag_name":"v9.9.9","assets":[{"name":"FeanorFS-linux-x86_64.pkg.tar.zst"},{"name":"FeanorFS-linux-x86_64.pkg.tar.zst.sha256"},{"name":"FeanorFS-linux-x86_64.tar.xz"},{"name":"FeanorFS-linux-x86_64.tar.xz.sha256"}]}'
sh scripts/install.sh >"$ROOT/arch.log"
[[ -f "$FAKE_PACMAN_MARKER" ]]
[[ ! -f "$FAKE_DNF_MARKER" ]]
[[ ! -f "$FAKE_APT_MARKER" ]]
[[ ! -f "$FEANORFS_TEST_CLI_MARKER" ]]
grep -Fq 'automatic desktop dependencies' "$ROOT/arch.log"

echo "Installer routing passed: no CLI-only downgrade, fail-closed macOS/Linux trust, headless opt-out, verified Linux tray launch, migration, and deb/rpm/Arch routing."
