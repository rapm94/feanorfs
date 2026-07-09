# GitHub automation

## Purpose

Own CI, security analysis, dependency updates, release orchestration, and
contributor templates.

## Ownership

- `workflows/ci.yml` — cross-platform Rust, SDK, tray, dependency, and workflow gates.
- `workflows/security.yml` — CodeQL, zizmor, and scheduled dependency audits.
- `workflows/release-plz.yml` — post-CI version PR and tag automation.
- `workflows/release.yml` — generated cargo-dist release workflow.
- `workflows/tray-release.yml` — post-release macOS tray artifacts.
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
- Pull requests require the fast Linux gates: format, Clippy, tests,
  dependency policy, and workflow lint. MSRV, macOS/Windows tests, docs,
  release builds, SDK, tray, and CodeQL run on `main` before release.
- Release-plz may tag only after successful CI on a trusted `main` push.
- Privileged `workflow_run` jobs validate source repository, event, branch/tag,
  conclusion, and exact commit before using secrets or uploading artifacts.
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
- `zizmor --persona=pedantic --min-severity=medium .github`
- `cargo deny check`
- `dist plan`

## Child DOX Index

No child directories require separate contracts.
