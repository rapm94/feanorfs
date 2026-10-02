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

## AI-11 — agent-first coordination surface (2026-10-01)

Local macOS evidence for branch `improvements/agent-first`. Secret-free.

### Multi-agent eval with real Claude Code agents

`python3 eval/run.py eval/scenarios/<name>.json --timeout 900 --agent-cmd
"claude -p {prompt} --output-format json --setting-sources project
--strict-mcp-config --permission-mode acceptEdits --allowedTools
'Bash(feanorfs:*)' 'Bash(python3 -m unittest:*)'"` (Claude Code 2.1.231,
two simulated machines, isolated from personal settings).

| Scenario | Mode | Human interruptions | Conflicts | Lost edits | Tests | Cost | Wall |
|---|---|---|---|---|---|---|---|
| overlap | feanorfs | 2 (scope decisions) | 0 | 0 | pass | $0.58 | 92 s |
| overlap | worktrees | 1 (merge conflict) | 1 | 1 | pass | $0.19 | 84 s |
| disjoint | feanorfs | 2 (scope decisions) | 0 | 0 | pass | $1.16 | 456 s |
| disjoint | worktrees | 0 | 0 | 0 | pass | $0.25 | 28 s |

Reading: scoped turns prevent the lost edit that parallel branches suffer
when two agents touch the same place, at roughly 3–5× the token cost of
uncoordinated work. The disjoint FeanorFS wall time is dominated by model
latency (one agent spent 428 of 441 s in API calls). Earlier runs drove two
fixes now on the branch: one-shot agents quit or polled while waiting for a
decision (`agent next --wait`, which returns at once when an action is
ready), and settle actions now carry the agent's settled snapshot. Running
eval agents with personal hooks enabled let a command-rewriting hook break
their shell calls; the harness documents the isolation flags.

### Coordination overhead after the one-call protocol (2026-10-02)

Turn-level traces (`--output-format stream-json`) showed cost tracks turns:
each turn re-sends about 35–40k cached context tokens, and 8–9 of an
agent's 11–17 tool calls were protocol bookkeeping (`next`, `propose`,
`guard`, `settle`, `complete`). After `agent claim`/`agent done`, the
automatic coordinator (`agent coordinate`), and the Claude Code hooks
(`--hooks`: claiming PreToolUse guard plus a Stop hook running `agent done`):

| Scenario | Mode | Turns per agent | Coordinator decisions | Human interruptions | Conflicts | Lost edits | Cost | vs worktrees |
|---|---|---|---|---|---|---|---|---|
| overlap | feanorfs, hooks | 4–5 | 2 | 0 | 0 | 0 | $0.24 | 1.14× |
| overlap | feanorfs, claim prompt (no hooks) | 4 | 2 | 0 | 0 | 0 | $0.22 | 1.09× |
| overlap | worktrees | — | — | 1 | 1 | 1 | $0.21 | 1× |
| disjoint | feanorfs, hooks | 4–8 | 2 | 0 | 0 | 0 | $0.27 | 1.06× |
| disjoint | feanorfs, claim prompt (no hooks) | 4–5 | 2 | 0 | 0 | 0 | $0.24 | 1.17× |
| disjoint | worktrees | — | — | 0 | 0 | 0 | $0.25 | 1× |

Without hooks, agents first also called `agent done` and took 9–10 turns
($0.37, ~1.8×); `agent run` now finishes the claim when the agent exits
cleanly, so the prompt asks only for `agent claim`. Worktree baselines in the
no-hooks runs: $0.20 (overlap, again 1 conflict and 1 lost edit) and $0.21
(disjoint). Before these changes the same scenarios cost $0.58 (overlap) and
$1.16 (disjoint) under FeanorFS. Wall time remains higher (about 65 s vs 21–35 s)
because hooks wait for decisions and for edits to land; that is waiting, not
tokens.

### Codex without hooks (2026-10-02)

codex-cli 0.154.0 (`codex exec`, ChatGPT sign-in, so no price), workspace-write
sandbox with loopback network, same claim-only prompt. Tokens count input
plus output once; "weighted" prices cached input at 0.1× and output at 8×
input.

| Scenario | Mode | Shell commands | Coordinator decisions | Conflicts | Lost edits | Tokens (uncached / cached / out) | Weighted vs worktrees |
|---|---|---|---|---|---|---|---|
| overlap | feanorfs | 18 | 2 | 0 | 0 | 63k / 394k / 1.7k | 1.67× |
| overlap | worktrees | 6 | — | 1 | 1 | 49k / 139k / 0.8k | 1× |
| disjoint | feanorfs | 15 | 2 | 0 | 0 | 49k / 261k / 1.3k | 1.32× |
| disjoint | worktrees | 7 | — | 0 | 0 | 43k / 146k / 0.9k | 1× |

With the current skill installed temporarily (the original was restored
byte-for-byte afterwards), correctness was the same and weighted tokens were
1.75× (overlap: 17 vs 9 commands, 52k / 455k / 1.7k vs 43k / 146k / 0.8k) and
1.74× (disjoint: 17 vs 8 commands, 55k / 334k / 1.4k vs 36k / 152k / 0.8k).
Each extra command re-reads the whole cached context, so cost follows command
count: agents read the skill, then followed its `agent status` and
`agent done` advice on top of the one required `agent claim`. After the skill
was revised to lead with claim → edit → exit (same procedure), runs took
14–15 commands and about 10% less input (overlap 452k vs 507k total input
tokens, disjoint 356k vs 390k; worktrees 188–189k); the remaining
FeanorFS-specific commands were reading the skill, one claim, re-reading the
claimed file, and an optional `agent done`.

Coordination itself was one `agent claim` per agent (one agent re-ran it
while waiting). The other extra commands came from the machine's global
Codex instructions and an installed `feanorfs-collaboration` skill from an
older release (18.9 KB), which sent agents to `agent inbox` and
`agent status`; `feanorfs integrate status` now reports such copies as
partially configured. The first attempt lost both edits safely: Codex's
`shell_snapshot` replayed a `PATH` whose older `feanorfs` lacked `claim`, so
the agents refused to edit unclaimed files (`--disable shell_snapshot`).

### Codex with hooks (2026-10-02)

Same scenarios with `--hooks` and the bare task prompt; `{codex_hooks}`
defined FeanorFS's claiming guard and `agent done` Stop hook for each Codex
session and trusted exactly those two (Codex's trust hash, checked against
the 10 hashes Codex had recorded for the machine's own hooks; no user config
read or written, personal hooks untrusted):

| Scenario | Mode | Shell commands | Coordinator decisions | Conflicts | Lost edits | Total tokens | vs worktrees |
|---|---|---|---|---|---|---|---|
| overlap | feanorfs, hooks | 7 | 2 | 0 | 0 | 192k | 1.02× |
| overlap | worktrees | 7 | — | 1 | 1 | 189k | 1× |
| disjoint | feanorfs, hooks | 8 | 2 | 0 | 0 | 192k | 1.02× |
| disjoint | worktrees | 7 | — | 0 | 0 | 189k | 1× |

With hooks, Codex agents spend no commands on coordination, matching Claude
Code's hooks result.

### Guard hook in real Claude Code

A scratch workspace with `linux`'s accepted scope `src/**` and `feanorfs
integrate --host claude --guard-hook --project <ws>`; `claude -p` as
`FEANORFS_AGENT=codex` asked to write `src/new.rs` and append to
`README.md`: the hook blocked `src/new.rs` with the scope reason and the
agent reported it; `README.md` was edited. In an unrelated folder the hook
exits 0 and creates no workspace state (`client/tests/coordination_cli.rs`).

### Coverage-guided fuzzing

`cargo +nightly fuzz run <target> -- -max_total_time=60` (cargo-fuzz 0.12.0):
`tree_codec` 1.11 M, `invites` 1.07 M, `signals` 0.97 M, `aead` 1.11 M
executions from empty corpora, no crashes. The stable mutation smoke
(`common/tests/parser_fuzz.rs`) runs in every `cargo test`.

On GitHub runners (`workflow_dispatch` at `3edc9f5`, 2026-10-02): Security
run 36999197690 fuzzed all four targets for 120 s each without a crash, and
CI run 36999194960 passed `Multi-agent eval (scripted)`.

Not covered here: capability routing and Git-baseline warnings between two
physical machines.
