# app

## Purpose

Own Axum request guards, grouped route handlers, and route-level tests. `../app.rs` remains router/auth shell and public `AppState` surface.

## Ownership

- `guards.rs` — client-format and migration-fence admission.
- `routes_legacy.rs` — legacy sync and upload.
- `routes_objects.rs` — download, workspace listing, and head compare-and-swap.
- `routes_publication.rs` — manifests, migration start, and format lifecycle.
- `routes_pair_relay.rs` — optional public, bounded, secret-blind WebSocket pairing rendezvous; never a file-traffic tunnel.
- `routes_tunnel_relay.rs` — optional public, bounded WebSocket forwarding for opaque inner-TLS private-hub streams; relay state owns only pending sockets and random route keys in memory.
- `tests/` — validation/auth and publication/migration scenarios.

## Local Contracts

- Publication handlers hold shared/read lock; format stamp and migration start hold write lock.
- Validate hash/path/body before storage side effects.
- Authenticate first, then acquire the global protected-request permit before body extraction. Upload and manifest routes also use dedicated four/two-permit caps. Return 503 on saturation and hold every permit until the streaming response reaches EOF or its body is dropped.
- Head publication always requires a root-bound immutable manifest; a legacy-format header cannot bypass that invariant. Treat `(workspace_id, snapshot_id)` as the manifest/head identity in retention logic.
- Publish verified blob bytes atomically from a distinct same-directory temporary file; a download may observe the previous or replacement complete ciphertext, never a partial concurrent upload.
- A metadata failure never unlinks the verified CAS path; identical hashes are shared and a concurrent or prior reference may already depend on it. GC removes genuinely unreferenced blobs.
- Keep download open/read atomic; no separate existence probe.
- Return typed status codes without exposing database errors.
- Pair relay paths accept only 128-bit lowercase-hex public session IDs. Bound global socket admission before upgrade, configure 16-KiB frame/message/write buffers, forward bounded binary/Ping/Pong frames through cancellation-safe directional pumps, expire pending offers, and keep the relay route separate from the bearer-authenticated hub API router.
- Tunnel relay paths accept only 256-bit lowercase-hex routes. Bound socket admission before upgrade, pending hosts globally/per route, active tunnels, 64-KiB frame/message/write buffers, total bytes, idle time, and lifetime; use cancellation-safe directional pumps for binary/Ping/Pong only and never parse inner TLS.

## Work Guidance

- Keep router wiring in `../app.rs`; handlers belong in responsibility modules.

## Verification

- `cargo test -p feanorfs-server --locked`
- `cargo clippy -p feanorfs-server --all-targets --locked -- -D warnings`

## Child DOX Index

No child DOX files.
