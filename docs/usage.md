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

The server advertises itself via mDNS (Bonjour) on the local network, so clients can auto-discover it with `feanorfs connect` (no IP needed). Use `--no-mdns` to disable this when behind a reverse proxy or on the internet.

| Flag | Description | Default |
|---|---|---|
| `--password <PASS>` | Require clients to authenticate with a Bearer token | none (open access) |
| `--no-mdns` | Disable mDNS service advertisement | disabled (mDNS on) |

The server password can also be set via the `FEANORFS_SERVER_PASSWORD` environment variable.

For internet deployments, put a TLS-terminating reverse proxy (e.g. Caddy) in front and use `--no-mdns`:

```bash
caddy reverse-proxy localhost:3030  # auto-HTTPS, port 443
```

Log verbosity can be tuned via `RUST_LOG` (see [Environment](#environment) below).

## Client

### `connect` — connect to a server (cached globally)

```bash
feanorfs connect [URL] [--password <SERVER_PASS>]
```

| Argument / Flag | Description |
|---|---|
| `URL` (optional) | Server URL. If omitted, auto-discovers via mDNS on the local network. |
| `--password <PASS>` | Server access password (for servers that require authentication). |

Caches the server URL (and optional password) in `~/.feanorfs/global.json` so that subsequent commands (`init`, `sync`, etc.) don't need an explicit URL.

**On a LAN:** `feanorfs connect` with no args discovers the server automatically via mDNS/Bonjour. No IP address needed.

**On the internet or Tailscale:** `feanorfs connect https://my-server.com:3030 --password "server-pass"`. For Tailscale, mDNS works across the tailnet if multicast DNS relay is enabled.

### `init` — initialize a workspace

```bash
feanorfs init [SERVER_URL] --workspace <WORKSPACE_ID> [--password <E2EE_PASS>] [--server-password <SERVER_PASS>]
```

| Argument / Flag | Description | Default |
|---|---|---|
| `SERVER_URL` (optional) | Server URL. If omitted, uses the URL cached by `feanorfs connect`. | from cache |
| `--workspace`, `-w` | Workspace ID to sync with | `default` |
| `--password`, `-p` | E2EE encryption password. If omitted, one is auto-generated and saved. | auto-generated |
| `--server-password` | Server access password (overrides cached value from `connect`) | from cache |

Creates `.feanorfs/config.json` and `.feanorfs/local_cache.db` in the current directory.

**E2EE is always on.** If you don't pass `--password`, a 64-character hex key is generated using a CSPRNG and stored in the workspace config. You must use the same E2EE password on all machines that share a workspace — without it, files can't be decrypted.

The server password and E2EE password are different things:
- **Server password** — gates *access* to the server (who can talk to it at all).
- **E2EE password** — gates *readability* of file contents (who can decrypt the blobs).

**Example:**
```bash
# After `feanorfs connect` (URL cached):
feanorfs init --workspace my-project --password "correct-battery-horse-staple"

# Or one-shot with explicit URL:
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
feanorfs sync [--lazy] [--no-watch]
```

| Flag | Description |
|---|---|
| `--lazy` | Fetch metadata only; create 0-byte placeholder files instead of downloading full contents |
| `--no-watch` | Perform a single sync pass and exit. Do not enter the real-time watch loop. |

Performs `pull` then `push` in a single pass. Downloads are processed first so that local state is aligned before uploading.

By default, `sync` enters real-time watch mode after the initial pass (same as `feanorfs watch`). This is the intended seamless workflow — run `feanorfs sync` once and it keeps your workspace mirrored across machines as you work. Pass `--no-watch` to run a single sync and exit — useful in scripts, CI, or cron jobs where blocking forever is undesirable.

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

### `workspaces` — show server workspaces

```bash
feanorfs workspaces [SERVER_URL]
```

| Argument | Description |
|---|---|
| `SERVER_URL` (optional) | Server URL to query. If omitted, uses the configured workspace's server. |

Queries the server and prints all workspace IDs that have at least one non-deleted file. Aliased as `feanorfs list` and `feanorfs ls` for convenience.

**Note:** workspaces are listed based on file metadata on the server. A freshly `init`-ed workspace that has never pushed any files will not appear in the list.

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
#   Local changes to push (run 'feanorfs push'):
#     [modify/add] src/lib.rs
#   Remote changes to pull (run 'feanorfs pull'):
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

The server respects `RUST_LOG` for controlling log verbosity. The client writes trace-level logs to `.feanorfs/feanorfs.log` regardless of `RUST_LOG`.

```bash
# Server: enable debug logging for the server crate and tower-http
RUST_LOG=feanorfs_server=debug,tower_http=debug cargo run --bin feanorfs-server

# Server: silence everything except warnings
RUST_LOG=warn cargo run --bin feanorfs-server
```

If `RUST_LOG` is unset, the server defaults to `feanorfs_server=info,tower_http=info`.

All other client configuration is stored in `.feanorfs/config.json`.
