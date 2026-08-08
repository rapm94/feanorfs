# FeanorFS product TODO

This is the only authoritative open-work list. Shipped work belongs in
`CHANGELOG.md`; remove completed or superseded items instead of retaining a
backlog history.

## Founder tasks

These require account ownership, infrastructure ownership, or representative
human acceptance. Never commit credentials or paste them into issues, logs, or
chat.

### F1. Provide trusted desktop-signing access

- [ ] Add Developer ID Application/Installer and App Store Connect notarization
  credentials to GitHub Actions for the universal macOS `.dmg`/`.pkg`.
- [ ] Configure Azure Artifact Signing through GitHub OIDC for the Windows CLI,
  tray, and installer `.exe`, and provide the stable public signer identities
  that release verification must enforce.

Done when one immutable tag publishes notarized macOS and Authenticode Windows
products whose evidence matches the approved signer identities. Unsigned
releases must remain clearly labeled as development builds.

### F2. Provide the production off-LAN relay

- [ ] Choose the production relay domain, hosting account, budget, retention
  policy, abuse controls, and privacy terms.
- [ ] Add its deployment credentials through GitHub/environment secret stores;
  never place them in the repository or client builds.

Done when a normal user can pair away from home without configuring a relay URL,
while the relay remains unable to see workspace IDs, bearer tokens, paths, or
encrypted payload contents.

### F3. Accept the ordinary-user desktop experience

- [ ] Exercise install, Start/Join/Not Now, folder switching, login persistence,
  upgrade, recovery, and uninstall on supported macOS, Windows, Debian/Ubuntu,
  Fedora, and Arch/CachyOS desktop sessions.
- [ ] Record only OS/version and secret-free acceptance or a reproducible defect.

Signed macOS and Windows acceptance is blocked on F1. Off-LAN default pairing
acceptance is blocked on F2.

### F4. Lock the GitHub release control plane

- [ ] Add rules for every tag ref (not only `v*`) that restrict creation to
  the approved release automation identity, prevent update/deletion, and apply
  to administrators as well as other actors. This is the residual gate for
  cargo-dist's broad generated SemVer-like `release.yml` tag trigger, which must
  not be hand-edited to add repository-owned CI/Security API checks.
- [ ] Protect the `prod` environment with required reviewers, release-only
  deployment policy, and no administrator bypass.
- [ ] Enable required Code Owner reviews through the repository branch rules for
  the release/distribution surfaces already mapped in `.github/CODEOWNERS`.

Done when GitHub's rules/environment APIs report these controls enabled, only
approved release automation can create any tag that matches the generated
release trigger, no actor can update/delete any tag ref, and a non-release SHA
cannot obtain production approval.

## AI tasks

### AI-0. Obtain native CI evidence for the unattended agent runner

The unattended runner, supervisor ownership, durable request state, process-tree
cleanup, messaging correlation, CLI/MCP/events surfaces, and cross-platform test
harness are implemented and locally verified on the current working tree. The
lifecycle crash windows, stop acknowledgement, strict MCP contracts, resilient
typed events, canonical UTF-8 registry keys, collaboration protocol,
architecture documentation, and deterministic tray-lock release are complete.
Only evidence that requires immutable CI input and native platform runners
remains.

- [ ] Run the runner/supervisor lifecycle and process-tree fault tests on a
  native Windows CI worker for the exact reviewed commit. Prove suspended child
  startup, Job ownership, descendant termination, durable ambiguous/no-replay
  recovery, deferred stop acknowledgement, and reaping behavior. The
  `runner-windows-pr` job checks out the exact pull-request head (including a
  fork head), verifies its SHA, and runs the complete agent-core/client suites;
  preserve its successful log as evidence.
- [ ] Run the final npm publish-package verification with all six assembled
  cross-platform tarballs and `TARBALL_DIR` for the same commit. The
  `npm-release.yml` package job already builds the five native addons, assembles
  the facade plus five platform packages, packs all six tarballs, and runs
  `test:publish-packages`. Dispatch the reviewed repository branch in dry-run
  mode and preserve the successful job log and package artifacts; manual branch
  dispatch can never enter the publish job.
- [ ] Do not merge or release until both jobs above pass for the same immutable
  reviewed commit. A cross-compile, mock, skipped test, or single-platform
  substitute is not native lifecycle or packaging proof.

Current local evidence (2026-08-11): formatting and workspace-wide Clippy pass;
the tray crate passes 25 consecutive normal-parallel runs (1,325 tests); the
complete `cargo test --workspace --all-features --locked` suite passes; focused
runner (19 passed, 2 ignored), supervisor/events/MCP/process-tree, message CLI,
and collaboration-skill tests pass; `actionlint`, pedantic `zizmor` for
repository-owned workflows, `cargo dist generate --check`, whitespace/conflict/
secret scans, and the local Node release build/test/metadata pipeline pass.
Native Windows lifecycle evidence and the six-tarball npm publish-package proof
remain open because they require CI runners and artifacts unavailable to this
local worktree.

### AI-1. Finish installed-product mixed-version upgrade proof

The current branch adds useful partial evidence: source-level previous-release
state compatibility in `scripts/smoke-upgrade.sh`, same-path executable
identity unit coverage, a side-effect-free tray `--version` probe, `doctor`
executable-version diagnostics, and an authenticated hub minimum-version
probe. It does **not** yet start the previous release's separate hub, workspace
worker, CLI, and tray through each native installer and login manager.

- [ ] On supported macOS, Windows, Debian/Ubuntu, Fedora, and Arch/CachyOS
  products, install the previous release, create a real automatic private hub
  plus workspace/tray login jobs, install the new product, and prove every
  registered job and live process converges to the new executable bytes.
- [ ] Preserve and compare workspace identity, credential references/E2EE
  access, files, hub identity, opaque head, and exact reachable snapshot
  history across each upgrade. Clean up every test service/process.
- [ ] Exercise an actually incompatible advertised hub version through CLI,
  Rust, C, and TypeScript entry points and require one actionable fail-closed
  minimum-version error. Retain explicit compatibility evidence for the
  previous endpoint-less release.

### AI-2. Eliminate generated cargo-dist workflow trust exceptions

The repository now pins generated action references and attests archives,
checksums, and `dist-manifest.json` through `dist-workspace.toml`. Cargo-dist
0.32.0 still generates workflow-wide `contents: write`, dynamic shell template
expansion, an unpinned optional container expression, and an unverified
`curl | sh` bootstrap; direct edits to `release.yml` are forbidden.

- [ ] Adopt a maintained cargo-dist release or a pinned, reviewed generator fork
  that emits per-job least privilege and moves tag/matrix data out of shell
  template expansion.
- [ ] Replace the generated bootstrap with a version-and-digest-bound cargo-dist
  acquisition path and ensure every generated container image is digest-pinned.
- [ ] Regenerate `release.yml` from `dist-workspace.toml`; do not patch generated
  YAML after generation.

Done when `dist generate --check` passes and full-workflow pedantic `zizmor`
reports no medium-or-higher findings for generated `release.yml`.

### AI-3. Integrate the default relay after F2

Blocked on F2 (founder chooses the production relay endpoint). The existing
opaque relay APIs (`serve --relay`, `fnp2` pairing, inner-TLS tunnel), the
relay-only `doctor` probe, and the relay OCI product already cover the
transport surface; what remains is provisioning the chosen endpoint and adding
health/failover telemetry that contains no capabilities or workspace data,
then covering LAN-to-off-LAN fallback in product smoke tests.

### AI-4. Finish randomized-integrator field verification

The randomized integrator assignment layer (PRD `prd-random-integrator-assignment.md`) is
implemented end to end: canonical `ffint1` contract and auditable Blake3 ranking in
`common/src/integrator_contract.rs`; dispatcher state machine, crash-safe
`orchestrator/integrator-state.json` persistence, and read-only cross-machine conflict
materialization in `agent-core/src/integrator.rs`; CLI (`agent integrator assign|status|revoke|resume`,
`conflicts materialize`), MCP tools, metadata-only NDJSON integrator events, C FFI, TypeScript
wrappers, docs, and the collaboration skill. Unit, state-machine, FFI-smoke, and HTTP-hub
integration tests cover the full lifecycle, timeout/blocked fallback, stale replies,
cursor-reset fail-closed, hub-storage privacy, and staleness-safe materialization.

Remaining field evidence (not code-complete in the repo):

- [ ] Run a real two-computer (or network-isolated equivalent) dispatcher/worker/integrator
  scenario through the installed skill: assign, accept, materialize, reconcile with
  `conflicts keep --file`, digest, fallback, and revocation.
- [ ] Exercise macOS, Windows, and Linux protected-state behavior for
  `orchestrator/integrator-state.json` and the dispatcher lock (permissions, crash recovery).
- [ ] Validate the extended `feanorfs-collaboration` skill against cooperative and
  stale-agent scenarios (forward-testing the integrator role rules).

### AI-5. Make workspace-state identity and retirement portable

Unix workspaces with a stable birth identity now relocate safely and reject a
same-path replacement, and agent runtime state no longer creates top-level
workspace slots. Windows and filesystems without a stable birth identity still
need an upgrade-safe identity migration; historical state also lacks the
provenance required for automatic retirement.

- [ ] Add stable Windows and weak-filesystem identity without silently adopting
  an existing path-only slot during upgrade or same-path folder replacement.
- [ ] Replace the bounded O(N) moved-workspace search with a crash-safe identity
  index and serialize path-hash migration with full-lifetime cross-process state
  leases.
- [ ] Record authenticated ephemeral/tombstone provenance prospectively, then
  quarantine and grace expired state before deletion with fail-closed lease and
  identity revalidation. Never infer orphanhood from age, a missing location,
  registry absence, or a temporary-looking path.

Done when same-path replacement and relocation pass on macOS, Linux, and
Windows; concurrent old/new processes cannot split or retire live state; and
cleanup deletes only explicitly proven future-ephemeral/tombstoned state.

## Shipped AI work (removed per closeout)

Code review sweep fixes (docs/code-review-sweep.md): live-lock staleness
guard, placeholder readonly hydration fix, summary-tool EPIPE fallback, and a
precise watcher temp-file filter.

Merkle snapshot-engine review fixes (docs/snapshot-engine-review.md):
critical stale-mtime download bug (updates no longer silently revert), conflict leg-size and
flatten consistency fixes, 64 MiB manifest cap, multi-parent log diffs, runnable `smoke-test.sh`
wired into CI as `source-smoke`, durable uploaded-object dedupe with self-healing retry,
cache-first large-file reads, throttled local GC, undo upload skip, and bounded-concurrency
chunk uploads.


Completed and removed with acceptance evidence in the AI session that shipped
them: constant-cost tray status (size/entry-bounded worker-published
`worker-status.json`, cache-free routine reads, explicit fresh-status path,
large-workspace polling regression), and throttled periodic release awareness
(`update --periodic` with `~/.feanorfs/update-state.json`, shared `--json`
result across CLI/tray/`doctor`, no download/install/execute).
