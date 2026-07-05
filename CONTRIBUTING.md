# Contributing to FeanorFS

Thank you for your interest in improving FeanorFS. This document describes the development workflow and expectations for contributions.

## Development setup

### Prerequisites

- Rust 1.75 or later (`rustup default stable`)
- SQLite (bundled by `sqlx` via the `sqlite` feature — no system install required)
- `cargo-deny` (optional, for license/advisory audits): `cargo install cargo-deny`

### Getting started

```bash
git clone https://github.com/rapm94/feanorfs.git
cd feanorfs
cargo build
cargo test
```

### Running locally

```bash
# Terminal 1: blob hub (same binary as the sync client)
cargo run --bin feanorfs -- serve --token "dev-secret"

# Terminal 2: workspace
mkdir /tmp/feanorfs-test && cd /tmp/feanorfs-test
cargo run --bin feanorfs -- start http://localhost:3030 --workspace test --token "dev-secret" --no-watch
cargo run --bin feanorfs -- status
cargo run --bin feanorfs -- sync --no-watch
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
- **Skip control directories**: `.feanorfs` and `.git` must be hardcoded as skipped in directory scanning. Do not rely on `.gitignore` for these. Sync scope and ignore policy: [docs/sync-scope.md](docs/sync-scope.md).
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

Releases are fully automated via [release-plz](https://github.com/release-plz/release-plz) and [cargo-dist](https://github.com/axodotdev/cargo-dist):

1. Merge conventional commits to `main`.
2. **release-plz** opens a Release PR (version bump + `CHANGELOG.md`). Merge it.
3. **release-plz** pushes a `vX.Y.Z` git tag (`git_release_enable = false` in `release-plz.toml`).
4. **cargo-dist** (`.github/workflows/release.yml`) builds cross-platform archives, shell/PowerShell installers, checksums, and creates the GitHub Release.

### Maintainer setup

- Add repository secret **`RELEASE_PLZ_TOKEN`** (PAT with `contents:write` + `pull-requests:write`). The default `GITHUB_TOKEN` cannot trigger the cargo-dist workflow on tag push.

### Local dist checks

```bash
cargo install cargo-dist --locked
dist plan          # preview artifacts for the current workspace version
dist build --artifacts=all   # build locally (requires cross targets installed)
```

Config lives in `dist-workspace.toml` and `release-plz.toml`. Regenerate CI after dist config changes: `dist init -y` or `dist generate`.

