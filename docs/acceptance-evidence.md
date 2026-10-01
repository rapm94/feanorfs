# Acceptance Evidence Matrix (AI-2 / AI-5 / AI-6)

Generated 2026-08-21 against commit lineage after `4454333`. Secret-free.
Companion script: `scripts/acceptance-matrix.sh` prints current local-cell
status. The maintainer marks TODO.md boxes only after executing the listed
procedure in the required environment.

## AI-5 — portable workspace-state identity and retirement on CI

| Cell | Requirement | Evidence today | Command | Status |
|---|---|---|---|---|
| macOS unit/integration | same-path replacement, relocation, adoption refusal, lease contention, tombstone lifecycle | 11 tests in `agent-core/src/workspace_state_registry.rs`, 22 in `workspace_layout.rs`, green locally | `cargo test -p feanorfs-agent-core --locked workspace_state workspace_layout` | PASS |
| Linux runner | same matrix | ubuntu runs full suite in `.github/workflows/ci.yml` primary jobs (lines ~25-82); identity tests are not platform-gated off Linux | push to main; watch `ci.yml` | VERIFY-ON-PUSH |
| Windows runner | same matrix + standalone `windows-v2` typecheck | `cross-platform` job runs `cargo test --workspace --exclude feanorfs-tray --all-features --locked` on `windows-latest` (ci.yml:100-118) | same push | VERIFY-ON-PUSH |

## AI-2 — mixed-version protocol peers

| Cell | Requirement | Evidence today | Command / procedure | Status |
|---|---|---|---|---|
| ffwork1 reducer convergence | order-independent projection, causal dominance, bounded rebuild | 31 tests in `agent-core/src/work.rs`; `client/tests/work_engine.rs` CLI engine suite | `cargo test -p feanorfs-agent-core --locked work::` and `cargo test -p feanorfs-client --locked --test work_engine` | PASS |
| ffres1 assignment/result/answer | deterministic reducer, pending-order convergence, typed answers | 12 tests in `agent-core/src/resolution_protocol.rs`; `client/tests/resolution_protocol.rs`, `resolution_parity.rs` | `cargo test -p feanorfs-agent-core --locked resolution_protocol` + client suites | PASS |
| wire-shape stability | JSON contracts frozen for FFI/Node/CLI | `client/tests/contract_snapshots.rs` + tray snapshots | `cargo test -p feanorfs-client --locked contract_snapshots` | PASS |
| unknown/malformed profile rejection | older vs newer released products; unknown profiles must not alter projections (cursor bookkeeping may advance) | none against RELEASED binaries | install v(n) and v(n+1) from GitHub releases (AI-1 prerequisite), exchange one `ffwork1` intent and one `ffres1` assignment each direction, diff both `orchestrator/work-state.json` and resolution projections for equality | MISSING |
| legacy unfingerprinted conflicts stay manual-only | exercised on released pair | covered by reducer unit tests pre-release; field pair still required | same session as above | MISSING |

## AI-6 — continuous-agent field verification

| Cell | Requirement | Evidence today | Procedure | Status |
|---|---|---|---|---|
| process ownership per OS | supervisor restart during reconciliation, duplicate-owner rejection | lifecycle unit suites in `runner/` modules + `client/tests/agent_runner.rs`; OS-installer-level proof outstanding | on installed product (AI-1): start `agent run`, restart supervisor service mid-reconciliation, attempt second owner from separate terminal; expect clean takeover refusal and resumed reconciliation in `continuous-status.json` | PARTIAL |
| two-active-agent soak | zero lost updates, zero echo loops across conflict + disconnect + recovery | none instrumented | network-isolated LAN hub, two `agent run` sessions editing one path; force genuine conflict, resolve via `conflicts keep`, kill one side mid-flight, reconnect; assert identical worktrees and no repeated reconcile loops in logs | MISSING |
| LAN convergence p95 < 3 s | small-file two-client target with head-wait active | none measured | two clients, scripted 50-file edit loop on side A, poll side B mtime-to-head delta over ≥20 iterations, report p95 | MISSING |

Prerequisite for all AI-6/AI-2 field rows: AI-1 installed-product acceptance
(founder-dependent).

## AI-9 — local daily-workflow evidence (2026-09-12)

`scripts/acceptance-matrix.sh` now fails when a local check fails and retains
explicit skips for installed-product and real-network work.

| Local check | Evidence | Command |
|---|---|---|
| Shared status | Cached activity/time survive CLI/tray adapters; missing observations stay unknown; MCP offers read-only next steps | `cargo test -p feanorfs-client --test tray_status_engine --locked` |
| Human resolution review | Reviewed question generation is retained; stale answers refuse before staging; candidate verification runs in the engine; preserved versions refuse edits/symlinks; published notice is readable by both participants | `cargo test -p feanorfs-client --test resolution_parity --test resolution_protocol --locked` |
| Saved human answers | Injected outage leaves the exact accepted answer durable; a fresh CLI process retries it; wrong generations refuse; confirmed receipt avoids another send; the other client's reducer observes the same answer | `cargo test -p feanorfs-client --test resolution_protocol saved_human_answer_survives_outage_and_retries_without_rebinding --locked` |
| Git publication | Actual documented guard preserves staged/WIP/divergent state; valid advancement preserves untracked files; worktree indexes remain independent | `sh scripts/test-git-workflow.sh` |
| Two active agents | Two real CLI owners against a loopback hub: injected HTTP outage, preserved offline edits, conflict, explicit resolution without watcher shutdown, continued edits, controlled restart, snapshot-bound result received by requester, complete worktree byte equality | `cargo test -p feanorfs-client --test continuous_agents two_active_agents_resume_after_resolution_and_continue_work --locked -- --nocapture` |

The journey emits resolution-to-continued-work timing and intervention counts.
Its result body carries changed paths, reported verification, and a worktree
digest. The regression also exposed and fixes canonical-path event filtering
on macOS, a permanent conflict pause, and a conflict-metadata transition that
could republish the old visible leg over a resolution. A separate engine check
proves edits made after the captured conflict are not overwritten.

This is source-binary loopback evidence. It does not establish LAN p95,
installed service recovery, abrupt power-loss behavior, mixed released
versions, native dialog interaction, or Windows/Linux product acceptance.
Those field rows remain open. The user deferred real multi-device testing
on 2026-09-12; it does not block completion of this implementation.
