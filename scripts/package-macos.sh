#!/bin/sh
set -eu

# Do not leak Finder metadata into the installer payload.
COPYFILE_DISABLE=1
export COPYFILE_DISABLE

usage() {
  echo "Usage:" >&2
  echo "  $0 assemble VERSION FEANORFS_BIN TRAY_BIN BUILD_DIR" >&2
  echo "  $0 build VERSION BUILD_DIR OUTPUT_PKG" >&2
  echo "  $0 dmg VERSION INPUT_PKG OUTPUT_DMG" >&2
  exit 2
}

validate_version() {
  version="$1"
  if ! printf '%s\n' "$version" | grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+$'; then
    echo "Invalid package version: $version" >&2
    exit 2
  fi
}

assemble() {
  [ "$#" -eq 4 ] || usage
  version="$1"
  feanorfs_bin="$2"
  tray_bin="$3"
  build_dir="$4"
  validate_version "$version"

  [ -x "$feanorfs_bin" ] || { echo "Missing executable: $feanorfs_bin" >&2; exit 1; }
  [ -x "$tray_bin" ] || { echo "Missing executable: $tray_bin" >&2; exit 1; }
  [ ! -e "$build_dir/payload" ] || { echo "Build directory is not empty: $build_dir" >&2; exit 1; }

  app="$build_dir/payload/Applications/FeanorFS.app"
  mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
  mkdir -p "$build_dir/payload/usr/local/bin"
  mkdir -p "$build_dir/scripts"
  install -m 755 "$feanorfs_bin" "$build_dir/payload/usr/local/bin/feanorfs"
  install -m 755 "$tray_bin" "$app/Contents/MacOS/feanorfs-tray"

  info_plist="$app/Contents/Info.plist"
  cat > "$info_plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleDisplayName</key>
  <string>FeanorFS</string>
  <key>CFBundleExecutable</key>
  <string>feanorfs-tray</string>
  <key>CFBundleIdentifier</key>
  <string>com.feanorfs.tray</string>
  <key>CFBundleInfoDictionaryVersion</key>
  <string>6.0</string>
  <key>CFBundleName</key>
  <string>FeanorFS</string>
  <key>CFBundlePackageType</key>
  <string>APPL</string>
  <key>CFBundleShortVersionString</key>
  <string>$version</string>
  <key>CFBundleVersion</key>
  <string>$version</string>
  <key>LSUIElement</key>
  <true/>
</dict>
</plist>
EOF
  plutil -lint "$info_plist"

  cat > "$build_dir/scripts/postinstall" <<'EOF'
#!/bin/sh
set -eu

# Terminal/headless installers set this explicitly and perform any interactive
# handoff themselves. Finder's Installer.app does not, so a normal desktop
# install opens the same tray-first onboarding used on every other platform.
if [ "${FEANORFS_NO_LAUNCH:-}" = 1 ]; then
  exit 0
fi

console_user="$(/usr/bin/stat -f '%Su' /dev/console 2>/dev/null || true)"
case "$console_user" in
  ''|root|loginwindow|_mbsetupuser) exit 0 ;;
esac
console_uid="$(/usr/bin/id -u "$console_user" 2>/dev/null || true)"
case "$console_uid" in
  ''|*[!0-9]*) exit 0 ;;
esac

if /bin/launchctl print "gui/$console_uid" >/dev/null 2>&1; then
  /bin/launchctl asuser "$console_uid" \
    /usr/bin/sudo -u "$console_user" \
    /usr/bin/open -g /Applications/FeanorFS.app --args --first-run \
    >/dev/null 2>&1 || true
fi
exit 0
EOF
  chmod 755 "$build_dir/scripts/postinstall"
}

build_package() {
  [ "$#" -eq 3 ] || usage
  version="$1"
  build_dir="$2"
  output_pkg="$3"
  validate_version "$version"

  payload="$build_dir/payload"
  app="$payload/Applications/FeanorFS.app"
  [ -x "$payload/usr/local/bin/feanorfs" ] || { echo "Assemble the package first" >&2; exit 1; }
  [ -x "$app/Contents/MacOS/feanorfs-tray" ] || { echo "Assemble the package first" >&2; exit 1; }
  [ ! -e "$output_pkg" ] || { echo "Package already exists: $output_pkg" >&2; exit 1; }
  mkdir -p "$(dirname "$output_pkg")"
  xattr -cr "$payload"

  component_plist="$build_dir/components.plist"
  pkgbuild --analyze --root "$payload" "$component_plist"
  app_path="$(/usr/libexec/PlistBuddy -c 'Print :0:RootRelativeBundlePath' "$component_plist")"
  if [ "$app_path" != "Applications/FeanorFS.app" ]; then
    echo "pkgbuild did not discover FeanorFS.app" >&2
    exit 1
  fi
  /usr/libexec/PlistBuddy -c "Set :0:BundleIsRelocatable false" "$component_plist"
  /usr/libexec/PlistBuddy -c "Set :0:BundleOverwriteAction upgrade" "$component_plist"

  pkgbuild \
    --root "$payload" \
    --scripts "$build_dir/scripts" \
    --component-plist "$component_plist" \
    --identifier com.feanorfs.install \
    --version "$version" \
    --install-location / \
    "$output_pkg"
}

build_dmg() {
  [ "$#" -eq 3 ] || usage
  version="$1"
  input_pkg="$2"
  output_dmg="$3"
  validate_version "$version"

  [ -f "$input_pkg" ] || { echo "Missing package: $input_pkg" >&2; exit 1; }
  [ ! -e "$output_dmg" ] || { echo "Disk image already exists: $output_dmg" >&2; exit 1; }
  command -v hdiutil >/dev/null 2>&1 || { echo "hdiutil is required" >&2; exit 1; }

  stage="$(mktemp -d "${TMPDIR:-/tmp}/feanorfs-dmg.XXXXXX")"
  trap 'rm -rf "$stage"' EXIT HUP INT TERM
  install -m 644 "$input_pkg" "$stage/FeanorFS-macOS.pkg"
  mkdir -p "$(dirname "$output_dmg")"
  hdiutil create \
    -quiet \
    -fs HFS+ \
    -format UDZO \
    -srcfolder "$stage" \
    -volname "FeanorFS $version" \
    "$output_dmg"
}

[ "$#" -ge 1 ] || usage
command="$1"
shift
case "$command" in
  assemble) assemble "$@" ;;
  build) build_package "$@" ;;
  dmg) build_dmg "$@" ;;
  *) usage ;;
esac
