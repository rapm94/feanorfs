# GitHub automation

## Purpose

Own CI, security analysis, dependency updates, release orchestration, and
contributor templates.

## Ownership

- `workflows/ci.yml` — cross-platform Rust, SDK, tray, dependency, and workflow gates.
- `workflows/security.yml` — CodeQL, zizmor, and scheduled dependency audits.
- `workflows/release-plz.yml` — post-CI version PR and tag automation.
- `workflows/npm-release.yml` — trusted-tag native addon matrix, deterministic six-package assembly, and resumable npm publication with OIDC provenance.
- `workflows/release.yml` — generated cargo-dist release workflow.
- `workflows/tray-release.yml` — post-tag macOS tray artifacts (waits for cargo-dist).
- `dependabot.yml` — Cargo, npm, and GitHub Actions updates.
- `actionlint.yaml` — narrow suppressions for generated workflow shell.

## Local Contracts

- Repository-owned action references are immutable commit SHAs with version
  comments; Dependabot maintains them.
- Default permissions are read-only or empty. Grant write scopes only at the
  job that requires them.
- Checkout steps set `persist-credentials: false`.
- Core Linux/Windows jobs exclude `feanorfs-tray`; tray checks and releases run
  on macOS.
- GitHub Releases expose only the cross-platform `feanorfs` CLI and optional
  macOS tray. The legacy server binary remains source-only because
  `feanorfs serve` is the supported hub entrypoint.
- Pull requests require the fast Linux gates: format, Clippy, tests,
  dependency policy, and workflow lint. MSRV, macOS/Windows tests, docs,
  release builds, SDK, tray, and CodeQL run on `main` before release.
- Release-plz may tag only after successful CI on a trusted `main` push.
- Release PR automation updates Cargo versions first, then runs
  `assemble-metadata` on the release branch so npm facade, lockfile, and five
  native package manifests use the same version before merge.
- npm publication accepts only a version-matching `v*` tag whose peeled commit is reachable from `main` and has successful trusted push CI. Manual dispatch is dry-run only. Platform packages publish and verify before the facade; exact registry integrity permits idempotent reruns, while mismatches stop publication.
- Only the npm publish job receives `id-token: write`. Configure npm trusted publishers for `npm-release.yml` and set `NPM_TRUSTED_PUBLISHING=true`; `NPM_TOKEN` remains a bootstrap fallback for packages that do not yet exist.
- Privileged `workflow_run` jobs validate source repository, event, branch/tag,
  conclusion, and exact commit before using secrets or uploading artifacts.
  Tray release triggers on `v*` tag push and polls until cargo-dist publishes
  the GitHub Release before building.
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

## Child DOX Index

No child directories require separate contracts.
