#!/bin/sh
# FeanorFS install script — downloads pre-built binaries from GitHub Releases.
# Usage: curl -fsSL https://github.com/rapm94/feanorfs/releases/latest/download/install.sh | sh

set -eu

REPO="rapm94/feanorfs"
BINDIR="${BINDIR:-}"

err() { echo "error: $*" >&2; exit 1; }

# --- detect platform ---------------------------------------------------------
OS="$(uname -s)"
ARCH="$(uname -m)"

case "$OS" in
    Linux)  TARGET_OS="unknown-linux-gnu" ;;
    Darwin) TARGET_OS="apple-darwin" ;;
    *)      err "unsupported OS: $OS" ;;
esac

case "$ARCH" in
    x86_64|amd64)   TARGET_ARCH="x86_64" ;;
    aarch64|arm64)  TARGET_ARCH="aarch64" ;;
    *)              err "unsupported architecture: $ARCH" ;;
esac

TARGET="${TARGET_ARCH}-${TARGET_OS}"

# --- pick install directory --------------------------------------------------
if [ -z "$BINDIR" ]; then
    if [ -w /usr/local/bin ]; then
        BINDIR="/usr/local/bin"
    else
        BINDIR="${HOME}/.local/bin"
        mkdir -p "$BINDIR"
    fi
fi

# --- download ----------------------------------------------------------------
echo "Fetching latest release version..."
VERSION="$(curl -fsSL "https://api.github.com/repos/${REPO}/releases/latest" | sed -n 's/.*"tag_name": *"v\([^"]*\)".*/\1/p')"
[ -n "$VERSION" ] || err "could not determine latest version"

BASE_URL="https://github.com/${REPO}/releases/download/v${VERSION}"

install_bin() {
    name="$1"
    file="${name}-v${VERSION}-${TARGET}"
    url="${BASE_URL}/${file}"

    echo "Downloading ${name} v${VERSION} (${TARGET})..."
    tmp="$(mktemp)"
    curl -fsSL "$url" -o "$tmp" || err "download failed: ${url}"
    chmod +x "$tmp"
    mv "$tmp" "${BINDIR}/${name}"
    echo "Installed ${name} -> ${BINDIR}/${name}"
}

install_bin feanorfs
install_bin feanorfs-server

echo ""
echo "Done. feanorfs v${VERSION} installed to ${BINDIR}."
case ":${PATH}:" in
    *":${BINDIR}:"*) ;;
    *) echo "warning: ${BINDIR} is not in your PATH. Add it with: export PATH=\"${BINDIR}:\$PATH\"" ;;
esac
