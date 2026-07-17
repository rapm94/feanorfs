# GitHub automation

## Purpose

Own CI, security analysis, dependency updates, release orchestration, and
contributor templates.

## Ownership

- `workflows/ci.yml` — cross-platform Rust, SDK, tray, dependency, and workflow gates.
- `workflows/security.yml` — CodeQL, zizmor, and scheduled dependency audits.
- `workflows/release-plz.yml` — post-CI version PR and tag automation.
- `workflows/npm-release.yml` — manual dry-run native addon matrix and deterministic six-package assembly; automatic npm publication is disabled while releases ship only the app.
- `workflows/release.yml` — generated cargo-dist release workflow.
- `workflows/tray-release.yml` — post-tag universal macOS app/package signing, notarization, stapling, attestation, and upload (waits for cargo-dist).
- `workflows/desktop-release.yml` — post-tag Linux x86-64/ARM64 `.deb`/`.rpm`/tar desktop products and Azure Authenticode-signed Windows x86-64 desktop bundle (waits for cargo-dist).
- `workflows/relay-image.yml` — trusted-tag multi-architecture `ghcr.io/rapm94/feanorfs-relay` publication with SBOM and build provenance.
- `dependabot.yml` — Cargo, npm, Docker base-image, and GitHub Actions updates.
- `actionlint.yaml` — narrow suppressions for generated workflow shell.

## Local Contracts

- Repository-owned action references are immutable commit SHAs with version
  comments; Dependabot maintains them.
- Default permissions are read-only or empty. Grant write scopes only at the
  job that requires them.
- Checkout steps set `persist-credentials: false`.
- Fast core jobs may exclude native tray dependencies; main-branch desktop jobs build and test the tray natively on macOS, Linux, and Windows.
- GitHub Releases expose only the cross-platform `feanorfs` CLI and optional
  macOS/Linux/Windows tray products. The legacy server binary remains source-only because
  `feanorfs serve` is the supported hub entrypoint.
- Trusted tags publish the same `feanorfs serve --relay` implementation as a non-root, read-only-capable Linux OCI image for amd64/arm64. It generates its bearer token in a persistent volume, binds HTTP only behind an operator-owned TLS reverse proxy, passes a blocking Trivy scan for fixed high/critical runtime vulnerabilities, and publishes SBOM/provenance attestations; never add a second relay implementation or an open-hub default.
- The macOS package release requires Developer ID Application and Developer ID Installer certificates plus an App Store Connect notarization key. It signs the universal CLI and `FeanorFS.app` with hardened runtime and timestamping, signs the Installer package, notarizes and staples it, requires Gatekeeper package acceptance, and publishes verification evidence before upload. There is no unsigned fallback.
- Before packaging, the Developer ID CLI must pass `scripts/smoke-macos-keychain.sh`: auto-detected Keychain storage, redacted config, live credential reload, cleanup, and a public smoke record whose SHA-256 matches the packaged CLI. CI separately requires unsigned development builds to fail this gate.
- Native arm64/x86_64 jobs receive no Apple secrets. One privileged job combines them with `lipo`, builds `FeanorFS-macOS.pkg`, and uploads only the signed, notarized, stapled, checksummed, and attested universal package plus evidence and the verifying convenience installer.
- Linux release jobs publish exact native `.deb`/`.rpm` packages plus a four-file tar fallback only after architecture, dependency metadata, payload, install-script, `ldd`, SHA-256, and GitHub-attestation checks. Windows native builds must pass the complete Task Scheduler product smoke before becoming artifacts; the privileged job repeats that smoke after verifying Azure Authenticode on both executables, then publishes only the exact checksummed/attested bundle and installer. There is no unsigned Windows fallback.
- Pull requests require the fast Linux gates: format, Clippy, tests,
  dependency policy, and workflow lint. MSRV, macOS/Windows tests, docs,
  release builds, SDK, tray, and CodeQL run on `main` before release.
- Release-plz may tag only after successful CI on a trusted `main` push.
- Release PR automation updates Cargo versions first, then runs
  `assemble-metadata` on the release branch so npm facade, lockfile, and five
  native package manifests use the same version before merge.
- Release PR automation limits `git_only` history to `feanorfs-common`, wraps
  cargo package commands with `--no-verify`, and extracts generated archives
  because immutable historical tags contain unpublished internal crates.
  Pre-1.0 feature commits increment the app minor version. Main-branch CI
  remains the build gate.
- npm release automation is manual-dispatch and dry-run only. App release tags must not publish Node packages. Re-enable a tag trigger only after an explicit product decision and npm bootstrap authentication are in place.
- The dormant npm publish job retains `id-token: write`, exact-integrity checks, and `NPM_TOKEN` bootstrap support so publication can be reactivated without weakening provenance controls.
- Privileged `workflow_run` jobs validate source repository, event, branch/tag,
  conclusion, and exact commit before using secrets or uploading artifacts.
  Tray release triggers on `v*` tag push and polls until cargo-dist publishes
  the GitHub Release before building. Manual retries accept an existing release
  tag, verify the release and tag resolve to the same commit, then check out that
  immutable tag before uploading tray artifacts.
- Apple Application/Installer identities and notarization credentials are scoped to the privileged package steps, decoded only under `$RUNNER_TEMP`, imported into a temporary keychain, and removed by an `always()` cleanup step. Never expose them to native build steps or persist them as artifacts.
- `release.yml` is cargo-dist generated. Configure `dist-workspace.toml` and
  regenerate; never patch the workflow directly.

## Work Guidance

- Keep shell interpolation in `env`; do not expand event values directly into
  `run` scripts.
- Add timeouts and concurrency controls to every new workflow.
- Prefer GitHub-native security features and established ecosystem tools over
  custom scripts.

## Verification

- `actionlint`
- `zizmor --persona=pedantic --min-severity=medium` over repository-owned workflows and `dependabot.yml`; exclude cargo-dist-generated `release.yml` as the security workflow does.
- `cargo deny check`
- `dist plan`
- The macOS `tray` CI job assembles and expands an unsigned package, compares its payload binaries byte-for-byte, and verifies the app metadata and native architecture.
- `scripts/smoke-macos-product.sh` runs the expanded package through first-machine `start`, complete JSON lifecycle diagnostics, launchd argv/permission checks, tray startup, TLS rejection, MCP, pairing readiness, and reversible stop/resume while preserving the hub and encrypted workspace setup without printing secrets. Its first-run gate launches from an isolated unconfigured directory with `--first-run` and requires a process sample to reach native `CFUserNotificationDisplayAlert`; process liveness alone does not prove the start-or-join choice appeared.
- `scripts/test-install-routing.sh` proves Unix fallback, fail-closed macOS/Linux product routing, headless opt-out, and verified Linux tray-first launch with the exact `--first-run` hint. `scripts/smoke-linux-packages.sh` installs the exact native packages into digest-pinned Debian 13 and Fedora 44 containers, creates and verifies an idle format-v3 encrypted one-shot workspace with private config and real snapshot objects, and requires the tray to remain alive against it under clean Xvfb/D-Bus startup. `scripts/test-install-routing.ps1` proves Windows legacy fallback, missing/bad-checksum rejection, verified desktop installation, exact first-run launch arguments, and no-launch opt-out; the installer and release job require valid Authenticode before installation/publication.
- `scripts/smoke-windows-product.ps1` runs the native CLI/tray through first-machine hosting, redacted Credential Manager storage/reload/cleanup, interactive tray plus background hub/workspace Task Scheduler state and secret-free action checks, TLS-backed doctor/MCP status, and reversible stop/resume. Main/release builds run it unsigned; the privileged publish job reruns it only after both binaries have valid Authenticode signatures.
- `scripts/smoke-macos-keychain.sh` is release-only for success: it requires Developer ID Application authority and publishes no credential value or Keychain identifier.
- `scripts/smoke-relay-container.sh` builds the release image and proves non-root execution, read-only root filesystem compatibility, protected generated authentication, secret-free logs/argv, authenticated health behavior, enabled relay routes, and credential persistence across restart. Main CI retains that exact image only long enough for Trivy to block fixed high/critical runtime vulnerabilities.

## Child DOX Index

No child directories require separate contracts.
