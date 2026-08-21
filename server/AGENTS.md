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
- `/api/manifest` stores a canonical, immutable newline-delimited opaque blob set keyed by workspace and snapshot. The closure must contain its snapshot root and has at most 250,000 raw entries before deduplication. Exact-set retries are idempotent; expansion or shrinkage is rejected. New manifests are admitted under atomic 10,000-per-workspace/100,000-global row caps plus 1-GiB-per-workspace/8-GiB-global encoded-storage caps; exact retries and GC remain available at capacity. GC retains workspace-qualified current heads plus the configured day/count manifest window.
- Manifest upload rejects incomplete closures, and every head CAS requires a stored manifest regardless of the claimed client format.
- Format-v3 stamping deletes that workspace's flat `files` rows. Legacy sync and non-object uploads receive HTTP 426 even when callers spoof a v3 header.
- New legacy flat rows require canonical portable paths. A deletion may tombstone an unsafe path only when that exact workspace/path row already exists, so upgraded clients can retire metadata accepted by older releases without reopening unsafe insertion.
- Migration fences persist in SQLite. Only the matching `X-FeanorFS-Migration` token may upload, publish manifests, swap heads, or stamp format until cutover completes.
- HTTP publication handlers share an `RwLock` with periodic GC. GC takes the write side from mark through sweep so it never deletes from a stale live set while objects or manifests publish. Retained manifests are keyset-paged one bounded row at a time into a connection-local, disk-backed SQLite TEMP live set; blob membership is checked in fixed batches. Scratch-disk exhaustion or corrupt/oversized retained rows fail before manifest pruning, tombstone purge, or blob deletion—persistent manifest quotas are disk limits, never a RAM budget.
- Bearer token comparison uses `constant_time_eq` to prevent timing side-channels. Any future auth changes MUST keep the timing equality property.
- Protected API requests are admitted through a 64-permit semaphore in authenticated middleware before body extraction, with separate four-upload and two-manifest admission caps. Permits remain held through streaming response EOF/drop; saturation returns 503 instead of accumulating unbounded buffered bodies/tasks. Request bodies are capped at 100 MiB, and manifest `Content-Length` plus streamed bodies are capped at 64 MiB.
- Upload flow: compute `hash_bytes(body)` server-side and reject mismatches with 400 BEFORE writing the blob. Commit verified ciphertext through a distinct same-directory temporary file and atomic replacement so concurrent same-object uploads never expose partial bytes to downloads. If a flat-file DB upsert then fails, retain the verified immutable CAS object: it may predate or race this request and be shared by another workspace. Unreferenced objects are GC's responsibility.
- Download: a single `fs::read` covers both "missing" and "read error"; match `ErrorKind::NotFound` to 404 and everything else to 500. Do not reintroduce a separate `exists()` probe — the exists/read split is a TOCTOU window.
- `--token` and `--password` are aliases. Without either, generate and persist a 64-hex token at `data-dir/auth-token`; an explicit token rotates that file. `--allow-open` conflicts with a token. `FEANORFS_TOKEN` mirrors `--token`; `FEANORFS_PORT` and `FEANORFS_DATA_DIR` mirror their flags.
- Native Rustls HTTPS is default. Auto mode persists one private CA under `data-dir/tls/`, revalidates its CA/key usage, validity, self-signature, and exact certificate/private-key match on every startup, then refreshes the server leaf for current interfaces. CA/key directories and files are `0700`/`0600` on Unix. `--allow-http` is explicit reverse-proxy/development mode.
- Serialized CA and leaf private keys stay in `Zeroizing<String>` while being parsed or written; do not reintroduce ordinary in-memory key strings.
- Hub mDNS may publish scheme and a short public CA fingerprint, never the CA private key, bearer token, or a trust decision. Private-CA clients must arrive through an authenticated `fnh1` or `fnr1` capability.
- Automatic TLS leaves include `feanorfs-<CA fingerprint>.local`; mDNS
  advertises that CA-bound hostname with the host's explicit non-loopback IPv4
  records and re-registers it on IPv4 add/remove events. Interface and DHCP
  changes therefore require neither leaf regeneration nor fixed router leases.
  Custom certificate deployments retain ownership of their DNS names.
- Hub recovery bundles contain only the durable private CA certificate/key and bearer token. Seal them with fixed Argon2id parameters and XChaCha20-Poly1305; passphrases come from the interactive client and never argv/env. Export, import, and rotation bundle paths must resolve outside the hub data directory (including symlink aliases), and reads/encodings are bounded to 2 MiB before parsing or writing. Non-replace export/rotation uses atomic no-clobber publication rather than a check-then-overwrite. Import holds the offline runtime lock, verifies CA basic/key usage, validity, self-signature, and exact certificate/private-key public-key equality, durably fences multi-file replacement, removes stale leaf material, and resumes only with the same external bundle after interruption.
- Identity rotation requires the offline runtime lock, writes an encrypted backup outside the hub data directory before its durable replacement fence, reuses recovery-import validation/resume semantics, removes stale leaf material, and never changes the database, blobs, heads, manifests, or ciphertext. Old clients must fail until they explicitly authenticate the replacement capability.
- `--relay` (`--pair-relay` compatibility alias) is disabled by default and adds public pairing plus tunnel WebSocket routes outside hub bearer auth. Both relays cap admitted sockets before upgrade, configure WebSocket frame/message/write-buffer limits before reading a frame, and use independent directional pumps so cancellation cannot consume an unsent frame. Pairing uses 128-bit sessions, at most 4,096 admitted sockets, and bounded PAKE/AEAD exchanges. Tunnel routes are 256-bit, queue at most 4,096 pending/eight per route and 1,024 active tunnels, admit at most 6,144 sockets, accept only 64-KiB binary/Ping/Pong frames, close after five idle minutes, and cap each tunnel at 16 GiB/24 hours. Store no frames, routes, secrets, or workspace metadata. The protected hub router retains constant-time bearer authentication.
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

## Bounded opaque head-change waiting

- `app/head_wait.rs` owns the in-memory waiter registry keyed only by opaque
  workspace id: global (256) and per-workspace (16) bounds, wait durations
  capped at 30 s (below the client read-idle timeout), and disconnect cleanup
  via drop guards. Waiters are notified only after a durable head swap; a
  rejected CAS never wakes them. Exhausted waiter capacity returns HTTP 503,
  so clients treat saturation as retryable instead of immediately polling the
  unchanged head.
- `GET /api/head` accepts optional `after`/`wait_ms`; plain GETs keep the
  exact previous shape. No new route, table, or agent metadata exists — the
  hub remains content-blind and agent-blind.
