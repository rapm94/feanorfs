#!/bin/sh
# FeanorFS install script — installs both CLI and server from GitHub Releases.
#
# Uses cargo-dist-generated per-app installers (feanorfs-client-installer.sh and
# feanorfs-server-installer.sh). Pass BINDIR to either script via env vars:
#   FEANORFS_CLIENT_INSTALL_DIR / FEANORFS_SERVER_INSTALL_DIR
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

echo "Installing feanorfs (client) ${VERSION}..."
curl -fsSL "${BASE_URL}/feanorfs-client-installer.sh" | sh

echo "Installing feanorfs-server ${VERSION}..."
curl -fsSL "${BASE_URL}/feanorfs-server-installer.sh" | sh

echo ""
echo "Done. feanorfs ${VERSION} and feanorfs-server ${VERSION} installed."
