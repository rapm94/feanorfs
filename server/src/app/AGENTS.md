# app

## Purpose

Own Axum request guards, grouped route handlers, and route-level tests. `../app.rs` remains router/auth shell and public `AppState` surface.

## Ownership

- `guards.rs` — client-format and migration-fence admission.
- `routes_legacy.rs` — legacy sync and upload.
- `routes_objects.rs` — download, workspace listing, and head compare-and-swap.
- `routes_publication.rs` — manifests, migration start, and format lifecycle.
- `tests/` — validation/auth and publication/migration scenarios.

## Local Contracts

- Publication handlers hold shared/read lock; format stamp and migration start hold write lock.
- Validate hash/path/body before storage side effects.
- Keep download open/read atomic; no separate existence probe.
- Return typed status codes without exposing database errors.

## Work Guidance

- Keep router wiring in `../app.rs`; handlers belong in responsibility modules.

## Verification

- `cargo test -p feanorfs-server --locked`
- `cargo clippy -p feanorfs-server --all-targets --locked -- -D warnings`

## Child DOX Index

No child DOX files.
