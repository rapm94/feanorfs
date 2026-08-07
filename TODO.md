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
  tray, and installer `.exe`.

Done when one immutable tag publishes notarized macOS and Authenticode Windows
products. Unsigned releases must remain clearly labeled as development builds.

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

## AI tasks

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
