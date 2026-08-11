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
- Bound `local_state.json` at 128 MiB before reading or parsing; validate collection cardinalities before accepting or committing state. Parse the schema version without first duplicating the document into a generic JSON value.
- Preserve deterministic JSON field and vector ordering without cloning the complete state graph: serialize BTreeMaps by reference and sort only borrowed access-log and conflict-resolution entries.
- Stream canonical JSON directly into the atomic temporary file through a bounded writer; reject overflow before commit and retain the existing pre/post-commit durability semantics.

## Work Guidance

- Keep schema types separate from persistence mechanics.

## Verification

- `cargo test -p feanorfs-agent-core state::tests --locked`
- `cargo test -p feanorfs-agent-core --release 'state::tests::persistence::local_state_persistence_profile_100k' --locked -- --ignored --nocapture --exact` — opt-in streaming persistence profile; normal suites skip it.

## Child DOX Index

No child DOX files.
