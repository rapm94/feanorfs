#!/bin/sh
# shellcheck disable=SC2016 # Policy literals intentionally contain shell/GHA syntax.
set -eu

fail() {
    printf 'release workflow policy test failed: %s\n' "$1" >&2
    exit 1
}

require_text() {
    file=$1
    text=$2
    grep -F -- "$text" "$file" >/dev/null || fail "$file is missing: $text"
}

for workflow in \
    .github/workflows/desktop-release.yml \
    .github/workflows/tray-release.yml; do
    require_text "$workflow" 'case "$EVENT_NAME" in'
    require_text "$workflow" 'if [ "$INVOCATION_REF" != "refs/tags/$RELEASE_TAG" ]; then'
    require_text "$workflow" 'if [ "$INVOCATION_REF" != refs/heads/main ]; then'
    require_text "$workflow" 'if [ "$EVENT_NAME" = push ] && [ "$tag_sha" != "$INVOCATION_SHA" ]; then'
    require_text "$workflow" '-f head_sha="$tag_sha"'
    require_text "$workflow" '.head_sha == $sha and'
    require_text "$workflow" 'require_trusted_sha "$EXPECTED_SHA" "release commit"'
    require_text "$workflow" 'git merge-base --is-ancestor "$INVOCATION_SHA" origin/main'
    require_text "$workflow" 'git merge-base --is-ancestor "$EXPECTED_SHA" "$INVOCATION_SHA"'
    require_text "$workflow" 'require_trusted_sha "$INVOCATION_SHA" "manual recovery commit"'
    require_text "$workflow" 'ref: ${{ steps.release.outputs.sha }}'
    require_text "$workflow" 'ref: ${{ needs.wait-for-release.outputs.sha }}'
    require_text "$workflow" 'if: ${{ vars.RELEASE_SIGNING_ENABLED == '\''true'\'' }}'

    if grep -Fx -- '          if [ "$tag_sha" != "$INVOCATION_SHA" ]; then' "$workflow" >/dev/null; then
        fail "$workflow still equates a manual recovery SHA with the release tag SHA"
    fi
done

test "$(grep -Fc 'if: ${{ vars.RELEASE_SIGNING_ENABLED == '\''true'\'' }}' \
    .github/workflows/desktop-release.yml)" -eq 1 || \
    fail '.github/workflows/desktop-release.yml must gate exactly the privileged Windows publication job'
test "$(grep -Fc 'if: ${{ vars.RELEASE_SIGNING_ENABLED == '\''true'\'' }}' \
    .github/workflows/tray-release.yml)" -eq 1 || \
    fail '.github/workflows/tray-release.yml must gate exactly the privileged macOS publication job'

for workflow in \
    .github/workflows/desktop-release.yml \
    .github/workflows/npm-release.yml \
    .github/workflows/relay-image.yml \
    .github/workflows/release-plz.yml \
    .github/workflows/tray-release.yml \
    .github/workflows/unsigned-desktop-release.yml; do
    require_text "$workflow" '.conclusion == "success"'
    if grep -F -- 'status=completed' "$workflow" >/dev/null; then
        fail "$workflow relies on GitHub's lagging status index"
    fi
done

printf '%s\n' 'Deterministic release recovery policy passed.'
