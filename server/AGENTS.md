# server

## Purpose

Content-addressed encrypted object storage. Axum, Rustls, and SQLite transport and store opaque blobs, workspace heads, reachability manifests, and format markers. The server never decrypts objects, inspects filenames, or merges content. Bearer auth, native TLS, optional mDNS LAN advertisement, and multi-instance flags remain transport concerns.

## Ownership

- Crate: `feanorfs-server` (library + source-only compatibility binary
  `feanorfs-server`). The supported production and release path is embedded in
  the `feanorfs` binary via `feanorfs serve`.
- Source layout: `src/main.rs`, `src/serve.rs` (HTTPS/HTTP + GC entry), `src/tls.rs` (private CA + leaf identity), `src/recovery.rs` (encrypted CA/token export, crash-safe offline restore, and identity rotation), `src/private_file.rs` (private atomic runtime files), `src/app.rs` + `src/app/` (router, guards, grouped routes, bounded pairing and opaque inner-TLS relays, tests), `src/db.rs` (SQLite), `src/gc.rs`. Sync delta logic lives in `feanorfs_common::compute_sync_delta`.
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
- `--token` and `--password` are aliases. Without either, generate and persist a 64-hex token at `data-dir/auth-token`; an explicit token rotates that file. `--allow-open` conflicts with a token. `FEANORFS_TOKEN` mirrors `--token`; `FEANORFS_PORT` and `FEANORFS_DATA_DIR` mirror their flags.
- Native Rustls HTTPS is default. Auto mode persists one private CA under `data-dir/tls/` and refreshes the server leaf for current interfaces; CA/key directories and files are `0700`/`0600` on Unix. `--allow-http` is explicit reverse-proxy/development mode.
- Serialized CA and leaf private keys stay in `Zeroizing<String>` while being parsed or written; do not reintroduce ordinary in-memory key strings.
- Hub mDNS may publish scheme and a short public CA fingerprint, never the CA private key, bearer token, or a trust decision. Private-CA clients must arrive through an authenticated `fnh1` or `fnr1` capability.
- Automatic TLS leaves include `feanorfs-<CA fingerprint>.local`; mDNS
  advertises that CA-bound hostname with the host's explicit non-loopback IPv4
  records and re-registers it on IPv4 add/remove events. Interface and DHCP
  changes therefore require neither leaf regeneration nor fixed router leases.
  Custom certificate deployments retain ownership of their DNS names.
- Hub recovery bundles contain only the durable private CA certificate/key and bearer token. Seal them with fixed Argon2id parameters and XChaCha20-Poly1305; passphrases come from the interactive client and never argv/env. Import must hold the offline runtime lock, validate the CA/key pair, durably fence multi-file replacement, remove stale leaf material, and resume only with the same bundle after interruption.
- Identity rotation requires the offline runtime lock, writes an encrypted backup outside the hub data directory before its durable replacement fence, reuses recovery-import validation/resume semantics, removes stale leaf material, and never changes the database, blobs, heads, manifests, or ciphertext. Old clients must fail until they explicitly authenticate the replacement capability.
- `--relay` (`--pair-relay` compatibility alias) is disabled by default and adds public pairing plus tunnel WebSocket routes outside hub bearer auth. Pairing uses 128-bit sessions and bounded PAKE/AEAD frames. Tunnel routes are 256-bit, queue at most 4,096 pending/eight per route and 1,024 active connections, accept only 64-KiB binary/Ping/Pong frames, and cap each connection at 16 GiB/24 hours. Store no frames, routes, secrets, or workspace metadata. The protected hub router must retain constant-time bearer authentication.
- HTTP tracing records methods/status/latency without request URIs. Relay routes and workspace query metadata must not enter logs.
- Normal serving and offline GC hold `hub-runtime.lock` for their lifetime and refuse an incomplete `recovery-import.json` fence. Do not bypass either guard when adding server entry points.

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
