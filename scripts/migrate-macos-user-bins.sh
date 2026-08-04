#!/bin/sh
# Preserve obsolete per-user FeanorFS binaries that can shadow the native
# macOS package in shells whose PATH prefers ~/.local/bin.

set -eu

[ "$#" -eq 1 ] || {
  echo "Usage: $0 VERSION" >&2
  exit 2
}

safe_version="$(printf '%s' "$1" | tr -c 'A-Za-z0-9._-' '_')"
[ -n "$safe_version" ] || {
  echo "Cannot back up an older FeanorFS installation without a package version." >&2
  exit 2
}

legacy_dir="$HOME/.local/bin"
[ -d "$legacy_dir" ] || exit 0

backup_dir="${XDG_DATA_HOME:-$HOME/.local/share}/feanorfs/legacy-bin-backup/$safe_version"
for name in feanorfs feanorfs-tray; do
  legacy="$legacy_dir/$name"
  [ -e "$legacy" ] || [ -L "$legacy" ] || continue

  case "$name" in
    feanorfs) installed=/usr/local/bin/feanorfs ;;
    feanorfs-tray) installed=/Applications/FeanorFS.app/Contents/MacOS/feanorfs-tray ;;
  esac
  if [ -e "$installed" ] && [ "$legacy" -ef "$installed" ]; then
    continue
  fi

  umask 077
  mkdir -p "$backup_dir"
  destination="$backup_dir/$name"
  suffix=0
  while [ -e "$destination" ] || [ -L "$destination" ]; do
    suffix=$((suffix + 1))
    destination="$backup_dir/$name.$suffix"
  done
  mv "$legacy" "$destination"
  echo "Moved the older $legacy installation to $destination."
done
