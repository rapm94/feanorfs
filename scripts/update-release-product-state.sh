#!/bin/sh
# Keep release-plz's package selection aware of product files outside common/.

set -eu

mode="${1:---print}"
state_file="common/release-product-state.txt"

tracked_files() {
    if command -v jj >/dev/null 2>&1 && jj root >/dev/null 2>&1; then
        jj file list
    else
        git ls-files
    fi
}

hash_stream() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum | awk '{print $1}'
    else
        shasum -a 256 | awk '{print $1}'
    fi
}

hash_file() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1" | awk '{print $1}'
    else
        shasum -a 256 "$1" | awk '{print $1}'
    fi
}

product_hashes() {
    tracked_files | LC_ALL=C sort | while IFS= read -r path; do
        case "$path" in
            client/*|server/*|agent-core/*|tray/*|scripts/*|.github/*|Dockerfile.relay*) ;;
            *) continue ;;
        esac
        [ "$path" != "$state_file" ] || continue
        [ -f "$path" ] || continue
        printf '%s  %s\n' "$(hash_file "$path")" "$path"
    done
}

digest="$(product_hashes | hash_stream)"

case "$mode" in
    --print)
        printf '%s\n' "$digest"
        ;;
    --write)
        printf '%s\n' "$digest" > "$state_file"
        ;;
    --check)
        [ -f "$state_file" ] || {
            echo "error: missing $state_file" >&2
            exit 1
        }
        expected="$(sed -n '1p' "$state_file")"
        [ "$expected" = "$digest" ] || {
            echo "error: product release state is stale; run $0 --write" >&2
            exit 1
        }
        ;;
    *)
        echo "Usage: $0 [--print|--write|--check]" >&2
        exit 2
        ;;
esac
