#!/bin/sh
set -eu

REPOSITORY="${FEANORFS_REPOSITORY:-rapm94/feanorfs}"

[ "$(uname -s)" = "Darwin" ] || { echo "This installer requires macOS." >&2; exit 1; }

PACKAGE="FeanorFS-macOS.pkg"
BASE_URL="${FEANORFS_BASE_URL:-https://github.com/$REPOSITORY/releases/latest/download}"
TEMP_DIR="$(mktemp -d "${TMPDIR:-/tmp}/feanorfs-install.XXXXXX")"
trap 'rm -rf "$TEMP_DIR"' EXIT HUP INT TERM

curl --proto '=https' --tlsv1.2 -fLsS "$BASE_URL/$PACKAGE" -o "$TEMP_DIR/$PACKAGE"
curl --proto '=https' --tlsv1.2 -fLsS "$BASE_URL/$PACKAGE.sha256" -o "$TEMP_DIR/$PACKAGE.sha256"

(
  cd "$TEMP_DIR"
  shasum -a 256 -c "$PACKAGE.sha256"
)
/usr/sbin/pkgutil --check-signature "$TEMP_DIR/$PACKAGE" >/dev/null
/usr/sbin/spctl --assess --type install --verbose=2 "$TEMP_DIR/$PACKAGE"

if [ "$(id -u)" -eq 0 ]; then
  /usr/sbin/installer -pkg "$TEMP_DIR/$PACKAGE" -target /
else
  command -v sudo >/dev/null 2>&1 || {
    echo "Administrator access is required to install FeanorFS.app and /usr/local/bin/feanorfs." >&2
    exit 1
  }
  [ -t 1 ] || {
    echo "Run this installer from an interactive terminal so macOS can request administrator access." >&2
    exit 1
  }
  sudo -v
  sudo /usr/sbin/installer -pkg "$TEMP_DIR/$PACKAGE" -target /
fi

hash -r 2>/dev/null || true
resolved="$(command -v feanorfs 2>/dev/null || true)"
if [ -n "$resolved" ] && [ "$resolved" != "/usr/local/bin/feanorfs" ]; then
  echo "Warning: your shell currently resolves feanorfs to $resolved." >&2
  echo "Remove that older installation or put /usr/local/bin earlier on PATH." >&2
fi

echo "Installed /usr/local/bin/feanorfs and /Applications/FeanorFS.app."
if [ "${FEANORFS_NO_LAUNCH:-}" = 1 ] || [ "$(id -u)" -eq 0 ]; then
  echo "Open FeanorFS from Applications to start mirroring a folder."
  echo "Headless setup: feanorfs start [folder]"
elif /usr/bin/open -g /Applications/FeanorFS.app --args --first-run >/dev/null 2>&1; then
  echo "FeanorFS is now in your menu bar."
  echo "Choose Start Mirroring a Folder… to begin—no Terminal setup is required."
else
  echo "FeanorFS was installed, but the menu-bar app could not open in this session."
  echo "Open FeanorFS from Applications, or run: feanorfs start [folder]"
fi
