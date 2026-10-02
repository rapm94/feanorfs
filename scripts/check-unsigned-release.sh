#!/usr/bin/env bash
# Validate an existing preview before building or uploading. Never mutates it.
# Optional expected identity: SHA, release ID, literal target_commitish.
set -euo pipefail
: "${REPOSITORY:?}" "${GH_TOKEN:?}"
tag="${1:-}"
if [[ ! "$tag" =~ ^v[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
  echo '::error::Unsigned previews require a canonical application tag.' >&2
  exit 2
fi
object="$(gh api "repos/$REPOSITORY/git/ref/tags/$tag")"
sha="$(jq -er '.object.sha' <<<"$object")"
kind="$(jq -er '.object.type' <<<"$object")"
if [ "$kind" = tag ]; then
  object="$(gh api "repos/$REPOSITORY/git/tags/$sha")"
  sha="$(jq -er '.object.sha' <<<"$object")"
  kind="$(jq -er '.object.type' <<<"$object")"
fi
[[ "$kind" = commit && "$sha" =~ ^[0-9a-f]{40}$ ]]
release="$(gh api "repos/$REPOSITORY/releases/tags/$tag")"
jq -e --arg tag "$tag" '.tag_name == $tag and .prerelease == true and .draft == false' <<<"$release" >/dev/null
id="$(jq -er '.id' <<<"$release")"
target="$(jq -er '.target_commitish' <<<"$release")"
[[ "$id" =~ ^[1-9][0-9]*$ ]]
# Keep output single-line and API path-safe; normal Git branch names remain valid.
[[ -n "$target" && "$target" != *[!A-Za-z0-9._/-]* && "$target" != /* && "$target" != *..* ]]
release_sha="$(gh api "repos/$REPOSITORY/commits/$target" --jq '.sha')"
test "$release_sha" = "$sha"
if [ "$#" -gt 1 ]; then
  test "$#" -eq 4
  test "$sha" = "$2"
  test "$id" = "$3"
  test "$target" = "$4"
fi
printf 'sha=%s\nrelease_id=%s\nrelease_target=%s\n' "$sha" "$id" "$target"
