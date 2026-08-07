# PRD and Task Plan: Encrypted Agent Messaging and Randomized Integrator Assignment

**Date:** 2026-08-06

**Status:** Proposed

**Scope:** Encrypted cross-machine agent messaging plus consumer-layer orchestration for choosing one temporary integrator, coordinating workers, reconciling conflicts safely, and presenting a bounded human summary

---

## Summary

When multiple AI agents work on the same FeanorFS workspace from different
computers, they need a low-volume way to communicate intent and outcomes, and
the user should not become a manual merge queue. Agents will exchange encrypted
snapshot-tied `request`, `status`, `result`, and `blocked` signals without
creating project files. A long-lived orchestrator will use those signals to
coordinate workers and choose one temporary **integrator** for each bounded
work batch. Selection will be random, fair among eligible agents, reproducible
from the recorded draw, and independent of the hub.

The integrator collects worker outcomes, inspects preserved conflict legs,
creates and verifies reconciled candidates, and reports a concise digest. The
user is interrupted only for consequential or genuinely ambiguous decisions.

FeanorFS remains dumb storage and smart transport:

- the hub never selects an integrator or interprets agent work;
- `ffmsg1` continues to transport only encrypted `request`, `status`, `result`,
  and `blocked` signals;
- path ownership and agent identity remain advisory;
- FeanorFS never merges file content automatically;
- a consumer agent may explicitly submit a verified reconciled file through
  the existing conflict-resolution API.

This is randomized assignment, not a distributed security-grade leader
election. One authorized orchestrator is the dispatcher for a work batch.

---

## Problem Statement

### What problem are we solving?

Two or more agents can produce large, overlapping changes on different
computers. FeanorFS correctly preserves conflicts, but exposing every conflict,
patch, raw log, and competing explanation to the user would make the product
exhausting to supervise. The user may respond by ignoring conflicts, accepting
an arbitrary version, or abandoning multi-agent workflows.

Encrypted agent signals help agents communicate intent, but the current
protocol deliberately does not establish ownership, select a decision maker,
or resolve file content. If every agent believes it may integrate the work,
the system can produce duplicate effort, contradictory resolutions, and a
large volume of user-facing output.

Naive random selection is also unsafe. Independent machines can observe
different candidate sets, select different leaders, or select an offline agent.
A random choice without a bounded assignment lifecycle can leave a late agent
integrating after a fallback agent has already taken over.

### Why now?

Format-v3 snapshots, first-class encrypted conflict entries, `ffmsg1` signals,
agent workspaces, explicit conflict resolution, MCP, and NDJSON wakeups already
provide the required building blocks. Defining the arbitration layer now keeps
task scheduling and semantic decisions out of the hub while preventing the
human experience from degrading as more agents participate.

### Who is affected?

- **Primary users:** Developers supervising several coding agents across
  machines.
- **Primary automation users:** Agent orchestrators dispatching bounded work
  and consuming MCP, JSON, SDK, or NDJSON event surfaces.
- **Secondary users:** Worker agents contributing code, tests, review, or
  platform-specific validation.

---

## Goals

- Choose exactly one temporary integrator for a bounded work batch under one
  dispatcher.
- Distribute assignments randomly and fairly among eligible agents.
- Filter by capability and availability before drawing.
- Prefer an integrator that did not author either side of a conflict when one
  is available.
- Make the draw reproducible and testable without making it a security claim.
- Provide a safe fallback when a selected candidate does not accept or reports
  a candidate-specific blocker.
- Prevent automatic fallback after acceptance unless the active integrator is
  explicitly stopped, blocks, or is revoked by the dispatcher.
- Let the integrator reconcile routine conflicts without dumping code on the
  user.
- Escalate only bounded decisions that require human authority.
- Preserve every original conflict leg and an audit trail of the chosen
  resolution.
- Keep the hub, server schema, and project directory unchanged.

## Non-Goals

- Cryptographically proving agent identity or leadership.
- Treating recipient names, path claims, or integrator assignments as access
  control.
- Peer-to-peer consensus with no dispatcher.
- Starting or hosting an inactive model inside FeanorFS.
- General project management, backlog planning, chat, or token streaming.
- Sending patches, complete files, raw logs, credentials, `.env` values, or
  recovery material through agent signals.
- Automatically merging file content inside the FeanorFS transport engine.
- Automatically choosing the Git publication machine or performing Git/Jujutsu
  operations.
- Silently choosing between incompatible product, security, or data-model
  behavior.
- Weighted scheduling, performance-based ranking, or worker scoring in v1.

---

## Roles and Authority

| Role | Responsibility | Authority |
|---|---|---|
| Human/project owner | Defines product intent and escalation policy | Final authority for ambiguous or consequential decisions |
| Dispatcher/orchestrator | Builds the eligible roster, performs the draw, invokes agents, tracks assignment state, and selects fallbacks | Sole assignment authority for one work batch |
| Temporary integrator | Reconciles worker results, runs verification, explicitly resolves safe conflicts, and produces one digest | May act only within the accepted assignment and configured policy |
| Worker agent | Performs one bounded request and returns `result` or `blocked` | Owns task execution, never exclusive file ownership |
| FeanorFS client/agent layer | Preserves versions, transports signals, and exposes explicit resolution operations | Enforces storage and transition invariants, not semantic correctness |
| FeanorFS hub | Stores opaque objects, manifests, and heads | No agent, task, ownership, or merge authority |

The dispatcher is stable for the duration of a batch; the integrator is
temporary and randomly selected. If no external dispatcher is running, the
feature is unavailable rather than pretending the agents reached consensus.

---

## Product Decisions

1. **One dispatcher per active batch.** Random integrator assignment does not
   remove the need for one process that owns dispatch state.
2. **One integrator per batch.** Assignment is tied to an immutable
   `about_snapshot` and a unique `assignment_id`.
3. **Eligibility precedes randomness.** Platform, toolchain, model permissions,
   availability, and user policy are hard filters, not weights.
4. **Neutrality precedes randomness.** If a capable candidate did not author
   either conflicting result, draw from the neutral pool. Otherwise draw from
   the full eligible pool and disclose that no neutral integrator was
   available.
5. **The draw is auditable.** Use an OS-generated nonce and canonical candidate
   roster to produce a deterministic ranking that can be recomputed.
6. **Equal chance in v1.** Every candidate in the final pool has the same
   selection probability.
7. **Fallback order is part of the draw.** Failure to acknowledge selects the
   next ranked candidate rather than performing a new, manipulable draw.
8. **Acceptance changes timeout behavior.** Before acceptance, timeout may
   fall back automatically. After acceptance, timeout alone must not activate a
   second integrator; the dispatcher must first stop/revoke the old invocation
   or obtain a `blocked` reply.
9. **Signals remain low-volume.** The assignment, optional acceptance, and one
   final `result` or `blocked` response are coordination checkpoints, not chat.
10. **Human output is decision-oriented.** Default output contains outcomes,
    verification, risk, and unresolved decisions, never raw code or logs.
11. **Resolution remains explicit.** The integrator may create a reconciled
    file, but resolution occurs only through an explicit conflict operation
    such as `conflicts keep --file`.
12. **The hub remains unchanged.** Selection, state, and semantic work live in
    the consumer/agent layer.

---

## User Experience

### Normal successful batch

1. The dispatcher receives two or more worker results tied to a snapshot.
2. It filters the configured agent roster for required capabilities and
   availability.
3. It randomly ranks the eligible candidates and requests the first candidate
   to integrate the batch.
4. The selected agent refreshes, verifies the assignment context, accepts, and
   performs the integration in an isolated agent workspace.
5. Non-overlapping changes land normally. Overlapping changes remain preserved
   until the integrator creates and verifies a candidate.
6. The integrator explicitly resolves routine conflicts and sends one bounded
   result.
7. The user sees a digest such as:

   > Integrated both agent results. Resolved 3 compatible overlaps. 84 tests
   > passed. No user decision is required.

### Human decision required

The user sees one question with the minimum necessary context:

> Both implementations are valid but disagree on expired-session behavior.
> Should the product redirect to login or show an inline error? No version has
> been discarded.

The default view must not include patches or complete source files. Detailed
paths, diffs, conflict artifacts, and verification logs remain available on
demand.

### Selected integrator does not respond

1. The dispatcher waits for the configured pre-acceptance acknowledgement
   timeout.
2. It marks the attempt superseded in private dispatcher state.
3. It assigns the next candidate from the recorded ranking.
4. A late candidate must re-check assignment state before any mutating action
   and stop when its attempt has been superseded.

### Active integrator stops responding

Once an assignment is accepted, the dispatcher must not silently start a
second integrator. It must first do one of the following:

- receive a `blocked` reply;
- confirm that the controlled agent process has stopped;
- explicitly revoke the accepted assignment and record the reason; or
- escalate the uncertain state to the user.

This rule minimizes split-brain integration when network or model state is
ambiguous.

---

## Selection Logic

### Inputs

Each draw uses:

- `assignment_id`: 128 bits from the operating-system CSPRNG, encoded as hex;
- `selection_nonce`: 256 bits from the operating-system CSPRNG;
- `workspace_id`: available only inside the trusted client process and never
  emitted in user-facing output;
- `about_snapshot`: full reachable format-v3 snapshot ID;
- `task_summary`: bounded plain-language objective;
- `required_capabilities`: normalized, sorted capability identifiers;
- `candidates`: explicit candidate descriptors from the dispatcher;
- `conflict_authors`: optional names of agents that produced conflicting legs.

Candidate descriptors contain:

```json
{
  "name": "mac-test",
  "capabilities": ["ios", "rust"],
  "enabled": true,
  "available": true
}
```

Names use the existing FeanorFS agent-name validation. Capabilities are
lowercase ASCII identifiers with a bounded length and count. Duplicate names
or capabilities are rejected rather than silently normalized away.

### Eligibility

The dispatcher must:

1. Reject an empty roster, invalid names, duplicate agents, and unbounded input.
2. Remove disabled or unavailable candidates.
3. Remove candidates missing any required capability.
4. Apply explicit user exclusions.
5. Build a neutral subset that excludes conflict authors.
6. Use the neutral subset when non-empty; otherwise use the full eligible set
   and set `neutral_integrator=false`.
7. Escalate with a bounded reason when no candidate remains.

Presence must not be inferred solely from a recent `ffmsg1` signal. Signals
have no read receipts and cannot wake an inactive model. Availability comes
from the authorized dispatcher that owns or can invoke the participating
agent runners.

### Auditable random ranking

Build canonical JSON for the sorted final candidate pool and hash it to obtain
`roster_fingerprint`. For each candidate compute:

```text
score = BLAKE3(
  "feanorfs-integrator-selection-v1" ||
  len(workspace_id) || workspace_id ||
  about_snapshot ||
  assignment_id ||
  selection_nonce ||
  roster_fingerprint ||
  len(agent_name) || agent_name
)
```

Sort ascending by the 32-byte score and then by agent-name bytes as a
deterministic collision tie-breaker. The first candidate is selected and the
remaining order is the fallback order.

Length-prefix every variable-width value. Canonicalization must be implemented
once and reused across CLI, SDK, MCP, C, and TypeScript surfaces. Tests inject a
fixed nonce; production always uses the OS CSPRNG. User-provided production
seeds are not supported in v1.

The recorded assignment may expose the nonce, roster fingerprint, ranked agent
names, and algorithm version to authorized workspace participants. These are
audit values, not secrets. The workspace ID must remain out of messages and
logs; participants can verify through the trusted SDK without printing it.

### Fairness

V1 fairness means equal probability within the final eligible pool over many
independent draws. It does not guarantee strict round-robin rotation. The test
suite must perform a deterministic distribution check over many injected
nonces with a tolerance chosen to detect an obvious bias without creating a
flaky statistical test.

---

## Assignment Lifecycle

### States

```text
created -> offered -> accepted -> active -> completed
                    |          |          -> blocked
                    |          -> revoked -> offered(next)
                    -> timed_out -> offered(next)
```

Terminal states are `completed`, `blocked`, `requires_human`, and `cancelled`.
A timed-out pre-acceptance attempt is terminal for that candidate but not for
the overall assignment.

### Invariants

- An assignment concerns exactly one `about_snapshot`.
- Attempts use the immutable ranked candidate order.
- At most one attempt is accepted at a time.
- A candidate must refresh and re-check for supersession before acceptance,
  before publishing a reconciled snapshot, and before resolving a conflict.
- A final result states the snapshot actually inspected and tested.
- A result for a different file tree keeps the original request in `reply_to`
  and names both snapshots in the body/digest.
- Every accepted request finishes with exactly one `result` or `blocked` reply.
- A dispatcher restart loads durable state before emitting another assignment.
- A second dispatcher must fail closed on the workspace orchestration lock.
- Losing dispatcher state never authorizes a new integrator automatically.
- Worker and integrator messages remain advisory; safety comes from cooperative
  lifecycle checks and one dispatcher, not cryptographic identity.

### Failure classification

| Failure | Behavior |
|---|---|
| Candidate unavailable before acceptance | Select next ranked candidate |
| Candidate lacks runtime/toolchain after selection | Record candidate-specific blocker and select next |
| Task is impossible for every candidate | Escalate one bounded blocker |
| Accepted integrator explicitly blocks | Revoke/close attempt, then select next only if blocker is candidate-specific |
| Accepted integrator becomes unreachable | Stop/revoke if process control is certain; otherwise ask the user before fallback |
| Snapshot context cannot be established | Block; never claim or resolve against an unverified tree |
| Tests fail after reconciliation | Preserve candidate and conflict legs; escalate concise failure summary |
| Head changes during resolution | Reload state, revalidate assignment and conflict, then retry bounded CAS; never reuse a stale root |
| Inbox cursor resets | Treat coordination history as potentially incomplete and stop automatic mutation until state is recovered |

---

## Encrypted Messaging Foundation

Random integrator assignment depends on a complete, reliable-enough
coordination channel. Messaging is therefore part of this PRD, not an assumed
external service.

### Mental model

An agent signal is an ordinary encrypted format-v3 snapshot with no file-tree
changes:

- the current encrypted workspace tree root;
- the current workspace head as its parent;
- the sender name in `Snapshot.author`;
- a versioned signal envelope in `Snapshot.message`.

The signal never becomes a project file, never dirties Git, and requires no
new hub endpoint or plaintext server index. The hub observes the same opaque
objects, manifests, head swaps, sizes, and timing that it observes for normal
snapshot traffic.

### `ffmsg1` envelope

The canonical compact envelope is:

```text
ffmsg1:{"to":"agent-b","kind":"request","body":"Implement parser tests without changing the public API","about_snapshot":"<64-hex>","reply_to":null}
```

The enclosing encrypted snapshot supplies:

- `message_id`: the signal snapshot ID;
- `from`: `Snapshot.author`;
- `created_at_ms`: `Snapshot.created_at_ms`.

These derived fields must not be duplicated inside the envelope. Unknown
future message discriminators and malformed `ffmsg1` payloads remain ordinary
history messages and cannot crash or permanently block typed inbox reads.

### Message kinds and lifecycle

| Kind | Purpose | Required behavior |
|---|---|---|
| `request` | Assign one bounded task against a snapshot | Recipient refreshes and eventually returns `result` or `blocked` |
| `status` | One useful acceptance/progress checkpoint | No acknowledgement required; not a heartbeat stream |
| `result` | One terminal bounded outcome | Includes what changed/was verified and the actual inspected snapshot |
| `blocked` | One terminal bounded blocker | Explains the missing decision, capability, context, or dependency |

Every accepted request produces exactly one terminal `result` or `blocked`
reply. `status` is optional and limited to one per request unless a higher-level
orchestrator explicitly defines a different bounded contract. Messages are not
chat, planning transcripts, token streams, or build logs.

### Intent contract

A worker request must make intent clear without shipping code. It contains:

- the desired outcome;
- the exact `about_snapshot`;
- the bounded subsystem or paths involved when known;
- important constraints, including behavior that must not change;
- the acceptance check or expected evidence;
- the original request in `reply_to` for every follow-up.

Example:

```text
dispatcher -> agent-a
request: Add token-refresh handling in client/auth.rs. Preserve the public API.
         Run the focused auth tests and report only the outcome.
about: abc123…

agent-a -> dispatcher
result: Added refresh handling without changing the public API. 42 auth tests passed.
about: abc123…
reply_to: def456…
```

Path statements are coordination hints, not locks or exclusive ownership. If a
worker discovers that it must cross another task's boundary, it reports a
bounded blocker or requests coordination before creating avoidable overlap.

### Validation and limits

- Sender and recipient use existing FeanorFS agent-name validation.
- `to="*"` is the only broadcast form; senders cannot be `*`.
- Body is non-empty UTF-8 after trimming and at most 8 KiB.
- `about_snapshot` is a full reachable snapshot ID and defaults to the head
  observed when sending begins.
- `reply_to`, when present, is a full reachable `ffmsg1` signal snapshot ID.
- A terminal reply to an accepted request requires `reply_to`.
- CLI senders use `FEANORFS_AGENT`, then `human`, unless an explicit validated
  sender is supplied by controlled automation.
- Names, kinds, bodies, and references are validated before publication.
- Human summaries and generated orchestrator bodies apply stricter bounds from
  the digest and `ffint1` contracts.

### Send semantics

Sending uses the existing workspace-head compare-and-swap:

1. Load the current head and its tree root.
2. Create a no-file-change encrypted snapshot using that head as parent.
3. Upload the snapshot and complete opaque reachability manifest.
4. Compare-and-swap the workspace head.
5. On a CAS conflict, reload both the latest head and latest tree root before
   creating the next candidate.

The caller-selected `about_snapshot` remains stable across retries. Reloading
the root is mandatory so a concurrent signal can never roll back visible file
changes. Success is returned only after object upload, manifest publication,
and head swap complete. Offline or exhausted retries fail clearly; v1 has no
local outgoing queue.

### Inbox and delivery semantics

Inbox reads are read-only, bounded, and cursor-based:

- the caller supplies a recipient, optional previous workspace-head cursor,
  and bounded result limit;
- the result cursor is the workspace head observed by the read;
- with a cursor, traversal searches snapshots reachable from the current head
  but not reachable from the prior cursor;
- without a cursor, traversal returns the newest matching reachable signals up
  to the result limit;
- messages addressed to the recipient or `*` are returned;
- results are deduplicated by immutable message ID;
- display ordering may use `(created_at_ms, message_id)`, but causality uses
  `reply_to` and snapshot ancestry rather than wall-clock order;
- traversal scans at most 10,000 snapshots per call;
- unreachable cursors, exhausted traversal, or result overflow set
  `cursor_reset=true` and return only a bounded recent view;
- `cursor_reset=true` means older messages may have been missed and automatic
  mutating orchestration must stop until state is recovered.

Delivery is repeatable within explicit bounds, not exactly once. Reads may
redeliver. Reading does not publish an acknowledgement or reveal read state to
other participants.

### Agent lifecycle behavior

Agents following the collaboration contract must:

1. check their inbox at startup;
2. check again after sync, refresh, or land;
3. check before claiming completion;
4. refresh before acting on a request;
5. verify the requested file tree rather than trusting a head ID alone, because
   newer message-only snapshots can retain the same tree;
6. report the snapshot actually inspected and tested;
7. keep the request's `reply_to` even when a newer snapshot was tested;
8. send at most one useful status update;
9. finish accepted work with exactly one `result` or `blocked` reply;
10. use paths, counts, and summaries instead of code or raw logs.

Signals cannot wake an inactive model. A runner must monitor NDJSON events or
poll the inbox and then invoke the intended agent.

### Who communicates with whom

```text
dispatcher -> workers       bounded requests
workers -> dispatcher       status, then result or blocked
dispatcher -> integrator    ffint1 assignment request
integrator -> dispatcher    acceptance, then digest or blocker
dispatcher -> human         one concise outcome or one focused decision
```

Direct worker-to-worker requests are allowed for genuinely bounded
coordination, but the default flow remains through the dispatcher so the user
does not receive duplicate narratives and the assignment state stays coherent.

### Messaging security and privacy

- Every workspace participant can read every decrypted signal, including
  messages addressed to another agent.
- Recipient routing is not an access-control boundary.
- V1 sender attribution is not cryptographically signed; a participant can
  claim another agent name.
- Never send credentials, tokens, pairing/recovery material, `.env` values,
  private keys, private prompts, file contents, or secrets intended for fewer
  than all workspace participants.
- NDJSON wakeups omit message bodies and contain only bounded routing metadata.
- Error text, URLs, argv, environment variables, hub logs, and routine tray
  state must not contain bodies or secret material.
- Message volume is bounded because every send advances snapshot history and
  publishes a manifest.

### Messaging interfaces

The existing interface family remains canonical:

```text
feanorfs agent send <to> --kind <request|status|result|blocked> \
  [--about <snapshot>] [--reply-to <message-id>] [--from <agent>] <body>

feanorfs agent inbox [--for <agent>] [--after <head-cursor>] [--limit <n>]
```

Rust, C, TypeScript, JSON, MCP `agent_send`/`agent_inbox`, and NDJSON events
must expose matching validation and result semantics. Adapters delegate to the
agent-core implementation and do not duplicate encryption, CAS, graph
traversal, or cursor logic.

---

## Signal Profile

No new `ffmsg1` message kind is needed. The orchestration layer uses the
existing kinds:

| Event | `ffmsg1` kind |
|---|---|
| Integrator assignment | `request` |
| Acceptance or useful progress | `status` |
| Verified integration digest | `result` |
| Candidate/task blocker | `blocked` |

For reliable automation, bodies use a versioned, compact profile inside the
ordinary string body:

```text
ffint1:{"type":"assignment","assignment_id":"…","attempt":0,"selected":"agent-b","about_snapshot":"…","roster_fingerprint":"…","neutral_integrator":true,"task":"Integrate parser implementation and tests"}
```

Replies use the same `assignment_id`, the original assignment signal in
`reply_to`, and `accepted`, `result`, or `blocked` profile types. The profile
must fit inside the existing 8 KiB body limit. Unknown `ffint` versions remain
ordinary message text and cannot break `ffmsg1` inbox traversal.

Signal bodies contain summaries and paths only. They must never contain:

- patches or complete file contents;
- raw build/test logs;
- credentials, tokens, pairing codes, recovery kits, or `.env` values;
- private prompts, hidden reasoning, or model transcripts;
- data intended for fewer than all workspace participants.

Routine orchestration should require no more than one assignment, one
acceptance when useful, and one terminal reply per attempt.

---

## Conflict Reconciliation Policy

### What the integrator may handle without asking

Subject to configured policy, the integrator may resolve when all of the
following are true:

- every conflict leg and the base are available or their deletion sentinel is
  explicit;
- the requested and inspected snapshots are established;
- the combined intent is compatible and does not require a product choice;
- the result does not weaken authentication, authorization, encryption,
  recovery, data retention, or destructive-operation safeguards;
- public APIs and persistent formats remain compatible unless the task
  explicitly authorized a change;
- relevant verification passes;
- the integrator can explain the resolution in a short outcome statement.

### Mandatory human escalation

The integrator must stop and ask one focused question when any of these apply:

- two implementations encode incompatible product behavior;
- accepting either side can lose user data or discard unique work;
- security, privacy, credentials, cryptography, recovery, or permissions are
  affected in a way not explicitly authorized;
- a public API, wire format, database schema, migration, or compatibility
  promise changes;
- binary or generated content has no trustworthy reconstruction path;
- required tests cannot run or fail after reconciliation;
- conflict history is incomplete, a cursor reset occurred, or a required leg
  cannot be authenticated and fetched;
- the integrator authored a conflicting side and no neutral reviewer exists,
  when policy requires neutral review;
- the dispatcher cannot prove the prior accepted integrator has stopped.

### Cross-machine conflict availability

Format-v3 tree entries retain content-addressed `base`, `ours`, and `theirs`
legs, but current human-readable conflict artifacts and pending-conflict rows
are local state. The selected integrator must be able to materialize an
authenticated conflict bundle on its own machine from the encrypted head.

The implementation must therefore provide a read-only operation that:

1. loads the current snapshot and first-class conflict entry;
2. validates the relative path and every ciphertext hash;
3. fetches and decrypts each available leg using the existing object/file
   domains and large-file reconstruction rules;
4. creates `.original`, `.local`, and `.cloud` artifacts under protected
   global FeanorFS state, never inside the project;
5. represents absent legs with the existing sentinels;
6. records a local pending-conflict row without changing the shared head;
7. refuses stale materialization when the conflict has already been resolved;
8. returns typed paths and availability flags without exposing contents in
   signals or routine logs.

The integrator creates a candidate in an isolated agent workspace, runs the
required checks there, and then explicitly resolves with the verified file.
FeanorFS does not generate or bless the semantic merge.

---

## Human Digest Contract

The default terminal, tray, MCP, and agent-facing result must be bounded and
structured:

```json
{
  "assignment_id": "…",
  "integrator": "agent-b",
  "about_snapshot": "…",
  "inspected_snapshot": "…",
  "state": "completed",
  "landed_paths": 12,
  "resolved_conflicts": 3,
  "remaining_conflicts": 0,
  "verification": {
    "status": "passed",
    "summary": "84 tests passed"
  },
  "outcome": "Integrated parser implementation and tests.",
  "risks": [],
  "decision_required": null
}
```

Bounds:

- outcome: at most 512 UTF-8 bytes;
- verification summary: at most 512 bytes;
- risks: at most 10 entries, 256 bytes each;
- decision question: one question, at most 512 bytes;
- paths: counts by default, with a bounded path list available on demand;
- no patch, file content, raw log, or model reasoning fields.

The UI presents details progressively: outcome first, decision if required,
then optional paths/diffs/artifacts. A completed digest with no decision returns
without requiring user acknowledgement.

---

## Persistence and Recovery

Dispatcher state lives in protected global workspace state, for example:

```text
~/.feanorfs/workspaces/<opaque-id>/orchestrator/integrator-state.json
```

Requirements:

- separate advisory lock for the active dispatcher;
- schema version;
- atomic private-file replacement and platform-appropriate permissions;
- no credentials, file contents, workspace key, bearer token, or raw logs;
- bounded completed-assignment history;
- durable current assignment, nonce, canonical roster fingerprint, ranking,
  attempt, request message IDs, acceptance, and inbox cursor;
- crash recovery that resumes observation without sending a duplicate request;
- cursor reset handling that fails closed and asks for recovery rather than
  inferring an assignment result;
- explicit takeover flow when moving the dispatcher to another computer;
- safe cleanup that never removes conflict artifacts or user work.

The signal history is audit evidence but is not sufficient by itself for
exactly-once execution. Local durable state and the single-dispatcher invariant
remain required.

---

## Interface Requirements

Exact naming may follow the existing `agent` CLI structure, but every surface
must expose the same canonical contracts.

### CLI

```text
feanorfs agent integrator assign \
  --about <snapshot-id> \
  --candidate <agent-name>... \
  --require <capability>... \
  [--exclude-author <agent-name>...] \
  [--ack-timeout <duration>] \
  <task-summary>

feanorfs agent integrator status [<assignment-id>]
feanorfs agent integrator revoke <assignment-id> --reason <summary>
feanorfs agent integrator resume
```

Human output shows only the selected agent, assignment state, and next action.
Global `--json` returns canonical types. Automation must not scrape human text.

### MCP

Add bounded tools that delegate to the canonical implementation:

- `integrator_assign`
- `integrator_status`
- `integrator_revoke`
- `conflict_materialize`

Descriptions must repeat the advisory-identity, all-participants-readable,
single-dispatcher, and no-automatic-merge boundaries.

### SDK and bindings

- Rust owns canonical selection, state, and result types.
- `feanorfs-client` provides thin CLI/MCP adapters.
- C FFI accepts/returns bounded JSON using the existing allocation/free model.
- TypeScript exposes typed async wrappers matching the JSON contract.
- No adapter reimplements candidate filtering, canonicalization, ranking, or
  lifecycle transitions.

### Events

NDJSON may emit bounded metadata-only events:

- `integrator_assigned`
- `integrator_accepted`
- `integrator_completed`
- `integrator_blocked`
- `integrator_requires_human`

Events omit task bodies, decision details, paths, file content, and raw logs.
The authorized orchestrator reads typed state after a wakeup.

### Collaboration skill

Extend `skills/feanorfs-collaboration/` to teach agents to:

- accept the integrator role only when selected for the current attempt;
- refresh and verify snapshot context before acting;
- re-check for supersession before every mutating operation;
- never claim path ownership or leadership as a security fact;
- reconcile in an isolated workspace and verify before `conflicts keep --file`;
- send a bounded digest rather than code or raw logs;
- escalate only according to the configured decision policy;
- stop on cursor reset, missing conflict legs, stale assignment, or uncertain
  prior-integrator state.

---

## Implementation Tasks

### Task MSG-1 — Freeze the encrypted messaging contract

- [ ] Define the exact `ffmsg1` discriminator and canonical compact JSON
  envelope.
- [ ] Define `request`, `status`, `result`, and `blocked` behavior.
- [ ] Derive message ID, sender, and creation time from the enclosing snapshot.
- [ ] Validate names, body bounds, reachable snapshots, and reply references.
- [ ] Keep unknown versions and malformed payloads harmless to history and
  typed inbox reads.
- [ ] Add canonical encode/decode and malformed-input fixtures.

**Done when:** the wire contract is versioned, bounded, deterministic, and
independent of every UI/adapter.

### Task MSG-2 — Implement race-safe signal publication

- [ ] Publish signals as encrypted no-file-change snapshots.
- [ ] Preserve the current tree root and latest head parent.
- [ ] Reload both head and root on every CAS retry.
- [ ] Keep caller-selected `about_snapshot` stable across retries.
- [ ] Upload the complete reachability manifest before reporting success.
- [ ] Fail clearly when offline or CAS retries are exhausted; do not imply an
  outgoing queue.

**Done when:** send/send, send/sync, send/land, send/undo, and send/conflict-
resolution races retain all signals and the newest visible file tree.

### Task MSG-3 — Implement bounded graph-delta inbox reads

- [ ] Read by recipient, optional prior-head cursor, and bounded limit.
- [ ] Traverse the reachable graph delta across multi-parent history.
- [ ] Match direct and broadcast messages and deduplicate by message ID.
- [ ] Return a reusable observed-head cursor.
- [ ] Enforce the 10,000-snapshot scan bound and maximum result limit.
- [ ] Set `cursor_reset=true` for unreachable cursors, scan exhaustion, or
  result overflow.
- [ ] Keep reads mutation-free with no read receipts or acknowledgements.

**Done when:** ordinary, merged, undo, overflow, reset, redelivery, malformed,
and unknown-version histories return deterministic bounded results.

### Task MSG-4 — Add intent and reply discipline

- [ ] Require bounded requests to state outcome, snapshot, constraints, and
  expected evidence.
- [ ] Teach workers that paths are advisory scope hints rather than locks.
- [ ] Limit routine work to one useful status and one terminal reply.
- [ ] Require terminal replies to reference their request.
- [ ] Require agents to state the snapshot actually inspected/tested.
- [ ] Block or coordinate before crossing another task's stated boundary.
- [ ] Prohibit code, patches, raw logs, hidden reasoning, and secrets in
  generated signal bodies.

**Done when:** two agents can make their intentions and outcomes clear without
creating a chat stream or sending source content through messages.

### Task MSG-5 — Expose CLI, JSON, SDK, FFI, TypeScript, and MCP messaging

- [ ] Add concise `agent send` and cursor-based `agent inbox` CLI operations.
- [ ] Add canonical JSON input/result types and snapshots.
- [ ] Expose Rust workspace methods and thin client re-exports.
- [ ] Add C JSON ABI functions/header declarations.
- [ ] Add TypeScript types and async wrappers.
- [ ] Add bounded MCP `agent_send` and `agent_inbox` schemas and dispatch.
- [ ] Preserve additive SDK compatibility and one canonical implementation.

**Done when:** every surface validates and returns the same messages, cursor,
reset flag, and errors for the same fixtures.

### Task MSG-6 — Add metadata-only wakeups for orchestrators

- [ ] Detect newly reachable signals from the NDJSON event loop.
- [ ] Emit message ID, sender, recipient, kind, and `about_snapshot` only.
- [ ] Omit bodies, paths, task summaries, and result details.
- [ ] Deduplicate wakeups by message ID with bounded memory.
- [ ] Report cursor reset/overflow instead of claiming complete delivery.
- [ ] Document that a runner must invoke an inactive model after the wakeup.

**Done when:** an orchestrator can wake the intended agent without event-body
leakage or an exactly-once claim.

### Task MSG-7 — Prove messaging privacy and product boundaries

- [ ] Assert no plaintext sender, recipient, body, kind, or snapshot context is
  present in hub object storage, URLs, or server logs.
- [ ] Assert sends/reads create no project file and do not change Git status.
- [ ] Keep the hub router and database schema unchanged.
- [ ] State that routing is not access control and identity is advisory.
- [ ] Prohibit credentials, `.env`, recovery, pairing, and private key material.
- [ ] Bound body size, scan work, result count, event cache, and error output.

**Done when:** security tests and threat-model review support every messaging
privacy claim without implying per-recipient secrecy or signed identity.

### Task MSG-8 — Document and validate the collaboration lifecycle

- [ ] Maintain `docs/agent-communication.md` as the canonical human protocol.
- [ ] Document messaging contracts in `docs/agent-api.md` and `docs/usage.md`.
- [ ] Update the threat model with spoofing, visibility, redelivery, reset, and
  inactive-model limitations.
- [ ] Ship and validate the FeanorFS collaboration skill.
- [ ] Test startup, post-sync/refresh/land, and pre-completion inbox checks.
- [ ] Add an end-to-end Linux-worker/macOS-test exchange tied to one snapshot.

**Done when:** an agent can complete a cross-machine request using only the
documented protocol and skill while the human sees a short outcome.

### Task INT-1 — Freeze the product and authority contract

- [ ] Document the human, dispatcher, integrator, worker, client, and hub roles.
- [ ] State that the dispatcher is stable while the integrator rotates.
- [ ] Define the single-dispatcher invariant and fail-closed behavior.
- [ ] Define which decisions the integrator may make and which require a human.
- [ ] Explicitly preserve the no-auto-merge and advisory-identity boundaries.
- [ ] Reconcile this PRD with the earlier messaging PRD's v1 non-goals by
  scoping assignment semantics to the consumer/orchestrator layer.

**Done when:** README/design docs cannot be read as granting the hub, messages,
or a randomly selected name authority to discard work.

### Task INT-2 — Define canonical contracts and bounds

- [ ] Add canonical input/result/state types for candidates, assignments,
  attempts, verification summaries, and human digests.
- [ ] Define `ffint1` assignment/reply profiles carried inside `ffmsg1.body`.
- [ ] Add schema versions and forward-compatible unknown-field behavior.
- [ ] Bound candidates, capabilities, task summaries, risks, paths, and history.
- [ ] Reject duplicate candidates, invalid names, invalid capabilities, and
  unreachable snapshot IDs before publishing a request.
- [ ] Add stable JSON fixtures before wiring adapters.

**Done when:** Rust, JSON, C, TypeScript, MCP, CLI, and documentation agree on
one bounded contract.

### Task INT-3 — Implement eligibility and neutrality filtering

- [ ] Filter disabled, unavailable, incapable, and explicitly excluded agents.
- [ ] Require every requested capability.
- [ ] Prefer candidates that did not author a conflicting side.
- [ ] Report when no neutral candidate exists without blocking an otherwise
  allowed assignment.
- [ ] Return a typed, concise no-candidate reason.
- [ ] Never infer liveness from signal recency alone.

**Done when:** table-driven tests cover empty, duplicate, incapable, offline,
excluded, neutral, and no-neutral rosters.

### Task INT-4 — Implement the auditable random ranking

- [ ] Generate assignment IDs and selection nonces from the OS CSPRNG.
- [ ] Canonicalize the eligible roster deterministically.
- [ ] Domain-separate and length-prefix the Blake3 ranking input.
- [ ] Produce selected and fallback candidates from one immutable draw.
- [ ] Make fixed nonces injectable only through test/internal APIs.
- [ ] Add cross-language golden vectors.
- [ ] Add a non-flaky distribution regression that detects obvious bias.

**Done when:** identical inputs and nonce produce identical rankings on every
supported platform and production has no caller-controlled seed.

### Task INT-5 — Implement the assignment state machine

- [ ] Implement created, offered, accepted, active, completed, blocked,
  superseded, revoked, requires-human, and cancelled transitions.
- [ ] Reject illegal or duplicate transitions.
- [ ] Use the precomputed next candidate for pre-acceptance timeout/fallback.
- [ ] Prohibit timeout-only fallback after acceptance.
- [ ] Require explicit stop, revocation, or blocker before replacing an active
  integrator.
- [ ] Re-check assignment currency before acceptance and mutation.
- [ ] Preserve the original request and snapshot context across fallbacks.

**Done when:** deterministic tests cover every legal transition, stale reply,
late acceptance, duplicate event, timeout, blocker, revocation, and fallback.

### Task INT-6 — Add crash-safe dispatcher persistence

- [ ] Add schema-versioned, locked, atomic protected-file state.
- [ ] Persist the draw and request ID before considering an offer active.
- [ ] Persist inbox cursor and terminal reply IDs.
- [ ] Resume without duplicating an assignment signal.
- [ ] Bound terminal history and preserve active records.
- [ ] Fail closed on corruption, unsupported schema, lock contention, and cursor
  reset.
- [ ] Add an explicit, audited dispatcher takeover path.

**Done when:** fault-injection tests at every write boundary recover to one
unambiguous assignment or a bounded human escalation.

### Task INT-7 — Publish and consume `ffint1` through existing signals

- [ ] Encode assignment as `request`, acceptance/progress as `status`, and
  terminal outcome as `result` or `blocked`.
- [ ] Validate `reply_to`, `about_snapshot`, selected agent, attempt, and
  assignment ID.
- [ ] Keep all bodies under the existing 8 KiB limit.
- [ ] Ignore unknown `ffint` versions without breaking ordinary inbox reads.
- [ ] Prevent patches, raw logs, or secrets from entering generated bodies.
- [ ] Preserve existing `ffmsg1` CAS retry and reachability behavior.

**Done when:** concurrent file publication, assignment publication, and worker
results retain the newest file root and every reachable signal.

### Task INT-8 — Make encrypted conflicts portable to the integrator

- [ ] Add read-only lookup of first-class conflict entries at a snapshot.
- [ ] Fetch and authenticate base/ours/theirs legs, including large files.
- [ ] Materialize artifacts only under private global FeanorFS state.
- [ ] Reuse existing deletion sentinels and path-traversal defenses.
- [ ] Register local pending state without publishing a new head.
- [ ] Refuse a stale or already-resolved conflict.
- [ ] Return typed artifact metadata without routine content output.
- [ ] Prove project files and Git status are unchanged by materialization.

**Done when:** an integrator on a third computer can reconstruct the same
authenticated conflict triple and inspect it offline after fetch.

### Task INT-9 — Add the safe reconciliation workflow

- [ ] Create reconciliation candidates in isolated agent workspaces.
- [ ] Keep `.original`, `.local`, `.cloud`, and optional `.proposed` artifacts.
- [ ] Require the integrator to state intended behavior and relevant checks.
- [ ] Run verification against the exact inspected snapshot/candidate.
- [ ] Apply only through an explicit local/cloud/both/file resolution choice.
- [ ] Revalidate assignment and conflict state before head publication.
- [ ] Preserve all immutable content objects and conflict history after
  resolution.
- [ ] Block rather than guess when any required leg or verification is missing.

**Done when:** compatible overlapping edits can be explicitly reconciled and
verified without hiding or deleting either input version.

### Task INT-10 — Implement bounded escalation and human digests

- [ ] Implement the mandatory escalation matrix from this PRD.
- [ ] Emit one focused question rather than several open-ended prompts.
- [ ] Return counts and summaries by default; details are opt-in.
- [ ] Enforce digest field/count/byte bounds.
- [ ] Omit code, patches, raw logs, secrets, and model reasoning.
- [ ] Allow successful, verified batches to finish without user acknowledgement.
- [ ] Make uncertain leadership state visible and fail closed.

**Done when:** snapshot tests prove routine completion fits in one concise view
and every consequential ambiguity becomes one actionable question.

### Task INT-11 — Add CLI and JSON surfaces

- [ ] Add assign, status, revoke, and resume commands under `agent integrator`.
- [ ] Add cross-machine conflict materialization to `conflicts` or the
  integrator command group.
- [ ] Keep smart defaults for agent callers while requiring explicit candidate
  input from the authorized dispatcher.
- [ ] Add human-output tests and canonical `--json` fixtures.
- [ ] Ensure errors name preserved state and the safe next action.
- [ ] Never place task bodies, workspace IDs, credentials, or conflict contents
  in argv generated for automatic services.

**Done when:** a script can drive the full lifecycle from JSON without parsing
human output, while a person sees only a concise digest.

### Task INT-12 — Add MCP, SDK, C, and TypeScript surfaces

- [ ] Add bounded MCP schemas and dispatch for assignment/status/revocation and
  conflict materialization.
- [ ] Put canonical behavior in Rust and keep every adapter thin.
- [ ] Add Rust facade methods, C JSON ABI functions/header updates, and
  TypeScript types/wrappers.
- [ ] Preserve SDK-1 additive compatibility.
- [ ] Add adapter contract snapshots and smoke tests.
- [ ] Ensure tool descriptions state that identity and leadership are advisory.

**Done when:** every supported embedding receives the same ranking, state,
digest, and errors for the same fixture.

### Task INT-13 — Add events and runner integration

- [ ] Emit bounded metadata-only integrator lifecycle wakeups.
- [ ] Deduplicate event IDs using bounded memory.
- [ ] Report reset/overflow without claiming complete delivery.
- [ ] Provide a reference runner loop that monitors events or inbox cursors and
  invokes the selected active agent.
- [ ] Demonstrate that signals alone do not wake a model.
- [ ] Stop/revoke controlled agent processes before post-acceptance fallback.

**Done when:** a two-computer runner test assigns, invokes, observes, and
finishes one integrator without exposing message bodies in events.

### Task INT-14 — Update collaboration guidance

- [ ] Extend the collaboration skill with integrator acceptance, supersession,
  reconciliation, verification, and escalation rules.
- [ ] Add compact examples for successful integration, fallback, blocker, and
  human decision.
- [ ] Update `docs/agent-communication.md`, `docs/agent-api.md`,
  `docs/usage.md`, and `docs/threat-model.md`.
- [ ] Explain random assignment versus security-grade election.
- [ ] Explain that one Git publication machine is still selected separately.
- [ ] Validate and forward-test the skill with cooperative and stale-agent
  scenarios.

**Done when:** an agent following only the installed skill cannot reasonably
infer that it may auto-merge, dump code on the user, or act after supersession.

### Task INT-15 — Security, concurrency, and end-to-end verification

- [ ] Test simultaneous worker result, assignment, sync, land, undo, and
  conflict-resolution head CAS races.
- [ ] Test two dispatchers and require the second to fail on the local lock;
  document that cross-machine dual dispatchers are unsupported and fail-safe
  orchestration requires one authorized runner.
- [ ] Test late/offline agents, cursor resets, stale snapshots, missing legs,
  corrupted ciphertext, and interrupted writes.
- [ ] Assert no plaintext task, agent roster, conflict content, or digest body
  appears in hub storage or server logs.
- [ ] Assert no project-local metadata and no Git/Jujutsu reads or writes.
- [ ] Test macOS, Windows, and Linux protected-state behavior.
- [ ] Run formatting, clippy, workspace tests, SDK contract tests, and Node/C
  smoke tests.

**Done when:** the feature survives deterministic races and fault injection
without lost files, dual accepted integrators, silent conflict choices, or
unbounded user output.

---

## Acceptance Criteria

### Encrypted messaging

- [ ] A send publishes one encrypted no-file-change snapshot and cannot roll
  back a concurrent file update.
- [ ] Two concurrent sends remain reachable and keep causal reply references.
- [ ] Signals never create project files or dirty Git.
- [ ] Inbox graph-delta traversal works across multi-parent history.
- [ ] Inbox results are bounded, cursor-based, and explicit about reset or
  possible missed history.
- [ ] Reads may redeliver but never claim exactly-once delivery or publish read
  receipts.
- [ ] Every accepted request has exactly one terminal `result` or `blocked`
  reply tied to the original request.
- [ ] Results state the snapshot actually inspected and tested.
- [ ] Generated bodies contain intent/outcome summaries, not code or raw logs.
- [ ] CLI, JSON, Rust, C, TypeScript, MCP, and events expose compatible
  contracts.
- [ ] NDJSON wakeups omit message bodies.
- [ ] No new server route, table, plaintext index, or project metadata exists.
- [ ] Documentation makes all-participant visibility, advisory attribution,
  redelivery, cursor reset, and inactive-model behavior explicit.

### Selection

- [ ] The final eligible pool is capability-correct and bounded.
- [ ] Neutral candidates are preferred when available.
- [ ] Every eligible candidate has equal selection probability in v1.
- [ ] The recorded nonce and roster reproduce selected/fallback order.
- [ ] Production callers cannot inject a selection seed.
- [ ] No eligible candidate produces a concise, non-mutating escalation.

### Lifecycle

- [ ] One dispatcher produces at most one accepted integrator per batch.
- [ ] Pre-acceptance timeout advances to the next recorded candidate.
- [ ] Post-acceptance timeout cannot silently activate a fallback.
- [ ] Late, duplicate, stale, and superseded replies are harmless.
- [ ] Crash recovery never emits a duplicate active assignment.
- [ ] Cursor reset or lost state stops mutation and becomes visible.

### Reconciliation

- [ ] A selected integrator on another computer can authenticate and
  materialize every available conflict leg.
- [ ] Reconciliation occurs in an isolated agent workspace.
- [ ] FeanorFS never generates or automatically applies a semantic merge.
- [ ] Explicit resolution revalidates current head and conflict state.
- [ ] Failed verification preserves all legs and does not clear the conflict.
- [ ] Security/product/data-loss ambiguity always reaches the user.

### Human experience

- [ ] Routine success requires no user merge work.
- [ ] Default results contain summaries and counts, not code or raw logs.
- [ ] At most one bounded decision question is presented per escalation.
- [ ] Detailed paths and artifacts remain available on demand.
- [ ] The user remains the final authority without becoming the routine
  integrator.

### Product boundaries

- [ ] No new hub endpoint, agent-aware server table, or plaintext index exists.
- [ ] No project-local orchestration or conflict metadata exists.
- [ ] Messages remain encrypted, low-volume, and all-participants-readable.
- [ ] Identity and assignment remain advisory rather than security claims.
- [ ] FeanorFS performs no Git/Jujutsu operation.

---

## Verification Matrix

| Scenario | Expected result |
|---|---|
| Two signals publish concurrently | Both remain reachable and the latest file tree is preserved |
| Signal publishes during file sync/land | Message remains reachable without rolling back file changes |
| Inbox traverses a multi-parent merge | All matching delta messages are returned once per message ID in that result |
| Inbox cursor is unreachable or bounded out | `cursor_reset=true`; mutating orchestration stops |
| Inbox read repeats | Redelivery is safe and no read receipt is published |
| Unknown or malformed message version | Typed inbox ignores it; raw history remains intact |
| Agent is inactive | Metadata wakeup is emitted, but no claim is made that the model started |
| Two eligible agents, fixed nonce | Same selected/fallback order on every platform |
| Selected agent never accepts | Next ranked candidate receives the assignment |
| Selected agent accepts, then goes quiet | No second integrator starts without stop/revoke/user action |
| Late acceptance after supersession | Rejected as stale; no mutation |
| Worker and assignment publish concurrently | Both signals and newest file root remain reachable |
| Integrator works from a third computer | Conflict legs authenticate and materialize outside project |
| Compatible source conflict | Verified candidate explicitly resolves conflict |
| Incompatible product behavior | One bounded user question; no version discarded |
| Security-sensitive conflict | Mandatory human escalation |
| Verification failure | Candidate preserved, conflict remains pending, concise blocker returned |
| Inbox cursor reset | Automatic integration stops fail-closed |
| Dispatcher crashes after send | Restart observes existing request; no duplicate send |
| Unknown `ffint2` body | Visible as ordinary signal text; v1 parser remains safe |
| Malicious/oversized roster or digest | Rejected before publication or persistence |
| Hub storage inspection | No plaintext messages, roster, paths, or content |

---

## Success Metrics

| Metric | Target | Measurement |
|---|---:|---|
| Project files changed by messaging | 0 | Before/after filesystem and Git-status assertions |
| Lost file updates during signal CAS races | 0 | Deterministic send/sync/land race tests |
| Message bodies in NDJSON wakeups | 0 | Event contract snapshots |
| New message hub endpoints/tables | 0 | Router/schema diff |
| Accepted requests lacking a terminal reply | 0 in controlled end-to-end runs | Runner lifecycle tests |
| Routine compatible conflicts requiring user action | 0 | End-to-end cooperative-agent scenarios |
| Code/patch/raw-log bytes in default digest | 0 | Contract tests and redaction assertions |
| Simultaneously accepted integrators per batch | 1 maximum | State-machine and runner race tests |
| Lost conflict legs | 0 | Cross-machine materialization and resolution tests |
| New hub endpoints/tables | 0 | Router and migration diff |
| Project files created by orchestration | 0 | Before/after filesystem and Git-status tests |
| Unexplained semantic decisions | 0 | Digest and resolution audit records |
| Selection bias among equal candidates | Within deterministic test tolerance | Fixed multi-seed distribution test |

Qualitative success means a user can supervise several agents by reading one
short outcome or answering one focused question, while still being able to
inspect every preserved version when desired.

---

## Risks and Mitigations

| Risk | Likelihood | Impact | Mitigation |
|---|---|---|---|
| Two cross-machine dispatchers run simultaneously | Medium | High | Explicit single-dispatcher contract, local lock, runner ownership, takeover flow, fail-closed documentation |
| Offline agent is selected | Medium | Medium | Dispatcher-owned availability filter, acknowledgement timeout, immutable fallback order |
| Accepted agent acts after revocation | Low/Medium | High | Re-check before mutation, controlled process stop, stale-attempt rejection, no automatic ambiguous fallback |
| Random selection chooses an unsuitable agent | Medium | High | Capability and policy filtering before draw; candidate-specific blocker fallback |
| Integrator favors its own implementation | Medium | Medium | Prefer neutral candidates and disclose when neutrality is impossible |
| Integrator silently weakens behavior | Medium | High | Mandatory escalation matrix, exact-snapshot verification, explicit resolution audit |
| Messages become verbose chat | High | Medium | `ffint1` profile, strict bounds, one terminal response, no code/log bodies |
| User still receives too much detail | Medium | High | Bounded digest and progressive disclosure |
| Conflict artifacts are unavailable remotely | High before implementation | High | Authenticated cross-machine materialization task and tests |
| Users interpret assignment as secure authority | Medium | High | Repeat advisory-identity boundary in CLI, MCP, docs, and skill |
| Assignment snapshots increase head churn | Low/Medium | Low | Low-volume lifecycle and no heartbeats through snapshot history |

---

## Alternatives Considered

### Let every worker integrate its own changes

- **Pros:** No scheduler.
- **Cons:** Duplicate arbitration, inconsistent conflict choices, and noisy user
  output.
- **Decision:** Rejected.

### Ask the user to choose an integrator every time

- **Pros:** Clear authority.
- **Cons:** Turns routine coordination into recurring user work.
- **Decision:** Rejected as the default; retained as an override.

### Fixed permanent integrator

- **Pros:** Simple and predictable.
- **Cons:** Bottleneck, uneven cost, platform mismatch, and single-agent bias.
- **Decision:** Rejected as the only policy. A user may explicitly pin one when
  required.

### Independent peer election on every computer

- **Pros:** No central dispatcher.
- **Cons:** Candidate rosters and liveness observations diverge; `ffmsg1`
  identity is advisory; signals provide no lease or exactly-once execution.
- **Decision:** Rejected for v1.

### Hub-managed leader lease

- **Pros:** Stronger centralized arbitration.
- **Cons:** Makes the hub agent-aware, adds endpoints/state, and violates the
  dumb-storage boundary.
- **Decision:** Rejected.

### Automatically merge with diff3

- **Pros:** Fewer visible conflicts.
- **Cons:** Textual cleanliness does not establish semantic correctness and
  violates the no-auto-merge rule.
- **Decision:** Rejected. A diff3 proposal may remain an input to an isolated,
  verified consumer workflow.

### Strict round-robin assignment

- **Pros:** Perfect short-term fairness and easy debugging.
- **Cons:** Predictable, ignores the requested random choice, and still needs
  capability/liveness filtering.
- **Decision:** Deferred as an optional policy; v1 uses equal random draws with
  an auditable fallback order.

---

## Documentation Deliverables

- [ ] Add the multi-agent authority model to `docs/usage.md`.
- [ ] Extend `docs/agent-communication.md` with the `ffint1` profile and explain
  why it is not a new message kind.
- [ ] Extend `docs/agent-api.md` with assignment, digest, and conflict-
  materialization contracts.
- [ ] Extend `docs/threat-model.md` with dual-dispatcher, spoofed identity,
  stale integrator, and code-dump risks.
- [ ] Update the collaboration skill and its protocol reference.
- [ ] Add a short troubleshooting runbook for blocked, stale, cursor-reset, and
  dispatcher-takeover states.
- [ ] Add an end-to-end example with two workers on different computers, a
  third randomly selected integrator, and one concise user digest.

---

## Open Questions for RFC Review

| Question | Proposed default |
|---|---|
| Where should canonical orchestration state types live? | Agent/consumer layer, with thin client and binding adapters; never server |
| Should a user be able to pin an integrator? | Yes, explicit override; random remains default when multiple eligible agents exist |
| What is the pre-acceptance timeout? | Configurable by the runner with a conservative documented default; never passed through automatic-service secrets/argv |
| Can the original worker be selected when no neutral candidate exists? | Yes, if policy allows; disclose `neutral_integrator=false` |
| Should accepted integrators have a time lease? | No automatic expiry in v1; explicit stop/revoke avoids split-brain ambiguity |
| How is cross-machine dispatcher takeover authorized? | Explicit user/runner action after the previous dispatcher is stopped; never inferred from silence |

---

## Repository Verification

Focused tests should run while implementing each task. Final verification must
include:

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features --locked
```

Also run the existing SDK contract snapshots, Node loop/pack tests, C ABI smoke
tests, collaboration-skill validation, and a real two-computer or equivalent
network-isolated orchestration scenario.
