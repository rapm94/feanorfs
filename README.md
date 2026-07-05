# FeanorFS

> Zero-knowledge working-directory sync for developers — **Dropbox for your uncommitted code.**

FeanorFS synchronizes your working directory to a lightweight blob server using content-addressed storage (CAS) and end-to-end encryption (E2EE). The server only ever sees encrypted hashes and scrambled bytes — your plaintext never leaves your machine.

It is designed for one specific situation: you write code on more than one machine and want your uncommitted work-in-progress to follow you without thinking about it. A background client (CLI `watch` today; system tray tomorrow) keeps your working files mirrored across machines. Self-host the server or use a managed instance — same open-source stack either way.

## Not a Git replacement

**FeanorFS is not version control and does not replace Git.** It does not track history, branches, commits, or diffs. There is no `log`, no `blame`, no `revert`, no `bisect`, no merge semantics, no conflict resolution.

FeanorFS is a single-axis mirror of your current working directory. It captures **what files look like right now**, not how they got there. If you need history or collaboration, use Git. FeanorFS is for the messy in-between state — the half-written functions, the experimental refactor, the TODO notes — that you don't want to commit yet but do want available on your other machine without thinking about it.

**Use FeanorFS when:**
- You work across a desktop and a laptop and want your uncommitted edits to appear on both without running `git stash` + `git push` + `git pull` every time you switch.
- You want to pick up exactly where you left off on another machine, including untracked files that Git doesn't see.
- You want this to happen automatically in the background (`feanorfs watch`) without remembering to push.

**Keep using Git for:**
- Committed history, branches, tags, releases.
- Collaboration with other people.
- Code review, bisect, revert, cherry-pick.
- Anything where you need to answer "what changed and when".

FeanorFS complements Git — it syncs the working directory that Git ignores. It does not touch `.git/`, does not interact with your repository, and works equally well inside or outside a Git project.

## Features

- **Zero-knowledge E2EE** — Files are encrypted on the client using a symmetric keystream derived from your password and the file's relative path via Blake3's Extendable Output Function (XOF). The server stores only encrypted hashes and ciphertext blobs.
- **Content-addressed storage** — File contents are stored as Blake3-hashed blobs, enabling deduplication and integrity verification on upload.
- **Metadata sync via SQLite** — Both client and server maintain SQLite databases for diff negotiation. The client sends its metadata to `/api/sync/diff` and receives a precise delta (upload, download, delete).
- **Lazy hydration** — Pull with `--lazy` to fetch metadata only and create 0-byte placeholder files. Hydrate on demand with `feanorfs hydrate <path>` or `feanorfs cat <path>`.
- **Real-time watch** — `feanorfs watch` monitors filesystem changes (debounced 500ms) and auto-syncs with the server.
- **Local cache** — The client caches plaintext/encrypted hash pairs keyed by `(mtime, size)` to avoid re-hashing unchanged files on every scan.
- **Cross-platform paths** — All paths are normalized to forward slashes before DB operations.
- **Agent workspaces** — `feanorfs agent spawn|commit|list|clean|run` for isolated copy-on-write sandboxes and three-way conflict detection (FeanorFS does not merge — consumers reconcile).
- **Catch-up summary** — `feanorfs summary [--summarize]` lists files added/modified/deleted since your last session.
- **Predictive hydration** — After `cat`/`hydrate`, co-occurring siblings are prefetched in the background (local-only).
- **Library + JSON API** — Use `feanorfs_client` from Rust, or add `--json` to CLI commands for machine-readable output.

## Architecture

```
┌──────────────┐     encrypted blobs      ┌──────────────┐
│   Client     │  ────────────────────▶   │   Server     │
│  (feanorfs)  │   metadata via JSON      │  (Axum+SQLite)│
│              │  ◀────────────────────   │              │
└──────────────┘   /api/sync/diff         └──────────────┘
       │                                          │
       │ .feanorfs/local_cache.db                 │ server-data/db.sqlite
       │ .feanorfs/config.json                    │ server-data/blobs/<hash>
       └──────────────────────────────────────────┘
```

For a deeper breakdown, see [docs/architecture.md](docs/architecture.md) and [docs/threat-model.md](docs/threat-model.md).

## Installation

### Pre-built binaries (cargo-binstall)

If you have Rust installed, cargo-binstall downloads the pre-built archive for
your platform and installs it to `~/.cargo/bin`:

```bash
cargo install cargo-binstall
cargo binstall feanorfs-client
cargo binstall feanorfs-server
```

### Install script (no Rust required)

```bash
curl -fsSL https://raw.githubusercontent.com/rapm94/feanorfs/main/scripts/install.sh | sh
```

Installs both `feanorfs` and `feanorfs-server` via [cargo-dist](https://github.com/axodotdev/cargo-dist)
installers. Supports Linux and macOS on x86_64 and ARM64.

Per-app installers are also published on each release:

```bash
# client only
curl --proto '=https' --tlsv1.2 -LsSf \
  https://github.com/rapm94/feanorfs/releases/latest/download/feanorfs-client-installer.sh | sh

# server only
curl --proto '=https' --tlsv1.2 -LsSf \
  https://github.com/rapm94/feanorfs/releases/latest/download/feanorfs-server-installer.sh | sh
```

### From source

```bash
cargo install --path client --bin feanorfs
cargo install --path server --bin feanorfs-server
```

### Build from repository

```bash
git clone https://github.com/rapm94/feanorfs.git
cd feanorfs
cargo build --release
# Binaries: target/release/feanorfs and target/release/feanorfs-server
```

## Quick start

### 1. Start the blob server

**Internet deployment (recommended):**
```bash
feanorfs-server --token "server-secret"
caddy reverse-proxy localhost:3030   # TLS on :443
```

**Multi-instance (SaaS-ready):**
```bash
feanorfs-server --port 3001 --data-dir /data/alice --token "alice-token"
feanorfs-server --port 3002 --data-dir /data/bob   --token "bob-token"
# Caddy routes subdomains to ports
```

**LAN deployment (optional mDNS auto-discovery):**
```bash
feanorfs-server --mdns
```

### 2. Mirror this folder

**Internet (primary):**
```bash
cd /path/to/your/project
feanorfs setup https://my-server.com --workspace my-workspace --token "server-secret"
# generates encryption key, copies to clipboard, prints attach command
```

**LAN (with mDNS):**
```bash
cd /path/to/your/project
feanorfs setup --workspace my-workspace --lan
```

**From another machine (link the same mirror):**
```bash
cd /path/to/your/project
feanorfs attach my-workspace --encryption-key a1b2...    # paste from machine A
feanorfs sync --no-watch                                 # download files
```

The encryption key is auto-generated if you don't provide one. It's copied to your clipboard and a ready-to-paste `attach` command is printed. Save it — without it, your files cannot be decrypted.

Check your configuration at any time:
```bash
feanorfs config
```

### 3. Sync

```bash
# Check what would change
feanorfs status

# Upload local changes (encrypted)
feanorfs push

# Download remote changes
feanorfs pull

# Bidirectional sync (one-shot, no watch loop)
feanorfs sync --no-watch

# Lazy sync (metadata only, 0-byte placeholders)
feanorfs sync --lazy

# Hydrate a specific lazy placeholder
feanorfs hydrate src/main.rs

# Print a file (auto-hydrates if needed)
feanorfs cat src/main.rs

# Real-time watch + auto-sync
feanorfs watch

# List active workspaces on the server
feanorfs workspaces
```

See [docs/usage.md](docs/usage.md) for the full CLI reference.

## Security

FeanorFS provides end-to-end encryption using a symmetric XOR cipher driven by Blake3's XOF. The server is zero-knowledge: it cannot read your file contents.

**E2EE is always on.** Every workspace has an encryption password — if you don't provide one, a 64-character CSPRNG-generated key is created automatically. The same E2EE password must be used on all machines sharing a workspace.

**Server authentication** is optional. Run `feanorfs-server --token <TOKEN>` to require a Bearer token on all API requests (`--password` is accepted as an alias). On LAN, the server advertises itself via mDNS so clients can discover without typing an IP. On the internet, use `--no-mdns` and put a TLS-terminating reverse proxy (Caddy, nginx) in front.

**Important limitations** (see [docs/threat-model.md](docs/threat-model.md) for the full analysis):

- The encryption is **stream cipher based on Blake3 XOF**, not an authenticated encryption scheme (AES-GCM, ChaCha20-Poly1305). The client mitigates active-server tampering by re-hashing downloaded ciphertext and comparing to the expected `encrypted_hash` before decrypting — but this is not a substitute for AEAD.
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

All files in the workspace directory are synced, including hidden files and files that would be ignored by `.gitignore`. The `.feanorfs/` and `.git/` directories are always skipped.

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

## Project structure

```
feanorfs/
├── common/     # Shared data models + Blake3 XOF encryption primitives
├── server/     # Axum blob server + SQLite metadata coordinator
├── client/     # CLI + feanorfs_client library (cache, scanner, sync engine)
└── docs/       # Architecture, threat model, and usage documentation
```

## License

[MIT](LICENSE) © 2026 Raul Puigbó
