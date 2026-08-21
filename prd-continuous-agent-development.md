# PRD and Task Plan: Continuous Agent Development

**Date:** 2026-08-13  
**Status:** Implemented; installed-product and network field verification pending  
**Scope:** Automatic, event-driven synchronization of active isolated agent
worktrees using FeanorFS's existing snapshot, land, refresh, runner, event,
conflict, and supervisor machinery

> Implementation record: the source implementation and automated verification
> described here are complete. `TODO.md` remains the repository's only
> authoritative open-work list; AI-6 owns the remaining installed-product,
> network-soak, and LAN convergence evidence.

---

## Summary

FeanorFS makes development by active coding agents feel continuous. An
agent edits ordinary files in its existing isolated worktree, and FeanorFS
automatically reconciles those changes into the shared encrypted workspace.
Every other active agent automatically receives non-overlapping changes.
Humans and agents exchange existing `ffmsg1` signals against exact snapshots,
and compatible hub clients wake as soon as the opaque workspace head changes.

The normal experience has no Git dependency and no push/pull-shaped agent
workflow:

```text
agent edits
    -> FeanorFS detects a quiet filesystem change burst
    -> existing land/reconcile logic advances the shared WIP snapshot
    -> other clients wake on the head change
    -> existing refresh logic updates untouched paths in active agents
    -> conflicts remain explicit when paths overlap
```

`agent land` and `agent refresh` remain implementation and recovery tools, but
an active agent does not invoke them during normal operation. FeanorFS still
does not merge file content, interpret code, host models, synchronize Git
metadata, or become a chat database.

The core idea borrowed from Delta is continuity between work and communication,
not CRDT editing:

```text
code snapshot S1
    -> feedback signal about S1
    -> code snapshot S2
    -> verification result about S2
```

---

## Decision Statement

Build a **continuous reconciliation controller** around existing FeanorFS
primitives.

- Use the current single encrypted workspace head as shared WIP authority.
- Use existing repeatable agent land semantics for outbound agent changes.
- Use existing safe refresh semantics for inbound workspace changes.
- Watch active agent worktrees with the same bounded/debounced discipline as
  the normal workspace watcher.
- Extend the existing authenticated `GET /api/head` operation with an optional
  bounded wait mode; do not add an agent-aware hub route or table.
- Keep `ffmsg1` low-volume and snapshot-linked. Continuity means immediate
  wakeup and causal context, not token or transcript streaming.
- Activate continuous reconciliation only while an agent is actually active:
  initially, while launched through `feanorfs agent run` or owned by an enabled
  configured runner.
- Keep dormant agent worktrees preserved but inactive. Upgrading FeanorFS must
  never cause an old, unfinished agent directory to land automatically.

---

## Problem Statement

### Baseline before this change

FeanorFS already contains most of the required machinery:

| Capability | Baseline behavior | Main implementation |
|---|---|---|
| Workspace file watching | Local filesystem changes are debounced for 500 ms and synchronized automatically | `client/src/watch.rs` |
| Remote workspace observation | The normal watcher performs a periodic pass every 45 seconds | `client/src/watch.rs` |
| Agent isolation | `agent spawn` creates an isolated ordinary-file worktree and records a base snapshot | `agent-core/src/agent/spawn.rs` |
| Repeatable publication | `agent land` performs three-way reconciliation, CAS publication, materialization, conflict registration, and advances the agent base | `agent-core/src/agent/land.rs` |
| Safe inbound update | `agent refresh` applies paths the agent has not touched and defers overlaps | `agent-core/src/agent/refresh.rs` |
| Agent communication | `ffmsg1` signals are encrypted no-file-change snapshots tied to exact snapshot IDs | `agent-core/src/messages.rs` |
| Agent activation | A configured runner polls its inbox every 500 ms, refreshes before launch, and invokes one bounded child at a time | `client/src/cli/agent_runner.rs` |
| Orchestrator wakeups | `events` emits local changes, mirror state, conflicts, messages, and integrator events | `client/src/cli/events.rs` |
| Background ownership | One supervisor owns hubs, workspace watchers, runners, and the tray | `client/src/cli/supervisor.rs` |

### Discontinuities addressed

- The normal workspace is watched automatically, but isolated agent worktrees
  are not part of that continuous watch loop.
- Agent changes reach the shared workspace only when something explicitly
  invokes `agent land`.
- Workspace changes reach an existing agent worktree only when something
  explicitly invokes `agent refresh` or the configured runner prepares a new
  request.
- A healthy remote head change may wait for the 30-second event poll or the
  45-second workspace poll before another process notices it.
- The runner and events surfaces both poll the same opaque head independently;
  there is no reusable head-change wait primitive.
- A signal-only snapshot advances the head even though its file-tree root is
  unchanged. A continuous controller must distinguish communication activity
  from file activity to avoid needless filesystem writes and feedback loops.
- The user-visible lifecycle still reads like `spawn -> refresh -> land`, even
  though the desired experience is simply “the active agents stay current.”

### Baseline user impact

The existing operations are safe but episodic. Agents can unknowingly work
against stale files until the next request or manual refresh. Reviewers see a
final result rather than the evolving shared WIP. Multi-machine agents spend
time coordinating synchronization commands instead of working, and the
product feels more like a sequence of transfers than one continuous workspace.

### Why now?

Format-v3 snapshots, CAS publication, conflict preservation, agent runners,
signals, events, and the supervisor are already implemented. The remaining
work is primarily coordination and lifecycle integration. Doing it now avoids
inventing a second worktree protocol or incorrectly adopting Git/CRDT concepts
that would weaken FeanorFS's existing safety and privacy model.

### Who is affected?

- **Primary users:** Developers running one or more coding agents on the same
  unfinished project across computers.
- **Primary automation users:** Configured agent runners and external
  orchestrators consuming MCP, JSON, or NDJSON events.
- **Secondary users:** Humans following agent progress, resolving genuine
  conflicts, or moving between machines while work continues.

---

## Goals

- Make active agents converge automatically on non-overlapping shared WIP.
- Require no Git repository, Git command, commit, branch, push, or pull.
- Remove manual `agent land` and `agent refresh` from the normal active-agent
  loop while retaining them for diagnostics and recovery.
- Begin remote reconciliation promptly after a successful head CAS on a
  healthy compatible hub instead of waiting for the periodic poll.
- Keep work and communication causally connected through snapshot IDs,
  `about_snapshot`, and `reply_to`.
- Preserve every current conflict, encryption, path-safety, migration, and
  crash-recovery invariant.
- Keep the hub content-blind and agent-blind.
- Bound filesystem events, network waiters, retries, queues, state, and output.
- Preserve a plain-files human path: every active agent still works in an
  ordinary directory.

## Non-Goals

- Git replacement features such as staging, commits, branches, tags, rebases,
  release history, or Git-compatible protocols.
- Reading or changing `.git` or `.jj` state. The feature must work in a folder
  that has neither.
- CRDTs, operational transforms, per-keystroke replication, or automatic text
  merging.
- Automatically deciding which conflicting implementation is correct.
- Treating each automatically produced WIP snapshot as coherent, tested, or
  ready for release.
- Streaming model tokens, complete transcripts, patches, source files, or raw
  build logs through `ffmsg1`.
- Starting an inactive model merely because a head or signal changed.
- Process sandboxing. Existing agent worktrees continue to provide data
  isolation only.
- Per-agent server authorization or cryptographically signed agent identity in
  v1. The shared workspace key remains the participant trust boundary.
- A new hub route, agent table, plaintext path index, or project-local metadata.

---

## Terminology

- **Shared WIP:** The current file tree represented by the workspace head. It
  is unfinished transport state, not a release or commit.
- **Active agent:** An existing FeanorFS agent whose process is currently owned
  by `agent run` or an enabled configured runner. Dormant agent directories are
  not active.
- **Continuous reconciliation:** Automatic execution of the existing land and
  refresh semantics in reaction to coalesced filesystem and head changes.
- **Settled tree:** A tree root for which no local agent change is waiting to be
  reconciled and no applicable remote path is waiting to be refreshed.
- **Settled snapshot:** A reachable snapshot carrying the settled tree. Later
  signal-only heads may have the same tree root.
- **Attention state:** A conflict, unsafe ambiguity, corrupt state, exhausted
  retry, or lifecycle condition that stops automatic mutation until a human or
  authorized consumer acts.
- **Signal-only head:** A snapshot whose tree root matches its file-state
  predecessor and whose change is an encrypted `ffmsg1` message.

---

## End-State User Experience

### Starting an interactive agent

```bash
feanorfs agent spawn worker
feanorfs agent run worker -- codex
```

While the command is alive:

1. FeanorFS treats `worker` as active.
2. File changes in the agent worktree are coalesced and reconciled
   automatically.
3. Non-overlapping workspace changes arrive in the agent worktree
   automatically.
4. Snapshot-linked messages wake the agent's orchestrator/event consumer.
5. The child works with normal files and does not issue sync, push, pull,
   refresh, or land commands.
6. On process exit, FeanorFS makes one bounded final reconciliation attempt and
   reports whether the work settled, remains offline, or needs attention.

### Running an unattended agent

The existing one-time runner setup remains explicit because it grants process
execution authority. Once the configured runner is enabled, its selected agent
is active and the same continuous reconciliation controller runs for its
lifetime. A request still has one bounded execution and one terminal reply;
the runner does not replay ambiguous executions.

### Two agents edit different paths

1. `agent-a` edits `src/parser.rs`.
2. After the quiet-period debounce, FeanorFS uses the existing land engine to
   publish a snapshot authored by `agent-a`.
3. `agent-b` receives a head-change wakeup.
4. Its controller uses the existing refresh engine to apply `src/parser.rs`
   because `agent-b` has not touched it.
5. `agent-b` edits `tests/parser.rs`; the same flow updates `agent-a`.
6. Neither agent performs an explicit transfer operation.

### Two agents edit the same path

1. Both agents independently edit `src/parser.rs` from the same base.
2. The first clean CAS advances shared WIP.
3. Refresh for the other agent defers the overlapping path and preserves its
   local file.
4. Its next reconciliation runs the existing three-way conflict path.
5. FeanorFS records the encrypted conflict and materializes authenticated
   base/local/cloud artifacts in private global state.
6. Automatic mutation pauses according to the existing pending-conflict gate.
7. A human or authorized integrator resolves explicitly with `conflicts keep`.
8. FeanorFS never creates or applies a semantic merge.

### Feedback arrives while an agent works

1. A reviewer sends a `request` or follow-up signal about snapshot `S1`.
2. The head wait completes immediately and the local event/runner layer reads
   the message through the existing bounded inbox.
3. The active harness decides when to present the feedback to the model.
4. The agent's next changes become snapshot `S2`.
5. A result references the settled snapshot actually inspected or tested, not
   merely the original request head.

FeanorFS transports and wakes. It does not inject prompt text into an arbitrary
model process; that remains the runner or orchestrator's responsibility.

### Working while offline

1. The active agent continues changing its isolated ordinary files.
2. The controller retains a bounded dirty flag and enters `offline`; it does
   not discard or repeatedly rewrite the worktree.
3. Retry uses the existing bounded transport backoff.
4. After reconnection, the controller observes the current head and runs the
   normal three-way reconciliation.
5. Non-overlapping work converges. Overlap becomes an explicit conflict.

---

## Product Contracts and Invariants

### Activation boundary

- `agent spawn` alone never activates continuous reconciliation.
- `agent run <name> -- ...` activates it for the child lifetime.
- An enabled configured runner activates it for that runner's configured agent.
- One process-lifetime lease owns reconciliation for an agent. A second owner
  fails clearly rather than racing.
- Stopping a runner or exiting `agent run` preserves the agent worktree and
  base state.
- Upgrades do not activate dormant agents automatically.

### Reconciliation boundary

- A local agent filesystem burst sets one bounded dirty flag; it does not
  enqueue one job per event.
- Exactly one land or refresh operation may mutate one agent at a time.
- A head change during reconciliation sets a rerun flag and is processed after
  the current operation; it does not start a concurrent operation.
- Existing `LandLock`, `SyncLock`, runner leases, CAS retries, stable file
  reads, download journals, and conflict gates remain authoritative.
- Automatic outbound reconciliation always uses `clean=false` and
  `propose=false`.
- Automatic inbound reconciliation never uses refresh `--replace`.
- Local volatility and retryable transport failures retry after debounce or
  backoff. Conflicts, corrupt state, unsafe identity, and unsupported schema
  enter attention instead of looping.

### Snapshot semantics

- The shared workspace head remains the only remotely authoritative WIP ref.
- Automatic agent snapshots use the agent name as author and preserve normal
  parent relationships.
- A successful automatic land advances the agent base exactly as manual land
  does today, allowing repeated reconciliation.
- Signal publication remains no-file-change CAS publication using the newest
  tree root.
- Controllers compare decrypted snapshot tree roots, not only head IDs. A
  signal-only head wakes messaging but causes no agent file refresh.
- A snapshot produced during active editing is WIP. Only an explicit
  verification/result signal claims that a particular settled snapshot was
  inspected or tested.

### Conflict semantics

- Non-overlapping paths converge automatically.
- Overlapping edits, edit/delete, file/directory, case, symlink/reparse, unsafe
  alias, and pending-conflict cases retain existing fail-closed behavior.
- No path is automatically selected as local or cloud.
- No textual or semantic merge is generated by the transport layer.
- Conflict artifacts remain outside the project and all legs remain
  recoverable.

### Communication semantics

- Existing `request`, `status`, `result`, and `blocked` message kinds remain.
- Existing body, scan, result, cursor, and reachability bounds remain.
- A follow-up is another bounded signal connected by `reply_to`; no chat table
  or high-frequency heartbeat is introduced.
- The activity stream may wake immediately but still provides best-effort,
  cursor-based delivery with explicit reset behavior.
- A terminal result must identify a reachable settled snapshot whose tree
  matches the work that was actually tested.
- Signals remain readable by every workspace participant, and sender identity
  remains advisory.

### Privacy and server boundary

- The hub sees no plaintext agent name, path, message, file content, or tree.
- Optional head waiting operates only on the already-visible opaque workspace
  ID and opaque head ID.
- No service argv, environment variable, log, event, or status snapshot gains
  credentials, invitations, E2EE keys, message bodies, or recovery material.
- The existing `GET /api/head` route may gain bounded wait semantics; no new
  route or database table is added.

---

## Runtime Model

### Controller inputs

One controller combines four input classes:

| Input | Source | Result |
|---|---|---|
| Agent filesystem mutation | `notify` watcher on the active agent worktree | Set local-dirty generation and schedule outbound reconciliation after debounce |
| Opaque head change | Bounded wait on existing head read | Read latest snapshot; refresh files only when the tree root changed |
| Agent signal | Existing graph-delta inbox after a head wake | Emit metadata wakeup and let runner/orchestrator consume the typed body |
| Lifecycle control | Child exit, runner stop, cancellation, supervisor restart | Flush safely when possible, persist status, release lease, never replay ambiguous execution |

### Controller state

The compact state machine exposes:

```text
starting
idle
local_dirty
reconciling_local
refreshing_remote
offline
needs_attention
stopping
```

Status fields:

```json
{
  "schema_version": 1,
  "agent": "worker",
  "active": true,
  "phase": "idle",
  "observed_head": "<64-hex>",
  "settled_snapshot": "<64-hex>",
  "pending_local": false,
  "deferred_count": 0,
  "attention": null
}
```

Persist only what is needed to recover safely: schema version, active owner
identity, last observed head/tree, dirty generation, last settled snapshot, and
an attention reason. Recompute file differences from authoritative snapshots
and files after restart. Do not persist message bodies, raw output, file
contents, or an unbounded event history.

### Coalescing and feedback-loop prevention

- Use the existing 500 ms debounce as the starting default.
- Drain bursts into one dirty generation.
- Keep at most one operation in flight and one “rerun required” bit.
- After land materializes the shared folder, the normal workspace watcher may
  observe those writes; its resulting sync must be a no-op against the already
  published root.
- After refresh writes an agent directory, mark the applied generation so the
  agent watcher does not republish unchanged remote work as agent-authored
  work.
- A signal-only head wakes inbox processing but does not touch either worktree.
- No-op land/refresh checks publish no new snapshot.

### Head-change waiting

Extend the existing authenticated `GET /api/head` query with optional bounded
parameters conceptually equivalent to:

```text
GET /api/head?workspace_id=<opaque>&after=<opaque-head>&wait_ms=<bounded>
```

Required behavior:

- If the current head differs from `after`, respond immediately with the
  current `HeadResponse`.
- If it matches, wait until a successful head CAS or the server-side timeout.
- Return the same bounded response shape; the client compares head IDs.
- Cap wait duration below the existing 60-second HTTP read-idle timeout.
- Bound concurrent waiters globally and per workspace.
- Notify waiters only after the head swap is durably accepted.
- Preserve authentication, migration, request limits, TLS, relay, and private
  CA behavior.
- Negotiate/fallback safely with older hubs. An old hub that ignores new query
  parameters must not cause a client busy loop.
- Embedded LocalHub mode may use an in-process notification or bounded polling
  fallback, but it must expose the same `ApiClient` result semantics.

This is a transport wakeup about an opaque CAS value, not an agent-aware server
feature.

---

## Implementation Tasks

Task IDs group reviewable capabilities. They do not override `TODO.md` or
require that unrelated tasks be implemented in strict numerical order. Checked
items are implemented and covered by the repository's source-level tests. The
only unchecked task below is field verification retained in `TODO.md` AI-6.

### Task CAD-1 — Freeze continuous reconciliation semantics

**Work:**

- [x] Define active, dormant, settled, dirty, offline, deferred, and attention
  states.
- [x] Confirm that continuous mode uses the current shared workspace head and
  does not introduce per-agent remote refs.
- [x] Confirm that `agent run` and an enabled configured runner are the v1
  activation boundaries.
- [x] Specify automatic land as shared-WIP synchronization, not code approval or
  release publication.
- [x] Specify which errors retry and which stop for attention.
- [x] Specify final reconciliation behavior on normal exit, cancellation,
  timeout, crash, and offline shutdown.
- [x] Define a versioned, bounded `ContinuousAgentStatus` JSON fixture before
  wiring CLI output.

**Implementation files:**

- `common/src/agent_contract.rs`
- `agent-core/src/agent.rs`
- `docs/agent-api.md`
- `docs/agent-communication.md`

**Done when:** one table-driven contract gives the same transition and error
classification for interactive agents, configured runners, CLI status, events,
and tests, without granting the controller semantic merge authority.

### Task CAD-2 — Add bounded opaque head-change waiting

**Work:**

- [x] Extend `HeadQuery` on the existing route with optional observed-head and
  bounded-wait parameters.
- [x] Add a bounded notification registry to `AppState` keyed only by opaque
  workspace ID.
- [x] Notify waiters after a successful `swap_head`, including file, signal,
  undo, land, and conflict-resolution publications.
- [x] Enforce global/per-workspace waiter limits and a maximum wait below the
  client's read-idle timeout.
- [x] Ensure timed-out or disconnected waiters release all permits and state.
- [x] Preserve current immediate GET behavior when optional parameters are
  absent.
- [x] Add compatible LocalHub behavior without adding a route or persistent
  table.
- [x] Add a capability signal or response behavior that lets a new client
  distinguish supported waiting from an old server ignoring query fields.

**Implementation files:**

- `common/src/lib.rs`
- `server/src/app.rs`
- `server/src/app/routes_objects.rs`
- `server/src/app/tests/publication.rs`
- `agent-core/src/hub.rs`
- `agent-core/src/hub/routes_objects.rs`
- `agent-core/src/head.rs`
- `agent-core/src/api.rs`

**Tests:**

- [x] Immediate return when the head already differs.
- [x] Wake after successful CAS and no wake after rejected CAS.
- [x] Timeout returns the unchanged head without an error.
- [x] Authentication and format checks still fail closed.
- [x] Workspace A publication never wakes workspace B as changed.
- [x] Waiter exhaustion is bounded and actionable.
- [x] Cancellation/disconnect releases capacity.
- [x] Old-hub fallback cannot spin or hammer the endpoint.

**Done when:** a compatible client can wait efficiently for an opaque head
change with no new route, agent metadata, plaintext content, or unbounded
resource use.

### Task CAD-3 — Add a reusable client head observer

**Work:**

- [x] Add `ApiClient::wait_for_head_change` with cancellation and a typed
  supported/fallback outcome.
- [x] Keep the 45-second periodic pass as a recovery backstop, not the healthy
  primary path.
- [x] Use bounded jitter/backoff after transport failures and unsupported old
  hubs.
- [x] Load the new snapshot only after observing a different head.
- [x] Compare snapshot tree roots so signal-only heads do not trigger file
  reconciliation.
- [x] Expose one reusable observer abstraction to the workspace watcher,
  events loop, and agent runner instead of reimplementing retry rules.
- [x] Ensure relay-backed and private-CA clients use the existing verified HTTP
  stack unchanged.

**Implementation files:**

- `agent-core/src/head.rs`
- `agent-core/src/api.rs`
- `client/src/watch.rs`
- `client/src/cli/events.rs`
- `client/src/cli/agent_runner.rs`

**Done when:** workspace sync, signal wakeups, and live-agent refresh begin from
one bounded head-observation contract, with periodic polling retained only for
compatibility and recovery.

### Task CAD-4 — Add active-agent ownership and lifecycle

**Work:**

- [x] Add a process-lifetime reconciliation lease for one `(workspace,
  agent-name)` pair.
- [x] Make `agent run` acquire the lease before launching its child and release
  it only after final reconciliation and child cleanup.
- [x] Reuse the configured runner's existing exact process/session ownership
  rather than creating a second competing lease.
- [x] Reject a simultaneous `agent run`, manual land/refresh, or second
  controller when it cannot prove exclusive mutation authority.
- [x] Preserve the existing runner invariant that an interrupted
  launching/running request is ambiguous and never automatically replayed.
- [x] Keep dormant agent directories inactive across supervisor restart and
  package upgrade.
- [x] Add a startup reconciliation that reads current files/head instead of
  trusting stale controller status.

**Implementation files:**

- `agent-core/src/agent/runner.rs`
- `agent-core/src/agent/continuous.rs`
- `agent-core/src/agent.rs`
- `client/src/cli/agent.rs`
- `client/src/cli/agent_runner.rs`
- `client/src/cli/supervisor.rs`

**Done when:** exactly one proven owner may continuously mutate an active agent
worktree, while stopping or upgrading preserves all files and never activates
dormant work.

### Task CAD-5 — Watch active agent worktrees safely

**Work:**

- [x] Start one recursive `notify` watcher for the active agent directory.
- [x] Reuse the existing event admission rules: ignore access-only events,
  internal state, temp artifacts, `.git`, `.jj`, symlinks, and excluded cache
  trees.
- [x] Debounce for 500 ms and drain a burst into one dirty generation.
- [x] Bound the channel and retain a dirty bit when the channel is full so an
  event cannot be silently lost.
- [x] Detect worktree removal/replacement and fail closed on identity mismatch.
- [x] Suppress refresh-produced filesystem events using observed content/base
  state rather than a timing-only ignore window.
- [x] On child exit, drain the current burst and make one bounded final
  reconcile attempt.

**Implementation files:**

- `client/src/watch.rs`
- `client/src/cli/agent.rs`
- `client/src/cli/agent_runner.rs`
- `client/src/cli/agent_live.rs`
- `agent-core/src/agent/runtime.rs`

**Tests:**

- [x] Atomic editor save and multi-file burst produce one scheduled pass.
- [x] Scanner access events produce no pass.
- [x] Refresh writes do not create an outbound echo loop.
- [x] Queue saturation retains eventual reconciliation.
- [x] Same-path directory replacement or unsafe alias stops mutation.
- [x] A final edit immediately before process exit is not ignored.

**Done when:** active agent file changes always cause an eventual bounded
reconcile attempt, without raw-event storms, alias traversal, or feedback
loops.

### Task CAD-6 — Automate outbound agent reconciliation

**Work:**

- [x] Add a guarded/internal land entry point analogous to
  `refresh_agent_guarded`, usable by the controller that already owns the exact
  agent lease.
- [x] Call existing land behavior with `clean=false` and `propose=false` after
  each quiet local generation.
- [x] Preserve pre-land full sync, three-way comparison, object upload,
  manifest publication, CAS retry/recovery, materialization, conflict
  registration, and agent-base advancement.
- [x] Treat a no-change result as settled without publishing another snapshot.
- [x] Retry file-volatility and retryable transport failures with bounded
  debounce/backoff.
- [x] Stop for pending conflicts, unsafe paths, corrupt state, incompatible
  format, or unprovable ownership.
- [x] Mark the exact resulting tree and reachable snapshot as settled.

**Implementation files:**

- `agent-core/src/agent/land.rs`
- `agent-core/src/agent/land/publish.rs`
- `agent-core/src/agent/land/materialize.rs`
- `agent-core/src/agent.rs`
- `agent-core/src/agent/continuous.rs`

**Done when:** an active agent's non-conflicting saved files reach the shared
encrypted workspace and advance its base without any user-visible land/push
operation, while every existing land fault-injection recovery remains valid.

### Task CAD-7 — Automate inbound refresh for active agents

**Work:**

- [x] Wake the controller after a different workspace head is observed.
- [x] Read the snapshot and skip file work when only the head ID changed and
  the tree root did not.
- [x] Bring the shared main worktree current through the existing sync engine
  before refreshing the agent.
- [x] Use `refresh_agent_guarded` with default safe behavior; never use
  `--replace` automatically.
- [x] Apply remote-only paths and defer paths with agent-local overlap.
- [x] Record refreshed versus deferred counts in bounded controller status.
- [x] Schedule one outbound pass only if genuine agent-local changes remain
  after refresh.
- [x] Pause on global pending-conflict state instead of repeatedly attempting
  refresh/land.

**Implementation files:**

- `agent-core/src/agent/refresh.rs`
- `agent-core/src/agent/continuous.rs`
- `client/src/commands.rs`
- `client/src/watch.rs`
- `client/src/cli/agent_runner.rs`

**Done when:** a change reconciled by one active agent appears automatically in
every other online active agent that has not touched the same path, while
overlapping local work remains intact and visible as deferred/conflicting.

### Task CAD-8 — Connect continuous code changes to existing communication

**Work:**

- [x] Drive signal inbox reads from the head observer rather than the
  30-second healthy-path event poll.
- [x] Keep `ffmsg1` wire format and message kinds unchanged unless a separately
  reviewed additive field is proven necessary.
- [x] Continue emitting metadata-only wakeups; fetch bodies through the typed
  inbox only after authorization.
- [x] Let an active harness subscribe through existing events/MCP or check its
  inbox at safe tool boundaries; do not inject text into an arbitrary child.
- [x] Treat follow-up feedback as another bounded request/reply chain, not a
  transcript stream.
- [x] Add settled snapshot/tree information to status so an agent can tie a
  result to the exact reconciled code it tested.
- [x] Require configured-runner completion to distinguish “terminal signal
  observed” from “final file generation settled.”
- [x] Preserve current no-replay and exactly-once disclaimers.

**Implementation files:**

- `agent-core/src/messages.rs`
- `client/src/cli/events.rs`
- `client/src/cli/agent_runner.rs`
- `client/src/cli/agent.rs`
- `skills/feanorfs-collaboration/SKILL.md`
- `skills/feanorfs-collaboration/references/protocol.md`

**Done when:** reviewers and agents can exchange snapshot-accurate feedback
during active development with prompt wakeups, while messages remain bounded
coordination checkpoints and an inactive model remains inactive.

### Task CAD-9 — Expose concise live status and activity

**Work:**

- [x] Add a bounded JSON projection for active agent, phase, observed head,
  settled snapshot, local-dirty flag, deferred count, and attention reason.
- [x] Integrate it into `agent status` without scanning every worktree during
  routine tray refresh.
- [x] Emit metadata-only lifecycle events such as
  `agent_reconcile_started`, `agent_reconciled`,
  `agent_reconcile_deferred`, and `agent_reconcile_attention`.
- [x] Derive a read-only activity view from existing snapshot history and
  signals rather than storing a second timeline.
- [x] Show changed-path counts by default; expose detailed paths only on an
  explicit local inspection surface.
- [x] Add a fixed, secret-free tray/doctor status for live reconciliation
  health and attention, reusing worker-published snapshots.
- [x] Ensure status never includes message bodies, file contents, credentials,
  endpoints, process arguments, or unbounded errors.

**Implementation files:**

- `common/src/agent_contract.rs`
- `common/src/tray_contract.rs`
- `client/src/cli/agent.rs`
- `client/src/cli/events.rs`
- `client/src/tray.rs`
- `client/src/cli/workspace.rs`
- `tray/`

**Done when:** a human or orchestrator can tell whether each active agent is
settled, syncing, offline, deferred, or blocked without reading logs or issuing
manual transfer commands.

### Task CAD-10 — Make retries, shutdown, and recovery fail-safe

**Work:**

- [x] Reuse existing retryable-transport classification and exponential
  backoff.
- [x] Keep one dirty generation and one rerun bit across transient failures;
  never create an unbounded operation queue.
- [x] Separate file-reconciliation recovery from runner execution recovery: a
  CAS/materialization retry may be safe even when replaying the model request
  is not.
- [x] On restart, recover a committed land through existing head/parent checks
  before doing new work.
- [x] On cursor reset, preserve files but stop automatic request execution
  according to current runner policy.
- [x] On conflict or corrupt controller state, publish bounded attention and
  require explicit repair/reset rather than discarding work.
- [x] Make stop wait for child/process-tree cleanup and the final reconcile
  outcome without hanging indefinitely.
- [x] Add fault injection at notification, scan, upload, manifest, CAS,
  materialization, refresh, status-write, and shutdown boundaries.

**Implementation files:**

- `agent-core/src/agent/continuous.rs`
- `agent-core/src/agent/land.rs`
- `agent-core/src/agent/runner.rs`
- `client/src/cli/agent_runner.rs`
- `client/src/cli/process_tree.rs`
- `client/src/cli/supervisor.rs`

**Done when:** every interruption recovers to one of three honest outcomes—
settled, retryably offline, or needs attention—with all files and conflict legs
preserved and no ambiguous model execution replayed.

### Task CAD-11 — Add compatibility and rollout guards

**Work:**

- [x] Keep optional head-wait parameters backward compatible with current
  immediate head reads.
- [x] Detect old hubs and retain bounded periodic polling without busy loops.
- [x] Require format v3 for continuous active-agent reconciliation.
- [x] Do not auto-enable the feature for pre-existing dormant agents during an
  upgrade.
- [x] Permit old clients to continue normal workspace sync while new clients
  use continuous agents against the same head.
- [x] Add a rollback path that stops continuous ownership but leaves the agent
  worktree, base ref, workspace files, snapshots, and credentials intact.
- [x] Keep automatic service argv credential-free and path-only.
- [x] Document any minimum compatible hub/client version before release.

**Implementation files:**

- `agent-core/src/api.rs`
- `agent-core/src/head.rs`
- `client/src/cli/agent.rs`
- `client/src/cli/agent_runner.rs`
- `client/src/cli/supervisor.rs`
- `docs/usage.md`

**Done when:** mixed supported versions degrade to current safe polling/manual
agent behavior rather than losing work, spinning, or activating unintended
agents.

### Task CAD-12 — Complete security, concurrency, and end-to-end verification

**Work:**

- [x] Add deterministic state-machine tests for every controller phase and
  transition.
- [x] Add two-agent HTTP-hub tests for non-overlapping convergence and
  overlapping conflict preservation.
- [x] Add concurrent land/signal/sync/undo/conflict-resolution CAS tests.
- [x] Add signal-only-head tests proving zero agent file writes and no refresh
  echo.
- [x] Add large-file, delete/edit, file/directory, case-conflict, lazy-file,
  symlink/reparse, and ignore-scope coverage.
- [x] Add offline editing/reconnect, hub restart, client restart, child exit,
  and supervisor restart scenarios.
- [x] Add old-hub compatibility and bounded head-wait load tests.
- [x] Inspect hub storage and logs for plaintext agent, message, path, and file
  leakage.
- [x] Prove the feature works in a directory without `.git` or `.jj` and never
  invokes either tool.
- [ ] Run an installed two-computer or network-isolated field scenario with
  two active agents, live feedback, a conflict, explicit resolution, and
  recovery after disconnection.

**Implementation files:**

- `agent-core/src/agent/tests.rs`
- `client/tests/`
- `server/src/app/tests/publication.rs`
- `test-support/`
- `scripts/` for an eventual installed-product smoke scenario

**Source implementation done when:** the automated portions of the matrix
pass with no lost updates, automatic merge, unbounded loop, plaintext
regression, Git dependency, or false claim of exactly-once agent execution.
That source gate is complete; installed/network field completion remains
`TODO.md` AI-6.

### Task CAD-13 — Update documentation and agent guidance

**Work:**

- [x] Update `README.md` to describe continuous active agents without
  push/pull or Git requirements.
- [x] Update `docs/usage.md` with activation, normal operation, status,
  attention, stop, offline, and recovery flows.
- [x] Update `docs/agent-communication.md` to distinguish continuous wakeup
  from high-volume chat and model activation.
- [x] Update `docs/agent-api.md` with live status/event contracts and explicitly
  state whether they are inside SDK-1 or CLI-only like the runner projection.
- [x] Update `docs/threat-model.md` with live-work propagation, incomplete-WIP,
  stale controller, waiter exhaustion, and active-process risks.
- [x] Update the collaboration skill so an active agent does not manually land
  or refresh, waits for a settled snapshot before a verification result, and
  still stops on cursor reset or attention.
- [x] Add troubleshooting for offline, deferred, conflict, duplicate owner,
  old hub, and final-reconcile failure.
- [x] Record the stable product contract in the nearest applicable
  `AGENTS.md` files during implementation.

**Done when:** a user and an installed collaboration-aware agent can operate
the feature from documentation alone without inferring Git, CRDT merge,
process sandboxing, chat streaming, or automatic semantic approval.

---

## Acceptance Criteria

Checked criteria are implemented and covered by deterministic, HTTP-hub, or
real-process source tests. The unchecked LAN measurement remains `TODO.md`
AI-6 and is not claimed by this change.

### Continuous development

- [x] An active agent's non-conflicting saved change reaches the shared
  workspace without an explicit sync, land, push, or pull command.
- [x] The same change reaches another online active agent that has not touched
  that path without an explicit refresh or pull command.
- [x] A burst of filesystem events produces at most one reconciliation at a
  time and eventual processing of the final generation.
- [x] A dormant agent never publishes work merely because FeanorFS starts or is
  upgraded.
- [x] Agent process exit makes one bounded final reconcile attempt and reports
  the honest outcome.

### Causality and communication

- [x] A file snapshot, feedback signal, subsequent file snapshot, and result
  remain reachable in causal order through the existing snapshot DAG.
- [x] A compatible healthy client wakes promptly after a head CAS instead of
  waiting for the periodic poll.
- [x] A signal-only head produces an inbox/event wakeup and zero filesystem
  changes.
- [x] A terminal result names a reachable settled snapshot matching the tree
  actually inspected or tested.
- [x] Signals remain bounded, low-volume, cursor-based, and explicit about
  reset/redelivery limitations.
- [x] A head or message change never claims to activate an inactive model.

### Conflict and data safety

- [x] Non-overlapping paths converge automatically.
- [x] Same-path and structural overlaps preserve all versions and require an
  explicit resolution.
- [x] FeanorFS never generates or applies a content merge.
- [x] Refresh never overwrites an agent-local edited path.
- [x] A CAS race, crash, network loss, process exit, or supervisor restart
  loses no accepted snapshot, local edit, conflict leg, or signal.
- [x] Unsafe aliases, symlinks/reparse points, and workspace identity changes
  fail closed.

### Performance and bounds

- [x] On a healthy LAN/private hub, a small saved change begins remote
  reconciliation without the 30/45-second polling delay.
- [ ] End-to-end small-file convergence between two idle active agents targets
  p95 under 3 seconds, excluding intentional backoff and conflict resolution.
- [x] There is at most one head waiter per long-lived worker/workspace and one
  mutation in flight per active agent.
- [x] Wait duration, waiter counts, event queues, retry state, persisted state,
  errors, status, and activity results are bounded.
- [x] Old-hub fallback cannot produce a tight request loop.

### Privacy and product boundaries

- [x] Hub storage, URLs beyond existing opaque identifiers, and logs contain no
  plaintext agent names, paths, messages, or file content.
- [x] No credentials, invitations, keys, recovery data, or message bodies
  enter service argv, environment variables, status snapshots, or metadata
  wakeups.
- [x] No new server route, agent table, path index, or project-local FeanorFS
  metadata is introduced.
- [x] The feature works in a plain directory without Git/Jujutsu and performs
  no Git/Jujutsu operation.
- [x] Agent worktrees are described as data isolation, never process
  sandboxing.

---

## Verification Matrix

This remains the complete product verification matrix. The local automated
suite covers the controller, HTTP/LocalHub head waiting, ownership, conflict,
signal-only, shutdown, and compatibility cases. Installed macOS/Windows/Linux
ownership, a network-isolated two-agent soak, and the LAN p95 measurement are
still pending under `TODO.md` AI-6.

| Scenario | Expected result |
|---|---|
| Agent A edits `a.rs`; Agent B is idle | A reconciles automatically; B refreshes automatically |
| A edits `a.rs`; B edits `b.rs` concurrently | Both changes become reachable and both agents converge |
| A and B edit `a.rs` from one base | All legs are preserved; automatic mutation pauses for explicit resolution |
| Agent writes ten files in one burst | Events coalesce; final generation is reconciled once settled |
| File changes again during land | Stable-read/CAS logic rejects stale input and retries the newest generation |
| Remote head advances during refresh | Current pass completes safely; one rerun observes the newer head |
| Signal publishes with unchanged tree | Inbox wakes; no worktree write or outbound echo occurs |
| Signal and land publish concurrently | Both remain reachable and the latest file tree is preserved |
| Agent refresh writes remote files | Agent watcher does not republish them as new local work |
| Agent exits immediately after a final save | Final burst is drained and one bounded reconcile is attempted |
| Hub is offline during edits | Local work remains; bounded retry resumes and three-way reconciles on reconnect |
| Hub restarts during a long wait | Wait fails retryably; fallback/backoff recovers without losing the observed cursor |
| Old hub ignores wait parameters | Client detects unsupported waiting and uses bounded periodic fallback |
| Head waiter limit is exhausted | Request fails/bounds cleanly; no resource leak or global outage |
| Land CAS succeeds before client crash | Existing recovery finishes materialization and advances the agent base safely |
| Runner child was active during crash | File state is recoverable; request execution is marked ambiguous and not replayed |
| Pending workspace conflict exists | Continuous mutation pauses and reports attention |
| Same agent is launched twice | Second reconciliation owner is rejected before mutation |
| Dormant dirty agent exists during upgrade | No automatic land or file modification occurs |
| Workspace contains no `.git`/`.jj` | Full continuous scenario succeeds |
| Hub data and logs are inspected | Only existing opaque IDs, ciphertext metadata, counts, and timing are visible |

---

## Success Metrics

| Metric | Target | Measurement |
|---|---:|---|
| Manual land/refresh operations in healthy active-agent scenario | 0 | End-to-end workflow test |
| Healthy compatible-hub remote polling delay | 0 occurrences | Timing/integration test |
| Lost local or accepted file updates | 0 | Deterministic race and fault-injection tests |
| Automatically merged conflict paths | 0 | Conflict contract tests |
| Signal-only head filesystem writes | 0 | Instrumented integration test |
| Duplicate simultaneous reconcile owners per agent | 0 | Lease/lifecycle tests |
| Tight-loop requests against an old/unavailable hub | 0 | Compatibility/backoff test |
| Plaintext agent/message/path/file leakage at hub | 0 | Storage, URL, and log inspection |
| Git/Jujutsu commands invoked | 0 | Plain-directory test and process instrumentation |
| Small-file LAN convergence target | p95 < 3 s | Two-client benchmark excluding backoff/conflict time |

Qualitative success means two agents can work for an extended session and see
each other's non-overlapping WIP and feedback without discussing
synchronization commands, while a genuine overlap remains obvious, preserved,
and recoverable.

---

## Risks and Mitigations

| Risk | Likelihood | Impact | Mitigation |
|---|---|---|---|
| An incomplete multi-file edit becomes visible as WIP | High | Medium | Debounce and stable reads; label automatic snapshots as WIP; require an explicit settled verification signal before treating a snapshot as tested |
| Automatic land changes the perceived safety boundary | Medium | High | Activate only for a proven active process; document shared-WIP semantics; keep dormant agents inactive and conflicts explicit |
| Refresh and watcher create an infinite echo | Medium | High | Compare tree/content/base state, suppress applied generations, publish no-op nothing, and add loop-count regression tests |
| Long polls exhaust server request capacity | Medium | High | Dedicated global/per-workspace bounds, bounded duration, disconnect cleanup, and load tests |
| Old servers ignore wait parameters and cause busy polling | Medium | Medium | Explicit support detection plus bounded periodic fallback and jitter |
| Signal head churn causes unnecessary file work | High without root check | Medium | Compare snapshot tree roots before sync/refresh |
| Two processes believe they own one agent | Medium | High | Exact process-lifetime reconciliation lease and fail-closed ownership checks |
| Agent writes while reconciliation reads | High | Medium | Existing descriptor-anchored stable reads, generation rerun, and volatility classification |
| Controller crash is confused with model execution crash | Medium | High | Separate idempotent file reconciliation from fail-closed runner request replay state |
| Continuous signals become chat/token streaming | Medium | Medium | Retain existing kinds and bounds; no heartbeats; use events only as metadata wakeups |
| Users assume every WIP snapshot passed tests | Medium | Medium | Distinguish settled transport state from verification results in status and docs |
| A malicious shared-key participant forges agent activity | Existing trust limitation | High | Keep attribution advisory; do not use names as authorization; document shared-key boundary |

---

## Alternatives Considered

### Continue explicit `land` and `refresh`

- **Pros:** No lifecycle changes and clear manual boundaries.
- **Cons:** Retains the episodic push/pull-shaped experience and stale active
  agents.
- **Decision:** Keep as diagnostics/recovery, not the normal active-agent flow.

### Give every agent a remotely visible branch/ref

- **Pros:** Partial work is visible without changing shared WIP.
- **Cons:** Adds remote ref semantics, retention rules, UI, and a branch-shaped
  model that the existing shared-head reconciliation does not require.
- **Decision:** Rejected. Reuse the current workspace head and
  conflict behavior.

### Run every agent directly in the shared main folder

- **Pros:** Existing workspace watcher already synchronizes it.
- **Cons:** Removes agent data isolation and lets partial agents overwrite each
  other before three-way reconciliation.
- **Decision:** Rejected.

### Automatically refresh but require explicit land

- **Pros:** Agents stay current without publishing partial work.
- **Cons:** Other agents still cannot observe development until a manual final
  operation; communication and code remain episodic.
- **Decision:** Rejected for continuous mode; this remains the current safe
  manual mode for dormant agents.

### Add CRDT or operation-level collaborative editing

- **Pros:** Fine-grained simultaneous editing.
- **Cons:** Requires content semantics, editor integration, new storage
  contracts, and implicit merge decisions; conflicts can still be semantically
  incompatible.
- **Decision:** Rejected.

### Add a WebSocket agent/session service to the hub

- **Pros:** Natural bidirectional streaming.
- **Cons:** Makes the hub agent-aware and duplicates existing authenticated
  head/snapshot transport.
- **Decision:** Rejected. Add bounded wait semantics to the existing opaque
  head read and keep messages in encrypted snapshots.

### Poll the head every 500 ms from every process

- **Pros:** Minimal code change.
- **Cons:** Continuous network/database load, poor scale, battery impact, and
  duplicated retry behavior.
- **Decision:** Rejected as the healthy path; bounded polling remains only as
  compatibility/recovery fallback.

---

## Rollout Requirements

Requirements 1-3, 6-7, and the deterministic portion of requirement 4 are
implemented. The network soak in requirement 4 and installed-product evidence
in requirement 5 remain `TODO.md` AI-6.

1. Land the head-wait and observer work behind compatibility detection with no
   behavior change for active agents.
2. Validate head waiting under HTTP, private CA, relay, and LocalHub paths.
3. Introduce continuous reconciliation as opt-in for development builds while
   retaining manual land/refresh.
4. Run deterministic race/fault tests and a network-isolated two-agent soak.
5. Validate installed macOS, Windows, and Linux process ownership and shutdown.
6. Make it the default only for agents actively launched through `agent run`
   or an enabled configured runner; never for dormant agents.
7. Keep an explicit stop/rollback route that preserves every file and snapshot.

The implementation may combine reviewable steps, but the compatibility and
activation guards must exist before automatic mutation ships broadly.

---

## Resolved Design Decisions

| Question | Adopted decision |
|---|---|
| What activates continuous reconciliation? | The lifetime of `agent run` or an enabled configured runner; spawn alone is insufficient |
| Should automatic reconciliation run while the model child is still active? | Yes, after a 500 ms quiet burst; resulting snapshots are explicitly WIP |
| How does an agent identify code safe to report as tested? | Wait until status exposes a settled snapshot/tree, then reference that snapshot in the result |
| Should signal bodies become a persistent conversation document? | No; keep bounded `ffmsg1` coordination and derive activity from snapshot history |
| Should head waiting use a new endpoint? | No; extend authenticated `GET /api/head` with optional bounded semantics |
| What happens on old hubs? | Retain current bounded periodic polling and manual-agent compatibility; never busy-loop |
| Should all spawned agents be refreshed while dormant? | No; dormant files/base state remain untouched until explicit action or activation |
| Does automatic land mean approval? | No; it only advances shared WIP. Verification and conflict resolution remain explicit |
| Do SDK/FFI/TypeScript bindings control the long-lived controller in v1? | Prefer a CLI/supervisor projection like the current runner; expose canonical status/events additively after the lifecycle is stable |

---

## Documentation Deliverables

- [x] `README.md`: continuous active-agent product description and no-Git
  requirement.
- [x] `docs/usage.md`: activation, activity, shutdown, offline, attention, and
  recovery runbook.
- [x] `docs/agent-communication.md`: causal code/signal flow, prompt wakeup,
  settled snapshots, and low-volume boundary.
- [x] `docs/agent-api.md`: status and event JSON contracts.
- [x] `docs/threat-model.md`: active propagation, incomplete WIP, waiter
  exhaustion, stale ownership, and process-isolation limits.
- [x] `skills/feanorfs-collaboration/`: live-agent lifecycle guidance and
  snapshot-accurate terminal replies.
- [x] Applicable `AGENTS.md` files: stable ownership, invariants, and
  verification commands after implementation.

---

## Repository Verification

Focused tests should run with each task. Final source verification must include:

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features --locked
```

Also run the existing SDK contract snapshots, Node/C smoke tests if their
contracts change, collaboration-skill validation, and server security checks.
The installed/network-isolated scenario is deliberately tracked as remaining
field evidence in `TODO.md` AI-6.

---

## References

- [FeanorFS README](README.md)
- [Agent communication](docs/agent-communication.md)
- [Agent API](docs/agent-api.md)
- [Usage](docs/usage.md)
- [Threat model](docs/threat-model.md)
- [Delta announcement](https://zed.dev/blog/introducing-delta)
- [DeltaDB overview](https://zed.dev/blog/introducing-deltadb)
