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
- `workflows/tray-release.yml` — cargo-dist reusable job for universal macOS app/package/DMG signing, notarization, stapling, attestation, and artifact staging (`RELEASE_SIGNING_ENABLED=true` only).
- `workflows/desktop-release.yml` — cargo-dist reusable job for Linux x86-64/ARM64 `.deb`/`.rpm`/`.pkg.tar.zst`/tar products and optional Azure Authenticode-signed Windows x86-64 installer EXE and bundle. It runs only under `workflow_call` from the generated graph, verifies the exact tag/commit with a trusted-CI gate, and stages products as `artifacts-*` uploads; it never polls, creates, or mutates a GitHub Release (`gh release` is absent).
- `workflows/validate-release-assets.yml` — final pre-announcement gate for the exact 30-asset unsigned-signing-state or 45-asset signing-enabled manifest and every checksum.
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
  job that requires them. Repository-owned signing, final announcement, and registry
  publication jobs declare the `prod` environment; TODO F4 still owns required
  reviewers, deployment restrictions, and administrator-bypass controls.
- Checkout steps set `persist-credentials: false`.
- Fast core jobs may exclude native tray dependencies; main-branch desktop jobs build and test the tray natively on macOS, Linux, and Windows.
- GitHub Releases expose only the cross-platform `feanorfs` CLI and optional
  macOS/Linux/Windows tray products. The legacy server binary remains source-only because
  `feanorfs serve` is the supported hub entrypoint.
- Trusted tags publish the same `feanorfs serve --relay` implementation as a non-root, read-only-capable Linux OCI image for amd64/arm64. It generates its bearer token in a persistent volume, binds HTTP only behind an operator-owned TLS reverse proxy, passes a blocking Trivy scan for fixed high/critical runtime vulnerabilities, and publishes SBOM/provenance attestations; never add a second relay implementation or an open-hub default.
- The macOS signed-product job requires `RELEASE_SIGNING_ENABLED=true`, Developer ID Application and Developer ID Installer certificates, and an App Store Connect notarization key. When enabled, it signs the universal CLI and `FeanorFS.app` with hardened runtime and timestamping, signs/notarizes/staples the Installer package, wraps that exact package in a separately notarized/stapled DMG, requires Gatekeeper acceptance, and stages verification evidence with the products. When disabled, the privileged job is skipped and no macOS installer is claimed; there is no unsigned fallback on the trusted route.
- Before packaging, the Developer ID CLI must pass `scripts/smoke-macos-keychain.sh`: auto-detected Keychain storage, redacted config, live credential reload, cleanup, and a public smoke record whose SHA-256 matches the packaged CLI. CI separately requires unsigned development builds to fail this gate.
- Native arm64/x86_64 jobs receive no Apple secrets. When signed publication is enabled, one privileged job combines them with `lipo`, builds `FeanorFS-macOS.pkg` and its exact DMG container, and stages only the signed/notarized/stapled/checksummed/attested products plus evidence and the verifying convenience installer.
- Linux release jobs stage exact native `.deb`/`.rpm`/`.pkg.tar.zst` packages plus a four-file tar fallback only after architecture, dependency metadata, payload, install-script, `ldd`, explicit absence of the unused distro-variant `libxdo` ABI, SHA-256, clean-container, and GitHub-attestation checks. Main CI and the trusted tag-triggered Windows release run the Task Scheduler product smoke in explicit headless-runner mode: the single `FeanorFS\Agent` supervisor task plus the `InteractiveToken` tray task must run (no per-hub/per-workspace tasks may exist), and the tray must settle ready or running without depending on a hosted runner's desktop session. The separate manual preview workflow retains the complete interactive tray smoke. Windows native builds also compile/install/uninstall the Inno Setup product before becoming artifacts; the privileged job repeats those smokes after verifying Azure Authenticode on both executables and the installer EXE, then stages only the exact checksummed/attested products. The trusted route has no unsigned fallback; manual preview artifacts remain explicitly named and prerelease-only.
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
- Reusable desktop workflows consume cargo-dist's exact plan, resolve its
  canonical stable tag to the invocation SHA before building, prove main
  reachability plus successful exact-SHA CI and `Security success`, check out
  only that SHA, and bind every intermediate artifact name to it. They never
  wait for or mutate a GitHub Release. Final products alone use the
  `artifacts-*` staging namespace; cargo-dist's publish gate verifies the exact
  combined names and checksums before the one announcement job creates the
  public release. Relay and unsigned-preview trust gates retain their separate
  exact-SHA checks; unsigned uploads revalidate the original prerelease ID, tag,
  and target immediately before mutation.
- Apple Application/Installer identities and notarization credentials are scoped to the privileged package steps, decoded only under `$RUNNER_TEMP`, imported into a temporary keychain, and removed by an `always()` cleanup step. Never expose them to native build steps or persist them as artifacts.
- `release.yml` is cargo-dist generated. Configure immutable action commits,
  custom graph jobs, attestation filters, and other settings in
  `dist-workspace.toml`, then
  regenerate with its documented cargo-dist version; never patch the workflow
  directly. The reusable platform jobs join repository-owned CI/Security API
  gates before announcement; restricted creation and immutable updates/deletions
  for every matching tag remain the residual control in TODO F4.
- zizmor findings inside generated `release.yml` are triaged, not suppressed
  in place: tag-ref template expansions run only on maintainer tag pushes and
  never on untrusted PR content (PR runs use plan mode), matrix run/args
  expansions come from static `dist-workspace.toml` configuration, and
  `secrets: inherit` on custom desktop/tray calls passes only repository
  secrets to repository-owned reusable workflows. Addressing the residual
  `unpinned-images` container-matrix diagnostic requires a cargo-dist
  regeneration decision; inputs remain bounded until then.
- cargo-dist generates attested CLI archives only and publishes them together with the exact validated custom product set; it must not generate shell/PowerShell installers that look like the tray-inclusive desktop product. Public installer routing belongs to `scripts/install.sh`, the signed macOS package/DMG, verified Linux native packages/full bundle, and the Authenticode Windows setup EXE.
- Relay image publication builds amd64 and arm64 `feanorfs` binaries inside the pinned Bookworm Rust environment on matching native runners, assembles each through `Dockerfile.relay-binary` with the same Bookworm runtime ABI, attests each architecture, records each Buildx digest in an immutable workflow artifact, then merges those digests without resolving architecture tags. Never restore QEMU workspace compilation or copy a newer host-glibc binary into the runtime image.

## Work Guidance

- Keep shell interpolation in `env`; do not expand event values directly into
  `run` scripts.
- Add timeouts and concurrency controls to every new workflow.
- Prefer GitHub-native security features and established ecosystem tools over
  custom scripts.

## Verification

- `actionlint`
- `zizmor --persona=pedantic --min-severity=medium` over repository-owned workflows and `dependabot.yml`. The only unsuppressed findings live in the cargo-dist-generated `release.yml` and are pinned to their generator source: `excessive-permissions` (`contents: write` at workflow level, emitted by cargo-dist's generated release job permissions), `template-injection` (the `dist plan/host` tag interpolation, generator-owned and expression-safe by construction since the expansion targets cargo-dist's own shell), and `unpinned-images` (the `matrix.container.image` indirection). They are excluded from the security workflow's pedantic run until a cargo-dist regeneration changes them; authored workflows carry zero unsuppressed findings.
- `cargo deny check`
- `dist generate --check` and `dist plan`
- CI's `workflow-lint` job requires actionlint, correct hosted-only Windows
  `-HeadlessRunner` placement, `scripts/test-release-workflow-policy.sh`, and
  the offline `scripts/test-release-evidence.sh` contract. The policy test
  proves reusable platform jobs cannot mutate a release and the generated
  graph waits for the exact asset validator before announcement.
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
