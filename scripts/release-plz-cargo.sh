#!/usr/bin/env bash
# Keep release-plz git-only history independent of unrelated workspace crates.

set -euo pipefail

release_package="feanorfs-common"
real_cargo="${FEANORFS_REAL_CARGO:-}"
if [[ -z "$real_cargo" ]]; then
    real_cargo="$(rustup which cargo)"
fi

historical_workspace=false
if [[ "${1:-}" == "package" ]]; then
    for arg in "$@"; do
        if [[ "$arg" == "--workspace" ]]; then
            historical_workspace=true
            break
        fi
    done
fi

if [[ "$historical_workspace" != true ]]; then
    exec "$real_cargo" "$@"
fi

# release-plz 0.3.160 packages the complete historical workspace for every
# git-only crate. FeanorFS has exactly one git-only crate, and it has no local
# dependencies, so packaging unrelated unpublished members adds no information
# and makes an immutable tag depend on crates.io publication state.
for arg in "$@"; do
    case "$arg" in
        -p|--package|--package=*|-p?*|--exclude|--exclude=*)
            echo "error: unexpected package selector in release-plz cargo invocation: $arg" >&2
            exit 2
            ;;
    esac
done

metadata="$($real_cargo metadata --format-version 1 --no-deps --locked)"
if ! jq -e --arg name "$release_package" '
    [.packages[] | select(.name == $name)] as $matches |
    ($matches | length) == 1 and
    ($matches[0].dependencies | all(.[]; .path == null))
' <<<"$metadata" >/dev/null; then
    echo "error: $release_package must exist exactly once and have no path dependencies" >&2
    exit 1
fi

package_version="$(jq -er --arg name "$release_package" \
    '.packages[] | select(.name == $name) | .version' <<<"$metadata")"
target_dir="$(jq -er '.target_directory' <<<"$metadata")"

package_args=()
has_locked=false
has_no_verify=false
for arg in "$@"; do
    case "$arg" in
        --workspace)
            ;;
        --locked)
            has_locked=true
            package_args+=("$arg")
            ;;
        --no-verify)
            has_no_verify=true
            package_args+=("$arg")
            ;;
        *)
            package_args+=("$arg")
            ;;
    esac
done

package_args+=(--package "$release_package")
if [[ "$has_no_verify" != true ]]; then
    package_args+=(--no-verify)
fi
if [[ "$has_locked" != true ]]; then
    package_args+=(--locked)
fi

"$real_cargo" "${package_args[@]}"

archive="$target_dir/package/$release_package-$package_version.crate"
package_dir="$target_dir/package/$release_package-$package_version"
if [[ ! -f "$archive" ]]; then
    echo "error: cargo did not create expected historical archive $archive" >&2
    exit 1
fi
if [[ -e "$package_dir" ]]; then
    echo "error: historical package directory already exists: $package_dir" >&2
    exit 1
fi

tar -xzf "$archive" -C "$target_dir/package"
if [[ ! -f "$package_dir/Cargo.toml" ]]; then
    echo "error: historical package archive did not contain Cargo.toml" >&2
    exit 1
fi
