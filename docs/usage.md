# Usage

Full reference for the FeanorFS CLI (`feanorfs`) and server (`feanorfs-server`).

## What FeanorFS is (and isn't)

FeanorFS is a working-directory mirror for developers who use more than one machine. It keeps your current files in sync across machines automatically, including untracked and uncommitted work that Git doesn't see.

**It is not version control.** No history, no branches, no commits, no diffs, no conflict resolution. It captures a snapshot of what your files look like right now, nothing more. Use Git for history and collaboration. Use FeanorFS for the uncommitted in-between state that you want available on your other machine without thinking about it.

## Server

### Start the blob server

```bash
cargo run --bin feanorfs-server -- --password "your-server-secret"
```

The server listens on `0.0.0.0:3030` and creates its data directory at `server-data/` (relative to the working directory where it was launched):

```
server-data/
├── db.sqlite       # metadata database
└── blobs/          # content-addressed ciphertext blobs
```

| Flag | Description | Default |
|---|---|---|
| `--token <TOKEN>` | Authentication token (Bearer auth). `--password` is accepted as an alias. | none (open access) |
| `--port <PORT>` | Port to listen on. Use different ports for multi-instance deployments. | `3030` |
| `--data-dir <DIR>` | Data directory for SQLite DB and blob storage. Each instance should have its own. | `./server-data` |
| `--mdns` | Enable mDNS service advertisement for LAN discovery | off |

All flags can also be set via environment variables: `FEANORFS_TOKEN`, `FEANORFS_PORT`, `FEANORFS_DATA_DIR`.

### Internet deployment (recommended)

For internet-facing deployments, put a TLS-terminating reverse proxy (Caddy) in front and run the server with `--token`:

```bash
# Server (on your VPS/cloud):
feanorfs-server --token "server-secret"

# TLS via Caddy (auto-HTTPS, port 443):
caddy reverse-proxy localhost:3030
```

mDNS is off by default — it's only useful on LAN and can't cross routers.

### Multi-instance deployment (SaaS-ready)

Run multiple isolated instances behind a single Caddy, each with its own data directory and port:

```bash
feanorfs-server --port 3001 --data-dir /data/alice --token "alice-token"
feanorfs-server --port 3002 --data-dir /data/bob   --token "bob-token"
```

Caddy routes subdomains to ports:
```
alice.feanorfs.app { reverse_proxy localhost:3001 }
bob.feanorfs.app   { reverse_proxy localhost:3002 }
```

Each instance is fully isolated: separate SQLite DB, separate blob storage, separate auth token. This is the deployment model for the managed SaaS — same binary, no code changes needed.

### LAN deployment

For local-only setups, enable mDNS so clients can auto-discover without typing an IP:

```bash
feanorfs-server --mdns
```

Clients can then use `feanorfs connect --lan` to find the server automatically.

Log verbosity can be tuned via `RUST_LOG` (see [Environment](#environment) below).

## Client

### `connect` — connect to a server (cached globally)

```bash
feanorfs connect <URL> [--password <SERVER_PASS>]
feanorfs connect --lan [--password <SERVER_PASS>]
```

| Argument / Flag | Description |
|---|---|
| `URL` (required unless `--lan`) | Server URL (e.g. `https://my-server.com:3030`). |
| `--token <TOKEN>` | Server access token (Bearer auth). `--password` is accepted as an alias. If omitted and server requires auth, prompts interactively. |
| `--lan` | Discover server on local network via mDNS instead of providing a URL. |

Caches the server URL (and optional token) in `~/.feanorfs/global.json` so that subsequent commands (`init`, `sync`, etc.) don't need an explicit URL.

**Internet (primary):** `feanorfs connect https://my-server.com:3030 --token "server-token"`

**LAN (with mDNS):** `feanorfs connect --lan` (requires server started with `--mdns`)

### `init` — initialize a workspace

```bash
feanorfs init [SERVER_URL] --workspace <WORKSPACE_ID> [--password <E2EE_PASS>] [--server-password <SERVER_PASS>]
```

| Argument / Flag | Description | Default |
|---|---|---|
| `SERVER_URL` (optional) | Server URL. If omitted, uses the URL cached by `feanorfs connect`. Use `--lan` to discover via mDNS instead. | from cache |
| `--workspace`, `-w` | Workspace ID to sync with | `default` |
| `--password`, `-p` | E2EE encryption password. If omitted, one is auto-generated, copied to clipboard, and a ready-to-paste `join` command is printed. | auto-generated |
| `--server-token` | Server access token (overrides cached value from `connect`). `--server-password` accepted as alias. | from cache |
| `--lan` | Discover server on local network via mDNS instead of using cached URL | off |

Creates `.feanorfs/config.json` and `.feanorfs/local_cache.db` in the current directory.

If a server URL is provided, `init` implicitly caches it (same as running `connect` first). This means on machine A you can do everything in one command:

```bash
feanorfs init https://my-server.com:3030 --workspace my-project
# → caches server URL, generates E2EE key, copies to clipboard, prints join command
```

**Internet (primary flow):**
```bash
feanorfs connect https://my-server.com:3030 --password "server-secret"
feanorfs init --workspace my-project
# → generates E2EE key, copies to clipboard, prints join command
```

**LAN (with mDNS):**
```bash
feanorfs init --workspace my-project --lan
# → discovers server, generates E2EE key, prints join command
```

When the E2EE password is auto-generated, the output looks like:

```
Initialized FeanorFS workspace!
  Blob Server:  http://192.168.1.50:3030
  Workspace ID: my-project
  Encryption:   Enabled (Blake3 XOF E2EE)

E2EE password: a1b2c3d4e5f6...
Copied to clipboard.

Join from another machine:
  feanorfs join my-project --password a1b2c3d4e5f6...

Save this password! Without it, your files cannot be decrypted.
```

### `join` — join an existing workspace

```bash
feanorfs join <WORKSPACE> --password <E2EE_PASS> [--server-url <URL>] [--server-password <SERVER_PASS>]
```

| Argument / Flag | Description |
|---|---|
| `WORKSPACE` (required) | Workspace ID to join |
| `--password`, `-p` (required) | E2EE encryption password (must match the one used by other machines in this workspace) |
| `--server-url` | Server URL. If omitted, uses cached connection from `feanorfs connect`. |
| `--server-token` | Server access token (for servers that require authentication). `--server-password` accepted as alias. |
| `--lan` | Discover server on local network via mDNS instead of using cached URL |

Combines `connect` + `init` into one command. Use this on machine B with the password printed by `init` on machine A.

```bash
# Internet:
feanorfs join my-project --password a1b2c3d4... --server-url https://my-server.com:3030

# LAN (with mDNS):
feanorfs join my-project --password a1b2c3d4... --lan

# Or if already connected via `feanorfs connect`:
feanorfs join my-project --password a1b2c3d4...
feanorfs sync --no-watch   # pull files
```

### `config` — show current configuration

```bash
feanorfs config
```

Prints both the global connection state (`~/.feanorfs/global.json`) and the workspace config (`.feanorfs/config.json`) in the current directory. Useful for checking which server you're connected to and whether E2EE is enabled.

```
Global connection (~/.feanorfs/global.json):
  Server:        http://192.168.1.50:3030
  Server auth:   enabled

Workspace (.feanorfs/config.json):
  Server:        http://192.168.1.50:3030
  Workspace ID:  my-project
  E2EE:          enabled
  E2EE key:      a1b2c3...d4e5
  Server auth:   enabled
```

### `show-key` — show E2EE password

```bash
feanorfs show-key
```

Prints the E2EE encryption password for this workspace and copies it to your clipboard. Also prints a ready-to-paste `join` command for other machines. Use this when you've lost the key that was auto-generated during `init`.

### `doctor` — diagnose connection issues

```bash
feanorfs doctor
```

Runs a health check: verifies global config, workspace config, E2EE status, server reachability, workspace existence on server, and local cache DB. Reports `[OK]`, `[WARN]`, `[INFO]`, or `[FAIL]` for each check.

```
Running diagnostics...

[OK]  Global config: server at https://my-server.com:3030
[OK]  Workspace config: my-project on https://my-server.com:3030
[OK]  E2EE: enabled
[OK]  Server reachable: 3 workspace(s) found
[OK]  Workspace 'my-project' exists on server
[OK]  Local cache DB: accessible

All checks passed.
```

Use `doctor` as the first troubleshooting step when something isn't working.

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

### Global flags

| Flag | Description |
|---|---|
| `--json` | Emit structured JSON for status-returning commands (`status`, `push`, `pull`, `sync`, `hydrate`, `cat`, `summary`, `agent commit`). Useful for scripting and the `feanorfs_client` result types. |

### `agent` — isolated workspace sandboxes

```bash
feanorfs agent spawn <NAME>
feanorfs agent commit <NAME> [--json]
feanorfs agent list
feanorfs agent clean <NAME>
feanorfs agent run <NAME> -- <COMMAND> [ARGS...]
```

| Subcommand | Description |
|---|---|
| `spawn` | Create a copy-on-write snapshot under `.feanorfs/agents/<NAME>/` and record the server's per-file view as the base snapshot. Requires the workspace E2EE password. |
| `commit` | Diff the agent workspace against its base snapshot. Detects concurrent edits (base/ours/theirs) and writes conflict files under `.feanorfs/conflicts/`. FeanorFS does **not** merge — reconcile conflicts yourself, then sync. |
| `list` | List agent workspaces with active snapshots. |
| `clean` | Remove an agent workspace and its snapshot rows. |
| `run` | Execute a command with the agent directory as the working directory (Level 1 sandbox). |

Agent names must be simple identifiers (no `/`, `\`, `.`, or `..`).

### `summary` — session catch-up diff

```bash
feanorfs summary [--summarize]
```

Compares the current workspace against the previous session marker (`last_session.last_scan` in the local cache DB). Prints paths grouped as added, modified, or deleted.

| Flag | Description |
|---|---|
| `--summarize` | Pipe the structured diff to `FEANORFS_SUMMARY_CMD` (default `feanorfs-llm`) for human-readable prose. Falls back to a plain path listing if the command is not on `PATH`. Only paths and metadata are sent — never file contents. |

Run `feanorfs summary` at the start of a session; it updates the session marker after displaying the diff.

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
