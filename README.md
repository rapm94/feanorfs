# FeanorFS

> Zero-knowledge working-directory sync for developers with multiple machines.

FeanorFS synchronizes your working directory to a lightweight blob server using content-addressed storage (CAS) and end-to-end encryption (E2EE). The server only ever sees encrypted hashes and scrambled bytes — your plaintext never leaves your machine.

It is designed for one specific situation: you write code on more than one machine and want your uncommitted work-in-progress to follow you without manually pushing to a remote every time you switch desks. FeanorFS runs in the background and keeps your working files mirrored across machines.

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

### From source

```bash
cargo install --path client --bin feanorfs
cargo install --path server --bin feanorfs-server
```

### Build from repository

```bash
git clone https://github.com/raulpuigbo/fs-sync.git
cd fs-sync
cargo build --release
# Binaries: target/release/feanorfs and target/release/feanorfs-server
```

## Quick start

### 1. Start the blob server

```bash
cargo run --bin feanorfs-server
# Listens on http://0.0.0.0:3030, advertises via mDNS on local network
```

For internet deployments, add `--password` and put Caddy in front for TLS:
```bash
feanorfs-server --password "server-secret" --no-mdns
caddy reverse-proxy localhost:3030   # auto-HTTPS on port 443
```

### 2. Connect + initialize a workspace

**On a LAN (mDNS auto-discovery):**
```bash
cd /path/to/your/project
feanorfs connect                          # auto-discovers server
feanorfs init --workspace my-workspace    # E2EE password auto-generated
```

**On the internet:**
```bash
cd /path/to/your/project
feanorfs connect https://my-server.com --password "server-secret"
feanorfs init --workspace my-workspace --password "your-e2ee-password"
```

The E2EE password is auto-generated if you don't provide one. Save it — other machines must use the same password to decrypt your files.

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

**Server authentication** is optional. Run `feanorfs-server --password <PASS>` to require a Bearer token on all API requests. On LAN, the server advertises itself via mDNS so clients can discover without typing an IP. On the internet, use `--no-mdns` and put a TLS-terminating reverse proxy (Caddy, nginx) in front.

**Important limitations** (see [docs/threat-model.md](docs/threat-model.md) for the full analysis):

- The encryption is **stream cipher based on Blake3 XOF**, not an authenticated encryption scheme (AES-GCM, ChaCha20-Poly1305). It does not provide integrity verification of ciphertext.
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

Files matching `.gitignore` patterns are automatically excluded from sync. The `.feanorfs/` and `.git/` directories are always skipped.

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
fs-sync/
├── common/     # Shared data models + Blake3 XOF encryption primitives
├── server/     # Axum blob server + SQLite metadata coordinator
├── client/     # CLI client with local cache, scanner, and sync engine
└── docs/       # Architecture, threat model, and usage documentation
```

## License

[MIT](LICENSE) © 2026 Raul Puigbó
