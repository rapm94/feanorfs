# Sync scope and ignore policy

FeanorFS mirrors the **current contents** of a workspace folder. This document records why we choose what to sync, what to skip by default, and what we deliberately do **not** do.

---

## Principles

1. **Sync the working directory, not the git index.** Untracked and gitignored paths are often the highest-value files (`.env`, local config, scratch notes, WIP). FeanorFS exists to move that context between machines.
2. **No git coupling.** A workspace does not need to be a git repo. Honoring `.gitignore` would pull in nested rules, negation, global excludes, and submodule semantics — complexity that belongs in git, not in a sync tool.
3. **Ongoing cost matters more than first sync.** Build and package-manager trees are not just large once; they **churn on every build** (`target/`, `node_modules/`, …). The watcher would rehash, encrypt, and upload gigabytes per compile. A "sync everything, first sync is slow" policy makes the tool unusable on typical Rust/Node projects, not merely slow on day one.
4. **Small frozen defaults, not a growing framework list.** Expanding `DEFAULT_IGNORES` to dozens of framework-specific directories is a maintenance treadmill (the slippery slope). Defaults stay short; admission to the list is gated by a written criterion below.
5. **One optional escape hatch.** `.feanorfsignore` covers project-specific exclusions. Most users should never need it.

---

## What always syncs

- All files under the workspace root, including hidden files and paths that `.gitignore` would exclude (e.g. `.env`, `.vscode/`, local overrides).
- Non-git workspaces (`feanorfs start ~/notes`) behave the same.

## What never syncs (hardcoded)

| Path | Reason |
|------|--------|
| `.feanorfs/` | Client state: config, cache DB, agent dirs, locks |
| `.git/` | VCS metadata — not user work product |
| `.feanorfs/agents/` | Scanned only inside agent isolation; excluded from main workspace walk |
| Symlinks | Reported by `status` as sorted, deduplicated skipped paths; links are never followed |
| Valid `CACHEDIR.TAG` directories | Regeneratable cache trees declared by their owning tools; contents remain untouched on disk |

## Default ignores (`DEFAULT_IGNORES`)

Built-in patterns in `client/src/local.rs` — applied by the scanner, watcher, and `prune-ignored`. Current list:

```
target/  node_modules/  .DS_Store  *.swp  *~  .venv/  __pycache__/  dist/  build/  .next/  .cache/
```

These cover the highest-churn, highest-volume artifact trees across common stacks without per-project configuration.

### Admission criterion (for future additions)

A path may be added to `DEFAULT_IGNORES` only if **all** of the following hold:

1. **Regeneratable** — reproducible from source + lockfiles or a single install/build command.
2. **Typically large or high-churn** — would dominate upload bandwidth, CPU, or watcher debounce if synced.
3. **Not user-authored content** — unlikely to hold scratch work, secrets, or config the user wants on another machine.

If a candidate fails (3), it stays out of defaults even when gitignored in most repos (e.g. `tmp/`, `log/`, bare `vendor/`).

### Rejected alternatives

| Alternative | Why not |
|-------------|---------|
| Sync everything | Ongoing artifact churn; unusable on real projects |
| Honor `.gitignore` | Excludes `.env` and WIP; git semantics; not all workspaces are repos |
| Large framework denylist (~50+ entries) | Slippery slope; every new bundler/framework adds maintenance |
| `--use-gitignore` as default | Same exclusion problem; extra mode surface |

## `.feanorfsignore`

Optional, gitignore-syntax file at the workspace root. Use for project-specific artifact dirs not in `DEFAULT_IGNORES` (e.g. custom `out/`, `vendor/` in PHP).

- **Not required** for typical Rust/Node/Python projects — defaults cover the heavy dirs.
- **Not a substitute for git** — duplicating an entire `.gitignore` defeats the product goal (sync what git ignores).
- Hidden `prune-ignored` removes server metadata for paths that newly match ignore rules.

## `CACHEDIR.TAG`

[Cachedir Tag Specification](https://bford.info/cachedir/): tools mark regeneratable cache directories with a `CACHEDIR.TAG` file (Cargo writes one in `target/`). Skipping tagged directories is principled and self-maintaining — tool authors declare caches; FeanorFS does not curate framework names.

FeanorFS prunes a tagged directory from both main-workspace and agent-workspace scans only when `CACHEDIR.TAG` starts with this exact signature, including the trailing LF:

```text
Signature: 8a477f597d28d172789f06886806bc55
```

A missing LF, different signature, non-file tag, or symlinked tag is invalid and does not prune the directory. A tag at the workspace root is deliberately exempt: pruning the root could hide the whole workspace and turn existing tracked files into remote deletions, so the root tag and its siblings sync normally. FeanorFS never follows links to inspect tags and never deletes skipped cache contents. Tag support complements (not replaces) the small `DEFAULT_IGNORES` list — e.g. `node_modules/` does not use the tag today.

## Implementation map

| Concern | Location |
|---------|----------|
| `DEFAULT_IGNORES`, walker, `.feanorfsignore` | `agent-core/src/local.rs` — `build_workspace_walker`, `scan_local_directory` |
| Prune tracked paths matching ignores | `client/src/commands.rs` — `prune_ignored` |
| Agent workspace scan | Separate walk under `.feanorfs/agents/<name>/`; same ignore machinery |

Git ignore machinery is explicitly disabled on the walker (`git_ignore(false)`, `git_exclude(false)`, `git_global(false)`).
