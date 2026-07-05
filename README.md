# FeanorFS

> **Dropbox for your uncommitted code** — end-to-end encrypted, self-host or managed.

FeanorFS synchronizes your working directory to a lightweight blob server using content-addressed storage (CAS) and end-to-end encryption (E2EE). The server only ever sees encrypted hashes and scrambled bytes — your plaintext never leaves your machine.

It is designed for one specific situation: you write code on more than one machine and want your uncommitted work-in-progress to follow you without thinking about it — like Dropbox, but for the files you have not committed yet. Run `feanorfs start` once and it keeps your working files mirrored across machines. Self-host the server or use a managed instance — same open-source stack either way.

## Scope

FeanorFS mirrors the **current contents** of a working directory to a blob server. It is **not version control**: no history, branches, tags, or merge UI. Use a VCS for that.

It syncs files on disk (including gitignored/untracked paths — often the point), skips `.git/` and `.feanorfs/`, and blocks common artifact trees (`target/`, `node_modules/`, …) by default. It does not read `.gitignore`. See [docs/sync-scope.md](docs/sync-scope.md).

## Features

**Agent isolation:** run multiple coding agents on one folder without trampling each other. FeanorFS detects overlaps and tells you when two agents — or you and an agent — touched the same file.

**Background sync (Dropbox-like):** your uncommitted work follows you between machines. Install, point at a folder, forget.

- **Zero-knowledge E2EE** — ChaCha20-Poly1305 AEAD for new blobs; format v2 rejects legacy XOR. Run `feanorfs migrate` on older workspaces.
- **Content-addressed storage** — Blake3-hashed blobs with deduplication and upload integrity checks.
- **Default ignores** — small built-in denylist for high-churn artifacts; optional `.feanorfsignore` for edge cases (does not honor `.gitignore`).
- **One verb onboarding** — `feanorfs start` creates, joins (`fnr1-…`), or resumes; then syncs and watches.
- **Single binary** — install `feanorfs` once; `feanorfs serve` runs the blob hub, `start --local` uses an in-process hub (no daemon).
- **Agent loop** — `spawn` → `status` → `refresh` → `land` → `conflicts keep`. Data isolation, not process sandboxing.
- **Conflict surfacing** — `.original`/`.local`/`.cloud` triples; bare `feanorfs conflicts` lists pending paths.
- **Lazy hydration**, **local cache**, **catch-up summary**, **library + `--json` API**.
- **Orchestrator surfaces** — hidden `events` (NDJSON) and `mcp` (MCP protocol + tools).
- **Server GC** — `feanorfs serve --gc-only`; periodic `--gc-interval` while serving.

## Architecture

One binary (`feanorfs`): sync client by default, blob hub via `feanorfs serve`. Local workspaces can use an in-process hub (`start --local`) with no separate process.

```
┌─────────────────────────────────────────────────────────────┐
│  feanorfs (single install)                                  │
│  ┌─────────────┐   feanorfs serve    ┌──────────────────┐  │
│  │ sync client │ ── HTTP / local ──▶ │ Axum hub+SQLite  │  │
│  │ start/sync  │                    │ server-data/     │  │
│  └─────────────┘                    └──────────────────┘  │
│       │ .feanorfs/local_cache.db                            │
└───────┼─────────────────────────────────────────────────────┘
        │ encrypted blobs + /api/sync/diff
        ▼
   (remote hub or embedded LocalHub)
```

For the security analysis, see [docs/threat-model.md](docs/threat-model.md). Architecture details live in the `AGENTS.md` files at the repo root and in each crate.

## Installation

One binary covers sync and self-hosted hub. Install `feanorfs` — you do not need a separate server package unless you want a server-only deploy (`feanorfs-server`, legacy).

### Pre-built binaries (cargo-binstall)

```bash
cargo install cargo-binstall
cargo binstall feanorfs-client   # installs `feanorfs`
```

### Install script (no Rust required)

```bash
curl -fsSL https://raw.githubusercontent.com/rapm94/feanorfs/main/scripts/install.sh | sh
```

Installs `feanorfs` via [cargo-dist](https://github.com/axodotdev/cargo-dist). Set `FEANORFS_INSTALL_SERVER=1` to also install the optional legacy `feanorfs-server` binary. Linux and macOS on x86_64 and ARM64.

Per-app installer (client — recommended):

```bash
curl --proto '=https' --tlsv1.2 -LsSf \
  https://github.com/rapm94/feanorfs/releases/latest/download/feanorfs-client-installer.sh | sh
```

### From source

```bash
cargo install --path client --bin feanorfs
```

### Build from repository

```bash
git clone https://github.com/rapm94/feanorfs.git
cd feanorfs
cargo build --release
# Binary: target/release/feanorfs  (includes `serve` subcommand)
```

## Quick start

### 1. Start the blob server

```bash
feanorfs serve --token "server-secret"
feanorfs serve --mdns   # LAN discovery
caddy reverse-proxy localhost:3030   # TLS on :443
```

Multi-instance: `feanorfs serve --port 3001 --data-dir /data/alice --token "alice-token"`

### 2. Mirror this folder

```bash
cd /path/to/your/project

# Create on a remote server (prints fnr1-… invite)
feanorfs start https://my-server.com --workspace my-workspace --token "server-secret"
feanorfs start 127.0.0.1:3030 --workspace my-workspace --token "server-secret"  # bare host:port OK

# Join from another machine
feanorfs start fnr1-...

# Local-only embedded hub
feanorfs start --local --workspace my-workspace

# Resume an existing workspace (sync + watch)
feanorfs start
feanorfs start ~/other/project   # resume/create in another folder
```

Inspect config: `feanorfs config` · full key + invite: `feanorfs config --key`

### 3. Sync

```bash
feanorfs status                    # read-only diff
feanorfs sync --no-watch           # one-shot bidirectional sync
feanorfs sync                      # sync + watch (default)
feanorfs sync --up --no-watch      # upload only
feanorfs sync --down --lazy        # lazy download
feanorfs hydrate src/main.rs       # materialize a placeholder
feanorfs cat src/main.rs           # print (auto-hydrates)
feanorfs summary                   # what changed since last session
```

See [docs/usage.md](docs/usage.md) for the full CLI reference. Agent loop demo: [scripts/demo-agent-loop.sh](scripts/demo-agent-loop.sh).

## Security

FeanorFS provides end-to-end encryption using ChaCha20-Poly1305 (AEAD) with per-file keys derived from your encryption key and the file path. The server is zero-knowledge: it cannot read your file contents.

**E2EE is always on.** Every workspace has an encryption password — if you don't provide one, a 64-character CSPRNG-generated key is created automatically. The same E2EE password must be used on all machines sharing a workspace.

**Server authentication** is optional. Run `feanorfs serve --token <TOKEN>` to require a Bearer token on all API requests (`--password` is accepted as an alias). On LAN, use `feanorfs serve --mdns` so clients can discover without typing an IP. On the internet, put a TLS-terminating reverse proxy (Caddy, nginx) in front.

**Important limitations** (see [docs/threat-model.md](docs/threat-model.md) for the full analysis):

- Format v2 workspaces reject non-AEAD blobs. Unmigrated v1 workspaces still accept legacy XOR on decrypt — run `feanorfs migrate`, then see [SEC-6](docs/roadmap.md) for removing the legacy path entirely. Until migrated, only sync against servers you trust.
- The server can observe metadata: file paths, sizes, modification times, and encrypted hashes. Path confidentiality is NOT protected.
- The server password travels in cleartext over HTTP. For internet deployments, always use TLS (Caddy/nginx reverse proxy).
- Passwords are stored in plaintext in `.feanorfs/config.json` and `~/.feanorfs/global.json`. Protect your workspace directory accordingly.

To report a security vulnerability, see [SECURITY.md](SECURITY.md).

## Configuration

The client stores its configuration in `.feanorfs/config.json`:

```json
{
  "server_url": "http://localhost:3030",
  "workspace_id": "my-workspace",
  "encryption_password": "auto-generated-or-user-provided-e2ee-key",
  "server_password": "optional-server-access-password"
}
```

The global server connection is cached in `~/.feanorfs/global.json`:

```json
{
  "server_url": "http://192.168.1.50:3030",
  "server_password": "optional-server-access-password"
}
```

All files in the workspace directory are synced, including hidden files and paths git would ignore. `.feanorfs/` and `.git/` are always skipped; common build trees are skipped by default. Details: [docs/sync-scope.md](docs/sync-scope.md).

## Development

```bash
# Build all crates
cargo build

# Run tests
cargo test

# Lint
cargo clippy --all-targets -- -D warnings

# Format check
cargo fmt --check

# License/advisory audit (requires cargo-deny)
cargo install cargo-deny
cargo deny check
```

See [CONTRIBUTING.md](CONTRIBUTING.md) for the development workflow.

## Roadmap

Open backlog: [docs/roadmap.md](docs/roadmap.md). **Next up:** tray MVP (menu-bar client shelling `feanorfs --json`).

| Priority | Theme |
|----------|-------|
| P1 | Tray app (DX-26–28) |
| P2 | Agent edge tests, sync polish, crypto cleanup (SEC-6), server history (GC-7) |
| P2 (blocked) | Account vault + NAT rendezvous (CONN-6/7) — needs hosted service |
| P3 | OS dataless files (DX-12), block chunking (CHUNK) |

## Project structure

```
feanorfs/
├── common/     # Shared data models + crypto
├── server/     # Hub library (embedded in `feanorfs serve`; optional `feanorfs-server` binary)
├── client/     # `feanorfs` binary: sync, agents, hub, MCP/events
└── docs/       # Threat model, usage, roadmap
```

## License

[MIT](LICENSE) © 2026 Raul Puigbó
