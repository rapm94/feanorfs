# Release and product automation

## Purpose

Own platform installers, native package assembly, and executable product smoke
tests used by local verification and GitHub release workflows.

## Ownership

- `install.sh`, `install-macos.sh`, and `install.ps1` — public platform-aware installers.
- `package-macos.sh`, `package-linux.sh`, and `linux-package.nfpm.yaml` — exact native desktop package assembly and metadata verification.
- `smoke-macos-product.sh`, `smoke-macos-keychain.sh`, `smoke-linux-packages.sh`, and `smoke-windows-product.ps1` — installed-product lifecycle, signed-credential, clean-distribution, and Task Scheduler desktop proof.
- `smoke-relay-container.sh` — hardened opaque-relay image proof.
- `test-install-routing.sh` and `test-install-routing.ps1` — fail-closed installer routing tests.
- `smoke-test.sh` and `demo-agent-loop.sh` — source-product and agent SDK demonstrations.

## Local Contracts

- A listed desktop artifact must pass checksum, payload, architecture, signature/attestation, and platform-specific validation. Verification failure never falls back to a weaker product.
- Package scripts emit only the documented CLI, tray, launcher/icon, license, and README payloads. Keep native dependency metadata synchronized with the tray implementation.
- `smoke-linux-packages.sh` mounts only the exact package read-only into digest-pinned supported Debian/Fedora images, requires normal dependency resolution, creates an idle format-v3 encrypted one-shot workspace with private config and real snapshot objects, and proves the tray remains alive against that workspace under isolated Xvfb/D-Bus startup. Docker is the CI default; Podman is an equivalent local runtime.
- Product smoke tests use isolated homes/data, clean up services/processes on every exit, and never print or place credentials, recovery passphrases, pairing capabilities, invites, routes, or private keys in argv/environment/logs.
- macOS product smoke launches the packaged tray from an isolated unconfigured working directory with `--first-run` and samples its main thread; success requires the native `CFUserNotificationDisplayAlert` start-or-join choice, not merely a tray process that stayed alive.
- Product smokes read and validate the automatic hub's persisted `listen-port`; never assume a fresh hub must own 3030 or place the selected port in service arguments.
- Windows product smoke requires real Credential Manager references, verifies global/workspace JSON redaction plus background reload, and deletes only its exact random credential targets during cleanup; never leave CI credentials orphaned.
- Downloaded tools and release artifacts use HTTPS, fail on transport errors, and are checksum- or signature-pinned before execution.
- Verified desktop installers hand an interactive user directly to the tray after installation. Launch only the exact installed tray with the public, non-secret `--first-run` hint after all checksum, signature, architecture, and payload checks succeed; never launch a CLI-only fallback or failed artifact. When no workspace resolves, the tray presents its native start-or-join choice and delegates the result to existing menu actions. Root/headless sessions and `FEANORFS_NO_LAUNCH=1` must remain noninteractive and print the terminal setup path. A tray launch failure must not roll back an otherwise verified installation.

## Work Guidance

- Keep Unix scripts POSIX `sh` unless an existing macOS-only workflow requires Bash.
- Keep PowerShell installers non-interactive by default and fail closed on Authenticode or checksum errors.
- Prefer mainstream package managers and native service/UI facilities over custom installers or supervisors.

## Verification

- `shellcheck scripts/*.sh`
- `scripts/test-install-routing.sh`
- `scripts/test-install-routing.ps1` on Windows
- `scripts/smoke-linux-packages.sh FEANORFS_DEB FEANORFS_RPM` on a Docker/Podman host
- `scripts/smoke-windows-product.ps1 FEANORFS_BIN FEANORFS_TRAY_BIN` on Windows; add `-RequireAuthenticode` for publishable binaries
- Platform-specific macOS and relay smoke commands documented in `.github/AGENTS.md`

## Child DOX Index

No child directories require separate contracts.
