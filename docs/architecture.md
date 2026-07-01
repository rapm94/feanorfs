# Architecture

FeanorFS is a three-crate Rust workspace implementing zero-knowledge filesystem synchronization over a lightweight blob server.

## Workspace layout

```
feanorfs/
├── common/     # Shared data models + Blake3 XOF encryption primitives
├── server/     # Axum blob server + SQLite metadata coordinator
├── client/     # CLI + feanorfs_client library (cache, scanner, sync engine, agents)
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
- `AgentSnapshotEntry`, `ConcurrentEdit`, `AgentCommitResult` — agent workspace isolation and three-way conflict detection types.

**Primitives:**
- `hash_bytes(bytes)` — Blake3 hash → hex string. Used for both plaintext (client cache) and ciphertext (CAS key) identification.
- `hash_file(path)` — Streaming Blake3 hash of a file (64KB buffer). Used to identify plaintext file contents on disk.
- `normalize_path(path)` — Replaces `\` with `/` for cross-platform consistency. All DB operations expect forward-slash paths.
- `pack_bytes(data, password, path)` / `unpack_bytes(data, password, path)` — ChaCha20-Poly1305 AEAD for new blobs (`AEAD_PREFIX_BYTE` marker). `unpack_bytes` falls back to legacy `crypt_bytes` XOR for older blobs.
- `crypt_bytes(data, password, path)` — Legacy symmetric XOR (still used for backward-compatible reads). See [threat-model.md](threat-model.md).
- `is_safe_rel_path(path)` — Rejects `..`, absolute paths, and `.feanorfs`/`.git` control paths.

### `server` — blob storage + metadata coordinator

Location: [`server/src/app.rs`](../server/src/app.rs), [`server/src/db.rs`](../server/src/db.rs), [`server/src/lib.rs`](../server/src/lib.rs)

An Axum HTTP server (default port 3030) with five endpoints:

| Endpoint | Method | Purpose |
|---|---|---|
| `/api/sync/peek` | POST | Read-only delta: receive client metadata, return `SyncResponse` (no DB mutations) |
| `/api/sync/diff` | POST | Alias for `/api/sync/peek` (backward compatible) |
| `/api/upload?workspace_id=&path=&hash=&size=&mtime=&deleted=` | POST | Receive encrypted bytes (or tombstone when `deleted=true`), verify hash, write blob, upsert DB |
| `/api/download/:hash` | GET | Stream encrypted blob bytes |
| `/api/workspaces` | GET | List workspace IDs with at least one non-deleted file |

**Storage:**
- `server-data/blobs/<hash>` — content-addressed ciphertext blobs.
- `server-data/db.sqlite` — `files` table: `(workspace_id, path, hash, size, mtime, deleted, updated_at)`, PK = `(workspace_id, path)`.

**Sync negotiation logic** (`compute_sync_delta` in `sync_delta.rs`):
Read-only LWW comparison — server never applies deletes or uploads during peek. Client applies `upload_required`, `download_required`, and `delete_local` in a separate apply phase via `/api/upload` (including `deleted=true` tombstones).

### `client` — CLI + library + sync engine

Location: [`client/src/lib.rs`](../client/src/lib.rs) (library), [`client/src/main.rs`](../client/src/main.rs) (CLI entry), [`client/src/local.rs`](../client/src/local.rs), [`client/src/api.rs`](../client/src/api.rs), [`client/src/commands.rs`](../client/src/commands.rs)

**CLI** (`clap`): `connect`, `init`, `join`/`attach`, `status`, `push`, `pull [--lazy]`, `sync [--lazy] [--no-watch]`, `hydrate [path]`, `cat <path>`, `watch`, `summary [--summarize]`, `conflicts list|resolve`, `agent spawn|commit|check|list|clean|run`, plus global `--json`.

**Library** (`feanorfs_client`): exposes `sync`, `push`, `pull`, `hydrate`, `cat`, agent helpers, and `Serialize`-derived result types for programmatic use.

**Local cache** (`local.rs`):
- `.feanorfs/config.json` — server URL, workspace ID, encryption password.
- `.feanorfs/local_cache.db` — SQLite tables: `local_files`, `agent_snapshots`, `file_access_log`, `last_session`.

**Directory scanning** (`scan_local_directory`):
1. Load all cached entries from `local_cache.db`.
2. Walk the directory tree using `ignore::WalkBuilder` (gitignore disabled — all files synced).
3. For each file on disk:
   - If cache hit (same `mtime` + `size` + `hydrated=true`): reuse cached hashes. No re-hashing.
   - If unhydrated placeholder (size=0, `hydrated=false`): reuse cached encrypted hash + server_mtime. Reports server_mtime to avoid false "local changed" detection.
   - If cache miss (modified or new): read bytes, compute `plaintext_hash = hash_bytes(bytes)`, `encrypted_hash = hash_bytes(pack_bytes(bytes, password, path))`.
4. For cached files no longer on disk: mark as `deleted=true` with stable tombstone mtime (`max(server_mtime, mtime) + 1`).
5. Upsert all disk entries into the cache DB.

**Sync flow** (`do_sync`):
1. Scan local directory → `HashMap<path, FileState>`.
2. `peek_sync` (read-only) → detect workspace conflicts via three-way compare against `last_synced_files`; block LWW for pending paths.
3. Process `download_required` (download, `unpack_bytes`, atomic write; TOCTOU mtime check).
4. Apply `delete_local` (remove file from disk; fail sync if removal errors).
5. Process `upload_required` (tombstones via `upload_tombstone`, else `pack_bytes` + upload).
6. Update `last_synced_files` for non-conflicted paths.

**Lazy hydration:**
- `pull --lazy` / `sync --lazy`: for each `download_required` file, write a 0-byte placeholder to disk and insert a cache entry with `hydrated=false` and `server_mtime = remote_mtime`.
- `hydrate <path>`: look up cache entry, if `hydrated=false`, download blob, decrypt, write to disk, update cache entry with `hydrated=true` and actual disk mtime.
- `cat <path>`: if the file is a placeholder, hydrate it first, then print contents.

**Watch mode:**
- Uses `notify::recommended_watcher` with recursive monitoring on the workspace `current_dir`.
- Filters out events in `.feanorfs/` and `.git/` directories.
- Debounces FS events (500ms), then calls `do_sync`.
- Also runs idle periodic sync every ~45s to catch remote changes.
- Exponential backoff after consecutive sync failures (skips FS and idle polls while backing off).

**Agent workspaces** (`agent.rs`): `spawn` creates a copy-on-write snapshot under `.feanorfs/agents/<name>/` and records server hashes in `agent_snapshots`. `commit` reuses `/api/sync/diff` with the base snapshot as the client view to detect concurrent edits; conflicts are written under `.feanorfs/conflicts/` — FeanorFS does not merge.

**Catch-up summary** (`summary.rs`): diffs the workspace against the previous `last_session.last_scan` marker. Optional `--summarize` shells out to `FEANORFS_SUMMARY_CMD` with path metadata only.

**Predictive hydration** (`predictive.rs`): records co-accessed paths locally in `file_access_log` and prefetches top siblings after `cat`/`hydrate`. Data never leaves the client.

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

### Workspace sync conflict detection (`last_synced_state`)

The client stores the last agreed file metadata snapshot in `last_session.last_synced_state` (JSON blob today). Before applying upload/download actions, it sends that snapshot to `/api/sync/diff` to learn what changed on the server since the last successful agree — the same pattern as `agent commit`, without adding server logic.

**Handled:** concurrent offline edits to files that existed at last sync; concurrent delete agreement (both sides delete the same path).

**Known limitations (dumb diff protocol):**

- **Concurrent offline creates** of the same new path on two clients cannot be detected in one round-trip. Once a client reports the path, the server treats it as known and the diff will not mark both upload and download. Last-writer-wins can still occur for brand-new paths. Fixing this would require the server to distinguish “new to this client” from “new to the workspace.”
- **`last_synced_state` storage** is a single JSON blob per workspace. This is fine for typical dev folders; very large trees (10k+ files) would benefit from a dedicated per-path table and incremental updates (future work).
