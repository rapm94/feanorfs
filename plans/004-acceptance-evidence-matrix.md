# Plan 004: Structure the AI-2 / AI-5 / AI-6 acceptance-evidence matrix into a runnable verification plan

> **Executor instructions**: This is a spike/design plan. Its deliverable is
> a structured evidence document plus (only where trivially scriptable) thin
> wrappers that invoke EXISTING commands — it must not implement new product
> behavior. Follow the steps, run the read-only/sandboxed verifications,
> write the deliverable, and update `plans/README.md`.
>
> **Drift check (run first)**: `git diff --stat 51d2ba8..HEAD -- TODO.md docs/ scripts/`
> If TODO.md changed since this plan was written, re-read the AI-2, AI-5, and
> AI-6 sections live and reconcile them with the excerpts below; treat
> contradictions as a STOP condition.

## Status

- **Priority**: P2
- **Effort**: M
- **Risk**: LOW
- **Depends on**: none (but its evidence feeds the next release decision)
- **Category**: direction (design/spike)
- **Planned at**: commit `51d2ba8`, 2026-08-21

## Why this matters

`TODO.md` gates the next release on three unfinished acceptance items:
AI-2 (mixed-version protocol peers must converge without corrupting typed
projections), AI-5 (workspace-state identity/retirement verified on Linux and
Windows CI runners), and AI-6 (continuous-agent field verification: process
ownership, two-agent conflict soak, p95 < 3 s LAN convergence). Today these
live as prose checkboxes; there is no single artifact that says which
evidence exists, which command produces missing evidence, and what "done"
means per cell. This plan turns them into a machine-readable-ish evidence
matrix so the maintainer can see release readiness at a glance and any
executor can produce the missing cells without re-deriving intent.

## Current state

- `TODO.md` sections (read them in full first):
  - `AI-2. Mixed-version protocol peers on released products` — requires an
    older released product vs a newer one exchanging `ffwork1` intents and
    `ffres1` profiles; unknown/malformed profiles must not create or alter
    typed projection entries; legacy unfingerprinted conflicts stay
    manual-only.
  - `AI-5. Verify portable workspace-state identity and retirement on CI` —
    implementation landed; remaining checkbox is the same matrix on Linux and
    Windows CI runners (cfg-gated tests already added).
  - `AI-6. Finish continuous-agent field verification` — process ownership
    and shutdown across OSes, network-isolated two-agent soak with genuine
    conflict + recovery, and measured small-file LAN convergence p95 < 3 s.
    References `prd-continuous-agent-development.md` for the full matrix.
- Existing tooling to cite, not reinvent:
  - `scripts/smoke-macos-product.sh`, `scripts/smoke-windows-product.ps1`
    (per client/AGENTS.md Verification section)
  - `cargo test --workspace --all-features --locked`
  - cfg-gated identity tests referenced by agent-core/AGENTS.md
    (`workspace_layout.rs`, `workspace_state_registry.rs`)
- Repo conventions: acceptance records are secret-free, record only
  OS/version and outcomes; founder tasks F1/F2/AI-1 require real installed
  products and are explicitly NOT satisfiable from source builds.

## Commands you will need

| Purpose | Command | Expected |
|---|---|---|
| Baseline suite | `cargo test --workspace --all-features --locked` | pass |
| CI workflow inspection | `rg -n 'windows-latest\|ubuntu' .github/workflows/ci.yml` | shows which runners run which jobs |
| Product smoke (macOS host) | `bash scripts/smoke-macos-product.sh FEANORFS_BIN FEANORFS_TRAY_BIN` | exit 0 when binaries supplied |

## Scope

**In scope**:
- Create `docs/acceptance-evidence.md` (new file)
- Optionally add one thin script `scripts/acceptance-matrix.sh` that prints
  the current local-evidence status by running existing commands
- Read-only runs of existing tests/scripts

**Out of scope** (do NOT touch):
- Any product source under `common/ server/ client/ agent-core/ tray/`
- `.github/workflows/*` — CI changes need maintainer review of runner costs;
  recommend them in the doc instead
- TODO.md itself — only the maintainer marks those boxes after real evidence

## Git workflow

- Branch: `advisor/004-acceptance-matrix`
- Commit style: conventional commits, e.g. `docs(acceptance): structure AI-2/AI-5/AI-6 evidence matrix`
- Do NOT push or open a PR unless the operator instructed it.

## Steps

### Step 1: Inventory what already passes locally

Run `cargo test --workspace --all-features --locked`. Map test names onto
matrix cells: identity/retirement tests → AI-5 rows (macOS column);
protocol reducer tests (`work.rs`, `resolution_protocol.rs`, contract
snapshot tests) → AI-2 rows (same-version column). Record actual test names.

**Verify**: suite exits 0; you have a list of covering test names per cell.

### Step 2: Write `docs/acceptance-evidence.md`

Structure:

```markdown
# Acceptance Evidence Matrix (AI-2 / AI-5 / AI-6)

Generated <date>, planned-at commit 51d2ba8. Secret-free.

| Item | Cell | Requirement | Evidence today | Command to produce | Status |
|------|------|-------------|----------------|--------------------|--------|
| AI-5 | macOS | same-path replacement, relocation, adoption refusal, lease contention, tombstone cleanup | <test names from Step 1> | cargo test ... | PASS/MISSING |
| AI-5 | Linux CI | same matrix | — | push branch; .github/workflows/ci.yml job <name> | MISSING |
| AI-5 | Windows CI | same matrix (+ windows-v2 standalone typecheck) | — | same | MISSING |
| AI-2 | v(n)-vs-v(n+1) ffwork1 | unknown profile rejected, cursors may advance | <test names or none> | two installed products per TODO.md | MISSING |
| AI-2 | ffres1 assignment/result/answer | projection convergence identical both directions | ... | ... | MISSING |
| AI-6 | process ownership per OS | supervisor restart during reconciliation, duplicate-owner rejection | ... | smoke scripts + manual | ... |
| AI-6 | two-agent soak | zero lost updates, zero echo loops | none yet | soak procedure below | MISSING |
| AI-6 | LAN convergence | p95 < 3 s small-file two-client | none yet | timing harness below | MISSING |
```

For MISSING AI-6 cells include a short concrete procedure (commands +
expected observations), not prose wishes: e.g. the soak = two workspaces on
one LAN hub, `agent run` both, inject conflicting edits to one path, resolve
via `conflicts keep`, disconnect one side mid-flight, reconnect, then diff
both worktrees and grep logs for lost-update markers.

### Step 3: Add the status printer script (optional but preferred)

`scripts/acceptance-matrix.sh`: bash, `set -euo pipefail`, runs only local
evidence commands (test filters from Step 1) and prints one line per local
cell: name + PASS/FAIL/SKIP. No network, no installs, no artifacts outside
`target/`.

**Verify**: `bash scripts/acceptance-matrix.sh` → exit 0, every line names a
cell.

### Step 4: Cross-check CI coverage claims

Read `.github/workflows/ci.yml` and state in the doc exactly which AI-5
tests run on which runner today, citing job names. Do not edit workflows.

**Verify**: doc contains at least one citation of the form
"`.github/workflows/ci.yml:<line>` — job `<name>` on `<runner>`".

## Test plan

- No new product tests. The script is verified by running it (Step 3).

## Done criteria

- [ ] `docs/acceptance-evidence.md` exists with all three items as row
      groups and no empty Evidence-today column entries (every cell says
      either concrete evidence or explicit MISSING)
- [ ] Every MISSING cell names the exact command or procedure that produces
      the evidence
- [ ] Script (if added) exits 0 locally
- [ ] `cargo test --workspace --all-features --locked` still exits 0
      (untouched, but proves baseline claim)
- [ ] No source files modified (`git status` shows only new docs/script)
- [ ] `plans/README.md` status row updated

## STOP conditions

Stop and report back if:

- TODO.md's AI-2/AI-5/AI-6 text has materially changed since 51d2ba8 such
  that the matrix rows above no longer match.
- The workspace suite fails on your machine for pre-existing reasons — note
  failures instead of fixing them (out of scope).
- You find yourself wanting to modify a workflow or product code — that is
  out of scope by design; document the recommendation instead.

## Maintenance notes

- The maintainer updates TODO.md boxes only after executing procedures on
  real environments; this matrix is the map, not the territory.
- When AI-1 lands (installed products), AI-2 rows upgrade from "procedure"
  to executable against releases — revisit then.
