---
name: feanorfs-collaboration
description: Coordinate coding agents across machines with FeanorFS encrypted signals, agent workspaces, and conflict resolution. Use when an agent must send or read snapshot-linked messages, spawn or land isolated agent work, resolve overlapping edits, or operate inside a FeanorFS runner child.
---

# FeanorFS multi-agent collaboration

Coordinate coding agents in a shared FeanorFS workspace through the encrypted
signal protocol. Treat routing and authorship as advisory. Keep signals out of
project files and Git state.

## Continuous active agents (no manual transfers)

When you are running under `feanorfs agent run` (or as an enabled configured
runner's child), your agent worktree is continuously reconciled:

1. **Never run `feanorfs sync`, `push`, `pull`, `agent land`, or
   `agent refresh` yourself.** The live controller lands your saved changes
   after each quiet burst and refreshes paths you have not touched. Manual
   transfer commands are rejected while the controller owns your agent.
2. **Wait for a settled snapshot before a verification result.** Read
   `feanorfs --json agent status <name>` and use its `live.settled_snapshot`
   as the `--about` snapshot of a `result` terminal. If it is absent, do not
   publish a result; a correlated `blocked` reply may retain the request's
   snapshot without claiming that pending work settled. Never claim a snapshot
   you did not inspect; a signal-only head changes the observed head id but not
   the tree or the existing settled snapshot.
3. **Stop on attention.** If status shows `needs_attention` (conflicts,
   unsafe path, corrupt state) or the runner reports `cursor_reset` /
   `ambiguous_execution`, stop mutating files and await explicit resolution.
   Overlapping edits are never merged automatically.
4. **Expect bounded final reconciliation on exit.** After your process ends,
   FeanorFS makes one bounded final attempt and reports settled/offline/
   attention. Offline work is preserved. An interactive owner must be run
   again after connectivity returns; an enabled configured runner retries
   while its controller remains active.

## Identify your agent

1. Prefer `FEANORFS_AGENT` (set by `agent run` and runner children).
2. Fall back to a name you know you were spawned as.
3. Never claim a different agent's name; attribution stays advisory.
4. Use a workspace-unique agent name (for example `linux-dev`, `mac-test`, `ci1`).
5. Keep the original shared workspace root available: CLI signal commands
   (`agent inbox`, `agent send`) and MCP use that control workspace from
   inside `agent run`; the isolated agent worktree has no project-local
   FeanorFS configuration.

## Check your inbox

1. Run `feanorfs agent inbox` at startup.
2. Re-check after every `feanorfs sync`, `feanorfs agent refresh`, or `feanorfs agent land`.
3. Re-check before declaring a task complete.
4. To read only new signals, pass the previous `cursor` as `--after <cursor>`; a `cursor_reset` result means older signals may have been missed — do not assume complete delivery.
5. Use `--json` (`feanorfs --json agent inbox --for <name>`) when another program consumes the result.

## Act on a request

1. Read `about_snapshot` on every request.
2. Before acting outside a configured runner-child invocation, refresh from the shared workspace root: use `feanorfs sync --no-watch` for the main worktree, or `feanorfs agent refresh <name>` for an isolated agent worktree. A runner child uses the pre-refreshed procedure below and must not reacquire its parent's runner lease.
3. Verify file context with JSON `status`/`agent status` and `feanorfs --json log`. A newer signal-only head has empty `changed_paths` and the same files, so a different head ID alone does not invalidate `about_snapshot`. Do not claim the requested snapshot when intervening file changes, deferred conflicts, local agent edits, or bounded/ambiguous history prevent you from establishing the same file tree.
4. If you test a different file tree, set the final reply's `--about` to the snapshot you actually inspected, keep the original request through `--reply-to`, and name both the requested and inspected snapshots in the body. Never associate a result with an untested snapshot.
5. Send at most one `status` update per request, and only when it adds real information.
6. Finish every accepted request with one `result` or `blocked` reply; do not
   infer exactly-once delivery from that child-side contract.
7. Reference the original request with `--reply-to <message-id>` and keep its `about_snapshot` when its file tree still applies.

## Handle a configured runner-child invocation

Follow this procedure only when an already-configured local runner starts this
process. Do not configure, start, stop, reset, or remove a persistent runner
unless the user explicitly asks for operator control.

1. Read exactly one `RunnerInvocation` JSON document from stdin until EOF.
   Require `schema_version: 1`, retain its `session_id`, and require its
   `message` to be a direct `request` to its `agent`.
2. Check that the invocation agent agrees with `FEANORFS_AGENT` when that
   variable is present. Use `FEANORFS_AGENT_DIR` as the agent worktree and
   `FEANORFS_WORKSPACE_ROOT` as the shared control root.
3. Treat the agent worktree as already refreshed by the parent runner
   immediately before launch. Do **not** call ordinary `feanorfs sync` or
   `feanorfs agent refresh`: the parent holds the runner lifetime lease, so a
   child refresh is rejected. Verify `message.about_snapshot` against JSON
   agent status/history before acting, then follow steps 3–7 of the request
   procedure above. If the requested file tree cannot be established, do not
   act; publish exactly one correlated `blocked` terminal naming the requested
   and observed snapshot. Keep the original requester, request message ID, and
   snapshot context for that reply.
4. Treat stdout and stderr as diagnostics only. Do not use either as a reply:
   the runner discards both, and a process exit does not complete the request.
5. Publish one terminal `result` or `blocked` through `feanorfs agent send`
   (or `agent_send`) from the configured agent to `message.from`, with
   `--reply-to <message-id>` and an accurate `--about` snapshot. Send an
   optional `status` only when it adds real information.
6. Publish `blocked` when the bounded task cannot complete. If the runner
   reports `cursor_reset`, `pending_overflow`, `ambiguous_execution`,
   `delivery_unknown`, or `preparation_failed` (local preparation failed before
   launch), stop and await explicit operator inspection/reset; do not launch a
   replacement child or replay the request yourself.

## Send a signal

```bash
feanorfs agent send <recipient> --kind <request|status|result|blocked> \
  [--about <snapshot-id>] [--reply-to <message-id>] [--from <name>] "<body>"
```

1. `kind` must be `request`, `status`, `result`, or `blocked`.
2. `--about` defaults to the current workspace head; pass it explicitly when the snapshot matters.
3. `--reply-to` must reference a real signal snapshot; use it for `result`/`blocked` replies.
4. Use `*` as recipient only for genuinely broadcast announcements.
5. Keep bodies bounded (maximum 8 KiB) and use paths and summaries, never raw logs or file contents.

## Safety rules

1. Never send credentials, recovery kits, pairing codes, `.env` values, or any secret intended for fewer than all workspace participants. Every participant can read every signal.
2. Never send `.env` contents or private keys, even though transport is end-to-end encrypted.
3. Never route work by path ownership claims; treat them as advisory.
4. Resolve file conflicts with `feanorfs conflicts` / `feanorfs conflicts keep <path> --local|--cloud`; never merge file content automatically.
5. Never fabricate delivery guarantees: reads may redeliver, and a cursor reset means possible missed signals after cursor, scan, or result bounds.
6. Never configure or control a persistent runner unless the user explicitly asks for that operator action.

## Choose activation

- Treat a signal as passive transport: it cannot start an inactive model or
  arbitrary process by itself.
- Let an external orchestrator monitor the NDJSON `events` metadata stream or
  poll `feanorfs --json agent inbox --for <name>` with a stored cursor, then
  choose whether to invoke an agent.
- Let an explicitly configured local runner invoke only its fixed local command
  for direct requests to its configured agent. Broadcasts remain readable in
  the normal inbox but do not invoke the runner child. Treat signals and this
  skill as passive instructions: neither one wakes an inactive model or process.

Read `references/protocol.md` for the exact envelope format, direct-request
admission, correlation, and links to the operator/API documentation.

## Integrator role (random assignment)

A dispatcher may randomly select one temporary **integrator** for a bounded
batch. Accept the integrator role **only** when you are the selected candidate
for the current attempt:

1. Read the `ffint1` assignment request (`feanorfs agent inbox`): verify
   `selected` names you, the `assignment_id`, `attempt`, `about_snapshot`, and
   `roster_fingerprint`. Ignore assignments where another agent is selected.
2. Refresh (`feanorfs sync --no-watch` or `feanorfs agent refresh <name>`) and
   verify the requested file tree with JSON `status`/`log` before accepting.
   A newer signal-only head with the same files does not invalidate the
   assignment.
3. Reply `--kind status` with an `ffint1` `accepted` profile
   (`feanorfs agent send <dispatcher> --kind status --about <snapshot> "ffint1:{...}"`).
4. Before **every** mutating operation, re-check for supersession: read your
   inbox for a newer `ffint1` assignment to another agent, a revocation, or a
   cursor reset; stop when your attempt is superseded.
5. Work in an isolated agent workspace. Materialize encrypted conflict legs
   read-only on your machine with `feanorfs conflicts materialize` (artifacts
   land under private global FeanorFS state; the head never changes).
6. Reconcile only when every leg is available, intent is compatible, no
   security/product/data-loss ambiguity exists, and verification passes.
   Create and verify a candidate in the agent workspace, then apply **only**
   through the explicit `feanorfs conflicts keep <path> --file <reconciled>`
   (or `--local`/`--cloud`/`--both`) operation. Never merge file content.
7. Send exactly one terminal reply: `--kind result` with an `ffint1` `result`
   profile carrying the bounded digest (outcome, verification summary,
   counts, risks, at most one decision question), or `--kind blocked` with an
   `ffint1` `blocked` profile. Keep the original request in `--reply-to` and
   name the snapshot you actually inspected.
8. Escalate (one focused question, no code) when: implementations disagree on
   product behavior; either side can lose data; security, credentials,
   cryptography, recovery, or permissions are affected; public APIs/formats
   change without authorization; required tests fail or cannot run; a cursor
   reset occurred; a leg cannot be authenticated; you authored a conflicting
   side and no neutral reviewer exists.

Dispatchers own dispatch state; you do not. Never act after supersession, and
never treat assignment as a security claim — it is advisory coordination.

## Work-intent protocol (proposal before mutation)

Coordinate intended write scope through encrypted `ffwork1` profiles before
mutating files. This is a prevention layer, not access control: identity,
path claims, and authorship remain advisory, and the hub never decides.

1. **Propose before mutation.** Before editing a scope that other agents may
   touch, run `feanorfs agent work propose --task <id> --path <p>...`
   (exact canonical paths or `dir/**` containment globs, sorted and unique)
   with `--coordinator <name>` when the proposal names one. Treat the result
   as `proposed`, never as accepted.
2. **Acceptance requires an observed decision.** Only the coordinator named
   by the proposal (or the operating context, default `human`) can apply a
   decision: `feanorfs agent work decide <proposal-message-id> --kind
   accept|reject|narrow|order|accept-overlap`. Do not start editing an
   accepted scope until `feanorfs agent work status` shows the proposal
   `accepted`; silence and timeouts never imply acceptance or yield.
3. **Observe continuously.** Run `feanorfs agent work status` before and
   after each mutation, and before declaring completion. If it reports
   `projection_incomplete`, acceptance is not fully provable: stop mutating
   until the closure is complete. Re-propose with a higher `--sequence` after
   a rejection instead of reusing an old proposal id.
4. **Amend explicitly.** Scope changes after acceptance go through
   `feanorfs agent work amend --task <id> --intent <intent-message-id>` with
   replacement paths/concerns/dependencies. Never silently edit outside the
   accepted scope.
5. **Yield explicitly.** Relinquish accepted overlap with
   `feanorfs agent work yield` while preserving local work; hand the overlap
   back to the coordinator instead of racing.
6. **Settle and finish.** Attach verification evidence naming the snapshot
   actually inspected (`feanorfs agent work settle --inspected <snapshot>`),
   then `feanorfs agent work complete` or `feanorfs agent work block`.
   Reference the exact accepted intent message id in every transition.
7. **No clock-only ownership inference.** Never claim another agent's scope
   because a timestamp is older or newer; causal references and observed
   decisions are the only authority. Author transitions key by
   `(task_id, agent, sequence)`; decisions key by exact proposal message id
   plus the authorized coordinator identity.
8. **Harness-neutral.** This protocol works identically through the CLI,
   Rust SDK, C FFI, TypeScript, MCP (`work_*` tools), and NDJSON `work_*`
   wakeups. Send operations never mutate the projection; only an observed
   signal does. Unknown/malformed `ffwork` profiles are ignored by the typed
   surfaces and never partially apply.

## Exact conflict resolution (scope first, resolution last)

Automatic conflict resolution is a last-resort pipeline for conflicts that
prevention could not avoid. Scope comes first; resolution comes last;
candidate submission never publishes; only guarded apply does. Legacy
path-only conflicts stay on the manual `conflicts keep` path and can never
enter automatic prepare/apply.

1. **Scope first, always.** Propose and observe accepted scope through the
   work-intent protocol (`feanorfs agent work propose …` / `work status`)
   before touching any path a conflict may involve. Never prepare a
   resolution job for a conflict you did not help scope.
2. **Prepare only after prevention is exhausted.** Run
   `feanorfs agent resolution prepare <path> --reason exhausted|violated
   --detail <text>` only when a real current conflict exists and every
   bounded prevention path is genuinely exhausted (or violated). Read the
   returned job: verify the `job_id`, `assignment_id`, `attempt`, `owner`,
   and the exact `conflict_fingerprint`. Prepare is read-only: it never
   mutates the worktree, conflict registry, artifacts, or head.
3. **Write the candidate to the engine-owned destination only.** The job's
   `candidate_destination` is create-new and immutable; create the candidate
   there and never overwrite it. The job's `allowed_output_paths` bound every
   path the harness may touch.
4. **Submit a validated result; submission never applies.** Submit one
   `ResolutionResult` (`feanorfs agent resolution submit <job-id> --result
   <file>`) with the exact job/assignment/attempt/fingerprint, a candidate
   descriptor whose hash/size/mode/deletion match the file, and passed
   verification evidence. Replay is rejected. Submission changes nothing
   outside the job store.
5. **Apply only with fresh verification.** `feanorfs agent resolution apply
   <job-id>` is the only publishing operation; it revalidates every identity
   field and the candidate immediately before one CAS. A typed stale outcome
   means the current conflict survived unchanged — stop, re-inspect, and
   re-prepare against the new head; never retry the same candidate blindly.
6. **Escalate with exactly one bounded question.** Send `requires_human`
   (one `question`, one typed `human_reason`) only for the allowed reasons:
   semantic ambiguity, unavoidable data loss, missing/authentication-failed
   leg, security/compatibility boundary change, required verification
   unavailable, indeterminate ownership, bounded resolver exhaustion,
   unsupported size/safety bound, or one explicit product decision. Offline
   conditions, first timeouts, signal-only heads, stale candidates, and
   ordinary lost CAS are never human reasons.
7. **Status is metadata only.** `feanorfs agent resolution status [<job-id>]`
   reports ids/state/counts (assignment state, submitted outcome) — never
   paths or bodies. The NDJSON stream emits `resolution_prepared`,
   `resolution_submitted`, `resolution_applied`, and `resolution_revoked`
   metadata wakeups on transitions.
8. **Protocol observation is metadata only.** `feanorfs agent resolution protocol-status [--rebuild]` projects the encrypted `ffres1` signal stream (ids/state/counts only). `feanorfs agent resolution assign <job-id>` publishes the assignment profile, `feanorfs agent resolution reply <job-id>` publishes the result profile, and `feanorfs agent resolution revoke <job-id> [--superseded]` publishes the revoke/supersede profile. The NDJSON stream adds `resolution_assigned`, `resolution_result_received`, and `resolution_human_answered` wakeups on protocol transitions.
9. **Human escalation is exact and local-first.** `feanorfs agent resolution answer <job-id> --defer|--keep-unresolved|--candidate <file>` records one typed human answer bound to the live projection (identity fields are never caller-supplied, so stale answers are impossible by construction); `feanorfs agent resolution publish-answer <job-id> --defer|--keep-unresolved|--candidate <file>` sends the `ffres1` human-answer profile. `feanorfs agent resolution defer <job-id>` records the terminal deferred state. `feanorfs agent resolution materialize <job-id>` reconstructs the conflict legs by id. `feanorfs agent resolution put <job-id> <file>` writes the immutable engine-owned candidate.
10. **The tray only projects.** Tray/menu surfaces show resolution counts and
   status; mutation stays in the CLI (`feanorfs agent resolution
   prepare|submit|apply|answer|defer|assign|reply|revoke|publish-answer`).
   Manual `conflicts keep` and human resolution remain available for every
   pending conflict.
