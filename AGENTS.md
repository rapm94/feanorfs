# FEANORFS KNOWLEDGE BASE

**Generated:** 2026-06-23T18:15:00+02:00
**Branch:** main
**Status:** Standalone SQLx/SQLite Integration + E2EE + Lazy Hydration Verified

## OVERVIEW
`FeanorFS` is a developer-focused uncommitted-code synchronization tool written in Rust. It utilizes a **self-contained local-first architecture**:
1. **Metadata Synchronization**: Coordinated via standard SQLite databases on both sides. The server maintains metadata in `server-data/db.sqlite`, and the client tracks cache status in `.feanorfs/local_cache.db`. Clients query the server's `/api/sync/diff` endpoint sending their local metadata to negotiate differences.
2. **Blob Storage**: File contents are saved in content-addressed storage (CAS) blobs on a lightweight Axum server, identified by their Blake3 hashes.
3. **End-to-End Encryption (E2EE)**: Zero-knowledge protection. Files are encrypted on upload and decrypted on download using a symmetric key stream generated from a password and relative path via **Blake3's Extendable Output Function (XOF)**. The cloud database and blob server only ever see encrypted hashes and scrambled bytes.
4. **On-Demand Hydration (Lazy Sync)**: Pulling with `--lazy` fetches metadata and creates 0-byte placeholders on disk. The actual file bytes are downloaded and decrypted on-demand via the `hydrate` or `cat` commands.

---

## STRUCTURE
```
feanorfs/
├── common/              # Shared data models and utilities
│   └── src/lib.rs       # Base FileState, Blake3 XOF crypt_bytes and hashing
├── server/              # Pure blob storage server
│   ├── src/db.rs        # SQLite metadata DB coordinator using SQLx
│   └── src/main.rs      # Axum routes for sync negotiation, blob uploads & downloads
├── client/              # CLI terminal client
│   ├── src/api.rs       # HTTP client request wrappers for blob transport
│   ├── src/commands.rs  # Push/pull/sync/hydrate/cat command implementations
│   ├── src/local.rs     # Client-side configuration, ignore rules, and local cache DB
│   ├── src/main.rs      # CLI subcommand routing
│   └── src/watch.rs     # Debounced real-time change watcher
└── server-data/         # Created by server to store file blobs and sqlite metadata (git-ignored)
```

---

## THE LOCAL CACHE DESIGN
To avoid unnecessary re-hashing of unchanged local files, the client maintains a local cache database:
1. **`local_cache.db`** (Local Cache): SQLite database created via `sqlx::SqlitePool`. Contains the `local_files` table mapping disk path modifications (`mtime`/`size` -> hashes).
2. **Double-hash and Server Mtime Tracking**:
   - `plaintext_hash`: Used to quickly detect disk modifications.
   - `encrypted_hash`: Stores the actual crypt hash matching the server blob key.
   - `server_mtime`: Tracks the server's official commit mtime. Reporting this to the server during diff negotiation avoids false local changes detection for unhydrated placeholders.

---

## WHERE TO LOOK

| Task / Feature | Location | Notes |
| :--- | :--- | :--- |
| FileState definition | [lib.rs](file:///Users/raulpuigbo/p/feanorfs/common/src/lib.rs#L8-L14) | Tracks relative path, Blake3 hash (encrypted), size, mtime, and deleted status. |
| Blake3 XOF E2EE | [lib.rs](file:///Users/raulpuigbo/p/feanorfs/common/src/lib.rs#L56-L70) | Symmetric XOR cipher driven by Blake3 Extendable Output Function (XOF). |
| Local Cache DB | [local.rs](file:///Users/raulpuigbo/p/feanorfs/client/src/local.rs#L31-L118) | Spins up the cache database using SQLx and exposes CRUD methods. |
| Directory Scanning | [local.rs](file:///Users/raulpuigbo/p/feanorfs/client/src/local.rs#L138-L269) | Uses `ignore` WalkBuilder. Matches size and mtime. Reports cached `server_mtime` for untouched placeholders. |
| HTTP API Wrappers | [api.rs](file:///Users/raulpuigbo/p/feanorfs/client/src/api.rs) | Wraps `/api/sync/diff`, `/api/upload`, `/api/download/:hash`, and `/api/workspaces`. Sends Bearer auth header when server password is configured. |
| CLI Actions | [main.rs](file:///Users/raulpuigbo/p/feanorfs/client/src/main.rs) | Subcommand router (`connect`, `init`, `join`, `config`, `show-key`, `doctor`, `status`, `push`, `pull`, `sync`, `hydrate`, `cat`, `watch`, `workspaces`). `--lan` flag enables mDNS discovery. Clipboard copy of E2EE key. Interactive token prompt. All output formatting lives here. |
| Sync Engine | [commands.rs](file:///Users/raulpuigbo/p/feanorfs/client/src/commands.rs) | Pure sync logic returning structured result types (`SyncResult`, `PushResult`, `PullResult`, `HydrateResult`, `CatResult`, `StatusResult`). No `println!` — UI-agnostic. Future desktop/TUI app calls these directly. |
| Change Watching | [watch.rs](file:///Users/raulpuigbo/p/feanorfs/client/src/watch.rs) | Debounced (500ms) filesystem watcher that triggers `do_sync` on changes. |

---

## CODE MAP

### Core Data Models ([common/src/lib.rs](file:///Users/raulpuigbo/p/feanorfs/common/src/lib.rs))
- `FileState`: Schema for paths, hashes, sizes, mtimes, and deleted states.
- `SyncRequest` & `SyncResponse`: Serialization structs for endpoint negotiation.

### Server API Endpoints ([server/src/main.rs](file:///Users/raulpuigbo/p/feanorfs/server/src/main.rs))
- `POST /api/sync/diff`: Compares incoming client list with `files` table and returns a delta response.
- `POST /api/upload?workspace_id=...`: Receives raw encrypted bytes, writes them to `server-data/blobs/<hash>`, and upserts DB metadata.
- `GET /api/download/:hash`: Streams raw file contents.
- `GET /api/workspaces`: Lists all workspace IDs that have at least one non-deleted file.
- **Auth middleware**: If `--token` is set, all routes require `Authorization: Bearer <token>` header. `--password` accepted as alias.
- **mDNS**: Server advertises `_feanorfs._tcp.local.` on port 3030 for LAN discovery when started with `--mdns` (off by default for internet deployments).
- **Multi-instance**: `--port` and `--data-dir` flags allow running multiple isolated instances behind a reverse proxy (SaaS deployment model).

### Client Database Schema ([client/src/local.rs](file:///Users/raulpuigbo/p/feanorfs/client/src/local.rs))
- `local_files` table: `path` (PK), `plaintext_hash`, `encrypted_hash`, `size`, `mtime` (disk), `server_mtime` (remote), `hydrated`.
- Global config: `~/.feanorfs/global.json` stores server URL + optional server password, cached by `feanorfs connect`.
- Workspace config: `.feanorfs/config.json` stores server URL, workspace ID, E2EE password, and optional server password.

---

## CONVENTIONS
1. **Cross-Platform Paths**: All files are tracked and uploaded using forward slashes (`/`). Always normalize path slashes using `feanorfs_common::normalize_path` before doing DB operations.
2. **No Redundant Hashing**: Check disk files against `local_cache.db` first. Rehash only if `mtime` or `size` differs.
3. **Zero-Knowledge Encryption**: Always encrypt file contents using `crypt_bytes` before calling `api.upload_file` and store the resulting `encrypted_hash` in the database.

---

## ANTI-PATTERNS (THIS PROJECT)
- **DO NOT** scan the `.feanorfs` or `.git` directories. These must be hardcoded as skipped in directory scanning.
- **DO NOT** trigger syncs on every raw filesystem change event. Filesystem saves are noisy. Debounce updates for 500ms using a channel.
- **DO NOT** download remote file bytes immediately during sync if `--lazy` is enabled. Write 0-byte placeholders instead.

---

## COMMANDS

### Workspace Commands
```bash
# Build all crates
cargo build

# Run unit and integration tests
cargo test
```

### Starting the Blob Server
```bash
# Runs the Axum server on port 3030
cargo run --bin feanorfs-server
```

### Client CLI Usage
```bash
# Connect to a server (internet: explicit URL; LAN: --lan for mDNS discovery)
# If server requires auth and no --password given, prompts interactively
cargo run --bin feanorfs -- connect https://my-server.com:3030 --password "server-pass"
cargo run --bin feanorfs -- connect --lan

# Initialize a workspace (uses cached server, auto-generates E2EE key)
# Prints a ready-to-paste join command and copies key to clipboard
cargo run --bin feanorfs -- init --workspace my-workspace
cargo run --bin feanorfs -- init --workspace my-workspace --lan

# Join an existing workspace from another machine (combines connect + init)
cargo run --bin feanorfs -- join my-workspace --password "e2ee-key-from-machine-A"

# Show current connection and workspace configuration
cargo run --bin feanorfs -- config

# Check differences between local directory and server
cargo run --bin feanorfs -- status

# Upload local additions, updates, and deletes
cargo run --bin feanorfs -- push

# Download remote updates and deletes (fully hydrated)
cargo run --bin feanorfs -- pull

# Perform a lazy pull (downloads metadata only, creating placeholder files)
cargo run --bin feanorfs -- pull --lazy

# Perform a bidirectional sync (pull + push)
cargo run --bin feanorfs -- sync

# Perform a lazy bidirectional sync
cargo run --bin feanorfs -- sync --lazy

# Perform a single sync pass without entering watch mode (for scripts/CI)
cargo run --bin feanorfs -- sync --no-watch

# Download and decrypt a specific placeholder file
cargo run --bin feanorfs -- hydrate src/main.rs

# Download and decrypt all unhydrated placeholder files
cargo run --bin feanorfs -- hydrate

# Print a file's contents, automatically hydrating it if it is a placeholder
cargo run --bin feanorfs -- cat src/main.rs

# Start real-time watch and sync loop
cargo run --bin feanorfs -- watch

# List all active workspaces on the server
cargo run --bin feanorfs -- workspaces
```
