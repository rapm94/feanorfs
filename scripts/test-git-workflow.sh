#!/bin/sh
# Exercise the actual documented publication guard in disposable Git clones.
set -eu
root=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
scratch=$(mktemp -d)
trap 'rm -rf "$scratch"' EXIT HUP INT TERM
# Isolate Git configuration, hooks and identity from the developer's profile.
export GIT_CONFIG_NOSYSTEM=1 GIT_CONFIG_GLOBAL=/dev/null
export GIT_AUTHOR_NAME=Recipe GIT_COMMITTER_NAME=Recipe
export GIT_AUTHOR_EMAIL=recipe@example.invalid GIT_COMMITTER_EMAIL=recipe@example.invalid
unset GIT_DIR GIT_WORK_TREE GIT_INDEX_FILE GIT_COMMON_DIR
git init -q --bare "$scratch/origin"
git clone -q "$scratch/origin" "$scratch/writer" 2>/dev/null
cd "$scratch/writer"
git checkout -q -b main
printf 'base\n' > tracked.txt
printf '.env\n' > .gitignore
git add .
git commit -qm base
git push -q origin main
git --git-dir="$scratch/origin" symbolic-ref HEAD refs/heads/main
git clone -q "$scratch/origin" "$scratch/reader"
base=$(git rev-parse HEAD)
printf 'published\n' > tracked.txt
git commit -qam published
git push -q origin main
published=$(git rev-parse HEAD)

awk '/^if git fetch origin &&$/ { copying=1 } copying { print } copying && /^fi$/ { exit }' \
    "$root/docs/usage.md" > "$scratch/guard.sh"
test -s "$scratch/guard.sh"
cd "$scratch/reader"
printf 'published\n' > tracked.txt
printf 'private\n' > .env
printf 'scratch\n' > notes.txt

refused() {
    before_head=$(git rev-parse HEAD)
    cp .git/index "$scratch/index-before"
    cp tracked.txt "$scratch/file-before"
    sh "$scratch/guard.sh" > "$scratch/output" 2>&1
    grep -q 'Baseline not advanced' "$scratch/output"
    test "$(git rev-parse HEAD)" = "$before_head"
    cmp .git/index "$scratch/index-before"
    cmp tracked.txt "$scratch/file-before"
}

# Even when disk exactly matches the published tree, staging is user intent.
printf 'staged-only draft\n' > tracked.txt
git add tracked.txt
printf 'published\n' > tracked.txt
refused
git reset -q "$base" -- tracked.txt

# Unpublished local bytes must not be marked clean.
printf 'unfinished\n' > tracked.txt
refused
printf 'published\n' > tracked.txt

# A local commit must not disappear even if the resulting files match.
git commit --allow-empty -qm divergent
refused
git reset -q --mixed "$base"

# The valid fast-forward changes metadata and preserves every working file.
sh "$scratch/guard.sh" > "$scratch/output" 2>&1
test "$(git rev-parse HEAD)" = "$published"
git diff --quiet
git diff --cached --quiet
test "$(cat tracked.txt)" = published
test "$(cat .env)" = private
test "$(cat notes.txt)" = scratch

# Separate branch worktrees keep independent indexes and working files.
git worktree add -q "$scratch/search" -b feature/search
cd "$scratch/search"
test -f .git
printf 'search work\n' > tracked.txt
git add tracked.txt
test "$(git branch --show-current)" = feature/search
cd "$scratch/reader"
test "$(git branch --show-current)" = main
test "$(cat tracked.txt)" = published
git diff --cached --quiet
printf '%s\n' 'PASS: publication guard preserves staging, WIP, divergent history and private files; branch worktrees remain independent.'
