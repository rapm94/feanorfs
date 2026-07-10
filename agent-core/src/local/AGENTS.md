# local

## Purpose

Own local workspace configuration, JSON-backed `ClientDb` operations, filesystem admission, hash-cached scanning, and focused local-state tests. `../local.rs` remains the thin public facade.

## Ownership

- `config.rs` — workspace/global configuration and E2EE key validation.
- `cache.rs` — cache CRUD plus migration import/export.
- `conflicts.rs` — pending conflict registry and resolution history.
- `access.rs` — predictive access weights and session keys.
- `walker.rs` — path normalization, ignore rules, symlink reporting, and `CACHEDIR.TAG` pruning.
- `scan.rs` — stable file observation, encrypted hash caching, and tombstone projection.
- `tests/` — focused behavior tests grouped by responsibility.

## Local Contracts

- Keep public names re-exported from `../local.rs`; consumers must not depend on private submodule paths.
- Preserve scanner race behavior: compare size and mtime before/after reading, and retain metadata observed before the read when stable.
- Never follow symlinks. Prune nested valid `CACHEDIR.TAG` trees, but not a tagged workspace root.
- Batch scanner cache changes through `bulk_upsert_cache_entries`.
- Keep access-log bounds and durable-state locking rules documented in the parent `agent-core/AGENTS.md`.

## Work Guidance

- Split tests by responsibility; avoid rebuilding a monolithic `tests` module.
- Keep each source file at or below 250 nonblank, noncomment lines.

## Verification

- `cargo test -p feanorfs-agent-core local::tests --locked`
- `cargo test -p feanorfs-agent-core --locked`
- `cargo clippy -p feanorfs-agent-core --all-targets --all-features --locked -- -D warnings`

## Child DOX Index

No child DOX files.
