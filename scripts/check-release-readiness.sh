#!/bin/sh
# Fail before tagging when product metadata or expected artifacts disagree.

set -eu

version="$(sed -n 's/^version = "\([^"]*\)"/\1/p' Cargo.toml | head -n 1)"
[ -n "$version" ] || { echo "error: missing workspace version" >&2; exit 1; }
tag="v$version"

cargo_versions="$(cargo metadata --no-deps --format-version 1 | jq -r '.packages[].version' | LC_ALL=C sort -u)"
[ "$cargo_versions" = "$version" ] || {
    echo "error: Cargo workspace versions do not all equal $version" >&2
    printf '%s\n' "$cargo_versions" >&2
    exit 1
}

npm --prefix bindings/ts run verify-metadata >/dev/null
node_version="$(node -p "require('./bindings/ts/package.json').version")"
[ "$node_version" = "$version" ] || {
    echo "error: Node facade version $node_version does not equal Cargo $version" >&2
    exit 1
}

grep -F "## [$version](https://github.com/rapm94/feanorfs/compare/" CHANGELOG.md >/dev/null || {
    echo "error: CHANGELOG.md has no canonical $version release section" >&2
    exit 1
}

head_sha="$(git rev-parse HEAD)"
if git rev-parse -q --verify "refs/tags/$tag" >/dev/null; then
    tag_sha="$(git rev-list -n 1 "$tag")"
    [ "$tag_sha" = "$head_sha" ] || {
        echo "error: existing $tag points to $tag_sha instead of $head_sha" >&2
        exit 1
    }
fi
if [ -n "${EXPECTED_SHA:-}" ] && [ "$EXPECTED_SHA" != "$head_sha" ]; then
    echo "error: release candidate $head_sha differs from CI-tested $EXPECTED_SHA" >&2
    exit 1
fi

grep -F 'https://raw.githubusercontent.com/rapm94/feanorfs/main/scripts/install.sh' README.md >/dev/null
grep -F 'installers = []' dist-workspace.toml >/dev/null || {
    echo "error: cargo-dist must not publish misleading CLI-only installers" >&2
    exit 1
}
sh -n scripts/install.sh
scripts/update-release-product-state.sh --check

for asset in \
    FeanorFS-macOS.dmg \
    FeanorFS-macOS.pkg \
    "FeanorFS-linux-\$ASSET_ARCH.deb" \
    "FeanorFS-linux-\$ASSET_ARCH.rpm" \
    "FeanorFS-linux-\$ASSET_ARCH.pkg.tar.zst" \
    "FeanorFS-linux-\$ASSET_ARCH.tar.xz" \
    FeanorFS-windows-x86_64-setup.exe; do
    if ! grep -F "$asset" .github/workflows/tray-release.yml .github/workflows/desktop-release.yml >/dev/null; then
        echo "error: release workflows do not name expected artifact $asset" >&2
        exit 1
    fi
done

printf 'Release dry run passed for %s at %s.\n' "$tag" "$head_sha"
