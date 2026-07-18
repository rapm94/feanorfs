#!/bin/sh
# FeanorFS install script — installs the current product from GitHub Releases.
#
# Current macOS and Linux releases install both the CLI and tray from one
# verified platform bundle. Older or unsupported releases use cargo-dist's
# CLI-only installer and say so explicitly.
#
# Usage:
#   curl -fsSL https://raw.githubusercontent.com/rapm94/feanorfs/main/scripts/install.sh | sh

set -eu

REPO="${FEANORFS_REPOSITORY:-rapm94/feanorfs}"
RELEASE_API="${FEANORFS_RELEASE_API:-https://api.github.com/repos/${REPO}/releases/latest}"

err() { echo "error: $*" >&2; exit 1; }
fetch() { curl --proto '=https' --tlsv1.2 -fLsS "$@"; }

launch_desktop_tray() {
    platform="$1"
    tray="$2"
    if [ "${FEANORFS_NO_LAUNCH:-}" = 1 ] || [ "$(id -u)" -eq 0 ]; then
        echo "Open FeanorFS from your application menu to start mirroring a folder."
        echo "Headless setup: feanorfs start /path/to/project"
        return
    fi

    if [ "$platform" = macos ]; then
        if [ -d "$tray" ] && /usr/bin/open -g "$tray" --args --first-run >/dev/null 2>&1; then
            echo "FeanorFS is now in your menu bar."
            echo "Choose Start Mirroring a Folder… to begin—no Terminal setup is required."
            return
        fi
    elif { [ -n "${DISPLAY:-}" ] || [ -n "${WAYLAND_DISPLAY:-}" ]; } && \
        [ -n "${DBUS_SESSION_BUS_ADDRESS:-}" ] && [ -x "$tray" ]; then
        if command -v pgrep >/dev/null 2>&1 && \
            pgrep -u "$(id -u)" -x feanorfs-tray >/dev/null 2>&1; then
            echo "FeanorFS is already open in your system tray."
            echo "Choose Start Mirroring a Folder… to begin—no Terminal setup is required."
            return
        fi
        nohup "$tray" --first-run </dev/null >/dev/null 2>&1 &
        tray_pid=$!
        sleep 1
        if kill -0 "$tray_pid" 2>/dev/null; then
            echo "FeanorFS is now in your system tray."
            echo "Choose Start Mirroring a Folder… to begin—no Terminal setup is required."
            return
        fi
        wait "$tray_pid" 2>/dev/null || true
    fi

    echo "FeanorFS was installed, but the tray could not open in this session."
    echo "Open FeanorFS from your application menu, or run: feanorfs start /path/to/project"
}

echo "Fetching latest release version..."
RELEASE_JSON="$(fetch "$RELEASE_API")"
VERSION="$(printf '%s\n' "$RELEASE_JSON" | sed -n 's/.*"tag_name": *"\([^"]*\)".*/\1/p' | head -n 1)"
[ -n "$VERSION" ] || err "could not determine latest version"

BASE_URL="${FEANORFS_BASE_URL:-https://github.com/${REPO}/releases/download/${VERSION}}"

has_release_asset() {
    printf '%s\n' "$RELEASE_JSON" | grep -Eq '"name"[[:space:]]*:[[:space:]]*"'"$1"'"'
}

install_macos_package() {
    has_release_asset "FeanorFS-macOS.pkg.sha256" || \
        err "release ${VERSION} has a macOS package but no checksum"

    package="FeanorFS-macOS.pkg"
    temp_dir="$(mktemp -d "${TMPDIR:-/tmp}/feanorfs-install.XXXXXX")"
    trap 'rm -rf "$temp_dir"' EXIT HUP INT TERM

    echo "Installing FeanorFS ${VERSION} for macOS (CLI + menu-bar app)..."
    fetch "$BASE_URL/$package" -o "$temp_dir/$package"
    fetch "$BASE_URL/$package.sha256" -o "$temp_dir/$package.sha256"
    (
        cd "$temp_dir"
        shasum -a 256 -c "$package.sha256"
    )
    /usr/sbin/pkgutil --check-signature "$temp_dir/$package" >/dev/null
    /usr/sbin/spctl --assess --type install --verbose=2 "$temp_dir/$package"

    if [ "$(id -u)" -eq 0 ]; then
        FEANORFS_NO_LAUNCH=1 /usr/sbin/installer -pkg "$temp_dir/$package" -target /
    else
        command -v sudo >/dev/null 2>&1 || \
            err "administrator access is required to install FeanorFS.app and /usr/local/bin/feanorfs"
        [ -t 1 ] || \
            err "run this installer from an interactive terminal so macOS can request administrator access"
        sudo -v
        sudo /usr/bin/env FEANORFS_NO_LAUNCH=1 \
            /usr/sbin/installer -pkg "$temp_dir/$package" -target /
    fi

    hash -r 2>/dev/null || true
    resolved="$(command -v feanorfs 2>/dev/null || true)"
    if [ -n "$resolved" ] && [ "$resolved" != "/usr/local/bin/feanorfs" ]; then
        echo "Warning: your shell currently resolves feanorfs to $resolved." >&2
        echo "Remove that older installation or put /usr/local/bin earlier on PATH." >&2
    fi

    echo ""
    echo "Installed /usr/local/bin/feanorfs and /Applications/FeanorFS.app."
}

linux_asset_arch() {
    case "$(uname -m)" in
        x86_64|amd64) printf '%s\n' x86_64 ;;
        aarch64|arm64) printf '%s\n' aarch64 ;;
        *) return 1 ;;
    esac
}

linux_package_arch() {
    case "$1:$2" in
        deb:x86_64) printf '%s\n' amd64 ;;
        deb:aarch64) printf '%s\n' arm64 ;;
        rpm:x86_64) printf '%s\n' x86_64 ;;
        rpm:aarch64) printf '%s\n' aarch64 ;;
        arch:x86_64) printf '%s\n' x86_64 ;;
        arch:aarch64) printf '%s\n' aarch64 ;;
        *) return 1 ;;
    esac
}

run_as_root() {
    if [ "$(id -u)" -eq 0 ]; then
        "$@"
        return
    fi
    command -v sudo >/dev/null 2>&1 || \
        err "administrator access is required to install the Linux desktop package"
    [ -t 1 ] || \
        err "run this installer from an interactive terminal so Linux can request administrator access"
    sudo "$@"
}

install_linux_native_package() {
    arch="$1"
    format="$2"
    package_arch="$(linux_package_arch "$format" "$arch")"
    if [ "$format" = arch ]; then
        extension=pkg.tar.zst
    else
        extension="$format"
    fi
    asset="FeanorFS-linux-${arch}.${extension}"
    has_release_asset "$asset.sha256" || \
        err "release ${VERSION} has a Linux ${format} package but no checksum"
    command -v sha256sum >/dev/null 2>&1 || err "sha256sum is required to verify FeanorFS"

    temp_dir="$(mktemp -d "${TMPDIR:-/tmp}/feanorfs-install.XXXXXX")"
    trap 'rm -rf "$temp_dir"' EXIT HUP INT TERM
    echo "Installing FeanorFS ${VERSION} for Linux ${arch} with automatic desktop dependencies..."
    fetch "$BASE_URL/$asset" -o "$temp_dir/$asset"
    fetch "$BASE_URL/$asset.sha256" -o "$temp_dir/$asset.sha256"
    (
        cd "$temp_dir"
        sha256sum -c "$asset.sha256"
    )

    if [ "$format" = deb ]; then
        [ "$(dpkg-deb -f "$temp_dir/$asset" Package)" = feanorfs ] || \
            err "Linux package has an unexpected package name"
        [ "$(dpkg-deb -f "$temp_dir/$asset" Architecture)" = "$package_arch" ] || \
            err "Linux package architecture does not match this computer"
        if dpkg-deb --ctrl-tarfile "$temp_dir/$asset" | tar -tf - | \
            grep -Eq '^\./(preinst|postinst|prerm|postrm)$'; then
            err "Linux package unexpectedly contains maintainer scripts"
        fi
        run_as_root apt-get install -y --no-install-recommends "$temp_dir/$asset"
    elif [ "$format" = rpm ]; then
        [ "$(rpm -qp --queryformat '%{NAME}' "$temp_dir/$asset")" = feanorfs ] || \
            err "Linux package has an unexpected package name"
        [ "$(rpm -qp --queryformat '%{ARCH}' "$temp_dir/$asset")" = "$package_arch" ] || \
            err "Linux package architecture does not match this computer"
        [ -z "$(rpm -qp --scripts "$temp_dir/$asset")" ] || \
            err "Linux package unexpectedly contains install scripts"
        if command -v dnf >/dev/null 2>&1; then
            run_as_root dnf install -y --setopt=install_weak_deps=False "$temp_dir/$asset"
        else
            run_as_root yum install -y "$temp_dir/$asset"
        fi
    else
        metadata="$(bsdtar -xOf "$temp_dir/$asset" .PKGINFO)"
        printf '%s\n' "$metadata" | grep -Fx 'pkgname = feanorfs' >/dev/null || \
            err "Linux package has an unexpected package name"
        printf '%s\n' "$metadata" | grep -Fx "arch = $package_arch" >/dev/null || \
            err "Linux package architecture does not match this computer"
        if bsdtar -tf "$temp_dir/$asset" | grep -Fxq .INSTALL; then
            err "Linux package unexpectedly contains install scripts"
        fi
        run_as_root pacman -U --noconfirm "$temp_dir/$asset"
    fi

    hash -r 2>/dev/null || true
    echo "Installed feanorfs, the system tray, and its desktop launcher."
}

install_linux_bundle() {
    arch="$1"
    asset="FeanorFS-linux-${arch}.tar.xz"
    has_release_asset "$asset.sha256" || \
        err "release ${VERSION} has a Linux desktop bundle but no checksum"
    command -v sha256sum >/dev/null 2>&1 || err "sha256sum is required to verify FeanorFS"
    command -v tar >/dev/null 2>&1 || err "tar with xz support is required to install FeanorFS"
    command -v install >/dev/null 2>&1 || err "the POSIX install utility is required to install FeanorFS"

    temp_dir="$(mktemp -d "${TMPDIR:-/tmp}/feanorfs-install.XXXXXX")"
    trap 'rm -rf "$temp_dir"' EXIT HUP INT TERM
    echo "Installing FeanorFS ${VERSION} for Linux ${arch} (CLI + system tray)..."
    fetch "$BASE_URL/$asset" -o "$temp_dir/$asset"
    fetch "$BASE_URL/$asset.sha256" -o "$temp_dir/$asset.sha256"
    (
        cd "$temp_dir"
        sha256sum -c "$asset.sha256"
    )

    contents="$(tar -tJf "$temp_dir/$asset" | LC_ALL=C sort)"
    expected="$(printf '%s\n' \
        com.feanorfs.tray.desktop com.feanorfs.tray.svg feanorfs feanorfs-tray | LC_ALL=C sort)"
    [ "$contents" = "$expected" ] || err "Linux desktop bundle contains unexpected files"
    tar -xJf "$temp_dir/$asset" -C "$temp_dir"
    missing="$(ldd "$temp_dir/feanorfs-tray" 2>/dev/null | sed -n '/not found/p')"
    if [ -n "$missing" ]; then
        echo "$missing" >&2
        err "desktop libraries are missing; install GTK 3, Ayatana AppIndicator 3, and libxdo, then retry"
    fi

    install_dir="${BINDIR:-${FEANORFS_CLIENT_INSTALL_DIR:-$HOME/.local/bin}}"
    mkdir -p "$install_dir"
    install -m 755 "$temp_dir/feanorfs" "$install_dir/feanorfs"
    install -m 755 "$temp_dir/feanorfs-tray" "$install_dir/feanorfs-tray"
    data_home="${XDG_DATA_HOME:-$HOME/.local/share}"
    mkdir -p "$data_home/applications" "$data_home/icons/hicolor/scalable/apps"
    sed "s|^Exec=.*|Exec=$install_dir/feanorfs-tray|" \
        "$temp_dir/com.feanorfs.tray.desktop" > "$data_home/applications/com.feanorfs.tray.desktop"
    chmod 644 "$data_home/applications/com.feanorfs.tray.desktop"
    install -m 644 "$temp_dir/com.feanorfs.tray.svg" \
        "$data_home/icons/hicolor/scalable/apps/com.feanorfs.tray.svg"
    hash -r 2>/dev/null || true
    echo "Installed feanorfs and feanorfs-tray to $install_dir with an application-menu launcher."
}

if [ "$(uname -s)" = "Darwin" ] && has_release_asset "FeanorFS-macOS.pkg"; then
    install_macos_package
    launch_desktop_tray macos /Applications/FeanorFS.app
    exit 0
fi

if [ "$(uname -s)" = "Linux" ]; then
    arch="$(linux_asset_arch || true)"
    if [ -n "$arch" ] && [ -z "${BINDIR:-}" ] && [ -z "${FEANORFS_CLIENT_INSTALL_DIR:-}" ] && \
        command -v apt-get >/dev/null 2>&1 && command -v dpkg-deb >/dev/null 2>&1 && \
       has_release_asset "FeanorFS-linux-${arch}.deb"; then
        install_linux_native_package "$arch" deb
        launch_desktop_tray linux /usr/bin/feanorfs-tray
        exit 0
    fi
    if [ -n "$arch" ] && [ -z "${BINDIR:-}" ] && [ -z "${FEANORFS_CLIENT_INSTALL_DIR:-}" ] && \
        command -v rpm >/dev/null 2>&1 && \
        { command -v dnf >/dev/null 2>&1 || command -v yum >/dev/null 2>&1; } && \
       has_release_asset "FeanorFS-linux-${arch}.rpm"; then
        install_linux_native_package "$arch" rpm
        launch_desktop_tray linux /usr/bin/feanorfs-tray
        exit 0
    fi
    if [ -n "$arch" ] && [ -z "${BINDIR:-}" ] && [ -z "${FEANORFS_CLIENT_INSTALL_DIR:-}" ] && \
        command -v pacman >/dev/null 2>&1 && command -v bsdtar >/dev/null 2>&1 && \
       has_release_asset "FeanorFS-linux-${arch}.pkg.tar.zst"; then
        install_linux_native_package "$arch" arch
        launch_desktop_tray linux /usr/bin/feanorfs-tray
        exit 0
    fi
    if [ -n "$arch" ] && has_release_asset "FeanorFS-linux-${arch}.tar.xz"; then
        install_linux_bundle "$arch"
        launch_desktop_tray linux "${BINDIR:-${FEANORFS_CLIENT_INSTALL_DIR:-$HOME/.local/bin}}/feanorfs-tray"
        exit 0
    fi
fi

if [ -n "${BINDIR:-}" ]; then
    export FEANORFS_CLIENT_INSTALL_DIR="$BINDIR"
fi

echo "Installing feanorfs ${VERSION}..."
fetch "${BASE_URL}/feanorfs-client-installer.sh" | sh

echo ""
echo "Done. feanorfs ${VERSION} installed."
if [ "$(uname -s)" = "Darwin" ]; then
    echo "This release does not include the signed menu-bar package; the CLI is installed."
elif [ "$(uname -s)" = "Linux" ]; then
    echo "This release does not include a tray for this Linux architecture; the CLI is installed."
fi
echo "First computer:  feanorfs start /path/to/project"
echo "Another computer: feanorfs start <pair-code-or-invite> /path/to/project"
