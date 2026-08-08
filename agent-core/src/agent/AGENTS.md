# agent

## Purpose

Own agent workspace diffing, spawning, landing, refreshing, unattended runner lifecycle state, proposal generation, and focused validation tests. `../agent.rs` remains the public facade.

## Ownership

- `diff.rs` — three-way snapshot comparison and land candidate construction.
- `spawn.rs` — synchronized workspace copy with cleanup guard.
- `land.rs` + `land/` — conflict-gated land orchestration, head publication, and guarded materialization.
- `refresh.rs` — preserve/replace refresh semantics.
- `runner.rs` — durable body-free runner admission/execution state, generation-bound stores, lease-owning execution sessions, a read-only spawned-process identity for exact local orphan cleanup, configured-runner removal, and mutation guards.
- `runtime.rs` — agent-owned cache state and non-destructive legacy auxiliary-state adoption.
- `proposal.rs` — optional textual conflict proposals.
- `check.rs` and `tests.rs` — preview surface and name validation tests.

## Local Contracts

- Head compare-and-swap is land commit point. Every clean landed blob is uploaded, authenticated back through the object cache, and fsynced before CAS; publication still precedes activation in the main worktree.
- Preserve `after-stage`, `after-cas`, and `after-materialize` fault boundaries. Activation uses the shared rollback/journal materializer so a retry can complete a committed land without structural partial state.
- Land validates the full canonical candidate and materialization projection before each commit path. File↔directory transitions are applied as one staged plan; concurrent ancestor/descendant shapes fail closed before CAS.
- Refresh takes land then sync locks, batches all remote additions/deletions into one structural plan, and advances the agent base only after activation succeeds.
- Supervised runner execution requires persisted `enabled=true`; explicit foreground execution requires `enabled=false`. Inbox admission and begin/spawn/terminal/delivery transitions exist only on an exact configuration-bound execution session that owns the lifetime lease. Admission validates the session mode against current enablement inside the same state update as all cursor/pending changes. Any dropped active session is ambiguous on reacquisition, and unobservable terminal delivery pins the request as `delivery_unknown` without retrying it or advancing its inbox cursor.
- Enabling and destructive reset acquire the lifecycle lock, exact-generation lifetime lease, then state lock. Disabling is intentionally asymmetric: it is a generation-checked state-only cancellation/admission-stop signal that preserves pending and active checkpoints while a supervised session finishes its current terminal transition; future supervised begin admission fails while disabled.
- Inbox failure/recovery accounting is session-bound but mode-agnostic, so the lease owner can record polling health after disable without admitting or clearing work.
- Ordinary refresh and land hold one operation guard for the full mutation: either the configured agent's nonblocking lifetime lease or the workspace lifecycle lock proving that agent cannot become configured mid-operation. Lock order is lifecycle then lifetime. Trusted runner refresh reuses the same implementation through an identity-checked execution session; unconfigured agents never gain runner state as a side effect.
- `land(clean_after=true)` rejects a configured runner before context/network/worktree activity and requires explicit runner removal. Unconfigured cleanup reuses its already-held operation lifecycle guard instead of reacquiring it.
- Runner removal holds the workspace lifecycle lock plus the nonblocking lifetime lease, requires disabled state and explicit discard for pending/active/attention state, and removes only `state/runner` so worktree, base, and runtime remain intact.
- Every configure/reconfigure persists a new random generation. All `RunnerStore` handles and lifetime leases bind that generation and fail stale after removal or replacement.
- Agent scan/materialization cache lives at `agents/<name>/state/runtime`; an agent worktree is never registered as another top-level workspace. First open copies only identity-verified legacy `local_state.json` under source/destination locks and preserves the legacy directory. `spawn --replace` rotates any existing named agent root, even when its worktree is missing, through the rollback guard so failure restores worktree/base/runtime together and success starts with a fresh runtime cache. A failed rollback reports and preserves the original backup; backup deletion begins only after the new worktree and base ref are published. `clean_agent` removes the owned worktree, base ref, and runtime state together.
- Folder changes after land gating must divert rather than overwrite.
- Conflict content is surfaced, never auto-merged into working files.

## Work Guidance

- Keep public operations re-exported from `../agent.rs`.
- Keep land phases typed and independently reviewable.

## Verification

- `cargo test -p feanorfs-agent-core --locked`
- `cargo test -p feanorfs-client --test sync_engine --locked`

## Child DOX Index

No child DOX files.
