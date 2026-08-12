#!/usr/bin/env bash

set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
adapter="$script_dir/release-plz-cargo.sh"
test_root="$(mktemp -d)"
trap 'rm -rf "$test_root"' EXIT

fake_cargo="$test_root/fake-cargo"
test_log="$test_root/cargo.log"
test_target="$test_root/target"
fixture_root="$test_root/fixture"

cat >"$fake_cargo" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail

printf '%s\n' "$*" >>"$FEANORFS_TEST_LOG"
case "${1:-}" in
    metadata)
        jq -n \
            --arg target "$FEANORFS_TEST_TARGET" \
            --argjson path_dependency "${FEANORFS_TEST_PATH_DEPENDENCY:-false}" \
            '{
                target_directory: $target,
                packages: [{
                    name: "feanorfs-common",
                    version: "0.9.0",
                    dependencies: [{
                        name: "helper",
                        path: (if $path_dependency then "/workspace/helper" else null end)
                    }]
                }]
            }'
        ;;
    package)
        fixture="$FEANORFS_TEST_FIXTURE/feanorfs-common-0.9.0"
        mkdir -p "$fixture" "$FEANORFS_TEST_TARGET/package"
        printf '%s\n' '[package]' 'name = "feanorfs-common"' 'version = "0.9.0"' >"$fixture/Cargo.toml"
        tar -czf "$FEANORFS_TEST_TARGET/package/feanorfs-common-0.9.0.crate" \
            -C "$FEANORFS_TEST_FIXTURE" feanorfs-common-0.9.0
        ;;
    *)
        ;;
esac
EOF
chmod +x "$fake_cargo"

run_adapter() {
    FEANORFS_REAL_CARGO="$fake_cargo" \
    FEANORFS_TEST_FIXTURE="$fixture_root" \
    FEANORFS_TEST_LOG="$test_log" \
    FEANORFS_TEST_TARGET="$test_target" \
        "$adapter" "$@"
}

run_adapter package --allow-dirty --workspace
grep -Fx 'package --allow-dirty --package feanorfs-common --no-verify --locked' "$test_log" >/dev/null
if grep -E '^package .*--workspace([[:space:]]|$)' "$test_log" >/dev/null; then
    echo "error: historical package command retained --workspace" >&2
    exit 1
fi
test -f "$test_target/package/feanorfs-common-0.9.0/Cargo.toml"

: >"$test_log"
if FEANORFS_TEST_PATH_DEPENDENCY=true run_adapter package --allow-dirty --workspace >/dev/null 2>&1; then
    echo "error: adapter accepted a git-only package with a path dependency" >&2
    exit 1
fi
if grep -q '^package ' "$test_log"; then
    echo "error: adapter packaged a git-only crate after path validation failed" >&2
    exit 1
fi

: >"$test_log"
run_adapter check --locked
grep -Fx 'check --locked' "$test_log" >/dev/null

printf '%s\n' 'release-plz cargo adapter tests passed.'
