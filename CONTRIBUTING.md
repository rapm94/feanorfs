# Contributing to FeanorFS

Thank you for your interest in improving FeanorFS. This document describes the development workflow and expectations for contributions.

## Development setup

### Prerequisites

- Rust 1.88 or later (`rustup default stable`)
- No system SQLite install; the server and one-time legacy importer use bundled SQLx SQLite while the embeddable agent SDK uses JSON state.
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
- **Linting**: `cargo clippy --workspace --all-targets --all-features --locked -- -D warnings` (configured via `clippy.toml` — MSRV 1.88). CI enforces both. Clippy warnings are treated as errors.

### Conventions

- **Paths**: All file paths are tracked and uploaded using forward slashes (`/`). Normalize with `feanorfs_common::normalize_path` before cache or database operations.
- **No redundant hashing**: Check the private global workspace `local_state.json` first. Rehash only if `mtime` or `size` differs.
- **Zero-knowledge encryption**: File bytes and format-v3 tree/snapshot objects must be sealed through the existing AEAD object pipeline before upload. Never add a plaintext or legacy-XOR write path.
- **Error handling**: Use `anyhow::Result` for application code. Provide context with `.context()` or `.with_context()`. Avoid bare `.unwrap()` on fallible operations — use `.unwrap_or()` / `.unwrap_or_default()` only when the default is genuinely safe.
- **Skip control directories**: `.feanorfs` and `.git` must be hardcoded as skipped in directory scanning. Do not rely on `.gitignore` for these. Sync scope and ignore policy: [docs/sync-scope.md](docs/sync-scope.md).
- **Debounce filesystem events**: Filesystem saves are noisy. Debounce watcher events for 500ms using a channel.
- **Lazy placeholders**: Do not download remote file bytes during sync if `--lazy` is enabled. Write 0-byte placeholders instead.

## Testing

- **Unit tests** live in `#[cfg(test)] mod tests` blocks within each source file.
- **Integration tests** live in `tests/` directories next to each crate's `src/`.
- **Crypto tests**: Any change to `crypt_bytes`, `hash_bytes`, or `normalize_path` in `common/` must include or update tests verifying roundtrip and determinism.
- Run the full suite: `cargo test --workspace --all-features --locked`.

## Pull request process

1. **Open an issue first** for non-trivial changes (new features, breaking API changes, security-relevant modifications). Link the issue in your PR.
2. **Branch from `main`** and name your branch descriptively: `feat/add-aead-encryption`, `fix/sync-deadlock`, `docs/threat-model-update`.
3. **Keep PRs focused** — one logical change per PR. Split unrelated changes into separate PRs.
4. **Ensure CI passes locally before pushing**:
   ```bash
   cargo fmt --check
   cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
   cargo test --workspace --all-features --locked
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
5. **macOS package release** builds both architectures without secrets, combines them into a universal CLI and `FeanorFS.app`, signs both with Developer ID Application, signs the package with Developer ID Installer, notarizes and staples it, requires Gatekeeper acceptance, and uploads the package plus public verification evidence.
6. **Linux and Windows desktop release** builds verified native Linux `.deb`/`.rpm`/tar products on x86-64 and ARM64, then requires Azure Artifact Signing before publishing the checksummed Windows CLI/tray bundle.

### Maintainer setup

- Add repository secret **`RELEASE_PLZ_TOKEN`** (PAT with `contents:write` + `pull-requests:write`). The default `GITHUB_TOKEN` cannot trigger the cargo-dist workflow on tag push.
- Export a **Developer ID Application** certificate and private key as a password-protected PKCS#12 file. Store its one-line base64 encoding as **`APPLE_DEVELOPER_ID_P12_BASE64`** and its password as **`APPLE_DEVELOPER_ID_P12_PASSWORD`**.
- Export a **Developer ID Installer** certificate and private key as a separate password-protected PKCS#12 file. Store its one-line base64 encoding as **`APPLE_DEVELOPER_ID_INSTALLER_P12_BASE64`** and its password as **`APPLE_DEVELOPER_ID_INSTALLER_P12_PASSWORD`**.
- Create a team App Store Connect API key authorized for notarization. Store the one-line base64 encoding of its `.p8` file as **`APPLE_NOTARY_KEY_P8_BASE64`**, its key ID as **`APPLE_NOTARY_KEY_ID`**, and its issuer UUID as **`APPLE_NOTARY_ISSUER_ID`**.
- The macOS release job intentionally fails before upload when any Apple credential is absent or when Apple notarization/Gatekeeper verification fails. It has no unsigned fallback.
- Configure Azure federated credentials as `AZURE_CLIENT_ID`, `AZURE_TENANT_ID`, and `AZURE_SUBSCRIPTION_ID`, plus the `AZURE_ARTIFACT_SIGNING_ENDPOINT`, `AZURE_ARTIFACT_SIGNING_ACCOUNT_NAME`, and `AZURE_ARTIFACT_SIGNING_CERTIFICATE_PROFILE` repository variables. The Windows release job fails before signing or upload when any value is absent.

On macOS, create the one-line secret values without changing the source files:

```bash
base64 -i DeveloperIDApplication.p12 | tr -d '\n'
base64 -i DeveloperIDInstaller.p12 | tr -d '\n'
base64 -i AuthKey_XXXXXXXXXX.p8 | tr -d '\n'
```

### Local dist checks

```bash
cargo install cargo-dist --locked
dist plan          # preview artifacts for the current workspace version
dist build --artifacts=all   # build locally (requires cross targets installed)

# Validate the unsigned macOS package structure locally.
scripts/package-macos.sh assemble 0.4.0 target/release/feanorfs \
  target/release/feanorfs-tray /tmp/feanorfs-package
scripts/package-macos.sh build 0.4.0 /tmp/feanorfs-package \
  /tmp/FeanorFS-macOS-unsigned.pkg

# On Linux with nFPM, dpkg-deb, and rpm installed, build and verify both native packages.
scripts/package-linux.sh /tmp/feanorfs-linux-packages
```

Config lives in `dist-workspace.toml` and `release-plz.toml`. Regenerate CI after dist config changes: `dist init -y` or `dist generate`.
