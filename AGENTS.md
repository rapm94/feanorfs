# FS-SYNC KNOWLEDGE BASE

**Generated:** 2026-06-23T18:15:00+02:00
**Branch:** main
**Status:** Standalone SQLx/SQLite Integration + E2EE + Lazy Hydration Verified

## OVERVIEW
`fs-sync` is a developer-focused uncommitted-code synchronization tool written in Rust. It utilizes a **self-contained local-first architecture**:
1. **Metadata Synchronization**: Coordinated via standard SQLite databases on both sides. The server maintains metadata in `server-data/db.sqlite`, and the client tracks cache status in `.fs-sync/local_cache.db`. Clients query the server's `/api/sync/diff` endpoint sending their local metadata to negotiate differences.
2. **Blob Storage**: File contents are saved in content-addressed storage (CAS) blobs on a lightweight Axum server, identified by their Blake3 hashes.
3. **End-to-End Encryption (E2EE)**: Zero-knowledge protection. Files are encrypted on upload and decrypted on download using a symmetric key stream generated from a password and relative path via **Blake3's Extendable Output Function (XOF)**. The cloud database and blob server only ever see encrypted hashes and scrambled bytes.
4. **On-Demand Hydration (Lazy Sync)**: Pulling with `--lazy` fetches metadata and creates 0-byte placeholders on disk. The actual file bytes are downloaded and decrypted on-demand via the `hydrate` or `cat` commands.

---

## STRUCTURE
```
fs-sync/
├── common/              # Shared data models and utilities
│   └── src/lib.rs       # Base FileState, Blake3 XOF crypt_bytes and hashing
├── server/              # Pure blob storage server
│   ├── src/db.rs        # SQLite metadata DB coordinator using SQLx
│   └── src/main.rs      # Axum routes for sync negotiation, blob uploads & downloads
├── client/              # CLI terminal client
│   ├── src/api.rs       # HTTP client request wrappers for blob transport
│   ├── src/local.rs     # Client-side configuration, ignore rules, and local cache DB
│   └── src/main.rs      # CLI subcommand routing and debounced change watching
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
| FileState definition | [lib.rs](file:///Users/raulpuigbo/p/fs-sync/common/src/lib.rs#L8-L14) | Tracks relative path, Blake3 hash (encrypted), size, mtime, and deleted status. |
| Blake3 XOF E2EE | [lib.rs](file:///Users/raulpuigbo/p/fs-sync/common/src/lib.rs#L56-L70) | Symmetric XOR cipher driven by Blake3 Extendable Output Function (XOF). |
| Local Cache DB | [local.rs](file:///Users/raulpuigbo/p/fs-sync/client/src/local.rs#L31-L118) | Spins up the cache database using SQLx and exposes CRUD methods. |
| Directory Scanning | [local.rs](file:///Users/raulpuigbo/p/fs-sync/client/src/local.rs#L138-L269) | Uses `ignore` WalkBuilder. Matches size and mtime. Reports cached `server_mtime` for untouched placeholders. |
| HTTP API Wrappers | [api.rs](file:///Users/raulpuigbo/p/fs-sync/client/src/api.rs) | Wraps `/api/sync/diff`, `/api/upload`, and `/api/download/:hash`. |
| CLI Actions & Watching | [main.rs](file:///Users/raulpuigbo/p/fs-sync/client/src/main.rs#L73-L330) | Subcommand routers, watch debounce (500ms), and push/pull sync loops. |
| Hydration & Cat | [main.rs](file:///Users/raulpuigbo/p/fs-sync/client/src/main.rs#L552-L641) | Lazy download triggers and file decryption routines. |

---

## CODE MAP

### Core Data Models ([common/src/lib.rs](file:///Users/raulpuigbo/p/fs-sync/common/src/lib.rs))
- `FileState`: Schema for paths, hashes, sizes, mtimes, and deleted states.
- `SyncRequest` & `SyncResponse`: Serialization structs for endpoint negotiation.

### Server API Endpoints ([server/src/main.rs](file:///Users/raulpuigbo/p/fs-sync/server/src/main.rs))
- `POST /api/sync/diff`: Compares incoming client list with `files` table and returns a delta response.
- `POST /api/upload?workspace_id=...`: Receives raw encrypted bytes, writes them to `server-data/blobs/<hash>`, and upserts DB metadata.
- `GET /api/download/:hash`: Streams raw file contents.

### Client Database Schema ([client/src/local.rs](file:///Users/raulpuigbo/p/fs-sync/client/src/local.rs))
- `local_files` table: `path` (PK), `plaintext_hash`, `encrypted_hash`, `size`, `mtime` (disk), `server_mtime` (remote), `hydrated`.

---

## CONVENTIONS
1. **Cross-Platform Paths**: All files are tracked and uploaded using forward slashes (`/`). Always normalize path slashes using `fs_sync_common::normalize_path` before doing DB operations.
2. **No Redundant Hashing**: Check disk files against `local_cache.db` first. Rehash only if `mtime` or `size` differs.
3. **Zero-Knowledge Encryption**: Always encrypt file contents using `crypt_bytes` before calling `api.upload_file` and store the resulting `encrypted_hash` in the database.

---

## ANTI-PATTERNS (THIS PROJECT)
- **DO NOT** scan the `.fs-sync` or `.git` directories. These must be hardcoded as skipped in directory scanning.
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
cargo run --bin fs-sync-server
```

### Client CLI Usage
```bash
# Initialize a workspace with Server URL, workspace name, and master password
cargo run --bin fs-sync-client -- init http://localhost:3030 \
  --workspace my-workspace \
  --password "my-master-password"

# Check differences between local directory and server
cargo run --bin fs-sync-client -- status

# Upload local additions, updates, and deletes
cargo run --bin fs-sync-client -- push

# Download remote updates and deletes (fully hydrated)
cargo run --bin fs-sync-client -- pull

# Perform a lazy pull (downloads metadata only, creating placeholder files)
cargo run --bin fs-sync-client -- pull --lazy

# Perform a bidirectional sync (pull + push)
cargo run --bin fs-sync-client -- sync

# Perform a lazy bidirectional sync
cargo run --bin fs-sync-client -- sync --lazy

# Download and decrypt a specific placeholder file
cargo run --bin fs-sync-client -- hydrate src/main.rs

# Download and decrypt all unhydrated placeholder files
cargo run --bin fs-sync-client -- hydrate

# Print a file's contents, automatically hydrating it if it is a placeholder
cargo run --bin fs-sync-client -- cat src/main.rs

# Start real-time watch and sync loop
cargo run --bin fs-sync-client -- watch
```
