# server

## Purpose

Content-addressed encrypted object storage. Axum and SQLite store opaque blobs, workspace heads, reachability manifests, and format markers. The server never decrypts objects, inspects filenames, or merges content. Optional Bearer auth, optional mDNS LAN advertisement, and multi-instance flags remain transport concerns.

## Ownership

- Crate: `feanorfs-server` (library + source-only compatibility binary
  `feanorfs-server`). The supported production and release path is embedded in
  the `feanorfs` binary via `feanorfs serve`.
- Source layout: `src/main.rs`, `src/serve.rs` (HTTP + GC entry), `src/app.rs` + `src/app/` (router, guards, grouped routes, tests), `src/db.rs` (SQLite), `src/gc.rs`. Sync delta logic lives in `feanorfs_common::compute_sync_delta`.
- Runtime data lives in `server-data/` which is git-ignored and MUST stay server-local — never include it in distributions.

## Local Contracts

- Wire types come from `feanorfs-common`. Never redefine `FileState`/`SyncRequest`/`SyncResponse` here.
- `/api/sync/peek` and `/api/sync/diff` remain format-v1/v2 compatibility paths. Format-v3 clients use encrypted snapshot heads.
- `/api/head` is the single mutable snapshot commit point and uses SQLite `BEGIN IMMEDIATE` compare-and-swap.
- `/api/manifest` stores validated newline-delimited opaque blob IDs. GC retains current heads plus the configured day/count manifest window.
- Manifest upload rejects incomplete closures, and format-v3 head CAS requires a stored manifest.
- Format-v3 stamping deletes that workspace's flat `files` rows. Legacy sync and non-object uploads receive HTTP 426 even when callers spoof a v3 header.
- Migration fences persist in SQLite. Only the matching `X-FeanorFS-Migration` token may upload, publish manifests, swap heads, or stamp format until cutover completes.
- HTTP publication handlers share an `RwLock` with periodic GC. GC takes the write side so it never sweeps from a stale live set while objects or manifests publish.
- Bearer token comparison uses `constant_time_eq` to prevent timing side-channels. Any future auth changes MUST keep the timing equality property.
- Request bodies are capped at 100 MiB. Reachability manifests have an additional 8 MiB limit.
- Upload flow: compute `hash_bytes(body)` server-side and reject mismatches with 400 BEFORE writing the blob. If the DB upsert fails after the blob is on disk, the orphan blob MUST be removed before returning an error so no partial state survives.
- Download: a single `fs::read` covers both "missing" and "read error"; match `ErrorKind::NotFound` to 404 and everything else to 500. Do not reintroduce a separate `exists()` probe — the exists/read split is a TOCTOU window.
- `--token` and `--password` are aliases. `FEANORFS_TOKEN` env var mirrors `--token`. `--port` and `--data-dir` env aliases are `FEANORFS_PORT` and `FEANORFS_DATA_DIR` respectively.

## Work Guidance

- New SQL DDL must be added to `init_schema` with `CREATE TABLE IF NOT EXISTS`. Schema migrations are out of scope for the current design; if needed, add an `init_schema_v2` guarded by a `schema_version` row.
- Blob path inputs must pass `is_valid_hash` before being joined onto `storage_dir`. Never join user-supplied strings directly.
- All error paths return a typed `StatusCode` — never unwrap a DB result into a 500 with the original sqlx error visible to the client (leaks internals). Use `tracing::error!` for the full error and return `INTERNAL_SERVER_ERROR`.
- Logs go through `tracing`. No `println!` in server code.

## Verification

- `cargo test -p feanorfs-server` — covers upload validation, head races, format rejection, and retained-manifest GC.
- `cargo clippy -p feanorfs-server --all-targets -- -D warnings`.
- `cargo fmt -p feanorfs-server -- --check`.

## Child DOX Index

| Child | Purpose |
| :--- | :--- |
| [`src/app/`](src/app/AGENTS.md) | Axum format/migration guards, grouped route handlers, and route tests. |
