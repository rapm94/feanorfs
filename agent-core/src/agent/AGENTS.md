# agent

## Purpose

Own agent workspace diffing, spawning, landing, refreshing, proposal generation, and focused validation tests. `../agent.rs` remains the public facade.

## Ownership

- `diff.rs` — three-way snapshot comparison and land candidate construction.
- `spawn.rs` — synchronized workspace copy with cleanup guard.
- `land.rs` + `land/` — conflict-gated land orchestration, head publication, and guarded materialization.
- `refresh.rs` — preserve/replace refresh semantics.
- `proposal.rs` — optional textual conflict proposals.
- `check.rs` and `tests.rs` — preview surface and name validation tests.

## Local Contracts

- Head compare-and-swap is land commit point; publication precedes materialization.
- Preserve `after-stage`, `after-cas`, and `after-materialize` fault boundaries.
- Folder changes after land gating must divert rather than overwrite.
- Conflict content is surfaced, never auto-merged into working files.

## Work Guidance

- Keep public operations re-exported from `../agent.rs`.
- Keep land phases typed and independently reviewable.

## Verification

- `cargo test -p feanorfs-agent-core --locked`
- `cargo test -p feanorfs-client --test sync_engine --locked`

## Child DOX Index

No child DOX files.
