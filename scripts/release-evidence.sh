#!/usr/bin/env bash
# Release evidence helper: emits exact tag, commit SHA,
# workspace version, and release-channel evidence for one release tag.
#
# Usage: release-evidence.sh <release-tag> [expected-sha]
#
# Output (stdout, stable prefix format):
#   evidence_tag=<tag>
#   evidence_sha=<40-hex commit sha>
#   evidence_version=<workspace.package.version from Cargo.toml>
#   evidence_channel=<stable|prerelease>
#
# Exit codes: 0 success; 1 evidence failure; 2 usage/unsafe input.
#
# Security contract: the tag argument is validated against a strict safe
# character class BEFORE any use (API path segment / shell interpolation);
# anything else is rejected. Workflows keep their own trust decisions
# (where EXPECTED_SHA comes from) and pass it in explicitly.
set -euo pipefail

repo="${REPOSITORY:?REPOSITORY must name the owner/repo (github.repository)}"
: "${GH_TOKEN:?GH_TOKEN must be a repository-scoped token}"
tag="${1:-}"
expected_sha="${2:-}"
root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# Reject anything but the documented safe set: letters, digits, '.', '_', '+',
# '-'. Whole-string match (no per-line grep): control characters, quotes,
# spaces, slashes, and shell metacharacters are all rejected.
if [[ "$tag" == *[!A-Za-z0-9._+-]* || -z "$tag" ]]; then
  echo "::error::release-evidence: unsafe or empty release tag rejected" >&2
  exit 2
fi
# Reject lengths beyond GitHub's tag ceiling defensively.
if ((${#tag} > 256)); then
  echo "::error::release-evidence: release tag exceeds 256 bytes" >&2
  exit 2
fi

resolve_tag_commit() {
  local tag_object tag_sha tag_type
  tag_object="$(gh api "repos/$repo/git/ref/tags/$tag")"
  tag_sha="$(jq -er '.object.sha' <<<"$tag_object")"
  tag_type="$(jq -er '.object.type' <<<"$tag_object")"
  if [ "$tag_type" = "tag" ]; then
    local annotated
    annotated="$(gh api "repos/$repo/git/tags/$tag_sha")"
    tag_sha="$(jq -er '.object.sha' <<<"$annotated")"
    tag_type="$(jq -er '.object.type' <<<"$annotated")"
  fi
  if [ "$tag_type" != "commit" ]; then
    echo "::error::release-evidence: tag $tag does not resolve directly to a commit" >&2
    return 1
  fi
  printf '%s\n' "$tag_sha"
}

tag_sha="$(resolve_tag_commit)"
if ! grep -Eq '^[0-9a-f]{40}$' <<<"$tag_sha"; then
  echo "::error::release-evidence: resolved tag target is not a 40-hex commit" >&2
  exit 1
fi

# The release object must exist and point at the same commit (immutable
# publication: a tag alone is not a release).
release_target="$(gh api "repos/$repo/releases/tags/$tag" --jq '.target_commitish')"
release_sha="$(gh api "repos/$repo/commits/$release_target" --jq '.sha')"
if [ "$tag_sha" != "$release_sha" ]; then
  echo "::error::release-evidence: release target $release_sha differs from tag commit $tag_sha" >&2
  exit 1
fi

if [ -n "$expected_sha" ]; then
  if ! grep -Eq '^[0-9a-f]{40}$' <<<"$expected_sha"; then
    echo "::error::release-evidence: expected-sha must be 40-hex" >&2
    exit 2
  fi
  if [ "$tag_sha" != "$expected_sha" ]; then
    echo "::error::release-evidence: tag resolves to $tag_sha, expected $expected_sha" >&2
    exit 1
  fi
fi

version="$(sed -n 's/^version = "\([^"]*\)"/\1/p' "$root/Cargo.toml" | head -n1)"
if ! [[ "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+(-[A-Za-z0-9.-]+)?$ ]]; then
  echo "::error::release-evidence: workspace.package.version missing or non-semver in Cargo.toml" >&2
  exit 1
fi
channel="stable"
if [[ "$version" == *-* ]]; then
  channel="prerelease"
fi

printf 'evidence_tag=%s\nevidence_sha=%s\nevidence_version=%s\nevidence_channel=%s\n' \
  "$tag" "$tag_sha" "$version" "$channel"
