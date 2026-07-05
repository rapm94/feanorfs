# Usage

Full reference for the FeanorFS CLI (`feanorfs`) — one binary for sync client and blob hub (`feanorfs serve`). The standalone `feanorfs-server` package is an optional legacy server-only install.

## What FeanorFS is (and isn't)

FeanorFS is a working-directory mirror for developers who use more than one machine — think **Dropbox for the folder you are actively working in**, not for your git history. It keeps current files in sync across machines, including paths that are not in version control.

**It is not version control.** No history, branches, or merge UI — only the current snapshot on disk. Use a VCS for history and collaboration.

## Server

### Start the blob server

```bash
feanorfs serve --token "your-server-secret"
feanorfs serve --gc-only --data-dir server-data
feanorfs serve --mdns
```

Legacy server-only binary (same router, optional separate install):

```bash
feanorfs-server --token "your-server-secret"
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
| `--gc-only` | Run blob/tombstone GC once and exit (no HTTP listener) | off |
| `--gc-interval <SECS>` | Periodic GC while serving (`feanorfs serve` only) | off |

All flags can also be set via environment variables: `FEANORFS_TOKEN`, `FEANORFS_PORT`, `FEANORFS_DATA_DIR`.

### Internet deployment (recommended)

For internet-facing deployments, put a TLS-terminating reverse proxy (Caddy) in front and run the server with `--token`:

```bash
# Hub (recommended — same binary as the sync client)
feanorfs serve --token "server-secret"

# TLS via Caddy (auto-HTTPS, port 443):
caddy reverse-proxy localhost:3030
```

mDNS is off by default — it's only useful on LAN and can't cross routers.

### Multi-instance deployment (SaaS-ready)

```bash
feanorfs serve --port 3001 --data-dir /data/alice --token "alice-token"
feanorfs serve --port 3002 --data-dir /data/bob   --token "bob-token"
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
feanorfs serve --mdns
```

Clients can then use `feanorfs start --lan` to find the server automatically.

Log verbosity can be tuned via `RUST_LOG` (see [Environment](#environment) below).

## Client

### Visible commands

| Command | Purpose |
|---|---|
| `start` | Create, join, or resume a workspace — then sync and watch |
| `sync` | Upload/download changes; enters watch mode by default |
| `status` | Read-only diff vs the mirror |
| `hydrate` / `cat` | Materialize lazy placeholders / print a file |
| `summary` | What changed since your last session |
| `config` | Inspect workspace config (`--key` for full key + invite) |
| `doctor` | Troubleshoot connection and config |
| `serve` | Run a blob hub |
| `migrate` | Upgrade v1 workspaces to format v2 (AEAD) |
| `agent` | Isolated agent workspaces (`status`, `spawn`, `land`, …) |
| `conflicts` | List and resolve sync conflicts (`keep`, `show`) |

Hidden aliases remain for scripts: `setup`, `init`, `join`, `attach`, `connect`, `push`, `pull`, `watch`, `show-key`, `workspaces`, `prune-ignored`, `events`, `mcp`, `agent check`, `agent commit`, `conflicts open`, `conflicts history`.

### `start` — create, join, or resume (recommended entry point)

```bash
feanorfs start                                    # resume existing workspace
feanorfs start https://my-server.com:3030         # create on server
feanorfs start 127.0.0.1:3030                     # bare host:port (http:// added)
feanorfs start fnr1-…                             # join from invite
feanorfs start ~/projects/my-app                  # operate in another folder
feanorfs start --local                            # embedded local hub
feanorfs start --local --encryption-key <KEY>     # local hub with explicit key
feanorfs start --lan                              # mDNS discovery + create
feanorfs start --no-watch                         # sync once, no watch loop
```

| Flag / positional | Description |
|---|---|
| `target` (positional) | Server URL, `host:port`, `fnr1-…` invite, or folder path |
| `--folder` | Workspace directory (alternative to folder-as-target) |
| `--workspace`, `-w` | Workspace name (default: `default`) |
| `--encryption-key` | Manual re-link with explicit key (requires `--workspace`; allowed on configured folders) |
| `--token` | Server access token (`--password` accepted as alias) |
| `--lan` | Discover server via mDNS |
| `--local` | Embedded in-process hub (no remote server) |
| `--no-watch` | Sync once and exit (no watch loop) |

Hidden `init` / `setup` / `attach` / `join` remain for scripts: they configure only (no auto-sync+watch). Use `feanorfs start` for the full onboarding flow.

**Machine A — create:**
```bash
feanorfs start https://my-server.com:3030 --workspace my-project --token "server-token"
# → fnr1-… invite + encryption key on clipboard
```

**Machine B — join:**
```bash
feanorfs start fnr1-...
```

Remote setups print a `fnr1-…` invite. Local-hub workspaces (`--local`) are not portable via invite — share with `feanorfs serve --data-dir .feanorfs/hub-data`.

### `config` — show configuration

```bash
feanorfs config
feanorfs config --key    # full E2EE key + invite, copied to clipboard
```

Prints global connection (`~/.feanorfs/global.json`) and workspace config (`.feanorfs/config.json`). Default output truncates the key; `--key` shows the full value.

### `doctor` — diagnose connection issues

```bash
feanorfs doctor
```

Runs health checks: workspace config, E2EE, server reachability, local cache DB. Suggests `feanorfs migrate` for v1 workspaces.

### `status` — show local vs. remote differences

```bash
feanorfs status
```

Read-only scan + server diff. Does not modify files.

### `sync` — upload, download, watch

```bash
feanorfs sync [--lazy] [--no-watch]
feanorfs sync --up [--no-watch]      # upload only
feanorfs sync --down [--lazy]        # download only
```

| Flag | Description |
|---|---|
| `--up` / `--down` | One direction only (replaces hidden `push` / `pull`) |
| `--lazy` | Metadata only; 0-byte placeholders |
| `--no-watch` | Single pass and exit (scripts/CI) |

By default, `sync` enters real-time watch after the initial pass. Debounced 500 ms.

### `hydrate` / `cat`

```bash
feanorfs hydrate [PATH]     # materialize lazy placeholder(s)
feanorfs cat <PATH>         # print file (auto-hydrates)
```

### `summary` — session catch-up diff

```bash
feanorfs summary [--summarize] [--no-remember]
```

| Flag | Description |
|---|---|
| `--summarize` | Pipe paths/metadata to `FEANORFS_SUMMARY_CMD` (default `feanorfs-llm`) |
| `--no-remember` | Do not update the session baseline |

### `migrate` — upgrade to format v2

```bash
feanorfs migrate [--rekey]
```

Re-seals blobs as AEAD and bumps `format_version` to 2. `sync` nudges v1 workspaces to run this.

### Global flags

| Flag | Description |
|---|---|
| `--json` | Structured JSON. `status` includes `mirror_state` for tray clients. |

### `agent` — isolated workspace copies

```bash
feanorfs agent                    # list agents (one-line state when online)
feanorfs agent status [NAME]      # list all, or preview one agent
feanorfs agent spawn <NAME> [--no-sync] [--replace]
feanorfs agent refresh <NAME>
feanorfs agent land <NAME> [--clean] [--propose]
feanorfs agent clean <NAME>
feanorfs agent run <NAME> -- <COMMAND> [ARGS...]
```

| Subcommand | Description |
|---|---|
| `status` | List all agents (enriched when server reachable; names-only offline). Hidden `agent list` returns legacy JSON `{"agents": ["name"]}`. |
| `spawn` | APFS clonefile/copy snapshot with server base hashes |
| `refresh` | Pull cloud changes the agent hasn't touched |
| `land` | Apply clean work, upload, register conflicts |
| `clean` | Remove agent dir and snapshot rows |
| `run` | Run a command in the agent dir — not a sandbox |

**Isolation caveat:** data isolation only — see [threat-model.md](threat-model.md).

### `conflicts` — list, compare, resolve

```bash
feanorfs conflicts                              # list pending paths
feanorfs conflicts keep <PATH> --local|--cloud|--both
feanorfs conflicts keep <PATH> --file <RECONCILED>
feanorfs conflicts show <PATH> [--open]
```

Version files use `.original`/`.local`/`.cloud` suffixes.

| Subcommand | Description |
|---|---|
| (bare) / `list` | Pending paths blocked from sync |
| `keep` | Resolve by keeping local, cloud, both, or a reconciled file |
| `show` | Unified diff; `--open` launches editor compare |

### Orchestrator surfaces (hidden)

```bash
feanorfs events    # NDJSON: sync_state, folder_changed, conflict_risk, …
feanorfs mcp       # MCP protocol + tools (agent_*, conflicts_*, sync_status)
```

File contents never leave the machine on either surface.

## Examples

### First-time setup across two machines

```bash
# Machine A: start server + create workspace
machine-a$ feanorfs serve --token "server-secret" &
machine-a$ cd /path/to/project
machine-a$ feanorfs start http://localhost:3030 --workspace proj --token "server-secret" --no-watch

# Machine B: join + lazy sync
machine-b$ cd /path/to/project
machine-b$ feanorfs start fnr1-... --no-watch
machine-b$ feanorfs sync --down --lazy --no-watch
machine-b$ feanorfs cat src/main.rs   # hydrates + prints
```

### Continuous sync while working

```bash
feanorfs start          # resume + watch (or feanorfs sync)
vim src/lib.rs          # auto-syncs within 500ms of saving
```

### Check what changed before syncing

```bash
feanorfs status
# Output:
#   Local changes not yet on the mirror (run 'feanorfs sync --up'):
#     [modify/add] src/lib.rs
#   Changes on other machines to download (run 'feanorfs sync --down'):
#     [download]   README.md (2.3 KB)
```

## Synced files

FeanorFS syncs all files in the workspace directory. `.gitignore` is **not** honored — use `.feanorfsignore` for exclusions. The following directories are **always skipped**:
- `.feanorfs/` — client state (config, cache DB)
- `.git/` — Git repository metadata

## Agent loop and reconciliation (orchestrators)

FeanorFS isolates **files**, not processes. An agent workspace is a normal folder under `.feanorfs/agents/<name>/` — point any coding agent's cwd at it. `agent run` sets `FEANORFS_AGENT` and `FEANORFS_AGENT_DIR` on the child; it does **not** sandbox the process — absolute-path writes can escape the agent dir (see [threat-model.md](threat-model.md)).

### Loop

```bash
feanorfs sync --no-watch          # optional: ensure folder is current
feanorfs agent spawn ci1
feanorfs agent run ci1 -- cargo test
feanorfs agent status ci1           # read-only preview
feanorfs agent refresh ci1          # pull cloud changes agent hasn't touched
feanorfs agent land ci1             # apply clean work; register needs-attention
feanorfs conflicts
feanorfs conflicts keep <path> --local | --cloud | --both | --file <reconciled>
```

### Reconciliation protocol (LLM harnesses)

1. `feanorfs --json agent land <name>` — read `conflicts[]` with `original_file`, `local_file`, `cloud_file`, `hint`.
2. Optional: `feanorfs agent land <name> --propose` writes `<path>.proposed` via diff3 (text only; never auto-applied).
3. Reconcile locally — human or agent writes a candidate file.
4. **Verify** in a spawned agent workspace: `agent spawn verify` → copy candidate → `agent run verify -- <tests>`.
5. Only a green run earns `feanorfs conflicts keep <path> --file <candidate>`.

**Tiered policy:** clean `--propose` → reconciler agent → human escalation (binary files, sensitivity-list paths, or N failed verifications).

Resolution history: `feanorfs conflicts history --json` (records method, resolver from `FEANORFS_AGENT` or `human`).

### Demo script

```bash
./scripts/demo-agent-loop.sh
```

## Exit codes

| Code | Meaning |
|---|---|
| 0 | Success |
| 1 | Application error (config not found, network failure, IO error, etc.) |

## Environment

The server respects `RUST_LOG` for controlling log verbosity. The client writes trace-level logs to `.feanorfs/feanorfs.log` regardless of `RUST_LOG`.

```bash
# Server: enable debug logging for the server crate and tower-http
RUST_LOG=feanorfs_server=debug,tower_http=debug feanorfs serve --token "secret"

# Server: silence everything except warnings
RUST_LOG=warn feanorfs serve
```

If `RUST_LOG` is unset, the server defaults to `feanorfs_server=info,tower_http=info`.

All other client configuration is stored in `.feanorfs/config.json`.
