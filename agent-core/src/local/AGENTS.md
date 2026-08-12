# local

## Purpose

Own local workspace configuration, JSON-backed `ClientDb` operations, filesystem admission, hash-cached scanning, and focused local-state tests. `../local.rs` remains the thin public facade.

## Ownership

- `config.rs` — workspace/global configuration facade and E2EE key validation.
- `credential_platform.rs` — native-store policy, including signed-macOS detection and explicit test/headless override.
- `credentials.rs` — native OS credential-store references, fail-closed updates, and protected-file fallback.
- `private_file.rs` — atomic private JSON writes and Unix `0700`/`0600` enforcement.
- `cache.rs` — cache CRUD plus migration import/export.
- `conflicts.rs` — pending conflict registry and resolution history.
- `access.rs` — predictive access weights and session keys.
- `walker.rs` — path normalization, ignore rules, symlink reporting, and `CACHEDIR.TAG` pruning.
- `scan.rs` — stable file observation, encrypted hash caching, and tombstone projection.
- `tests/` — focused behavior tests grouped by responsibility.

## Local Contracts

- Keep public names re-exported from `../local.rs`; consumers must not depend on private submodule paths.
- Preserve scanner race behavior: open admitted files through one shared `WorkspaceReadRoot`, compare size and mtime from that same descriptor before/after reading, and retain metadata observed before the read when stable. Admission/open/type mismatches abort rather than becoming tombstones.
- Never follow symlinks. Validate nested `CACHEDIR.TAG` signatures through the shared descriptor root before pruning; never reopen a tag by pathname. A tagged workspace root remains exempt.
- Convert native relative paths with `portable_rel_path` before publishing or joining them. Unix backslashes are filename bytes and must be rejected rather than rewritten into `/`; Windows separator backslashes are normalized before the portable validator runs.
- Safe join preview may supply an in-memory ignore-policy override so the encrypted sender policy can govern the first scan before any destination file is written; ordinary scans read only the private global workspace policy.
- Batch scanner cache changes through `bulk_upsert_cache_entries`. Staged materialization applies its cache deletes and upserts together through one `apply_cache_changes` durable-state commit.
- Keep access-log bounds and durable-state locking rules documented in the parent `agent-core/AGENTS.md`.
- `Config`, `GlobalConfig`, and the credential `Secrets` payload use custom `Debug` output that redacts E2EE keys, bearer tokens, relay routes, and CA bodies.
- `load_workspace_id` is the narrow public-metadata projection for routine read-only UI polling. It deserializes only `workspace_id` and must never resolve an OS credential reference; operations that need transport or encryption still use fail-closed `load_config`.
- Preserve unattended-sync credential boundaries: signed macOS releases and supported Windows/Linux sessions use the native OS store in-process; configs contain only random references. Unsigned macOS/source builds and unavailable stores fall back to atomic Unix `0700`/`0600` files, but migrated configs fail closed instead of returning secrets to JSON.
- `validate_e2ee_key` accepts arbitrary historical keys only for format v1. Format v2/v3 requires exactly 64 lowercase hexadecimal characters; this is a canonical generated-key shape, not a claim that arbitrary hexadecimal text has entropy.
- The release workflow proves automatic signed-macOS detection with `scripts/smoke-macos-keychain.sh`; success must require Developer ID Application authority, a redacted config, live Keychain reload, and cleanup. Development/ad-hoc binaries must fail that smoke.

## Work Guidance

- Split tests by responsibility; avoid rebuilding a monolithic `tests` module.
- Keep each source file at or below 250 nonblank, noncomment lines.

## Verification

- `cargo test -p feanorfs-agent-core local::tests --locked`
- `cargo test -p feanorfs-agent-core --locked`
- `cargo clippy -p feanorfs-agent-core --all-targets --all-features --locked -- -D warnings`

## Child DOX Index

No child DOX files.
