# Plan 002: Bound the hub-to-hub object transfer with limited concurrency

> **Executor instructions**: Follow this plan step by step. Run every
> verification command and confirm the expected result before moving to the
> next step. If anything in the "STOP conditions" section occurs, stop and
> report — do not improvise. When done, update the status row for this plan
> in `plans/README.md` — unless a reviewer dispatched you and told you they
> maintain the index.
>
> **Drift check (run first)**: `git diff --stat 51d2ba8..HEAD -- client/src/hub_transfer.rs`
> If that file changed since this plan was written, compare the "Current
> state" excerpts against the live code before proceeding; on a mismatch,
> treat it as a STOP condition.

## Status

- **Priority**: P1
- **Effort**: M
- **Risk**: MED
- **Depends on**: none
- **Category**: perf
- **Planned at**: commit `51d2ba8`, 2026-08-21

## Why this matters

`transfer_snapshot_history` in `client/src/hub_transfer.rs` copies a
workspace's complete reachable encrypted history from one hub to another by
downloading and re-uploading each object in a serial loop. Every object pays
a full download round-trip plus an upload round-trip with no overlap, so a
history of N objects costs roughly N×2×RTT plus transfer time. For a
long-lived workspace (thousands of small objects) this makes hub migration
and relay rehosting painfully slow. Bounding 4–8 transfers in flight removes
most of the latency wall while preserving the protocol's ordering rule:
every object must be stored before any reachability manifest is published,
and manifests before the head compare-and-swap.

## Current state

- `client/src/hub_transfer.rs` — one-shot cross-hub history copy.
  The serial loop (`client/src/hub_transfer.rs:221-233`):

    ```rust
    for hash in &history.hashes {
        let ciphertext = source_api
            .download_file(hash)
            .await
            .with_context(|| format!("read source object {hash}"))?;
        if hash_bytes(&ciphertext) != *hash {
            bail!("source object hash mismatch for {hash}");
        }
        destination_api
            .upload_object(workspace_id, hash, ciphertext)
            .await
            .with_context(|| format!("write destination object {hash}"))?;
    }
    ```

  Ordering constraints already encoded around it (must survive the change):
  - objects loop completes BEFORE the manifest loop at lines 238-248;
  - head is only swapped after manifests publish (lines 250-266);
  - source-head stability is re-checked after the object copy (line 235).
- Conventions: `futures_util` is a workspace dependency; the codebase already
  uses `futures_util::StreamExt` (see `agent-core/src/sync_pass/mod.rs`
  imports). Errors are `anyhow::Result` with `.with_context`. Bounded
  concurrency in this repo means explicit numeric caps, not unbounded joins.

## Commands you will need

| Purpose | Command | Expected on success |
|---|---|---|
| Build | `cargo build --workspace --locked` | exit 0 |
| Target tests | `cargo test -p feanorfs-client --locked hub_transfer` | all pass |
| Client suite | `cargo test -p feanorfs-client --locked` | all pass |
| Lint | `cargo clippy -p feanorfs-client --all-targets --locked -- -D warnings` | exit 0 |

If no `hub_transfer` test filter matches, use the full client suite.

## Scope

**In scope** (the only files you should modify):
- `client/src/hub_transfer.rs`

**Out of scope** (do NOT touch):
- Any API method on `ApiClient` (`agent-core/src/api.rs`) — reuse
  `download_file` / `upload_object` as-is.
- Manifest publication, head swap, or source-stability checks — their order
  relative to the object loop is load-bearing.
- Server code.

## Git workflow

- Branch: `advisor/002-concurrent-hub-transfer`
- Commit style: conventional commits, e.g. `perf(client): bound concurrent object transfer between hubs`
- Do NOT push or open a PR unless the operator instructed it.

## Steps

### Step 1: Replace the serial loop with a bounded unordered stream

In `client/src/hub_transfer.rs`, replace the loop above with:

```rust
use futures_util::StreamExt;

/// ponytail: fixed cap; raise only if real-world transfers show headroom.
const TRANSFER_CONCURRENCY: usize = 6;

futures_util::stream::iter(history.hashes.iter())
    .map(|hash| async move {
        let ciphertext = source_api
            .download_file(hash)
            .await
            .with_context(|| format!("read source object {hash}"))?;
        if hash_bytes(&ciphertext) != *hash {
            anyhow::bail!("source object hash mismatch for {hash}");
        }
        destination_api
            .upload_object(workspace_id, hash, ciphertext)
            .await
            .with_context(|| format!("write destination object {hash}"))
    })
    .buffer_unordered(TRANSFER_CONCURRENCY)
    .try_collect::<Vec<()>>()
    .await?;
```

Keep everything after the loop byte-identical: the source-head re-check
(line 235) must still run only after ALL objects are stored.

**Verify**: `cargo build --workspace --locked` → exit 0.

### Step 2: Confirm failure semantics

`buffer_unordered` + `try_collect` stops polling new futures after the first
error but does not abort in-flight ones — that is acceptable: a failed
transfer leaves the destination without some objects, no manifest or head is
published afterward, so the destination stays consistent and a retry
re-copies idempotently (objects are content-addressed; re-upload is safe).

Add one test proving partial-failure safety: with a stub/failing destination
(see existing test harnesses under `client/tests/` for how hub fakes are
built), assert that after a mid-transfer failure, neither
`upload_manifest` nor `swap_head` was called for the affected snapshot set.

**Verify**: `cargo test -p feanorfs-client --locked` → all pass including
the new test.

## Test plan

- New test: mid-transfer destination failure → no manifest/head publication;
  model after the closest existing `hub_transfer` integration test in
  `client/tests/` (locate with `rg -l 'hub_transfer|transfer_snapshot' client/tests/`).
- Existing suite must stay green — especially ordering-sensitive tests.

## Done criteria

- [ ] Object copy loop uses `buffer_unordered` with an explicit constant cap
- [ ] Source-head re-check still executes strictly after the object phase
- [ ] New failure-ordering test passes
- [ ] `cargo test --workspace --all-features --locked` exits 0
- [ ] `cargo clippy --workspace --all-targets --locked -- -D warnings` exits 0
- [ ] No files outside `client/src/hub_transfer.rs` modified (`git status`)
- [ ] `plans/README.md` status row updated

## STOP conditions

Stop and report back if:

- The excerpt at `client/src/hub_transfer.rs:196-271` has drifted (function
  renamed, reordered phases, registry-skip logic added inside the loop).
- You discover per-object sequencing dependencies inside the loop (e.g.
  chunk uploads requiring parent manifests first) — those would forbid
  unordered execution; report instead of improvising an ordering scheme.
- The verification command for the new test fails twice after one fix
  attempt.

## Maintenance notes

- If `ApiClient` gains batch endpoints later, revisit this loop first —
  batching beats concurrency here.
- Reviewers should scrutinize: memory profile (each in-flight task holds one
  full ciphertext; cap × max-object-size is the worst case), and that error
  context names both source read and destination write failures.
