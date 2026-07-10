# state

## Purpose

Own crash-safe local-state persistence and focused tests. `../state.rs` owns schema and migration DTOs.

## Ownership

- `durable.rs` — lock acquisition, reload, mutation, and atomic commit.
- `tests/model.rs` — schema, deterministic serialization, and access-log bounds.
- `tests/atomic.rs` — injected pre/post-commit fault behavior.
- `tests/persistence.rs` — open/reopen, concurrency, corruption, and legacy guards.

## Local Contracts

- Initialize state while holding exclusive lock.
- Reads reload latest committed bytes under shared lock.
- Writes reload and commit under exclusive lock.
- Missing state after construction is corruption, not implicit reinitialization.

## Work Guidance

- Keep schema types separate from persistence mechanics.

## Verification

- `cargo test -p feanorfs-agent-core state::tests --locked`

## Child DOX Index

No child DOX files.
