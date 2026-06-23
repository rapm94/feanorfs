# Usage

Full reference for the FeanorFS CLI (`feanorfs`) and server (`feanorfs-server`).

## What FeanorFS is (and isn't)

FeanorFS is a working-directory mirror for developers who use more than one machine. It keeps your current files in sync across machines automatically, including untracked and uncommitted work that Git doesn't see.

**It is not version control.** No history, no branches, no commits, no diffs, no conflict resolution. It captures a snapshot of what your files look like right now, nothing more. Use Git for history and collaboration. Use FeanorFS for the uncommitted in-between state that you want available on your other machine without thinking about it.

## Server

### Start the blob server

```bash
cargo run --bin feanorfs-server
```

The server listens on `0.0.0.0:3030` and creates its data directory at `server-data/` (relative to the working directory where it was launched):

```
server-data/
├── db.sqlite       # metadata database
└── blobs/          # content-addressed ciphertext blobs
```

There are no configuration flags yet. To change the port, modify `server/src/main.rs` or run behind a reverse proxy.

## Client

### `init` — initialize a workspace

```bash
feanorfs init <SERVER_URL> --workspace <WORKSPACE_ID> --password "<PASSWORD>"
```

| Flag | Description | Default |
|---|---|---|
| `--workspace`, `-w` | Workspace ID to sync with | `default` |
| `--password`, `-p` | Encryption password (enables E2EE) | none (uses `default-secret-key`) |

Creates `.feanorfs/config.json` and `.feanorfs/local_cache.db` in the current directory.

**Example:**
```bash
feanorfs init http://localhost:3030 --workspace my-project --password "correct-battery-horse-staple"
```

### `status` — show local vs. remote differences

```bash
feanorfs status
```

Scans the workspace, queries the server for a diff, and prints:
- **Local changes to push** — files modified, added, or deleted locally.
- **Remote changes to pull** — files modified or added on the server.
- **Remote deletions to apply** — files deleted on the server that still exist locally.

Does not modify any files. Safe to run anytime.

### `push` — upload local changes

```bash
feanorfs push
```

Uploads all locally-modified files (encrypted) to the server and cleans up cache entries for deleted files. Does not download anything.

### `pull` — download remote changes

```bash
feanorfs pull [--lazy]
```

| Flag | Description |
|---|---|
| `--lazy` | Fetch metadata only; create 0-byte placeholder files instead of downloading full contents |

Downloads all remote changes and applies remote deletions. With `--lazy`, creates placeholder files that can be hydrated later with `hydrate` or `cat`.

### `sync` — bidirectional sync

```bash
feanorfs sync [--lazy]
```

Performs `pull` then `push` in a single pass. Downloads are processed first so that local state is aligned before uploading.

### `hydrate` — download and decrypt placeholders

```bash
feanorfs hydrate [PATH]
```

| Argument | Description |
|---|---|
| `PATH` (optional) | Specific file to hydrate. If omitted, hydrates all unhydrated placeholders. |

Downloads and decrypts the actual file contents for lazy placeholders. Updates the local cache to mark the file as hydrated.

### `cat` — print file contents (auto-hydrates)

```bash
feanorfs cat <PATH>
```

Prints the file's contents to stdout. If the file is an unhydrated placeholder, it is hydrated first automatically.

### `watch` — real-time sync

```bash
feanorfs watch
```

Monitors the workspace for filesystem changes and auto-syncs with the server. Uses a 500ms debounce to coalesce burst events (e.g., editor save sequences).

Performs an initial sync on startup to ensure state is aligned. Press `Ctrl+C` to stop.

## Examples

### First-time setup across two machines

```bash
# Machine A: start server + init + push
machine-a$ cargo run --bin feanorfs-server &
machine-a$ cd /path/to/project
machine-a$ feanorfs init http://localhost:3030 --workspace proj --password "s3cret-pass"
machine-a$ feanorfs push

# Machine B: init + lazy pull + hydrate on demand
machine-b$ cd /path/to/project
machine-b$ feanorfs init http://machine-a:3030 --workspace proj --password "s3cret-pass"
machine-b$ feanorfs pull --lazy
machine-b$ feanorfs cat src/main.rs   # hydrates + prints
```

### Continuous sync while working

```bash
# Terminal 1: watch for changes
feanorfs watch

# Terminal 2: edit files normally
vim src/lib.rs
# watch will auto-sync within 500ms of saving
```

### Check what changed before syncing

```bash
feanorfs status
# Output:
#   Local changes to push (run 'fs-sync push'):
#     [modify/add] src/lib.rs
#   Remote changes to pull (run 'fs-sync pull'):
#     [download]   README.md (2.3 KB)
```

## Ignored files

FeanorFS respects `.gitignore` patterns via the `ignore` crate. Files matching ignore patterns are excluded from sync.

The following directories are **always skipped** regardless of `.gitignore`:
- `.feanorfs/` — client state (config, cache DB)
- `.git/` — Git repository metadata

## Exit codes

| Code | Meaning |
|---|---|
| 0 | Success |
| 1 | Application error (config not found, network failure, IO error, etc.) |

## Environment

FeanorFS does not read any environment variables. All configuration is stored in `.feanorfs/config.json`.
