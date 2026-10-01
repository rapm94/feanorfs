#!/bin/sh
# Verify the matrix exit status without running Cargo or touching real profiles.
set -eu
root=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
scratch=$(mktemp -d)
trap 'rm -rf "$scratch"' EXIT HUP INT TERM
cat > "$scratch/cargo" <<'EOF'
#!/bin/sh
exit "${MATRIX_TEST_CARGO_EXIT:-0}"
EOF
chmod +x "$scratch/cargo"
PATH="$scratch:$PATH" MATRIX_TEST_CARGO_EXIT=0 \
    bash "$root/scripts/acceptance-matrix.sh" > "$scratch/pass"
grep -q '^PASS git-workflow$' "$scratch/pass"
if PATH="$scratch:$PATH" MATRIX_TEST_CARGO_EXIT=1 \
    bash "$root/scripts/acceptance-matrix.sh" > "$scratch/fail"; then
    printf '%s\n' 'FAIL: matrix hid failed checks' >&2
    exit 1
fi
grep -q '^FAIL ai5-state-identity$' "$scratch/fail"
grep -q '^SKIP ai6-two-agent-soak ' "$scratch/fail"
printf '%s\n' 'PASS: matrix reports failures through its exit status and retains field-test skips.'
