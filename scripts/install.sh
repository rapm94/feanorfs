#!/bin/sh
# FeanorFS install script — installs the `feanorfs` binary from GitHub Releases.
#
# One install covers sync client + blob hub (`feanorfs serve`).
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
fi

echo "Installing feanorfs ${VERSION}..."
curl -fsSL "${BASE_URL}/feanorfs-client-installer.sh" | sh

echo ""
echo "Done. feanorfs ${VERSION} installed."
echo "Run a hub with: feanorfs serve --token <TOKEN>"
