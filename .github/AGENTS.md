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
- `workflows/tray-release.yml` — post-tag universal macOS app/package/DMG signing, notarization, stapling, attestation, and upload (waits for cargo-dist).
- `workflows/desktop-release.yml` — post-tag Linux x86-64/ARM64 `.deb`/`.rpm`/`.pkg.tar.zst`/tar desktop products and Azure Authenticode-signed Windows x86-64 installer EXE and bundle (waits for cargo-dist).
- `workflows/relay-image.yml` — trusted-tag multi-architecture `ghcr.io/rapm94/feanorfs-relay` publication with SBOM and build provenance.
- `workflows/unsigned-desktop-release.yml` — manual, prerelease-only, conspicuously named unsigned desktop preview artifacts; never a trusted installer fallback.
- `dependabot.yml` — Cargo, npm, Docker base-image, and GitHub Actions updates.
- `CODEOWNERS` — maps founder review ownership for release trust and distribution surfaces; required Code Owner review remains pending in TODO F4.
- `actionlint.yaml` — narrow suppressions for generated workflow shell.

## Local Contracts

- Repository-owned action references are immutable commit SHAs with version
  comments; Dependabot maintains them. Generated cargo-dist action commits live
  in `dist-workspace.toml` and require regeneration rather than direct edits.
- Default permissions are read-only or empty. Grant write scopes only at the
  job that requires them. Repository-owned signing, release-upload, and registry
  publication jobs declare the `prod` environment; TODO F4 still owns required
  reviewers, deployment restrictions, and administrator-bypass controls.
- Checkout steps set `persist-credentials: false`.
- Fast core jobs may exclude native tray dependencies; main-branch desktop jobs build and test the tray natively on macOS, Linux, and Windows.
- GitHub Releases expose only the cross-platform `feanorfs` CLI and optional
  macOS/Linux/Windows tray products. The legacy server binary remains source-only because
  `feanorfs serve` is the supported hub entrypoint.
- Trusted tags publish the same `feanorfs serve --relay` implementation as a non-root, read-only-capable Linux OCI image for amd64/arm64. It generates its bearer token in a persistent volume, binds HTTP only behind an operator-owned TLS reverse proxy, passes a blocking Trivy scan for fixed high/critical runtime vulnerabilities, and publishes SBOM/provenance attestations; never add a second relay implementation or an open-hub default.
- The macOS release requires Developer ID Application and Developer ID Installer certificates plus an App Store Connect notarization key. It signs the universal CLI and `FeanorFS.app` with hardened runtime and timestamping, signs/notarizes/staples the Installer package, wraps that exact package in a separately notarized/stapled DMG, requires Gatekeeper acceptance, and publishes verification evidence before upload. There is no unsigned fallback.
- Before packaging, the Developer ID CLI must pass `scripts/smoke-macos-keychain.sh`: auto-detected Keychain storage, redacted config, live credential reload, cleanup, and a public smoke record whose SHA-256 matches the packaged CLI. CI separately requires unsigned development builds to fail this gate.
- Native arm64/x86_64 jobs receive no Apple secrets. One privileged job combines them with `lipo`, builds `FeanorFS-macOS.pkg` and its exact DMG container, and uploads only the signed/notarized/stapled/checksummed/attested products plus evidence and the verifying convenience installer.
- Linux release jobs publish exact native `.deb`/`.rpm`/`.pkg.tar.zst` packages plus a four-file tar fallback only after architecture, dependency metadata, payload, install-script, `ldd`, explicit absence of the unused distro-variant `libxdo` ABI, SHA-256, clean-container, and GitHub-attestation checks. Main CI and the trusted tag-triggered Windows release run the Task Scheduler product smoke in explicit headless-runner mode: the supervisor, hub, and workspace watcher must run, while the `InteractiveToken` tray task must be correctly registered and settle ready or running without depending on a hosted runner's desktop session. The separate manual preview workflow retains the complete interactive tray smoke. Windows native builds also compile/install/uninstall the Inno Setup product before becoming artifacts; the privileged job repeats those smokes after verifying Azure Authenticode on both executables and the installer EXE, then publishes only the exact checksummed/attested products. The trusted route has no unsigned fallback; manual preview artifacts remain explicitly named and prerelease-only.
- Pull requests require the fast Linux gates (format, Clippy, tests, dependency
  policy, and workflow lint) plus an exact-head native Windows run of the
  agent-core/client runner lifecycle suites. MSRV, complete macOS/Windows
  workspace tests, docs, release builds, SDK, tray, and CodeQL run on `main`
  before release.
- Release-plz may tag only an exact SHA with both successful CI and the named
  `Security success` aggregator on trusted `main` pushes. Automatic and manual
  recovery paths prove the repository, event, branch, SHA, and aggregator job
  through the Actions API before either release-plz command runs. Release gates
  select completed success from exact run records instead of relying on the
  Actions API's eventually consistent `status=completed` index.
- Release PR automation updates Cargo versions first, then runs
  `assemble-metadata` on the release branch so the npm facade, lockfile,
  generated native-loader checks, and five native package manifests use the
  same version before merge.
- Release PR automation deterministically updates `common/release-product-state.txt` and creates a conventional local carrier commit only when tracked client, server, agent-core, tray, installer, workflow, or relay-image files changed. This makes product-only changes select the release package without manual version edits. A merged versioned release candidate must pass `scripts/check-release-readiness.sh` before `release-plz release` may tag it.
- Release PR automation limits `git_only` history to `feanorfs-common`. Its
  checked-in Cargo adapter rewrites release-plz's historical `--workspace`
  package command to that one path-independent crate, requires the historical
  lockfile, and extracts its generated archive. This keeps immutable tags
  independent of unrelated unpublished workspace crates and fails closed if
  the git-only crate ever gains a path dependency.
  Pre-1.0 feature commits increment the app minor version. Exact-SHA main-branch
  CI plus the Security aggregator remain the build gate.
- npm release automation is manual-dispatch and dry-run only. A trusted actor
  may dispatch a repository branch so the exact reviewed commit receives the
  five-native-addon, six-tarball assembly and resumable-publisher proof before
  merge. App release tags must not publish Node packages. Re-enable a tag
  trigger only after an explicit product decision and npm bootstrap
  authentication are in place.
- The dormant npm publish job retains `id-token: write`, exact-integrity checks, and `NPM_TOKEN` bootstrap support so publication can be reactivated without weakening provenance controls.
- Privileged desktop workflows resolve a canonical stable tag to one immutable
  SHA before building, require the release target and successful cargo-dist run
  to use that SHA, prove main reachability plus successful exact-SHA CI and
  `Security success`, check out only the SHA, and bind intermediate artifact
  names to it. Tag-triggered runs require the exact immutable tag ref and SHA.
  Manual recovery runs require an exact CI/Security-green `main` invocation SHA
  descended from the release commit, then independently resolve, validate, and
  check out the immutable requested tag SHA; they never equate the recovery
  workflow SHA with an older release SHA. Relay and unsigned-preview trust gates
  require the same CI/Security proof; unsigned uploads revalidate the original
  prerelease ID, tag, and target immediately before mutation.
- Apple Application/Installer identities and notarization credentials are scoped to the privileged package steps, decoded only under `$RUNNER_TEMP`, imported into a temporary keychain, and removed by an `always()` cleanup step. Never expose them to native build steps or persist them as artifacts.
- `release.yml` is cargo-dist generated. Configure immutable action commits,
  attestation filters, and other settings in `dist-workspace.toml`, then
  regenerate with its documented cargo-dist version; never patch the workflow
  directly. Its broad generated SemVer-like tag trigger cannot join
  repository-owned CI/Security API gates, so restricted creation and immutable
  updates/deletions for every matching tag remain the residual control in TODO F4.
- cargo-dist publishes attested CLI archives only; it must not generate shell/PowerShell installers that look like the tray-inclusive desktop product. Public installer routing belongs to `scripts/install.sh`, the signed macOS package/DMG, verified Linux native packages/full bundle, and the Authenticode Windows setup EXE.
- Relay image publication builds amd64 and arm64 `feanorfs` binaries inside the pinned Bookworm Rust environment on matching native runners, assembles each through `Dockerfile.relay-binary` with the same Bookworm runtime ABI, attests each architecture, records each Buildx digest in an immutable workflow artifact, then merges those digests without resolving architecture tags. Never restore QEMU workspace compilation or copy a newer host-glibc binary into the runtime image.

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
- The macOS `tray` CI job assembles and expands an unsigned package, compares its payload binaries byte-for-byte, verifies the postinstall/metadata/native architecture, and mounts an unsigned DMG to compare its inner package byte-for-byte.
- `scripts/smoke-macos-product.sh` runs the expanded package through first-machine `start`, complete JSON lifecycle diagnostics, launchd argv/permission checks, tray startup, TLS rejection, MCP, pairing readiness, and reversible stop/resume while preserving the hub and encrypted workspace setup without printing secrets. Its first-run gate launches from an isolated unconfigured directory with `--first-run` and requires a process sample to reach native `CFUserNotificationDisplayAlert`; process liveness alone does not prove the start-or-join choice appeared.
- `scripts/test-install-routing.sh` proves Unix fallback, fail-closed macOS/Linux product routing, headless opt-out, and verified Linux tray-first launch with the exact `--first-run` hint. `scripts/smoke-linux-packages.sh` installs the exact native packages into digest-pinned Debian 13 and Fedora 44 containers on both architectures and official Arch on x86-64, creates an idle format-v3 encrypted workspace, and keeps the tray alive under Xvfb/D-Bus. Official Arch has no ARM64 container, so that matrix leg requires exact Arch metadata/payload checks plus native ARM64 Debian/Fedora execution. `scripts/test-install-routing.ps1` proves Windows setup-EXE checksum/signature routing and rejection of legacy remote execution; `scripts/smoke-windows-installer.ps1` proves exact payload, PATH, uninstall, and signatures. Publication requires valid Authenticode.
- `scripts/smoke-windows-product.ps1` runs the native CLI/tray through first-machine hosting, redacted Credential Manager storage/reload/cleanup, tray registration plus background hub/workspace Task Scheduler state and secret-free action checks, TLS-backed doctor/MCP status, and reversible stop/resume. Main CI and the trusted tag release pass `-HeadlessRunner` so a missing hosted-runner desktop cannot change the gate; the manual preview workflow retains the default interactive tray runtime proof, and the privileged publish job also adds `-RequireAuthenticode` after both binaries have valid signatures.
- `scripts/smoke-macos-keychain.sh` is release-only for success: it requires Developer ID Application authority and publishes no credential value or Keychain identifier.
- `scripts/smoke-test.sh` is wired into CI as the non-PR `source-smoke` job: it builds the release
  binary, then runs fmt/clippy/test/doc and a live two-client E2E (init, push, lazy pull, hydrate,
  update pull, agent spawn/run, summary, status) against a real hub. It is the regression guard for
  the format-v3 update-over-existing-file download path.
- `scripts/smoke-relay-container.sh` builds the release image and proves non-root execution, read-only root filesystem compatibility, protected generated authentication, secret-free logs/argv, authenticated health behavior, enabled relay routes, and credential persistence across restart. Main CI retains that exact image only long enough for Trivy to block fixed high/critical runtime vulnerabilities.
- The main-branch `upgrade-smoke` job is partial mixed-version evidence: it builds the previous release tag plus the current CLI, then runs `scripts/smoke-upgrade.sh` to prove same-path CLI replacement and preservation of workspace identity, encrypted configuration, files, and exact snapshot history. It does not prove a managed worker restart or replace the native installed-product hub/worker/tray/login-job matrix required by TODO AI-1.

## Child DOX Index

No child directories require separate contracts.
