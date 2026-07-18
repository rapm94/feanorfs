#!/bin/sh
# Build and verify native Linux desktop packages from native release binaries.

set -eu

output_dir="${1:-.}"
feanorfs_bin="${FEANORFS_BIN:-target/release/feanorfs}"
tray_bin="${FEANORFS_TRAY_BIN:-target/release/feanorfs-tray}"

command -v nfpm >/dev/null 2>&1 || {
    echo "error: nfpm is required" >&2
    exit 1
}
command -v dpkg-deb >/dev/null 2>&1 || {
    echo "error: dpkg-deb is required" >&2
    exit 1
}
command -v rpm >/dev/null 2>&1 || {
    echo "error: rpm is required" >&2
    exit 1
}
command -v tar >/dev/null 2>&1 || {
    echo "error: tar is required" >&2
    exit 1
}
[ -x "$feanorfs_bin" ] || { echo "error: missing $feanorfs_bin" >&2; exit 1; }
[ -x "$tray_bin" ] || { echo "error: missing $tray_bin" >&2; exit 1; }

case "$(uname -m)" in
    x86_64|amd64)
        asset_arch=x86_64
        nfpm_arch=amd64
        rpm_arch=x86_64
        ;;
    aarch64|arm64)
        asset_arch=aarch64
        nfpm_arch=arm64
        rpm_arch=aarch64
        ;;
    *)
        echo "error: unsupported Linux package architecture: $(uname -m)" >&2
        exit 1
        ;;
esac

nfpm_version="$(cargo metadata --no-deps --format-version 1 | \
    jq -r '.packages[] | select(.name == "feanorfs-client") | .version')"
[ -n "$nfpm_version" ] || { echo "error: could not determine FeanorFS version" >&2; exit 1; }

mkdir -p "$output_dir"
deb="$output_dir/FeanorFS-linux-$asset_arch.deb"
rpm_package="$output_dir/FeanorFS-linux-$asset_arch.rpm"
arch_package="$output_dir/FeanorFS-linux-$asset_arch.pkg.tar.zst"

NFPM_ARCH="$nfpm_arch"
NFPM_VERSION="$nfpm_version"
NFPM_FEANORFS_BIN="$(cd "$(dirname "$feanorfs_bin")" && pwd)/$(basename "$feanorfs_bin")"
NFPM_TRAY_BIN="$(cd "$(dirname "$tray_bin")" && pwd)/$(basename "$tray_bin")"
SOURCE_DATE_EPOCH="${SOURCE_DATE_EPOCH:-$(git show -s --format=%ct HEAD)}"
export NFPM_ARCH NFPM_VERSION NFPM_FEANORFS_BIN NFPM_TRAY_BIN SOURCE_DATE_EPOCH

nfpm package --config scripts/linux-package.nfpm.yaml --packager deb --target "$deb"
nfpm package --config scripts/linux-package.nfpm.yaml --packager rpm --target "$rpm_package"
nfpm package --config scripts/linux-package.nfpm.yaml --packager archlinux --target "$arch_package"

[ "$(dpkg-deb -f "$deb" Package)" = feanorfs ]
[ "$(dpkg-deb -f "$deb" Architecture)" = "$nfpm_arch" ]
dpkg-deb -f "$deb" Depends | grep -F 'libayatana-appindicator3-1' >/dev/null
dpkg-deb -f "$deb" Depends | grep -F 'libxdo3' >/dev/null
dpkg-deb -f "$deb" Depends | grep -F 'xdg-desktop-portal' >/dev/null
dpkg-deb -f "$deb" Depends | grep -F 'zenity' >/dev/null
deb_contents="$(dpkg-deb --fsys-tarfile "$deb" | tar -tf - | sed '/\/$/d' | LC_ALL=C sort)"
expected_contents="$(printf '%s\n' \
    ./usr/bin/feanorfs \
    ./usr/bin/feanorfs-tray \
    ./usr/share/applications/com.feanorfs.tray.desktop \
    ./usr/share/doc/feanorfs/LICENSE \
    ./usr/share/doc/feanorfs/README.md \
    ./usr/share/icons/hicolor/scalable/apps/com.feanorfs.tray.svg | LC_ALL=C sort)"
[ "$deb_contents" = "$expected_contents" ] || {
    echo "error: deb package contains unexpected files" >&2
    exit 1
}

[ "$(rpm -qp --queryformat '%{NAME}' "$rpm_package")" = feanorfs ]
[ "$(rpm -qp --queryformat '%{ARCH}' "$rpm_package")" = "$rpm_arch" ]
rpm -qpR "$rpm_package" | grep -Fx gtk3 >/dev/null
rpm -qpR "$rpm_package" | grep -Fx libayatana-appindicator-gtk3 >/dev/null
rpm -qpR "$rpm_package" | grep -Fx libxdo >/dev/null
rpm -qpR "$rpm_package" | grep -Fx xdg-desktop-portal >/dev/null
rpm -qpR "$rpm_package" | grep -Fx zenity >/dev/null
rpm_contents="$(rpm -qpl "$rpm_package" | LC_ALL=C sort)"
expected_rpm_contents="$(printf '%s\n' \
    /usr/bin/feanorfs \
    /usr/bin/feanorfs-tray \
    /usr/share/applications/com.feanorfs.tray.desktop \
    /usr/share/doc/feanorfs/LICENSE \
    /usr/share/doc/feanorfs/README.md \
    /usr/share/icons/hicolor/scalable/apps/com.feanorfs.tray.svg | LC_ALL=C sort)"
[ "$rpm_contents" = "$expected_rpm_contents" ] || {
    echo "error: rpm package contains unexpected files" >&2
    exit 1
}

arch_metadata="$(tar --zstd -xOf "$arch_package" .PKGINFO)"
printf '%s\n' "$arch_metadata" | grep -Fx 'pkgname = feanorfs' >/dev/null
printf '%s\n' "$arch_metadata" | grep -Fx "arch = $rpm_arch" >/dev/null
for dependency in gtk3 libayatana-appindicator xdotool xdg-desktop-portal zenity; do
    printf '%s\n' "$arch_metadata" | grep -Fx "depend = $dependency" >/dev/null
done
arch_contents="$(tar --zstd -tf "$arch_package" | \
    sed -e '/\/$/d' -e '/^\.BUILDINFO$/d' -e '/^\.MTREE$/d' -e '/^\.PKGINFO$/d' | \
    LC_ALL=C sort)"
expected_arch_contents="$(printf '%s\n' \
    usr/bin/feanorfs \
    usr/bin/feanorfs-tray \
    usr/share/applications/com.feanorfs.tray.desktop \
    usr/share/doc/feanorfs/LICENSE \
    usr/share/doc/feanorfs/README.md \
    usr/share/icons/hicolor/scalable/apps/com.feanorfs.tray.svg | LC_ALL=C sort)"
[ "$arch_contents" = "$expected_arch_contents" ] || {
    echo "error: Arch package contains unexpected files" >&2
    exit 1
}

for asset in "$deb" "$rpm_package" "$arch_package"; do
    sha256sum "$asset" > "$asset.sha256"
done

echo "Built verified Linux desktop packages for $asset_arch."
