#!/bin/sh
# FeanorFS install script — installs the `feanorfs` binary from GitHub Releases.
#
# One install covers sync client + blob hub (`feanorfs serve`).
# Set FEANORFS_INSTALL_SERVER=1 to also install legacy server-only `feanorfs-server`.
#
# Usage:
#   curl -fsSL https://raw.githubusercontent.com/rapm94/feanorfs/main/scripts/install.sh | sh

set -eu

REPO="rapm94/feanorfs"

err() { echo "error: $*" >&2; exit 1; }

echo "Fetching latest release version..."
VERSION="$(
    curl -fsSL "https://api.github.com/repos/${REPO}/releases/latest" \
        | sed -n 's/.*"tag_name": *"\([^"]*\)".*/\1/p'
)"
[ -n "$VERSION" ] || err "could not determine latest version"

BASE_URL="https://github.com/${REPO}/releases/download/${VERSION}"

if [ -n "${BINDIR:-}" ]; then
    export FEANORFS_CLIENT_INSTALL_DIR="$BINDIR"
    export FEANORFS_SERVER_INSTALL_DIR="$BINDIR"
fi

echo "Installing feanorfs ${VERSION}..."
curl -fsSL "${BASE_URL}/feanorfs-client-installer.sh" | sh

if [ "${FEANORFS_INSTALL_SERVER:-0}" = "1" ]; then
    echo "Installing feanorfs-server ${VERSION} (legacy server-only binary)..."
    curl -fsSL "${BASE_URL}/feanorfs-server-installer.sh" | sh
fi

echo ""
echo "Done. feanorfs ${VERSION} installed."
echo "Run a hub with: feanorfs serve --token <TOKEN>"
