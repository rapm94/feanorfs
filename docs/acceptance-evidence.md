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
