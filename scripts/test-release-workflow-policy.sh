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
    require_text "$workflow" 'workflow_call:'
    require_text "$workflow" 'DIST_PLAN: ${{ inputs.plan }}'
    require_text "$workflow" 'if [ "$INVOCATION_REF" != "refs/tags/$release_tag" ]; then'
    require_text "$workflow" 'if [ "$tag_sha" != "$INVOCATION_SHA" ]; then'
    require_text "$workflow" '-f head_sha="$expected_sha"'
    require_text "$workflow" '.head_sha == $sha and'
    require_text "$workflow" 'require_trusted_sha "$EXPECTED_SHA" "release commit"'
    require_text "$workflow" 'git merge-base --is-ancestor "$EXPECTED_SHA" origin/main'
    require_text "$workflow" 'ref: ${{ steps.release.outputs.sha }}'
    require_text "$workflow" 'ref: ${{ needs.verify-release.outputs.sha }}'
    require_text "$workflow" 'if: ${{ vars.RELEASE_SIGNING_ENABLED == '\''true'\'' }}'
    require_text "$workflow" 'toolchain: 1.88.0'

    ! grep -Eq '^  (push|workflow_dispatch):' "$workflow" || \
        fail "$workflow must run only as a cargo-dist reusable workflow"
    ! grep -F -- 'gh release' "$workflow" >/dev/null || \
        fail "$workflow must stage artifacts instead of mutating a public release"
    ! grep -F -- 'releases/tags' "$workflow" >/dev/null || \
        fail "$workflow still waits for a public release"
    ! grep -F -- 'wait-for-release' "$workflow" >/dev/null || \
        fail "$workflow still contains the old circular release dependency"
done

test "$(grep -Fc 'if: ${{ vars.RELEASE_SIGNING_ENABLED == '\''true'\'' }}' \
    .github/workflows/desktop-release.yml)" -eq 1 || \
    fail '.github/workflows/desktop-release.yml must gate exactly the privileged Windows publication job'
test "$(grep -Fc 'if: ${{ vars.RELEASE_SIGNING_ENABLED == '\''true'\'' }}' \
    .github/workflows/tray-release.yml)" -eq 1 || \
    fail '.github/workflows/tray-release.yml must gate exactly the privileged macOS publication job'

require_text .github/workflows/tray-release.yml 'name: artifacts-macos-${{ needs.verify-release.outputs.sha }}'
require_text .github/workflows/desktop-release.yml 'name: artifacts-linux-${{ matrix.asset_arch }}-${{ needs.verify-release.outputs.sha }}'
require_text .github/workflows/desktop-release.yml 'name: artifacts-windows-x86_64-${{ needs.verify-release.outputs.sha }}'

validator=.github/workflows/validate-release-assets.yml
require_text "$validator" 'workflow_call:'
require_text "$validator" 'pattern: artifacts-*'
require_text "$validator" 'cargo-dist plan does not contain the exact 11 core assets.'
require_text "$validator" 'expected_count=30'
require_text "$validator" 'expected_count=45'
require_text "$validator" 'sha256sum -c sha256.sum'

require_text dist-workspace.toml 'rust-toolchain-version = "1.88.0"'
require_text dist-workspace.toml 'local-artifacts-jobs = ["./tray-release", "./desktop-release"]'
require_text dist-workspace.toml 'publish-jobs = ["./validate-release-assets"]'
require_text dist-workspace.toml 'github-release = "announce"'
require_text dist-workspace.toml 'github-attestations-phase = "announce"'

require_text .github/workflows/release.yml 'custom-tray-release:'
require_text .github/workflows/release.yml 'custom-desktop-release:'
require_text .github/workflows/release.yml 'custom-validate-release-assets:'
require_text .github/workflows/release.yml 'needs.custom-validate-release-assets.result'

ci=.github/workflows/ci.yml
require_text "$ci" '[[ "$candidate" =~ ^v[0-9]+\.[0-9]+\.[0-9]+$ ]]'
require_text "$ci" 'git worktree add "$RUNNER_TEMP/previous" "$previous_tag"'
if grep -F -- 'PREV=$(git tag --sort=-v:refname | head -n 1)' "$ci" >/dev/null; then
    fail "$ci must not select an arbitrary non-release tag as the previous release"
fi

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

relay=.github/workflows/relay-image.yml
release_ready_line="$(grep -nF 'if [ "$ready" != true ]; then' "$relay" | cut -d: -f1)"
release_evidence_line="$(grep -nF 'evidence="$(scripts/release-evidence.sh "$RELEASE_TAG" "$EXPECTED_SHA")"' "$relay" | cut -d: -f1)"
test -n "$release_ready_line" && test -n "$release_evidence_line" && \
    test "$release_evidence_line" -gt "$release_ready_line" || \
    fail "$relay must invoke release-evidence only after the bounded cargo-dist publication wait"

printf '%s\n' 'Deterministic atomic release publication policy passed.'
