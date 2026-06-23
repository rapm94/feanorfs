# Architecture

FeanorFS is a three-crate Rust workspace implementing zero-knowledge filesystem synchronization over a lightweight blob server.

## Workspace layout

```
fs-sync/
├── common/     # Shared data models + Blake3 XOF encryption primitives
├── server/     # Axum blob server + SQLite metadata coordinator
├── client/     # CLI client with local cache, scanner, and sync engine
└── docs/       # This documentation
```

## Components

### `common` — shared primitives

Location: [`common/src/lib.rs`](../common/src/lib.rs)

Defines the data exchanged between client and server, and the cryptographic primitives.

**Data models:**
- `FileState` — the canonical metadata record: `{ path, hash, size, mtime, deleted }`. The `hash` field always stores the **encrypted** Blake3 hash (the CAS blob key), never the plaintext hash.
- `SyncRequest` — `{ workspace_id, files: Vec<FileState> }`. Sent by the client to `/api/sync/diff`.
- `SyncResponse` — `{ upload_required, download_required, delete_local }`. The server's delta computation.

**Primitives:**
- `hash_bytes(bytes)` — Blake3 hash → hex string. Used for both plaintext (client cache) and ciphertext (CAS key) identification.
- `hash_file(path)` — Streaming Blake3 hash of a file (64KB buffer). Used to identify plaintext file contents on disk.
- `normalize_path(path)` — Replaces `\` with `/` for cross-platform consistency. All DB operations expect forward-slash paths.
- `crypt_bytes(data, password, path)` — Symmetric XOR encryption using a Blake3 XOF keystream derived from `blake3(password ‖ path)`. Calling it twice with the same `(password, path)` returns the original bytes. See [threat-model.md](threat-model.md) for security properties.

### `server` — blob storage + metadata coordinator

Location: [`server/src/main.rs`](../server/src/main.rs), [`server/src/db.rs`](../server/src/db.rs)

An Axum HTTP server on port 3030 with three endpoints:

| Endpoint | Method | Purpose |
|---|---|---|
| `/api/sync/diff` | POST | Receive client metadata, compute delta, return `SyncResponse` |
| `/api/upload?workspace_id=&path=&hash=&size=&mtime=` | POST | Receive encrypted bytes, verify hash, write blob, upsert DB |
| `/api/download/:hash` | GET | Stream encrypted blob bytes |

**Storage:**
- `server-data/blobs/<hash>` — content-addressed ciphertext blobs.
- `server-data/db.sqlite` — `files` table: `(workspace_id, path, hash, size, mtime, deleted, updated_at)`, PK = `(workspace_id, path)`.

**Sync negotiation logic** (`handle_sync_diff`):
1. Fetch all server files for the workspace into a `HashMap<path, FileState>`.
2. For each client file:
   - If server has it and client mtime > server mtime: push to `upload_required` (or mark deleted on server if `deleted=true`).
   - If server has it and server mtime > client mtime: push to `download_required` (or `delete_local` if server file is deleted).
   - If mtimes equal but hashes differ: push to `upload_required` (client-side change detection).
   - If server doesn't have it: push to `upload_required` (or mark deleted on server).
3. For server files the client doesn't know about: push to `download_required`.

### `client` — CLI + sync engine

Location: [`client/src/main.rs`](../client/src/main.rs), [`client/src/local.rs`](../client/src/local.rs), [`client/src/api.rs`](../client/src/api.rs)

**CLI** (`clap`): `init`, `status`, `push`, `pull [--lazy]`, `sync [--lazy]`, `hydrate [path]`, `cat <path>`, `watch`.

**Local cache** (`local.rs`):
- `.feanorfs/config.json` — server URL, workspace ID, encryption password.
- `.feanorfs/local_cache.db` — SQLite `local_files` table: `(path PK, plaintext_hash, encrypted_hash, size, mtime, server_mtime, hydrated)`.

**Directory scanning** (`scan_local_directory`):
1. Load all cached entries from `local_cache.db`.
2. Walk the directory tree using `ignore::WalkBuilder` (respects `.gitignore`).
3. For each file on disk:
   - If cache hit (same `mtime` + `size` + `hydrated=true`): reuse cached hashes. No re-hashing.
   - If unhydrated placeholder (size=0, `hydrated=false`): reuse cached encrypted hash + server_mtime. Reports server_mtime to avoid false "local changed" detection.
   - If cache miss (modified or new): read bytes, compute `plaintext_hash = hash_bytes(bytes)`, `encrypted_hash = hash_bytes(crypt_bytes(bytes, password, path))`.
4. For cached files no longer on disk: mark as `deleted=true` with `mtime = now()`.
5. Upsert all disk entries into the cache DB.

**Sync flow** (`do_sync`):
1. Scan local directory → `HashMap<path, FileState>`.
2. Build `SyncRequest` and call `/api/sync/diff`.
3. Process `download_required` first (download, decrypt, write, update cache).
4. Apply `delete_local` (remove file from disk, delete cache entry).
5. Process `upload_required` (read, encrypt, upload, server upserts metadata).
6. Clean up cache entries for locally-deleted files.

**Lazy hydration:**
- `pull --lazy` / `sync --lazy`: for each `download_required` file, write a 0-byte placeholder to disk and insert a cache entry with `hydrated=false` and `server_mtime = remote_mtime`.
- `hydrate <path>`: look up cache entry, if `hydrated=false`, download blob, decrypt, write to disk, update cache entry with `hydrated=true` and actual disk mtime.
- `cat <path>`: if the file is a placeholder, hydrate it first, then print contents.

**Watch mode:**
- Uses `notify::recommended_watcher` with recursive monitoring.
- Filters out events in `.feanorfs/` and `.git/` directories.
- Sends a unit signal through a tokio mpsc channel on relevant events.
- Main loop: receive signal → sleep 500ms (debounce) → drain pending signals → call `do_sync` → repeat.

## Data flow diagram

```
                    Client                                       Server
                    ──────                                       ──────

              scan_local_directory()
                       │
                       ▼
         HashMap<path, FileState>          ──POST /api/sync/diff──▶   handle_sync_diff()
                   │                                                       │
                   │                                          ┌────────────┴────────────┐
                   │                                          │ compare client vs server │
                   │                                          │ files by path + mtime    │
                   │                                          └────────────┬────────────┘
                   │                                                       │
                   │◀────────────── SyncResponse ──────────────────────────┘
                   │   {upload_required, download_required, delete_local}
                   │
        ┌──────────┼──────────────────────────────┐
        ▼          ▼                              ▼
   download     delete_local                 upload_required
   + decrypt    remove file                  read + encrypt
   + write      + delete cache               + POST /api/upload
   + upsert                                  server: verify hash → write blob → upsert DB
   cache
```

## Design decisions

### Why Blake3 XOF for encryption instead of AES-GCM?

Simplicity and zero external crypto dependencies. Blake3 is already a dependency for content-addressed hashing. Its XOF mode produces a keystream that can be XORed with plaintext. The tradeoff is the lack of ciphertext authentication (no AEAD). See [threat-model.md](threat-model.md) for the security implications and planned improvements.

### Why SQLite for metadata instead of a custom binary format?

SQLite is ubiquitous, zero-config, and provides atomic transactions with WAL mode. It avoids the need to implement a custom indexed file format. The `sqlx` crate with the `sqlite` feature bundles SQLite, so no system dependency is required.

### Why store both `plaintext_hash` and `encrypted_hash` in the local cache?

- `plaintext_hash` — used to detect local modifications without re-reading the file (compared against disk `mtime`/`size`).
- `encrypted_hash` — the CAS blob key on the server. Needed to download the correct blob and to report to the server during sync negotiation.

### Why `server_mtime` in the local cache?

When a file is pulled as a lazy placeholder (0 bytes, `hydrated=false`), the client needs to report the server's official mtime during the next sync negotiation — not the disk mtime (which would be the placeholder's creation time). Without `server_mtime`, every sync would falsely detect the placeholder as "locally modified" and trigger an unnecessary upload.

### Why debounce watcher events for 500ms?

Filesystem writes are noisy — a single `save` in an editor can emit 3-10 events (temp file → rename, or partial writes). Without debouncing, the watch loop would trigger multiple rapid syncs for a single logical change. 500ms is long enough to coalesce burst events but short enough to feel responsive.
