#!/usr/bin/env bash
# shellcheck disable=SC2016
# Offline contract tests for scripts/release-evidence.sh:
# malicious/unsafe ref strings must be rejected BEFORE any network call, a
# deterministic fake API validates the complete stable success output, and an
# expected-SHA mismatch must fail closed.
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
helper="$root/scripts/release-evidence.sh"
failures=0

# A fake `gh` proves the helper never reaches the network for unsafe inputs.
fake_bin="$(mktemp -d)"
trap 'rm -rf "$fake_bin"' EXIT
cat >"$fake_bin/gh" <<'EOF'
#!/usr/bin/env bash
echo "gh should not be called for unsafe input" >&2
exit 99
EOF
chmod +x "$fake_bin/gh"


export REPOSITORY="owner/repo"
export GH_TOKEN="dummy-token"
export PATH="$fake_bin:$PATH"

reject() {
  local input="$1"
  if "$helper" "$input" >/dev/null 2>&1; then
    echo "FAIL: unsafe tag accepted: $input" >&2
    failures=$((failures + 1))
  else
    local code=$?
    if [ "$code" -eq 2 ]; then
      echo "ok: rejected unsafe tag (exit 2): $(printf '%q' "$input")"
    else
      echo "FAIL: unsafe tag exited $code instead of 2: $(printf '%q' "$input")" >&2
      failures=$((failures + 1))
    fi
  fi
}

reject ""
reject "v1.0.0; rm -rf /"
reject 'v1.0.0$(curl evil)'
reject 'v1.0.0`id`'
reject 'v1.0.0 | tee /tmp/x'
reject 'v1.0.0 & echo pwned'
reject "v1.0.0
evil"
reject 'v1.0.0/releases'
reject 'v1.0.0?query=1'
reject 'v1.0.0*'
reject 'v1.0.0[1]'
reject 'v1.0.0{b}'
reject 'v1.0.0<esc>'
reject 'v1.0.0 space'
reject 'v1.0.0"quote"'
reject "v1.0.0'single'"
reject "$(printf 'v1.%250s0.0' '')" # embedded spaces
reject "v1.$(printf 'a%.0s' $(seq 1 254))" # 257-byte all-safe tag: length bound
reject 'v1.0.0/../../etc/passwd'
reject 'v1.0.0#fragment'
reject 'v1.0.0%2e%2e'

fake_gh_log="$fake_bin/calls"
workspace_version="$(sed -n 's/^version = "\([^"]*\)"/\1/p' "$root/Cargo.toml" | head -n1)"
if ! [[ "$workspace_version" =~ ^[0-9]+\.[0-9]+\.[0-9]+(-[A-Za-z0-9.-]+)?$ ]]; then
  echo "FAIL: workspace version is missing or non-semver" >&2
  exit 1
fi
fake_tag="v$workspace_version"
fake_sha='0123456789abcdef0123456789abcdef01234567'
fake_channel=stable
if [[ "$workspace_version" == *-* ]]; then
  fake_channel=prerelease
fi
export FAKE_GH_LOG="$fake_gh_log"
export FAKE_SHA="$fake_sha"
export FAKE_TAG="$fake_tag"
cat >"$fake_bin/gh" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
printf '%s\n' "$*" >>"$FAKE_GH_LOG"
case "$*" in
  "api repos/owner/repo/git/ref/tags/$FAKE_TAG")
    printf '{"object":{"sha":"%s","type":"commit"}}\n' "$FAKE_SHA"
    ;;
  "api repos/owner/repo/releases/tags/$FAKE_TAG --jq .target_commitish")
    printf '%s\n' 'release-target'
    ;;
  "api repos/owner/repo/commits/release-target --jq .sha")
    printf '%s\n' "$FAKE_SHA"
    ;;
  *)
    printf 'unexpected fake gh call: %s\n' "$*" >&2
    exit 98
    ;;
esac
EOF
chmod +x "$fake_bin/gh"

expected_output="$(printf '%s\n' \
  "evidence_tag=$fake_tag" \
  "evidence_sha=$fake_sha" \
  "evidence_version=$workspace_version" \
  "evidence_channel=$fake_channel")"
actual_output="$("$helper" "$fake_tag" "$fake_sha")" || {
  echo "FAIL: valid release evidence path failed" >&2
  failures=$((failures + 1))
  actual_output=''
}
if [ "$actual_output" != "$expected_output" ]; then
  echo "FAIL: stable release evidence output changed" >&2
  diff -u <(printf '%s\n' "$expected_output") <(printf '%s\n' "$actual_output") >&2 || true
  failures=$((failures + 1))
else
  echo "ok: stable release evidence output is exact"
fi
if [ "$(wc -l <"$fake_gh_log" | tr -d ' ')" -ne 3 ]; then
  echo "FAIL: success path made an unexpected number of gh calls" >&2
  failures=$((failures + 1))
fi

if mismatch_output="$("$helper" "$fake_tag" aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa 2>&1)"; then
  echo "FAIL: expected-SHA mismatch was accepted" >&2
  failures=$((failures + 1))
else
  mismatch_code=$?
  if [ "$mismatch_code" -ne 1 ] ||
    ! grep -Fq 'expected aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa' <<<"$mismatch_output"; then
    echo "FAIL: expected-SHA mismatch did not fail with the stable diagnostic" >&2
    failures=$((failures + 1))
  else
    echo "ok: expected-SHA mismatch fails closed"
  fi
fi

if [ "$failures" -ne 0 ]; then
  echo "release-evidence contract tests failed: $failures" >&2
  exit 1
fi
echo "release-evidence contract tests passed (offline validation and success path)"
