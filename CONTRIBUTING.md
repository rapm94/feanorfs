# Contributing to FeanorFS

Thank you for your interest in improving FeanorFS. This document describes the development workflow and expectations for contributions.

## Development setup

### Prerequisites

- Rust 1.75 or later (`rustup default stable`)
- SQLite (bundled by `sqlx` via the `sqlite` feature — no system install required)
- `cargo-deny` (optional, for license/advisory audits): `cargo install cargo-deny`

### Getting started

```bash
git clone https://github.com/raulpuigbo/fs-sync.git
cd fs-sync
cargo build
cargo test
```

### Running locally

```bash
# Terminal 1: start the blob server
cargo run --bin feanorfs-server

# Terminal 2: initialize a test workspace and exercise the CLI
mkdir /tmp/feanorfs-test && cd /tmp/feanorfs-test
cargo run --bin feanorfs -- init http://localhost:3030 --workspace test --password "test-pass"
cargo run --bin feanorfs -- status
cargo run --bin feanorfs -- push
cargo run --bin feanorfs -- pull
cargo run --bin feanorfs -- sync --lazy
cargo run --bin feanorfs -- hydrate
```

## Code style

### Formatting and linting

- **Formatting**: `cargo fmt` (configured via `rustfmt.toml` — edition 2021, 100-char max width, module-granular imports).
- **Linting**: `cargo clippy --all-targets -- -D warnings` (configured via `clippy.toml` — MSRV 1.75). CI enforces both. Clippy warnings are treated as errors.

### Conventions

- **Paths**: All file paths are tracked and uploaded using forward slashes (`/`). Normalize with `feanorfs_common::normalize_path` before any DB operation.
- **No redundant hashing**: Check disk files against `local_cache.db` first. Rehash only if `mtime` or `size` differs.
- **Zero-knowledge encryption**: Always encrypt file contents with `crypt_bytes` before calling `api.upload_file`. Store the resulting `encrypted_hash` in the database.
- **Error handling**: Use `anyhow::Result` for application code. Provide context with `.context()` or `.with_context()`. Avoid bare `.unwrap()` on fallible operations — use `.unwrap_or()` / `.unwrap_or_default()` only when the default is genuinely safe.
- **Skip control directories**: `.feanorfs` and `.git` must be hardcoded as skipped in directory scanning. Do not rely on `.gitignore` alone for these.
- **Debounce filesystem events**: Filesystem saves are noisy. Debounce watcher events for 500ms using a channel.
- **Lazy placeholders**: Do not download remote file bytes during sync if `--lazy` is enabled. Write 0-byte placeholders instead.

## Testing

- **Unit tests** live in `#[cfg(test)] mod tests` blocks within each source file.
- **Integration tests** live in `tests/` directories next to each crate's `src/`.
- **Crypto tests**: Any change to `crypt_bytes`, `hash_bytes`, or `normalize_path` in `common/` must include or update tests verifying roundtrip and determinism.
- Run the full suite: `cargo test --all`.

## Pull request process

1. **Open an issue first** for non-trivial changes (new features, breaking API changes, security-relevant modifications). Link the issue in your PR.
2. **Branch from `main`** and name your branch descriptively: `feat/add-aead-encryption`, `fix/sync-deadlock`, `docs/threat-model-update`.
3. **Keep PRs focused** — one logical change per PR. Split unrelated changes into separate PRs.
4. **Ensure CI passes locally before pushing**:
   ```bash
   cargo fmt --check
   cargo clippy --all-targets -- -D warnings
   cargo test --all
   ```
5. **Update documentation** if your change affects user-facing behavior: `README.md`, `docs/`, `CHANGELOG.md`.
6. **Security-relevant changes** (encryption, auth, key handling) must include updates to `SECURITY.md` and `docs/threat-model.md`.
7. **Write a clear PR description** using the PR template. Reference the issue: `Closes #123`.

## Commit messages

Use [Conventional Commits](https://www.conventionalcommits.org/):

```
feat(client): add --dry-run flag to push command
fix(server): reject upload on hash mismatch before writing blob
docs(security): document ciphertext integrity limitation
test(common): add crypt_bytes roundtrip property test
chore: bump sqlx to 0.8.1
```

## Release process

1. Update `CHANGELOG.md` under a new `## [Unreleased]` → `## [x.y.z] - YYYY-MM-DD` heading.
2. Bump `version` in `[workspace.package]` of the root `Cargo.toml`.
3. Tag: `git tag -s vx.y.z -m "Release vx.y.z"`.
4. Push tag: `git push --tags`.

## Code of conduct

All contributors are expected to uphold the [Contributor Covenant Code of Conduct](CODE_OF_CONDUCT.md). Be kind.
