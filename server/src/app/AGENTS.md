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
- Publish verified blob bytes atomically from a distinct same-directory temporary file; a download may observe the previous or replacement complete ciphertext, never a partial concurrent upload.
- Keep download open/read atomic; no separate existence probe.
- Return typed status codes without exposing database errors.
- Pair relay paths accept only 128-bit lowercase-hex public session IDs. Forward bounded binary/Ping/Pong frames only, expire pending offers, and keep the relay route separate from the bearer-authenticated hub API router.
- Tunnel relay paths accept only 256-bit lowercase-hex routes. Bound pending hosts globally/per route, active tunnels, frame size, total bytes, and lifetime; forward binary/Ping/Pong only and never parse inner TLS.

## Work Guidance

- Keep router wiring in `../app.rs`; handlers belong in responsibility modules.

## Verification

- `cargo test -p feanorfs-server --locked`
- `cargo clippy -p feanorfs-server --all-targets --locked -- -D warnings`

## Child DOX Index

No child DOX files.
