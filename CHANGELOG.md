# Changelog

All notable changes to FeanorFS are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added
- **Agent workspaces** — `feanorfs agent spawn|commit|list|clean|run` for copy-on-write isolation and three-way concurrent-edit detection.
- **Library API** — `feanorfs_client` crate exposes `sync`, `push`, `pull`, `hydrate`, `cat`, and agent helpers for programmatic use.
- **JSON output** — global `--json` flag on status-returning commands.
- **Catch-up summary** — `feanorfs summary [--summarize]` diffs against the previous session marker.
- **Predictive hydration** — co-occurrence prefetch after `hydrate`/`cat` (local-only access log).
- **Integration tests** — in-process Axum harness in `client/tests/sync_engine.rs`.

### Changed
- Server routes extracted to `feanorfs_server` library (`server/src/app.rs`).
- Client binary now links the library crate (no duplicate module compilation).
- Upload path mirrors `server_mtime` into local cache to prevent re-download churn.

### Fixed
- Filesystem watcher observes workspace `current_dir`, not `"."`.
- Blob download uses single-read TOCTOU-safe path.

## [0.1.0] - 2026-06-23

### Added
- Initial release of FeanorFS, a developer-focused zero-knowledge filesystem sync tool.
- **Client CLI** (`feanorfs`) with subcommands: `init`, `status`, `push`, `pull`, `sync`, `hydrate`, `cat`, `watch`.
- **Server** (`feanorfs-server`) — Axum-based blob storage server with SQLite metadata coordination.
  - `POST /api/sync/diff` — metadata delta negotiation.
  - `POST /api/upload` — encrypted blob upload with hash verification.
  - `GET /api/download/:hash` — encrypted blob download.
- **End-to-end encryption** via Blake3 XOF symmetric XOR keystream, keyed by `(password, relative_path)`.
- **Content-addressed storage** — blobs stored by Blake3 hash, enabling deduplication and upload integrity verification.
- **Local cache** — SQLite-backed `local_cache.db` mapping `(path, mtime, size)` to `(plaintext_hash, encrypted_hash)` to avoid redundant re-hashing.
- **Lazy hydration** — `pull --lazy` fetches metadata only and creates 0-byte placeholders; `hydrate` and `cat` download and decrypt on demand.
- **Real-time watch** — `watch` subcommand monitors filesystem changes with 500ms debounce and auto-syncs.
- **Cross-platform path normalization** — all tracked paths use forward slashes.
- **`.gitignore` integration** — files matching ignore patterns are excluded from sync; `.feanorfs/` and `.git/` are always skipped.

### Security
- Zero-knowledge server: only encrypted hashes and ciphertext blobs are stored server-side.
- See `SECURITY.md` for the full threat model and known limitations.

[Unreleased]: https://github.com/rapm94/feanorfs/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/rapm94/feanorfs/releases/tag/v0.1.0
