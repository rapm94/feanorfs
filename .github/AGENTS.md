# GitHub automation

## Purpose

Own CI, security analysis, fuzzing, the multi-agent eval gate, dependency updates, release orchestration, and contributor templates.

## Ownership

- `workflows/ci.yml` — fast Linux PR gates; on `main`: MSRV, cross-platform tests, upgrade/source smokes, `agent-eval` (scripted `eval/` scenarios; any FeanorFS conflict or lost edit fails), docs, builds, SDK, tray, relay container, dependency and workflow lint.
- `workflows/security.yml` — CodeQL, zizmor, scheduled `cargo deny`, and `fuzz` (each cargo-fuzz target in `/fuzz` for two minutes outside PRs; crashing inputs uploaded).
- `workflows/release-plz.yml` — post-CI version PRs and tags.
- `workflows/release.yml` — cargo-dist generated; configure via `dist-workspace.toml`.
- `workflows/tray-release.yml`, `workflows/desktop-release.yml` — reusable signed macOS and Linux/Windows product jobs called by the generated graph.
- `workflows/validate-release-assets.yml` — final exact-manifest and checksum gate before announcement.
- `workflows/relay-image.yml` — trusted-tag amd64/arm64 relay image with SBOM and provenance.
- `workflows/unsigned-desktop-release.yml` — manual, prerelease-only, conspicuously named unsigned previews; never a trusted fallback.
- `workflows/npm-release.yml` — manual dry-run Node package assembly; publication disabled.
- `dependabot.yml`, `CODEOWNERS`, `actionlint.yaml`.

## Local Contracts

- Pin repository-owned actions to commit SHAs with version comments; cargo-dist's commits live in `dist-workspace.toml` and change only by regeneration. Never patch `release.yml`; triage its zizmor findings at the generator.
- Permissions default to read-only or empty; grant write scopes per job. Signing, announcement, and registry publication jobs use the `prod` environment. Checkout uses `persist-credentials: false`. Shell interpolation goes through `env`.
- PRs require fast Linux gates (fmt, Clippy, tests, dependency policy, workflow lint) plus the exact-head Windows runner lifecycle suites; expensive gates run on `main` before release.
- Release-plz tags only an exact SHA whose CI and `Security success` aggregator passed on trusted `main` pushes, proven through exact run records. Release PRs update Cargo versions, npm metadata, and `common/release-product-state.txt` together; `git_only` history covers only `feanorfs-common`; candidates must pass `scripts/check-release-readiness.sh`.
- Reusable desktop jobs resolve cargo-dist's tag to the invocation SHA, prove main reachability plus successful CI and Security, bind artifact names to that SHA, stage only `artifacts-*` products, and never create or mutate a GitHub Release. The generated graph's publish gate verifies exact names and checksums before one announcement job publishes.
- GitHub Releases ship only the `feanorfs` CLI and tray products; cargo-dist must not generate installers that look like the desktop product.
- macOS signing runs only with `RELEASE_SIGNING_ENABLED=true` and Developer ID Application/Installer plus notarization credentials, decoded under `$RUNNER_TEMP` into a temporary keychain removed by `always()` cleanup; native build jobs never see Apple secrets. The CLI must pass `scripts/smoke-macos-keychain.sh` before packaging. There is no unsigned fallback on the trusted route.
- Linux products are staged only after architecture, dependency, payload, install-script, `ldd`, checksum, clean-container, and attestation checks. Windows products require Authenticode on both executables and the installer before staging.
- The relay image reuses `feanorfs serve --relay`, runs non-root with a read-only-capable root, builds natively per architecture with the pinned Bookworm toolchain, passes a blocking Trivy scan, and publishes SBOM/provenance; never add a second relay implementation or an open-hub default.
- npm publication stays manual and dry-run until an explicit product decision; the dormant job keeps `id-token: write` and integrity checks.
- The fuzz job installs a pinned `cargo-fuzz` on nightly and runs the same properties the stable `common/tests/parser_fuzz.rs` checks every test run. The eval job builds the CLI and runs every `eval/scenarios/*.json` with `shell: bash` (pipefail) so a failing run fails the step.

## Work Guidance

- Add timeouts and concurrency controls to every new workflow; prefer GitHub-native and mainstream tools over custom scripts.

## Verification

- `actionlint`; `zizmor --persona=pedantic --min-severity=medium` over repository-owned workflows (only generator-owned findings in `release.yml` remain, excluded until regeneration).
- `cargo deny check`; `dist generate --check`; `dist plan`.
- CI `workflow-lint` runs `scripts/test-release-workflow-policy.sh` and `scripts/test-release-evidence.sh`.
- Product smokes and their coverage are owned by [scripts/AGENTS.md](../scripts/AGENTS.md).

## Child DOX Index

No child directories require separate contracts.
