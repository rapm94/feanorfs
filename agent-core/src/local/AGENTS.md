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
- Preserve scanner race behavior: compare size and mtime before/after reading, and retain metadata observed before the read when stable.
- Never follow symlinks. Prune nested valid `CACHEDIR.TAG` trees, but not a tagged workspace root.
- Safe join preview may supply an in-memory `.feanorfsignore` override so the encrypted sender policy can govern the first scan before any destination file is written; ordinary scans continue reading the on-disk file.
- Batch scanner cache changes through `bulk_upsert_cache_entries`.
- Keep access-log bounds and durable-state locking rules documented in the parent `agent-core/AGENTS.md`.
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
