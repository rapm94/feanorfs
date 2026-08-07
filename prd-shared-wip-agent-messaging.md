# PRD: Shared WIP Git UX and Encrypted Agent Signals

**Date:** 2026-08-04

**Status:** Ready for implementation

**Scope:** User experience contract, encrypted agent messaging, CLI/MCP/SDK surfaces, and agent guidance

---

## Summary

FeanorFS will present a synchronized folder as one **shared work-in-progress overlay** on top of independently managed Git baselines:

```text
working tree = local Git baseline + shared FeanorFS WIP
```

macOS and Linux remain normal Git clones. When one machine or agent edits a tracked file, Git correctly shows that file as modified on every synchronized clone. FeanorFS does not hide, stage, commit, reset, pull, merge, or synchronize Git metadata. Git remains the publication and history layer; FeanorFS carries the unfinished working state, including `.env` and other gitignored files.

Agents will coordinate through a low-volume encrypted **signal** protocol stored in FeanorFS snapshot history. Signals never become project files, never dirty Git, require no new hub endpoint, and remain opaque to the hub. The first protocol supports `request`, `status`, `result`, and `blocked` messages tied to immutable snapshot IDs.

This PRD deliberately does not turn FeanorFS into Git or into a chat/task-management product.

---

## Problem Statement

### Shared WIP and Git

Developers use FeanorFS because their unfinished working directory must follow them across heterogeneous machines. A representative workflow is:

- Linux performs ordinary development.
- macOS builds and tests an iOS simulator.
- Either machine may later become the active development machine.
- `.env`, local configuration, untracked files, and tracked edits must move together.
- Coding agents on either machine contribute to the same unfinished result.

Both machines therefore need normal Git repositories. Removing `.git` from a secondary machine prevents that machine from naturally becoming the active development environment.

The confusing behavior is that remote FeanorFS edits appear as local uncommitted Git changes. That state is technically correct, but the current product documentation does not fully explain the operating model, the commit boundary, or what happens after one machine advances its Git history. Users can mistake expected shared WIP for corruption or try unsafe workarounds such as syncing `.git`, hiding files with `assume-unchanged`, or automatically creating WIP commits.

There is no non-Git mechanism that can continuously change tracked files while keeping independent Git worktrees clean. Cleanliness means that the files equal each clone's local Git `HEAD`; advancing that metadata is Git's responsibility.

### Agent coordination

FeanorFS agents can spawn, refresh, inspect, land, and surface conflicts, but they cannot currently exchange intent or outcomes. A Linux development agent cannot directly tell a macOS test agent which snapshot to test, and the macOS agent cannot return a bounded pass/fail/blocker result without using a project file or an unrelated external service.

Using synchronized message files would:

- dirty Git or create untracked project files;
- violate FeanorFS's zero-project-metadata convention;
- create concurrent append/file conflicts;
- mix transport coordination with user work product.

The existing `events` command reports local filesystem and mirror state only. The existing MCP server exposes lifecycle operations but no cross-machine communication.

### Why now?

The multi-machine and agent-isolation capabilities already exist. Without a clear Git contract and a minimal coordination mechanism, their combined workflow is harder to trust than the underlying transport warrants. Clarifying this now prevents future Git coupling and avoids building communication through the wrong abstraction.

### Who is affected?

- **Primary users:** Developers using the same unfinished project on macOS and Linux, including platform-specific build/test machines.
- **Primary automation users:** Coding agents that spawn, refresh, test, and land work through FeanorFS.
- **Secondary users:** Orchestrators consuming the Rust, C, TypeScript, JSON, MCP, or NDJSON event surfaces.

---

## Product Decisions

1. **Both human development machines remain normal Git clones.** A build-only replica is allowed but is not the primary model.
2. **A FeanorFS workspace represents shared WIP, not a Git branch.** Participating clones should begin from the same Git baseline.
3. **Dirty Git state is expected while shared WIP exists.** Product copy must explain it rather than conceal it.
4. **Git-history operations are coordinated separately.** Only one participating machine should commit, rebase, or switch the shared branch at a time.
5. **FeanorFS never reads or writes `.git`/`.jj`, runs Git/Jujutsu commands, or changes VCS state.** Documentation may teach users how to use their VCS at the publication boundary.
6. **`.env` and other gitignored files continue to sync.** Machine-generated output remains controlled by the existing default and custom ignore policy.
7. **Agent communication is a low-volume transport primitive.** FeanorFS transports and authenticates ciphertext; consuming agents interpret message meaning.
8. **Signals use encrypted snapshot history.** No project metadata, new server endpoint, chat database, or plaintext routing metadata is introduced.
9. **Message attribution is advisory.** Workspace participants share the workspace key and can claim an agent name; v1 does not provide per-agent signatures.
10. **Every request and result is snapshot-aware.** Agents must state which immutable workspace snapshot they intended to use or actually tested.

---

## Proposed User Experience

### Mental model

Git answers:

> What intentional history has this clone published or received?

FeanorFS answers:

> What unfinished files should every authorized machine and agent currently see?

When Linux edits `src/app.rs`, both Linux and macOS should show `src/app.rs` as modified relative to the same Git baseline. That is not a synchronization error: both machines are looking at the same unfinished change.

### User flow: Linux develops, macOS tests

1. Linux and macOS begin at the same Git commit and branch.
2. Both existing clones join the same FeanorFS workspace.
3. Linux edits tracked files and `.env`.
4. FeanorFS mirrors those file contents to macOS.
5. Git on both machines reports tracked edits as uncommitted; `.env` remains ignored by Git but synchronized by FeanorFS.
6. macOS builds and tests the current files in the simulator.
7. macOS may edit files; those edits become part of the same shared WIP and appear on Linux.
8. Generated build output remains local through built-in cache rules or explicit `feanorfs ignore` patterns.

### User flow: changing the active development machine

1. Allow FeanorFS synchronization to settle.
2. Stop editing on the previous active machine.
3. Continue editing the already-dirty shared WIP on the other machine.
4. Do not create a handoff commit merely to change machines.

Both machines retain Git tooling, local history, blame, diff, editor integration, and the ability to become the publication machine.

### User flow: publishing the shared WIP to Git

1. Allow agents to finish or explicitly report that they are blocked.
2. Choose one machine as the Git-history writer for that publication boundary.
3. Review the complete shared WIP and resolve FeanorFS conflicts.
4. Commit and push through ordinary Git.
5. On other machines, fetch and advance the corresponding Git baseline through ordinary Git.
6. If the synchronized tracked files exactly match the published commit and the remote commit is a fast-forward, users may perform a metadata-only mixed reset after verifying those conditions. FeanorFS must not perform or suggest a destructive reset without those checks.
7. `.env` and other intentionally untracked/gitignored files remain in place.

The Git documentation should show the guarded form explicitly, with the user's real remote and branch substituted for `origin/main`:

```bash
if git fetch origin &&
   git merge-base --is-ancestor HEAD origin/main &&
   git diff --quiet origin/main --; then
  git reset --mixed origin/main
else
  printf '%s\n' 'Baseline not advanced: inspect the Git divergence first.' >&2
fi
```

The reset is appropriate only when the ancestry and tracked-tree checks succeed: the published commit is a fast-forward from the local `HEAD`, and the tracked working tree already equals that published commit. `--mixed` advances `HEAD` and the index while preserving working files and ignored/untracked files. If either check fails, the user must inspect the divergence with Git; FeanorFS does not resolve it.

If a different machine still contains tracked WIP that was not included in the commit, it remains visibly dirty and must be reviewed. FeanorFS must never mark it clean or discard it.

### User flow: changing Git branches

A FeanorFS workspace is not a branch selector. Independent branch switches would make one clone's files appear as a large remote WIP change on the other clones.

Before changing the shared baseline:

1. Settle or preserve the current shared WIP.
2. Pause editing and automatic synchronization as needed.
3. Use Git to align the intended branch/commit on every participating clone.
4. Resume FeanorFS only after the participating clones have the same baseline.

Teams that intentionally maintain simultaneous branch work should use separate folders and FeanorFS workspace identities. FeanorFS does not model those branches.

### User flow: agents coordinate a platform test

1. `linux-dev` lands a coherent set of file changes.
2. `linux-dev` sends `mac-test` a `request` signal tied to the landed snapshot.
3. `mac-test` reads its inbox, refreshes, and verifies that it is testing the requested snapshot or explicitly reports a newer/different snapshot.
4. `mac-test` sends one `status` update if useful.
5. `mac-test` sends either a bounded `result` or `blocked` reply referencing the original request.
6. `linux-dev` reads the reply before completing or sending another request.

Example:

```text
linux-dev -> mac-test
request: Run iOS simulator tests
about: abc123...

mac-test -> linux-dev
status: Testing abc123...
reply_to: def456...

mac-test -> linux-dev
result: Passed 42 tests on iPhone 16 simulator
about: abc123...
reply_to: def456...
```

---

## Agent Signal Protocol

### Transport

An agent signal is an ordinary encrypted format-v3 snapshot with:

- the latest workspace tree root;
- the latest workspace head as its parent;
- the sender name in `Snapshot.author`;
- the signal envelope in `Snapshot.message`;
- no file-tree changes.

The encoded message uses an exact versioned discriminator followed by canonical compact JSON:

```text
ffmsg1:{"to":"mac-test","kind":"request","body":"Run iOS simulator tests","about_snapshot":"<64-hex>","reply_to":null}
```

Fields derived from the enclosing snapshot are not duplicated in the payload:

- `message_id` = signal snapshot ID;
- `from` = `Snapshot.author`;
- `created_at_ms` = `Snapshot.created_at_ms`.

The snapshot object, author, message, tree, and paths remain encrypted. The hub observes only ordinary ciphertext objects, object sizes, manifests, head changes, and timing.

### Message kinds

| Kind | Meaning | Expected follow-up |
|---|---|---|
| `request` | Ask another agent to perform bounded work against a snapshot | `status`, then `result` or `blocked` |
| `status` | Short progress update | No acknowledgement required |
| `result` | Final bounded outcome | Requester consumes it |
| `blocked` | Final explanation of why the request cannot complete | Requester decides next action |

FeanorFS validates the enum and transports the body but does not interpret success, failure, task ownership, paths, or content semantics.

### Validation

- `from` and `to` must be valid FeanorFS agent names; `to="*"` is the only broadcast form.
- CLI callers derive `from` from `FEANORFS_AGENT`; when absent, the sender is `human` unless an embedding supplies an explicit validated sender.
- Agent names must be unique by convention within a shared workspace. No server-side registry is introduced.
- `body` must be non-empty UTF-8 after trimming and at most 8 KiB.
- `about_snapshot` defaults to the head observed when sending starts and must be a full reachable snapshot ID.
- `reply_to`, when present, must be a full reachable `ffmsg1` signal snapshot ID.
- Unknown snapshot messages remain ordinary history messages.
- Malformed `ffmsg1` payloads do not crash or block inbox traversal; they remain visible in raw history for diagnostics and are ignored by the typed inbox.
- Message bodies must not contain credentials, recovery capabilities, pairing codes, `.env` values, or other secrets intended for fewer than all workspace participants.

### Send semantics

Sending is append-only and uses the existing workspace-head compare-and-swap operation.

On every CAS retry, the sender must reload the latest head and reuse that head's latest tree root. It must never reuse a stale root after a concurrent file publication, because doing so could roll back visible files.

The signal's `about_snapshot` remains the caller-selected context even if concurrent changes advance the workspace head while the signal is being appended.

A successful send returns only after the encrypted snapshot object, reachability manifest, and head swap succeed. Offline or exhausted-CAS sends fail clearly; v1 does not add a local outgoing queue.

### Inbox semantics

Inbox reads are read-only and cursor-based. Within the scan and result bounds,
reusing the returned cursor gives repeatable delivery; after a reset or
overflow, delivery is explicitly best-effort and older signals may be missed:

- A caller supplies its recipient identity, an optional prior workspace-head cursor, and a bounded limit.
- The result cursor is the workspace head observed by the read.
- With a cursor, the inbox searches the graph delta: snapshots reachable from the current head but not reachable from the prior cursor.
- Without a cursor, the inbox returns the newest matching reachable signals up to the limit.
- Messages addressed to the recipient or `*` are returned.
- Results are deduplicated by signal snapshot ID.
- Display ordering may use `(created_at_ms, message_id)`, but ordering is not authoritative across machines. Causality uses `reply_to` and immutable snapshot IDs.
- Graph traversal scans at most 10,000 snapshots per inbox call. If the supplied cursor is unreachable or is not found within that bound, the result sets `cursor_reset=true` and returns a bounded recent view.
- `cursor_reset=true` explicitly means the caller may have missed older coordination signals and must not infer complete delivery.
- Reading does not publish acknowledgements, mutate history, or reveal read state to other participants.

### History and retention

Signals remain reachable snapshot history and may appear in `feanorfs log`. Human log output should render recognized signals concisely instead of printing raw protocol JSON. Existing JSON history remains backward compatible.

This mechanism is intentionally low-volume. Each signal advances the workspace head and uploads a reachability manifest. It is suitable for coordination checkpoints, not token streaming, raw build logs, or conversational chat.

---

## Interface Specifications

### CLI

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

Examples:

```bash
feanorfs agent send mac-test \
  --kind request \
  --about abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789 \
  "Run iOS simulator tests"

feanorfs --json agent inbox --for mac-test --after <previous-head>
```

### JSON contracts

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

Canonical Rust types belong in `feanorfs_common::agent_contract` and follow the existing additive SDK-1 compatibility policy.

### Rust SDK

`feanorfs-agent-core::Workspace` exposes blocking facade methods backed by async core operations:

```rust
pub fn send_message(&self, input: AgentMessageInput) -> Result<AgentSendResult>;
pub fn inbox(&self, query: AgentInboxQuery) -> Result<AgentInboxResult>;
```

The client crate re-exports thin wrappers. C FFI and TypeScript bindings expose the same JSON shapes and validation semantics.

### MCP

Add two tools to the existing MCP server:

```text
agent_send
  from?: string
  to: string
  kind: request | status | result | blocked
  body: string
  about_snapshot?: string
  reply_to?: string

agent_inbox
  for?: string
  after?: string
  limit?: integer (0..1000)
```

Tool descriptions must explain that all workspace participants can read messages, identity is advisory, and requests/results should carry exact snapshot context. The collaboration skill is not the sole documentation source.

### NDJSON events

The existing `events` stream emits a bounded wakeup record when it discovers a new signal:

```json
{
  "event": "agent_message",
  "message_id": "<signal-snapshot-id>",
  "from": "linux-dev",
  "to": "mac-test",
  "kind": "request",
  "about_snapshot": "<context-snapshot-id>"
}
```

The event omits `body`. An authorized orchestrator calls `agent_inbox` to retrieve the typed message. Event wakeups use the same bounded cursor semantics, are deduplicated locally by snapshot ID, and report reset/overflow because older wakeups may then have been missed.

### Collaboration skill

Ship a concise installable skill at:

```text
skills/feanorfs-collaboration/
├── SKILL.md
├── agents/openai.yaml
└── references/protocol.md
```

The skill must instruct an agent to:

1. Identify itself with a workspace-unique agent name.
2. Check its inbox at startup, after refresh/land, and before declaring completion.
3. Refresh before acting on a request for a newer snapshot.
4. Send one bounded `status` update only when useful.
5. Finish each accepted request with exactly one `result` or `blocked` reply.
6. State the snapshot actually tested, not merely the latest snapshot observed.
7. Use paths and summaries rather than raw logs or file contents.
8. Never send credentials, pairing/recovery material, or `.env` values.
9. Treat routing, authorship, and path claims as advisory rather than cryptographically signed locks.
10. Use FeanorFS conflict commands and never merge file content automatically.

The skill cannot wake an inactive model. Long-lived orchestration must monitor NDJSON events or poll `agent_inbox`, then invoke the appropriate agent with the skill loaded.

---

## End State

When this PRD is complete:

- [ ] Documentation consistently explains FeanorFS as shared WIP over a separately managed Git baseline.
- [ ] macOS and Linux may both remain full Git development clones.
- [ ] Users understand why synchronized tracked files appear dirty and how publication changes the Git baseline.
- [ ] `.env` remains synchronized while `.git` and `.jj` remain completely excluded.
- [ ] FeanorFS performs no Git/Jujutsu command, branch operation, commit, reset, merge, or metadata write.
- [ ] Named agents can exchange encrypted request/status/result/blocked signals tied to snapshots.
- [ ] Signal publication cannot roll back concurrent file changes.
- [ ] Signals create no workspace files and do not alter Git status.
- [ ] CLI, Rust, C, TypeScript, JSON, MCP, and events expose compatible behavior.
- [ ] A concise collaboration skill teaches agents the protocol and safety rules.
- [ ] Hub routes and server-side decision logic remain unchanged.
- [ ] Contract, concurrency, encryption, and cross-surface tests pass.

---

## Acceptance Criteria

### Shared WIP and Git UX

- [ ] README and usage documentation use the equation `working tree = local Git baseline + shared FeanorFS WIP` or an equivalent unambiguous explanation.
- [ ] Documentation includes the Linux-development/macOS-simulator workflow with `.env` synchronization.
- [ ] Documentation explicitly states that both clones being dirty with the same WIP is expected.
- [ ] Documentation distinguishes switching the active editing machine from publishing a Git commit.
- [ ] Documentation explains baseline alignment after a commit without implying FeanorFS performs Git operations.
- [ ] Documentation explains why independent branch switches are unsafe within one actively synchronized workspace.
- [ ] No FeanorFS UX path reads VCS metadata or inspects Git branches, indexes, remotes, or commits; scanners only recognize `.git`/`.jj` names to prune them without entering them.
- [ ] Existing tests continue to prove `.git`/`.jj` exclusion and gitignored-file inclusion.
- [ ] Product copy never calls FeanorFS a VCS, branch manager, merge tool, or Git replacement.

### Signal publication

- [ ] Sending a signal adds one encrypted no-file-change snapshot whose parent is the latest head and whose root equals the latest head's root.
- [ ] A CAS retry reloads both latest head and latest root before constructing its next candidate.
- [ ] Two concurrent signal sends preserve both signals in reachable history.
- [ ] A concurrent signal and file publication preserve both the signal and the newest file tree.
- [ ] Sending does not create, modify, delete, stage, or materialize any project path.
- [ ] No new server route, database table, plaintext index, or routing service is introduced.
- [ ] The hub cannot find message body, sender, recipient, kind, or snapshot context in plaintext storage.
- [ ] Body, recipient, kind, snapshot, and reply validation enforce the stated bounds.

### Inbox

- [ ] Inbox returns only messages addressed to the recipient or broadcast to `*`.
- [ ] Inbox returns a reusable workspace-head cursor.
- [ ] Graph-delta traversal finds messages across multi-parent agent-land history.
- [ ] Repeated reads may redeliver but never fabricate exactly-once guarantees.
- [ ] Cursor loss/reset is explicit and returns only a bounded recent view.
- [ ] Unknown and malformed snapshot messages cannot crash or permanently block inbox reads.
- [ ] Message IDs are immutable signal snapshot IDs.
- [ ] A result can reference and validate its request through `reply_to`.

### Agent and orchestrator surfaces

- [ ] Human CLI output is concise and global `--json` matches canonical fixtures.
- [ ] MCP `tools/list` exposes accurate bounded schemas for `agent_send` and `agent_inbox`.
- [ ] MCP tool calls delegate to agent-core rather than duplicate persistence or encryption logic.
- [ ] NDJSON events omit message bodies, support repeatable bounded cursor wakeups, and report reset/overflow.
- [ ] Rust `Workspace`, C FFI, and TypeScript bindings expose matching operations and result types.
- [ ] `docs/agent-api.md`, generated headers/types, and contract snapshots agree.
- [ ] The installed collaboration skill triggers for FeanorFS multi-agent coordination and uses the final tool names.

### Security and product boundaries

- [ ] Documentation states that recipient routing is not an access-control boundary.
- [ ] Documentation states that sender attribution is not cryptographically signed in v1.
- [ ] Message bodies are bounded and excluded from routine event output.
- [ ] Secrets and `.env` values are explicitly prohibited in agent signals even though transport is E2EE.
- [ ] FeanorFS does not interpret task success, assign work, enforce path ownership, wake models, or merge content.

---

## Technical Context

### Existing patterns to reuse

- `common/src/tree.rs` — `Snapshot` already carries encrypted `author`, `created_at_ms`, `message`, parents, and root.
- `agent-core/src/snapshot.rs` — owns encrypted snapshot writing, reachability manifests, and head compare-and-swap retry behavior.
- `agent-core/src/history.rs` — traverses reachable multi-parent history and produces stable `LogResult` entries.
- `agent-core/src/agent/land/publish.rs` — demonstrates file-publication CAS retry and multi-parent agent lands.
- `common/src/agent_contract.rs` — canonical additive JSON types and fixtures.
- `agent-core/src/lib.rs` — blocking `Workspace` facade consumed by every embedding.
- `client/src/cli/agent.rs` — visible agent CLI and `FEANORFS_AGENT` process identity convention.
- `client/src/cli/mcp.rs` — MCP schema and dispatch surface.
- `client/src/cli/events.rs` — current local NDJSON event loop and mirror-head polling.
- `feanorfs-ffi/src/lib.rs` and `feanorfs-ffi/feanorfs.h` — C JSON ABI.
- `bindings/ts/src/lib.rs` and `bindings/ts/contract.d.ts` — Node implementation and stable TypeScript contract.
- `client/tests/contract_snapshots.rs` — semver-sensitive JSON fixtures.
- `client/tests/sync_engine.rs` and `agent-core/src/agent/tests.rs` — real CAS, agent-land, and reconciliation coverage.
- `docs/sync-scope.md` — canonical policy for `.env`, gitignored paths, `.git`/`.jj`, and generated output.
- `docs/usage.md` and `README.md` — primary shared-WIP UX surfaces.
- `docs/threat-model.md` — security claims and trusted-participant boundaries.

### Required ownership

- Agent message operations live in `feanorfs-agent-core` first.
- Wire types live in `feanorfs_common::agent_contract`.
- `feanorfs-client` provides thin CLI/MCP/event adapters and re-exports.
- The hub remains generic opaque object/head/manifest storage.
- The skill contains behavior and tool usage, not encryption/CAS implementation details.

### Data model changes

- No server database migration.
- No project-local metadata.
- No new format-v3 tree or snapshot schema.
- Add canonical signal envelope and public result/query/input types.
- Store signals in the existing encrypted `Snapshot.message` field with the `ffmsg1:` discriminator.
- Store event/inbox cursors only in caller or private local state; never in the project or hub as read receipts.

### Compatibility

- Existing clients treat `ffmsg1` values as ordinary snapshot messages and continue syncing files correctly.
- Existing `LogEntry.message` remains an optional string; no field is renamed or removed.
- New SDK fields and types are additive under SDK-1.
- Older clients may display raw `ffmsg1` text in history but must not lose files or reject the workspace.
- Unknown future signal versions are ignored by typed inbox readers and retained in history.

---

## Verification Requirements

### Core and concurrency

- Unit-test canonical envelope encode/decode, enum validation, bounds, and malformed input.
- Test message-only snapshots retain the current tree root.
- Test send/send CAS races retain both messages.
- Test send/sync and send/agent-land races retain the newest tree and all messages.
- Test graph-delta inbox traversal over ordinary, merged, undo, and cursor-reset histories.
- Test that message send/inbox never materializes a project path.
- Test that unknown history messages remain unaffected.

### Security

- Inspect real hub storage in integration tests and assert that a unique plaintext message, sender, and recipient do not occur.
- Assert message bodies never appear in NDJSON wakeup events, hub logs, request URLs, or error text.
- Assert invalid snapshot IDs and oversized bodies fail before head publication.
- Assert private CA, token, E2EE, pairing, and recovery behavior is unchanged.

### Contracts and adapters

- Add canonical JSON fixture snapshots for `AgentSendResult`, `AgentMessage`, and `AgentInboxResult`.
- Add human and `--json` CLI tests.
- Add MCP schema and dispatch tests.
- Add C ABI smoke tests and regenerate `feanorfs.h`.
- Add TypeScript definitions, wrapper methods, and Node loop tests.
- Add NDJSON event tests for bounded metadata and deduplication.
- Validate the collaboration skill with the skill validator and forward-test it against a Linux-dev/macOS-test exchange.

### Repository checks

Run the relevant focused tests while iterating, followed by:

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features --locked
```

Also run existing SDK contract snapshots and Node/C smoke tests required by the crate-level `AGENTS.md` files.

---

## Success Metrics

### Quantitative

| Metric | Target | Measurement |
|---|---:|---|
| Project files changed by signal send/read | 0 | Before/after filesystem integration assertion |
| Git/Jujutsu metadata reads or writes introduced | 0 | Code review plus exclusion tests |
| New hub endpoints or plaintext message indexes | 0 | Router/schema diff |
| Maximum signal body | 8 KiB UTF-8 | Validation tests |
| Signal delivery model | Repeatable within explicit scan/result bounds; reset/overflow reports possible loss | Cursor/redelivery/overflow integration tests |
| Lost file updates during send/sync CAS races | 0 | Deterministic concurrency tests |
| NDJSON event body leakage | 0 bodies | Event contract tests |

### Qualitative

- A developer can explain why both Git clones are dirty without believing FeanorFS corrupted Git.
- The same developer can begin work on either macOS or Linux without changing the FeanorFS topology.
- A development agent can request a platform-specific test and receive an outcome tied to an exact snapshot.
- Operators can reason about messaging without learning a new server subsystem.

---

## Risks and Mitigations

| Risk | Likelihood | Impact | Mitigation |
|---|---|---|---|
| Users expect FeanorFS to keep Git `HEAD` clean or synchronized | High | High | Explicit shared-WIP model, publication flow, and branch warning; no misleading automation |
| Independent branch switches produce large apparent WIP changes | Medium | High | Document baseline alignment and separate workspace identities for simultaneous branches |
| A stale message CAS candidate rolls back files | Medium | Critical | Reload latest head and root on every retry; deterministic send/sync and send/land race tests |
| Message snapshots increase head/manifest churn | Medium | Medium | Bound message size and intended frequency; reject chat/log streaming; measure representative workspaces |
| Signal snapshots clutter human history | Medium | Low | Render recognized signals concisely; keep raw JSON compatibility |
| Agents act on a stale or different snapshot | Medium | High | `about_snapshot` default/validation plus skill requirement to report the snapshot actually tested |
| Sender names are spoofable by another workspace participant | Medium | Medium | Describe attribution as advisory; defer signatures rather than overclaim identity |
| Agents put secrets in messages | Medium | High | Skill, MCP descriptions, docs, event-body omission, and explicit all-participants-readable warning |
| A cursor misses messages across merge parents | Medium | High | Reachability-set graph delta rather than first-parent traversal |
| Offline agents are assumed to be awake | High | Medium | State that skills do not wake models; use event monitor/polling orchestrators |

---

## Alternatives Considered

### Remove Git from secondary machines

- **Pros:** No dirty Git status on build replicas.
- **Cons:** Prevents a secondary machine from naturally becoming a development machine; may break Git-aware tooling.
- **Decision:** Rejected as the primary UX. Build-only replicas remain possible.

### Synchronize `.git` or `.jj`

- **Pros:** Local metadata might appear aligned temporarily.
- **Cons:** Unsafe concurrent indexes/locks, platform-specific worktree state, repository corruption risk, and direct duplication of VCS responsibilities.
- **Decision:** Rejected and remains hard-excluded.

### Hide synchronized changes with `assume-unchanged` or `skip-worktree`

- **Pros:** Cleaner-looking status output.
- **Cons:** Git status becomes misleading and later operations can discard work.
- **Decision:** Rejected.

### Automatically create WIP commits or branches

- **Pros:** Git can transfer and identify changes.
- **Cons:** Pollutes intentional history, creates cleanup work, imports branch/merge semantics, and makes FeanorFS a Git workflow tool.
- **Decision:** Rejected.

### Store agent messages as synchronized project files

- **Pros:** Minimal transport implementation.
- **Cons:** Dirties Git, creates project litter and append conflicts, and mixes coordination with work product.
- **Decision:** Rejected.

### Add a hub messaging endpoint and database

- **Pros:** Efficient queue semantics and server push opportunities.
- **Cons:** Gives the hub agent-aware responsibilities and plaintext/metadata pressure; duplicates existing encrypted object transport.
- **Decision:** Rejected.

### Add a separate encrypted mailbox head

- **Pros:** Avoids workspace-head and manifest churn.
- **Cons:** Adds a second ref lifecycle, retention/GC rules, invite/recovery considerations, and protocol complexity.
- **Decision:** Deferred. Reconsider only with measured evidence that low-volume signal snapshots are too expensive.

### Rely only on MCP tool descriptions

- **Pros:** No skill distribution.
- **Cons:** Describes capability but does not reliably teach lifecycle behavior, snapshot discipline, or safety rules.
- **Decision:** Rejected. MCP remains self-describing, and a concise optional skill supplies procedural behavior.

---

## Non-Goals

- Keeping multiple Git clones automatically clean.
- Reading Git branch, index, remote, commit, stash, or worktree state.
- Running Git/Jujutsu commands or moving repository refs.
- Modeling FeanorFS workspaces as Git branches.
- Automatically selecting a commit machine.
- Synchronizing or repairing `.git`/`.jj`.
- Automatic WIP commits, stashes, pulls, rebases, merges, or resets.
- Chat rooms, threads, typing indicators, presence, reactions, read receipts, or message deletion.
- Exactly-once delivery.
- Private per-recipient encryption within one workspace.
- Cryptographic per-agent identity or signatures.
- Agent discovery, registration, leases, or enforced file ownership.
- General task planning, scheduling, or semantic interpretation.
- Agent process hosting, sandboxing, or wakeup by the skill itself.
- Message attachments, token streams, or raw build-log transport.
- Automatic conflict merging.

---

## Documentation Requirements

- [ ] Update `README.md` scope and background-sync copy with the shared-WIP/Git-baseline model.
- [ ] Add a complete “Git across machines” section to `docs/usage.md`.
- [ ] Extend `docs/sync-scope.md` with the `.env` plus shared tracked-WIP example.
- [ ] Add `docs/agent-communication.md` as the canonical human-readable signal protocol.
- [ ] Extend `docs/agent-api.md` with CLI, Rust, C, TypeScript, JSON, MCP, and cursor contracts.
- [ ] Extend `docs/threat-model.md` with shared-reader and advisory-attribution limitations.
- [ ] Ensure tray/onboarding copy says “shared working files/WIP” where “mirror” alone could imply Git cleanliness.
- [ ] Document generated-output exclusions for platform build/test agents.
- [ ] Document that a collaboration skill requires an active runner or orchestrator to poll/wake agents.

---

## Delivery Checklist by Surface

### Shared-WIP UX

- [ ] README explanation and example
- [ ] Usage guide for edit-machine switching, Git publication, baseline alignment, and branch changes
- [ ] Sync-scope `.env`/tracked-WIP example
- [ ] Relevant tray copy and copy tests
- [ ] Regression assertion that FeanorFS remains VCS-agnostic

### Core protocol

- [ ] `ffmsg1` canonical envelope and validation
- [ ] Message-only snapshot append with fresh-root CAS retry
- [ ] Reachability-delta inbox traversal
- [ ] Public input/query/result types and fixtures
- [ ] Concurrency, malformed-history, cursor, and encryption tests

### Product adapters

- [ ] Visible CLI `agent send` and `agent inbox`
- [ ] Rust `Workspace` methods and client re-exports
- [ ] C ABI functions/header and smoke tests
- [ ] TypeScript methods/types and smoke tests
- [ ] MCP tools and tests
- [ ] NDJSON wakeup event and tests
- [ ] Human history rendering for recognized signals

### Agent enablement

- [ ] Canonical protocol documentation
- [ ] `feanorfs-collaboration` skill initialized with the standard skill scaffold
- [ ] `SKILL.md`, protocol reference, and generated `agents/openai.yaml`
- [ ] Skill validation
- [ ] Forward test: Linux development request to macOS test agent
- [ ] Forward test: blocked response and stale-snapshot handling

---

## Resolved Questions

| Question | Decision |
|---|---|
| Should both machines keep Git? | Yes; both may be full development clones. |
| Should FeanorFS make Git clean? | No; dirty state truthfully represents shared WIP. |
| Should FeanorFS synchronize Git history? | No; Git remains responsible for publication and baseline advancement. |
| Should `.env` sync? | Yes, under the existing mirror policy. |
| Should messages be project files? | No. |
| Should the hub gain messaging semantics? | No. |
| Where do v1 messages live? | Versioned payloads in encrypted no-file-change snapshots. |
| Is this chat? | No; it is bounded coordination at workflow checkpoints. |
| How do agents learn the protocol? | Self-describing MCP plus an installable collaboration skill. |
| Can the skill wake an inactive agent? | No; an active orchestrator monitors events or polls inbox. |

---

## RFC Review Checklist

- [x] Problem statement covers human and agent workflows.
- [x] User flows cover cross-platform development, publication, branch changes, and platform testing.
- [x] Git and server boundaries are explicit.
- [x] Security and privacy limitations are explicit.
- [x] Interface contracts and compatibility expectations are defined.
- [x] Concurrency, cursor, history, and failure edge cases are covered.
- [x] Alternatives and non-goals prevent scope creep.
- [x] Acceptance criteria and verification requirements are executable.
