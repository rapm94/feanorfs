# hub_state

## Purpose

Own durable embedded-hub metadata, blob storage, and migration import/export. `../hub_state.rs` owns wire-compatible state types and `HubDb` identity.

## Ownership

- `store.rs` — workspace metadata, heads, manifests, fences, and blob I/O.
- `migration.rs` — SQLite migration DTO projection.

## Local Contracts

- Serialize metadata through `DurableJson` locking and atomic replacement.
- Format-v3 stamping requires a manifested head and clears flat rows plus migration fence atomically.
- Store each `(workspace_id, snapshot_id)` manifest once as a canonical closure containing its root and capped at `MANIFEST_MAX_ENTRIES` raw entries. Validate stored split lists directly without first joining an attacker-sized duplicate string. Same-set retries are idempotent; later expansion or shrinkage is rejected.
- Blob writes recreate the blob directory if removed.
- Unsafe legacy flat paths can transition only from an existing exact row to a tombstone; this cleanup path never inserts a row.

## Work Guidance

- Keep migration DTO conversion separate from live request storage operations.

## Verification

- `cargo test -p feanorfs-agent-core --test hub_tests --locked`
- `cargo test -p feanorfs-client migrate_sqlite --locked`

## Child DOX Index

No child DOX files.
