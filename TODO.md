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

## AI tasks

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
