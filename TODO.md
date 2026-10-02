# FeanorFS product TODO

This is the only authoritative open-work list. Shipped work belongs in
`CHANGELOG.md`; remove completed or superseded items instead of retaining a
backlog history.

## Founder tasks

These require account ownership or representative human acceptance. Never
commit credentials or paste them into issues, logs, or chat.

### F1. Provide trusted desktop-signing access

- [ ] Add Developer ID Application/Installer and App Store Connect notarization
  credentials to GitHub Actions for the universal macOS `.dmg`/`.pkg`.
- [ ] Configure Azure Artifact Signing through GitHub OIDC for the Windows CLI,
  tray, and installer `.exe`.

Done when the fail-closed workflows publish notarized macOS and Authenticode
Windows products from one immutable tag. Unsigned GitHub releases must not be
presented as trusted macOS or Windows installers.

### F2. Accept onboarding on ordinary desktop sessions

Blocked on F1 for macOS and Windows.

- [ ] Install through the trusted `.dmg`, `.exe`, `.deb`, `.rpm`, and
  `.pkg.tar.zst` products as ordinary users; accept or report a reproducible
  defect in tray-first Start/Join/Not Now, login persistence, update behavior,
  and clean uninstall.
- [ ] Repeat the released Arch package and tray flow in a real CachyOS Wayland
  session. The currently available CachyOS session is i3/X11, so automated SSH
  evidence cannot honestly satisfy the Wayland acceptance requirement.

Record only OS/version and secret-free acceptance or reproduction evidence.

### F4. Lock the GitHub release control plane

- [ ] Protect every tag accepted by the generated release workflow (not only
  `v*`): restrict creation to the release automation identity and prevent
  update/deletion, including by administrators.
- [ ] Protect the `prod` environment with required reviewers, release-only
  deployment policy, and no administrator bypass.
- [ ] Enable required Code Owner reviews through the repository branch rules for
  the release/distribution surfaces already mapped in `.github/CODEOWNERS`.

Done when GitHub's APIs report these controls enabled. This requires a distinct
release identity and an independent reviewer; repository code cannot create
either safely.

### F5. Decide format-v1 legacy decryption retirement

- [ ] Collect `feanorfs doctor --migration-report` from every real install you
  know of (it reports only aggregate format counts) and decide whether the v1
  XOR decrypt path and its migration fences can be removed.

Done when the decision and its evidence are recorded. Until then the legacy
path stays: removing it would make unmigrated v1 workspaces unreadable.

## AI tasks

### AI-11. Prove the agent-first coordination surface in the field

Local evidence (real Claude Code eval, guard hook, local fuzzing) is in
`docs/acceptance-evidence.md` (AI-11).

- [ ] Route a real request with `agent send cap:ios-build` from the Linux
  machine to the Mac and verify the Git baseline warning when the two clones
  sit on different commits.
- [ ] Measure Codex in hooks mode on the founder machine:
  `feanorfs integrate --host codex --auto-claim`, trust the hooks once in
  Codex, then run the eval without `--ignore-user-config` (an isolated run
  cannot trust them without also running personal hooks). Without hooks,
  Codex takes 14–15 commands vs 6–8 for worktrees (input tokens 1.9–2.4×;
  the revised skill cut 17 commands and ~10% of input).

Done when each item has recorded evidence.

### AI-10. Finish confirmed review fixes and acceptance

Preserve pre-existing working-tree changes. Order: data preservation/locking,
rekey, lifecycle/transport, then contracts/release tooling. Revalidate findings
before editing; false positives and compatibility constraints need evidence.

- [ ] Finish Unix materialization backup-retry/durability fixes and broader
  recovery coverage. Inode-mismatch deletion fallback removed: two regressions
  failed before the fix; all three focused recovery tests passed afterward.
  Independent static review approved that change. `client/tests/fault_recovery.rs`
  (11 tests) passes on macOS once its Unix case links `stage/new` like real
  publication does. Broader verification pending.
- [ ] Complete locking/registry/access-log, server GC/waiter/durability, and
  lifecycle batches; collect independent review and exact test results.
  First PR CI found two lock bugs, both fixed with tests: the sync-lock probe
  reported idle on Windows (mandatory locks hide the PID), and exclusive
  workspace-state leases failed on Linux while a parallel spawn briefly
  inherited the shared lease descriptor. It then exposed a continuous-agent
  race, fixed with a deterministic regression test: a no-op land adopted a
  head with an unresolved shared conflict as the agent base, so the agent
  later published its own leg over the resolution.
- [ ] Finish compatibility acceptance for source-bound rekey publication.
  Independent static review approved the publication lifecycle. Local-drift
  test review was cancelled. The proposed local-overwrite finding was not
  reproduced in the narrow legacy/v3 crash-resume case tested only after
  uncertain CAS: local edits survive the next sync with a pending conflict,
  and the peer retains committed candidate bytes. This is not broad local-drift
  safety evidence; no auto-merge is expected.
  Publication now journals the captured source and prepared
  candidate before CAS; retry reuses both IDs. Stale-source and exact-candidate
  snapshot regressions pass, as does legacy/v3 crash-after-CAS recovery with
  unchanged head and successful fresh-client decryption. All 41 negotiation
  tests pass. Older pre-publication journals missing source binding fail closed
  and retain keys for manual recovery; do not claim automatic compatibility.
  V3 fencing now persists on both hubs;
  explicit status assertions verify competing tokens and unfenced publication
  are rejected. All 14 parity tests and 87 server tests passed. Stamped resume
  no longer reacquires a released fence; legacy/v3 crash-before-config tests
  pass, and independent static review approved the scoped fence/resume fix.
  Journal-preservation regression reproduced then passed: plain migration
  preserves unfinished v3 rekey state; stamped/config-matching finalization is
  cleanup-only. All five migration integration tests passed locally.
- [ ] Complete contract/reducer review and compatibility handling. Two reducer
  test compile errors repaired and independently reviewed. Resolution producer
  now hashes the 32-hex job ID before recording verification input digests;
  all 35 resolution tests pass. Older persisted evidence containing raw job
  IDs still requires explicit compatibility handling before release.
- [ ] Complete transport/embedded-hub fixes after interrupted agent execution.
- [ ] Finish independent review and native Windows cleanup/verification for
  bounded tray capture; separate interactive join pipes remain unfixed.
  `CapturedCommand` now polls both streams without reader threads and bounds
  post-exit draining; Unix retains process-group termination, while Windows
  stops only the direct child. Main session reported successful
  `cargo check -p feanorfs-tray --locked --offline` and
  `cargo test -p feanorfs-tray --locked --offline`: 67/67 tests passed,
  including inherited-pipe timeout, cancellation, and over-limit coverage.
  Exact inherited-pipe regression passes in 0.17 s; an isolated reproduction
  of the old join ordering fails after 4.02 s (not a full old-revision run).
  Tray/common Clippy pass with warnings denied; all 181 common tests pass.
  The two equivalent common size-guard rewrites received independent approval.
  No native Windows runtime verification is claimed.
- [ ] Revalidate/fix staged desktop release evidence ordering, Windows checkout,
  unsigned release provenance/overwrite policy, and installer PATH removal.
  Zig example finding is resolved: sentinel-terminated fopen path; checked
  fputs/fclose before land. Independent review approved; Zig 0.16 build and
  isolated local-hub run verified exact task.txt bytes and zig1 cleanup.
- [ ] Run final formatting, Clippy, cross-crate tests, independent review, and
  DOX pass. Latest agent-core unit run: 432 passed, 0 failed, 5 ignored;
  migration integration subset: 5 passed. Mesh compiler warnings remain.
  Server follow-up review still identifies GC cancellation ownership,
  metadata-bearing GC logs, and timing-dependent waiter tests; its repair
  agent was cancelled. Cross-crate and platform acceptance remain pending.
  Do not treat these focused green runs as final acceptance.

Done when every confirmed finding has executable acceptance evidence or an
explicit compatibility/platform blocker. Native Windows, Wayland, signing,
and cross-NAT field claims require their actual environments. Remove this
entry when its implementation and verification work is complete.

### AI-9. Complete the shared-workspace daily workflow

- [x] Expose the same bounded activity observations and concrete next actions
  through ordinary CLI status, MCP, and tray, with unknown/stale states explicit.
- [x] Complete the agent handoff: exact tested snapshot, changed paths, reported
  verification, and result delivery to the requester while continuous work remains enabled.
- [x] Complete the conflict round trip through existing resolution jobs: human
  answer or configured resolver, guarded publication, and notification to both
  participants without manual watcher shutdown.
  Native answers must retain the assignment/fingerprint/question generation
  shown before the dialog; do not rebind an old answer to a newer question
  when submitting. Reuse engine validation of `HumanResolutionAnswer`.
  Local review, guarded application, atomic completion notices, and continued
  live work have executable coverage. Assignment/result signaling followed by
  guarded apply passes across local clients. Durable human-answer retries pass
  after an injected outage and from a fresh CLI process; the peer observes the
  same answer. A local answer is not a peer acknowledgement.
- [x] Execute the documented Git publication guard and preserve independent
  worktree indexes, untracked files, staging, and divergent local history.
- [x] Exercise two clients and two active agents through independent edits,
  conflict, disconnection, restart, resolution and continued work; verify file
  hashes and preserved versions, with convergence timing and manual interventions.
  The loopback source-binary journey now covers an injected outage, conflict,
  explicit resolution, continued work, controlled restart, and requester
  receipt with full worktree equality. Real multi-device testing is deferred
  by the user (2026-09-12) and does not block this implementation; the field
  acceptance rows in AI-1/AI-6/AI-7 remain unverified. Native-dialog interaction
  remains open. See `docs/acceptance-evidence.md`.

Done when each surface and the local complete journey have executable evidence;
deferred field testing must remain explicitly unverified.

### AI-1. Complete released-product installation acceptance

- [ ] Install the exact published products on macOS, CachyOS, and Windows.
  Verify matching versions, managed services, mDNS, `doctor`, and a bounded
  cross-machine sync while preserving the Mac workspaces as authoritative.

Done when the installed binaries have exact-release provenance and a
secret-free post-install record. Source builds, mounted installers, ad-hoc
signatures, and services executing from the development checkout do not count.

### AI-2. Mixed-version protocol peers on released products

- [ ] Exercise an older released product against a newer one (and vice versa)
  for `ffwork1` intents and `ffres1` assignment/result/answer profiles:
  unknown or malformed profiles must not create or alter typed protocol
  projection entries (observation cursors and bounded seen-ID bookkeeping may
  advance), and legacy unfingerprinted conflicts must stay manual-only. Use the
  installed products from AI-1.

Done when the two released versions converge on identical projections without
corruption, or a reproducible defect is recorded with OS/version evidence.

### AI-5. Verify portable workspace-state identity and retirement on CI

Implementation landed: stable `windows-v2` volume/file-index/creation-time
identity, explicit `-weak` Unix identities without birth times, one-time
provenance-recorded adoption of legacy path-only slots (recorded `location`
must prove the exact path), the crash-safe `.identity-index.json` replacing
the O(N) moved-workspace scan (mtime-guarded so duplicate identities still
fail closed), full-lifetime per-slot state leases serializing path-hash
migration and retirement, and `feanorfs retire <folder>` tombstone
grace/quarantine/verified-deletion with fail-closed lease and identity
revalidation. macOS unit/integration evidence passes locally, including
cross-process lease contention, real same-path replacement, relocation, and
the full tombstone lifecycle.

- [ ] Confirm the same matrix on the Linux and Windows CI runners (cfg-gated
  tests added; `windows-v2` type-checked standalone) before the next release.

Done when same-path replacement, relocation, adoption refusal, lease
contention, and tombstone cleanup pass on macOS, Linux, and Windows runners
with no split or retired live state.

### AI-6. Finish continuous-agent field verification

- [ ] Validate installed macOS, Windows, and Linux process ownership and
  shutdown: `agent run` final flush under each native installer, supervisor
  restart during active reconciliation, and duplicate-owner rejection from
  separate processes.
- [ ] Run a network-isolated two-active-agent soak with live feedback, a
  genuine conflict, explicit resolution, disconnection, and recovery,
  instrumented for zero lost updates and zero echo loops.
- [ ] Measure the small-file two-client LAN convergence target (p95 < 3 s,
  excluding backoff and conflict resolution) with the head-wait path active.

Done when the verification matrix in `prd-continuous-agent-development.md`
passes on installed products with no lost updates, automatic merge, unbounded
loop, plaintext regression, Git dependency, or false exactly-once execution
claim.

### AI-7. Mesh transport field hardening

P0–P3 shipped (see `docs/mesh-transport.md`); remaining before calling the
direct path production-default on real networks:

- [ ] Renew or release UPnP/PCP mappings on hub stop and supervisor shutdown;
  leases currently expire naturally after 30 minutes.
- [ ] Restrict punch admission to workspace members once membership is
  queryable without new hub endpoints; any signed identity is accepted today
  (bridged traffic still terminates at the token-authenticated hub).
- [x] Add last-path TTL to `mesh-state.json` projection so stale punched paths
  re-probe instead of pinning; QUIC keepalives shipped during the field test,
  and the tray now projects `unreachable` past the five-minute freshness bound.
- [x] Authenticated mDNS success now refreshes stale LAN candidates in config
  (live two-machine subnet-move evidence in `docs/mesh-field-evidence.md`);
- [ ] Two-machine cross-NAT punch soak with typed outcome stats from
  `mesh-state.json`; single-machine loopback evidence exists in
  `client/tests/mesh_evidence.rs`.

Done when a two-machine cross-NAT transfer completes over the punched path
with recorded stats, no relay configured, and no secret-bearing surface added.

### AI-8. Repair missing reachable historical objects

- [x] Reproduce the two-machine post-conflict history failure where a current
  head reaches snapshot `c2274d42…` locally but another client receives HTTP
  404 from the hub for that object.
- [x] Make publication validate or repair the complete reachable object closure
  before accepting its manifest/head, without weakening opaque hub storage.

Done: publication walks the bounded parent DAG and one typed missing-blob
repair re-uploads hash-verified cached ciphertext; live two-machine evidence in
`docs/mesh-field-evidence.md` ("Fixes shipped after the field test").
