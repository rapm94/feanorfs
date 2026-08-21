# Agent communication (encrypted signals)

Named FeanorFS agents coordinate across machines through a low-volume
encrypted **signal** protocol stored in ordinary snapshot history. Signals
never become project files, never dirty Git, and require no new hub endpoint.

## Mental model

A signal is an ordinary encrypted format-v3 snapshot with **no file-tree
changes**:

- the latest workspace tree root;
- the latest workspace head as its parent;
- the sender name in `Snapshot.author`;
- the signal envelope in `Snapshot.message`.

The hub observes only ordinary ciphertext objects, object sizes, manifests,
head changes, and timing — never plaintext routing, bodies, or snapshot
context.

### Transport is not activation

`ffmsg1`, the CLI, MCP, SDKs, and the inbox only transport or read signals. A
standalone signal cannot wake an arbitrary model or process. An external
orchestrator may monitor events or poll an inbox and decide what to invoke.
Separately, an operator may explicitly configure a local agent runner; it
invokes only that runner's fixed local command and only for direct requests to
its configured agent. See [the operator runbook](usage.md#agent-runner).

## Envelope

The encoded message uses an exact versioned discriminator followed by
canonical compact JSON:

```text
ffmsg1:{"to":"mac-test","kind":"request","body":"Run iOS simulator tests","about_snapshot":"<64-hex>","reply_to":null}
```

Fields derived from the enclosing snapshot are not duplicated in the payload:

- `message_id` = the signal snapshot ID (immutable);
- `from` = `Snapshot.author`;
- `created_at_ms` = `Snapshot.created_at_ms`.

## Message kinds

| Kind | Meaning | Expected follow-up |
|---|---|---|
| `request` | Ask another agent to perform bounded work against a snapshot | optional `status`; exactly one correlated `result` or `blocked` terminal |
| `status` | Short progress update | none |
| `result` | Final bounded outcome | the requester consumes it |
| `blocked` | Final explanation of why the request cannot complete | the requester decides next action |

FeanorFS validates the enum and transports the body; it never interprets
success, failure, task ownership, paths, or content semantics.

## Validation

- `from` and `to` are FeanorFS agent names; `to="*"` is the only broadcast
  form.
- CLI callers derive `from` from `FEANORFS_AGENT`; when absent the sender is
  `human` unless an embedding supplies an explicit validated sender.
- `body` is non-empty UTF-8 after trimming and at most 8 KiB.
- `about_snapshot` defaults to the head observed when sending starts and must
  be a full reachable snapshot ID.
- `reply_to`, when present, must be a full reachable `ffmsg1` signal snapshot
  ID.
- Unknown snapshot messages remain ordinary history messages. Malformed
  `ffmsg1` payloads do not crash or block inbox traversal; they remain visible
  in raw history and are ignored by typed inbox reads.

## Send semantics

Sending is append-only through the existing workspace-head compare-and-swap.
Every CAS retry reloads both the latest head and its tree root before
constructing the next candidate, so a retry can never roll back visible files.
The signal's `about_snapshot` remains the caller-selected context even if
concurrent changes advance the head while the signal is being appended.

A successful send returns only after the encrypted snapshot object, the
reachability manifest, and the head swap succeed. Offline or exhausted-CAS
sends fail clearly; there is no local outgoing queue.

## Inbox semantics

Inbox reads are read-only and cursor-based. Within the scan and result bounds,
reusing the returned cursor gives repeatable delivery; after a reset or
overflow, delivery is explicitly best-effort and older signals may be missed:

- A caller supplies its recipient identity, an optional prior workspace-head
  cursor, and a bounded limit.
- The result cursor is the workspace head observed by the read.
- With a cursor, the inbox searches the graph delta: snapshots reachable from
  the current head but not reachable from the prior cursor.
- Without a cursor, the inbox returns the newest matching reachable signals up
  to the limit.
- Messages addressed to the recipient or `*` are returned; results are
  deduplicated by signal snapshot ID.
- Display ordering may use `(created_at_ms, message_id)`, but ordering is not
  authoritative across machines. Causality uses `reply_to` and immutable
  snapshot IDs.
- Traversal scans at most 10 000 snapshots per inbox call. An unreachable
  cursor, an exhausted scan, or more matching messages than the requested
  result limit sets `cursor_reset=true` and returns a bounded recent view;
  `cursor_reset=true` means the caller may have missed older signals.
- Reading never publishes acknowledgements, mutates history, or reveals read
  state to other participants.

## Local runner delivery

The optional runner is a local activation mechanism, not a new signal kind or
transport. Its compact sequence is:

1. A requester sends a direct `request` to the configured agent. Broadcasts
   remain readable through the normal inbox but are not runner work.
2. The configured runner durably admits that direct request, refreshes its
   agent worktree, and invokes its one fixed local command with one bounded
   JSON invocation on stdin.
3. The child uses the ordinary signal surface to publish one terminal `result`
   or `blocked` from the configured agent to the requester, correlated by
   `reply_to` and describing the snapshot actually inspected. A `status` is
   optional. This one-terminal child contract is not an exactly-once transport
   guarantee.
4. The runner observes that correlated terminal before completing the request.
   A `result` completes the request only when its `about_snapshot` exactly
   matches the final snapshot proven settled by the controller; a stale result
   is ignored. A correlated `blocked` terminal may still complete an
   offline/attention outcome. For a known child/invocation failure it attempts
   a generic correlated `blocked` fallback. If terminal delivery cannot be established, or its
   durable inbox/execution state is unsafe, it stops for attention; it does
   not replay the request. A local refresh/preparation failure before launch
   is `preparation_failed`: no child ran, but the pending request remains
   pinned until an operator repairs or inspects local state and explicitly
   discards/resets it.

Only explicit runner setup and start can enable this path. Events and polling
remain the alternative for an external orchestrator. The full lifecycle,
attention reasons, process ownership, foreground ownership, stop
acknowledgements, and recovery commands are in [Usage: Agent runner](usage.md#agent-runner).

The runner owns the complete child process tree. Unix (including macOS) uses a
fresh process group and bounded `TERM`/`KILL` escalation; Windows creates the
child suspended, assigns and verifies a private kill-on-close Job Object, then
resumes it. Timeout, cancellation, and direct-child exit tear down descendants
before the cycle is reported. The supervisor restarts workers after crashes or
clean exits with bounded backoff, but a persisted launching/running checkpoint
is marked ambiguous on restart rather than replayed.

`agent runner stop` disables admission before removing registry intent. When a
workspace-specific supervisor authority exists, it waits for a durable
registry-generation acknowledgement bound to the supervisor's exact native
process identity. A genuinely fresh or already-disabled, unregistered runner
with no supervisor authority has no possible child acknowledgement, so
stop/setup skips that wait. Stale registry, status, or acknowledgement
authority remains fail-closed. Inbox
delivery remains best-effort rather than exactly-once: reads can redeliver or
reset, and the runner does not claim exactly-once execution.

## Example exchange

```text
linux-dev -> mac-test
request: Run iOS simulator tests
about: abc123…

mac-test -> linux-dev
status: Testing abc123…
reply_to: def456…

mac-test -> linux-dev
result: Passed 42 tests on iPhone 16 simulator
about: abc123…
reply_to: def456…
```

## CLI

```text
feanorfs agent send <recipient> --kind <request|status|result|blocked> [options] <body>

Options:
  --about <snapshot-id>      Snapshot the request/result concerns; defaults to current head
  --reply-to <message-id>    Signal snapshot being answered
  --from <agent-name>        Explicit sender for controlled automation; otherwise FEANORFS_AGENT or human
```

```text
feanorfs agent inbox [options]

Options:
  --for <agent-name>         Recipient; defaults to FEANORFS_AGENT or human
  --after <head-id>          Previous inbox cursor
  --limit <n>                Bounded result count; default 50, maximum 1000
```

Human output is concise. Global `--json` emits the stable result types below.

## JSON contracts

```json
{
  "message_id": "<signal-snapshot-id>",
  "about_snapshot": "<context-snapshot-id>"
}
```

```json
{
  "cursor": "<observed-workspace-head>",
  "cursor_reset": false,
  "messages": [
    {
      "message_id": "<signal-snapshot-id>",
      "from": "linux-dev",
      "to": "mac-test",
      "kind": "request",
      "body": "Run iOS simulator tests",
      "about_snapshot": "<context-snapshot-id>",
      "reply_to": null,
      "created_at_ms": 1785852000000
    }
  ]
}
```

## SDK, FFI, TypeScript, and MCP

- Rust: `feanorfs-agent-core::Workspace::send_message(AgentMessageInput)`
  and `Workspace::inbox(AgentInboxQuery)`; `feanorfs-client` re-exports thin
  wrappers. Canonical types live in `feanorfs_common::agent_contract`.
- C: `ffs_agent_send(root, input_json)` and `ffs_agent_inbox(root, query_json)`
  take and return JSON strings (see `feanorfs.h`).
- TypeScript: `sendMessage(root, input)` and `inbox(root, query)` in
  `@feanorfs/agent` (see `contract.d.ts`).
- MCP: `agent_send` and `agent_inbox` tools with bounded schemas. Tool
  descriptions explain that all workspace participants can read messages,
  identity is advisory, and requests/results should carry exact snapshot
  context.
- NDJSON `events`: one `agent_message` metadata record per new signal with
  `message_id`, `from`, `to`, `kind`, and `about_snapshot` — never the body.
  Normal bounded delivery may redeliver; IDs are deduplicated in a bounded
  in-process cache. A reset/overflow emits a separate metadata-only
  `agent_message_cursor_reset` record before the bounded wakeups:
  `{"event":"agent_message_cursor_reset","cursor":"<observed-workspace-head>","cursor_reset":true}`.
  It contains no message body, ID, routing, path, or integrator fields and
  means older wakeups may have been missed.

## Continuous wakeup (active agents)

For active agents, wakeup is prompt but the wire format is unchanged: no
chat table, no heartbeats, no token streaming. The existing authenticated
`GET /api/head` route accepts optional bounded wait parameters
(`after`/`wait_ms`); waiters wake only after a durable head swap. Clients
share one bounded head observer, so the workspace watcher, the events loop,
and the agent runner all react to the same opaque head change instead of
independently polling it.

Causal continuity stays snapshot-linked:

```text
code snapshot S1
    -> feedback signal about S1 (about_snapshot=S1)
    -> code snapshot S2 (continuous land while active)
    -> result about the settled snapshot S2 actually tested
```

- A **signal-only head** (same tree root, new message) wakes inbox/event
  consumers and writes zero files. Controllers compare decrypted tree roots,
  not head ids, so feedback never triggers file work or refresh echoes.
- Wakeup is **not activation**: no head or message change ever starts an
  inactive model. The runner or orchestrator decides when to present
  feedback.
- A terminal `result` must reference a reachable settled snapshot matching
  the tree actually inspected or tested. Active agents read the settled
  snapshot from `agent status` (the bounded `live` projection) before
  replying, and the configured runner flushes the final file generation
  before delivering its terminal reply.
- Old hubs ignore the wait parameters; clients detect the unsupported
  response and keep bounded periodic polling with jitter — never a busy
  loop. Mixed versions degrade to the previous safe behavior.

## Collaboration skill

An installable agent skill ships at `skills/feanorfs-collaboration/`. It
teaches agents to identify themselves, check the inbox at lifecycle points,
refresh before acting on a request, send one bounded `status` update only when
useful, finish each request with exactly one `result` or `blocked` reply,
state the snapshot actually tested, avoid secrets and raw logs, and treat
routing/authorship/path claims as advisory. While an agent is active under
continuous reconciliation it never runs `land`/`refresh`/`sync` itself; it
waits for a settled snapshot before sending a verification result and still
stops on cursor reset or attention. For activation choices, see
[Transport is not activation](#transport-is-not-activation) and
[Local runner delivery](#local-runner-delivery).

## Randomized integrator assignment (`ffint1`)

When several agents produce overlapping work in one workspace, the user
should not become a manual merge queue. An authorized **dispatcher**
orchestrator may choose one temporary **integrator** per bounded batch by
random draw and coordinate through the existing signal channel. This is
randomized assignment, not a security-grade leader election:

- the hub never selects an integrator or interprets agent work;
- `ffmsg1` still transports only encrypted `request`/`status`/`result`/`blocked`
  signals — `ffint1` is a versioned profile inside the ordinary body, not a
  new message kind;
- identity, path claims, and assignments remain advisory, never access
  control;
- FeanorFS never merges file content; the integrator reconciles explicitly
  with `conflicts keep --file` after verifying a candidate;
- one dispatcher owns dispatch state per batch; a second dispatcher fails
  closed on the workspace orchestration lock.

### The draw

1. The dispatcher filters the roster: disabled/unavailable candidates,
   candidates missing a required capability, and explicit exclusions are
   removed; candidates that authored a conflicting side are preferred *not*
   to integrate (neutral subset).
2. A 128-bit assignment id and 256-bit selection nonce come from the OS
   CSPRNG.
3. `roster_fingerprint` = Blake3 of the canonical JSON array of the sorted
   final pool; each candidate scores
   `BLAKE3("feanorfs-integrator-selection-v1" ‖ len-prefixed workspace_id,
   about_snapshot, assignment_id, selection_nonce, roster_fingerprint,
   agent_name)`; ascending score, agent-name tie-break. First = selected;
   the rest is the immutable fallback order.

### Lifecycle

```text
created -> offered -> accepted -> active -> completed
                    |          | -> blocked
                    |          -> revoked -> offered(next)
                    -> timed_out -> offered(next)
```

- Before acceptance, a timeout may advance to the next recorded candidate.
- After acceptance, timeout alone never activates a second integrator: the
  dispatcher must stop/revoke the accepted agent, receive a `blocked` reply,
  or escalate to the user.
- The selected agent must refresh, verify the snapshot context, re-check for
  supersession before every mutating operation, work in an isolated agent
  workspace, and finish with exactly one `result` (a bounded digest) or
  `blocked` reply referencing the original request through `reply_to`.
- Cursor reset or lost dispatcher state fails closed: automatic integration
  stops and a human recovers state. Losing dispatcher state never authorizes
  a new integrator automatically.

### Dispatcher CLI

```text
feanorfs agent integrator assign --about <snapshot-id> \
  --candidate <agent-name>... [--require <capability>...] \
  [--exclude <agent-name>...] [--exclude-author <agent-name>...] \
  [--ack-timeout <duration>] <task-summary>

feanorfs agent integrator status [<assignment-id>]
feanorfs agent integrator revoke <assignment-id> --reason <summary>
feanorfs agent integrator resume [--ack-timeout <duration>] [--fallback-on-blocked]
feanorfs conflicts materialize [--about <snapshot-id>] [--path <path>]...
```

Human output shows only the selected agent, assignment state, and next
action; automation uses global `--json`.

### Conflict materialization

First-class encrypted conflict entries live in the tree, but the
human-readable `.original`/`.local`/`.cloud` artifacts and pending rows are
local state. A selected integrator on another computer materializes the
authenticated triple read-only with `conflicts materialize`: artifacts are
written under protected global FeanorFS state, a local pending row is
registered, the head is never changed, and stale/already-resolved conflicts
are refused. Resolution then proceeds through the ordinary explicit
`conflicts keep <path> --local|--cloud|--both|--file <reconciled>` path.

### Integrator digest

The terminal result carries one bounded digest: outcome, verification
summary, counts (landed/resolved/remaining), at most 10 risks, and at most
one decision question. No code, patches, raw logs, credentials, `.env`
values, or model reasoning are ever placed in signal bodies.

### Exact conflict resolution jobs (`ffres1`)

Automatic resolution is a separate exact-fingerprint pipeline from the
explicit `conflicts keep` path. The engine binds every candidate to the
exact current conflict and only the engine publishes; a harness produces a
candidate, and the hub never merges file content.

1. **Prepare** (`feanorfs agent resolution prepare <path> --reason
   exhausted|violated --detail <text>`) requires a real current conflict in
   the workspace head and a typed prevention-exhausted/violated reason.
   Legacy unfingerprinted records (path-only, no identity sidecar) are
   visible and manually resolvable but can never enter automatic
   prepare/apply. The returned `ResolutionJob` carries the exact
   `conflict_fingerprint`, assignment/attempt/owner, engine-owned immutable
   `candidate_destination`, and the verification policy reference. Prepare
   never mutates the worktree, registry, artifacts, or head.
2. **Submit** (`feanorfs agent resolution submit <job-id> --result
   <file-or->`) records one validated resolver result. **Submit never
   applies**: it checks result schema/bounds, assignment/attempt/owner/
   fingerprint, and re-hashes the immutable candidate, then records the
   result without touching the worktree, registry, artifacts, or head.
   Replay (a second result for one job) is rejected.
3. **Apply** (`feanorfs agent resolution apply <job-id>`) is the only
   publishing operation. It revalidates every identity field and the
   candidate descriptor immediately before a single CAS; a lost CAS discards
   the plan and restarts complete validation (never a path-removal retry).
   Any typed stale outcome (`head_changed`, `identity_mismatch`,
   `assignment_revoked`, `candidate_hash_mismatch`, …) leaves the current
   conflict and its evidence untouched. Cleanup of the job, candidate, and
   artifacts happens only after confirmed publication.
4. **Status** (`feanorfs agent resolution status [<job-id>]`) reads a
   bounded ids/state/counts projection (assignment state, submitted outcome,
   timestamps) — never paths, identities, or bodies. NDJSON events emit
   metadata-only `resolution_prepared` / `resolution_submitted` /
   `resolution_applied` / `resolution_revoked` wakeups on state transitions.

The human path (`conflicts keep`, tray counts/status) remains fully
functional; the tray only projects resolution counts and never resolves.

### Troubleshooting

| Symptom | Meaning | Next step |
|---|---|---|
| `another integrator dispatcher is active` | Two dispatcher processes contend for one workspace | Stop the other dispatcher; one dispatcher per batch is required |
| Assignment stays `offered` past the timeout | Selected agent is offline or not polling | `agent integrator resume --ack-timeout <duration>` advances to the next recorded candidate |
| `requires_human` state | Inbox cursor reset or uncertain dispatcher state | Stop automatic mutation; recover the orchestrator state (`~/.feanorfs/workspaces/<opaque>/orchestrator/integrator-state.json`); do not infer results from signal history |
| Accepted integrator goes quiet | Post-acceptance timeout must not silently fall back | Stop/revoke the controlled agent process (`agent integrator revoke <id> --reason …`) or ask the user |
| `no active assignment` on resume | Nothing to resume (or state was lost) | If state was lost, recover it; losing dispatcher state never authorizes a new integrator automatically |
| `conflicts materialize` refuses | Conflict already resolved, legs changed, or snapshot stale | Re-run against the current head; resolve the pending conflict first |
### End-to-end example: two workers, one integrator, one digest

1. `linux-dev` and `mac-test` finish bounded tasks against snapshot `abc123…`
   and send `result` signals to the dispatcher (`human`).
2. The dispatcher assigns one batch:

   ```bash
   feanorfs agent integrator assign --about abc123… \
     --candidate linux-dev --candidate mac-test --candidate ci1 \
     --require rust --exclude-author linux-dev --exclude-author mac-test \
     "Integrate the parser implementation and its tests"
   # Assignment 57d576aa offered to 'ci1' (attempt 0).
   # Fallback order: mac-test, linux-dev.
   ```

   The draw used the neutral pool (ci1 authored neither side); the nonce,
   roster fingerprint, and ranking are recorded for audit.
3. `ci1` checks its inbox, refreshes, verifies the tree, and sends one
   `status` acceptance (`ffint1` `accepted`). The dispatcher's
   `agent integrator resume` moves the assignment to `accepted`.
4. `ci1` materializes the encrypted conflict legs on its own machine
   (`conflicts materialize`), reconciles compatible overlaps in its isolated
   agent workspace, runs the tests, and resolves explicitly with
   `conflicts keep <path> --file <reconciled>`.
5. `ci1` sends one `result` with an `ffint1` digest:

   > Integrated both agent results. Resolved 3 compatible overlaps.
   > 84 tests passed. No user decision is required.

6. The dispatcher observes `completed`; the user sees the digest and nothing
   else. When the implementations disagree on product behavior, step 5
   instead carries one focused question and the assignment stops at
   `requires_human` until the user decides — no version is ever discarded.

## Work-intent protocol (`ffwork1`)

Agents announce and negotiate intended write scope before editing through
encrypted `ffwork1` profiles carried inside ordinary `ffmsg1` signal bodies.
This is a prevention and coordination layer, not access control: identity,
path claims, and authorship remain advisory. There is no new message kind, no
server route, and no hub involvement.

### Model

- An **author** proposes bounded work for one task (`work_intent`: task id,
  causal base, paths, concerns, dependencies, capabilities, author sequence).
  Paths are canonical portable workspace-relative paths or the supported
  `dir/**` containment glob; collections must be sorted and unique.
- The proposal may name one **coordinator**, or the operating context
  supplies one (default `human`). Only that identity's decisions apply; the
  hub never decides.
- A **decision** (`work_decision`) accepts, rejects, narrows (a reduced scope
  inside the proposal), orders (relative to another proposal), or explicitly
  accepts elevated overlap risk (`accept_overlap` with derivable overlap
  entries). A decision references the exact proposal message id.
- After acceptance the author may **amend** scope/dependencies, **yield**
  (relinquish accepted overlap while preserving local work), **settle**
  (attach verification evidence naming the inspected snapshot), and finish
  with **complete** or **block** (terminal). A coordinator may **supersede**
  an applied decision, returning the proposal to pending.

### Deterministic projection

A local reducer in each workspace observes signals through the existing
`signals_since` traversal — never a second message store — and projects
deterministic state under the protected `orchestrator/` boundary
(`work-state.json`, advisory lock, atomic replacement, schema-versioned):

- Author transitions key by `(task_id, agent, sequence)`; a transition cannot
  decrease sequence, change immutable identity, skip required states, or act
  for another author.
- Decisions key by exact proposal message id plus the authorized coordinator
  identity. A decision from any other identity is retained as invalid
  evidence and never changes accepted state.
- Causal dominance wins. Concurrent same-author updates use the canonical
  message id only as a deterministic tie-breaker; the losing branch is
  retained as bounded protocol evidence. Duplicate delivery is idempotent.
- Clock fields are display/liveness hints only; no timeout alone transfers
  authority or implies acceptance.
- After a cursor reset the reducer rebuilds from the bounded reachable
  closure and reports `projection_incomplete=true` instead of inferring
  acceptance it cannot prove. State is bounded: active tasks, terminal
  history, evidence, seen ids, and pending transitions each have explicit
  caps; bound exhaustion drops deterministically and is counted.

### CLI

```text
feanorfs agent work propose --task <id> --path <p>... [--agent <name>] \
  [--sequence <n>] [--causal-base <id>] [--coordinator <name>] \
  [--concern <c>]... [--dependency <task>]... [--capability <cap>]... [--about <id>]

feanorfs agent work decide <proposal-message-id> --kind accept|reject|narrow|order|accept-overlap \
  [--reason <r>] [--path <p>]... [--concern <c>]... [--after <id>] [--overlap <json>]...

feanorfs agent work amend --task <id> --intent <id> [--path <p>]... [--concern <c>]... [--dependency <task>]...
feanorfs agent work yield --task <id> --intent <id> [--reason <r>]
feanorfs agent work settle --task <id> --intent <id> --inspected <snapshot> --verification <status> --summary <text>
feanorfs agent work complete --task <id> --intent <id> --outcome <text>
feanorfs agent work block --task <id> --intent <id> --reason <text>
feanorfs agent work status [--coordinator <name>]
```

Human output never claims a proposed scope is accepted: proposals report
`proposed` until an observed decision applies, and a cursor reset or bound
exhaustion marks the projection incomplete. Automation uses global `--json`
(`WorkSendResult` / `WorkStatusResult`; see [agent-api.md](agent-api.md)).

### Events

`feanorfs events` emits metadata-only `work_*` wakeups (IDs/state/counts
only — never path sets, scopes, or bodies):

```json
{"event":"work_intent","message_id":"<64-hex>","from":"linux-dev","to":"*","kind":"request","about_snapshot":"<64-hex>","task_id":"parser-impl","agent":"linux-dev","sequence":1}
{"event":"work_decision","message_id":"<64-hex>","task_id":"","proposal_message_id":"<64-hex>","decision":"accept"}
{"event":"work_settled","message_id":"<64-hex>","task_id":"parser-impl","sequence":3,"verification":"passed"}
```
