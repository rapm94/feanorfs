# hub_state

## Purpose

Own durable embedded-hub metadata, blob storage, and migration import/export. `../hub_state.rs` owns wire-compatible state types and `HubDb` identity.

## Ownership

- `store.rs` — workspace metadata, heads, manifests, fences, and blob I/O.
- `migration.rs` — SQLite migration DTO projection.

## Local Contracts

- Serialize metadata through `DurableJson` locking and atomic replacement.
- Format-v3 stamping requires a manifested head and clears flat rows plus migration fence atomically.
- Blob writes recreate the blob directory if removed.

## Work Guidance

- Keep migration DTO conversion separate from live request storage operations.

## Verification

- `cargo test -p feanorfs-agent-core --test hub_tests --locked`
- `cargo test -p feanorfs-client migrate_sqlite --locked`

## Child DOX Index

No child DOX files.
