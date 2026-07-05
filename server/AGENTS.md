# server

## Purpose

Content-addressed blob storage and sync metadata server. Axum HTTP server backed by SQLite (`server-data/db.sqlite`) and a flat blobs directory (`server-data/blobs/<hash>`). The server is dumb transport: it stores encrypted bytes and their Blake3 hashes, never decrypts, never inspects file content, and makes no semantic decisions. Optional Bearer auth, optional mDNS LAN advertisement, multi-instance via `--port` and `--data-dir`.

## Ownership

- Crate: `feanorfs-server` (library + optional legacy binary `feanorfs-server`). Production path: embedded in the `feanorfs` binary via `feanorfs serve`.
- Source layout: `src/main.rs`, `src/serve.rs` (HTTP + GC entry), `src/app.rs` (routes), `src/db.rs` (SQLite), `src/gc.rs`. Sync delta logic lives in `feanorfs_common::compute_sync_delta`.
- Runtime data lives in `server-data/` which is git-ignored and MUST stay server-local — never include it in distributions.

## Local Contracts

- Wire types come from `feanorfs-common`. Never redefine `FileState`/`SyncRequest`/`SyncResponse` here.
- `/api/sync/peek` and `/api/sync/diff` (alias) are read-only; both call `feanorfs_common::compute_sync_delta`.
- Bearer token comparison uses `constant_time_eq` to prevent timing side-channels. Any future auth changes MUST keep the timing equality property.
- Request body is capped at 100 MiB via `DefaultBodyLimit` to mitigate memory-exhaustion DoS. Do not raise this without a matching blob-size policy.
- Upload flow: compute `hash_bytes(body)` server-side and reject mismatches with 400 BEFORE writing the blob. If the DB upsert fails after the blob is on disk, the orphan blob MUST be removed before returning an error so no partial state survives.
- Download: a single `fs::read` covers both "missing" and "read error"; match `ErrorKind::NotFound` to 404 and everything else to 500. Do not reintroduce a separate `exists()` probe — the exists/read split is a TOCTOU window.
- `--token` and `--password` are aliases. `FEANORFS_TOKEN` env var mirrors `--token`. `--port` and `--data-dir` env aliases are `FEANORFS_PORT` and `FEANORFS_DATA_DIR` respectively.

## Work Guidance

- New SQL DDL must be added to `init_schema` with `CREATE TABLE IF NOT EXISTS`. Schema migrations are out of scope for the current design; if needed, add an `init_schema_v2` guarded by a `schema_version` row.
- Blob path inputs must pass `is_valid_hash` before being joined onto `storage_dir`. Never join user-supplied strings directly.
- All error paths return a typed `StatusCode` — never unwrap a DB result into a 500 with the original sqlx error visible to the client (leaks internals). Use `tracing::error!` for the full error and return `INTERNAL_SERVER_ERROR`.
- Logs go through `tracing`. No `println!` in server code.

## Verification

- `cargo test -p feanorfs-server` — currently the crate has no dedicated tests; integration behavior is exercised via the workspace `cargo test --workspace` run and locally via manual E2E (start server + two client fixtures in `test-client-a/b`).
- `cargo clippy -p feanorfs-server --all-targets -- -D warnings`.
- `cargo fmt -p feanorfs-server -- --check`.

## Child DOX Index

No child directories. `src/` is flat (`main.rs`, `serve.rs`, `app.rs`, `db.rs`, `gc.rs`, `lib.rs`).