# Plan 001: Verify decrypted plaintext so a failed AEAD decrypt can never silently yield garbage

> **Executor instructions**: Follow this plan step by step. Run every
> verification command and confirm the expected result before moving to the
> next step. If anything in the "STOP conditions" section occurs, stop and
> report — do not improvise. When done, update the status row for this plan
> in `plans/README.md` — unless a reviewer dispatched you and told you they
> maintain the index.
>
> **Drift check (run first)**: `git diff --stat 51d2ba8..HEAD -- common/src/lib.rs agent-core/src/sync_pass/download.rs agent-core/src/sync_pass/materialize/`
> If any in-scope file changed since this plan was written, compare the
> "Current state" excerpts against the live code before proceeding; on a
> mismatch, treat it as a STOP condition.

## Status

- **Priority**: P1
- **Effort**: M
- **Risk**: MED
- **Depends on**: none
- **Category**: security
- **Planned at**: commit `51d2ba8`, 2026-08-21

## Why this matters

FeanorFS workspaces configured with format v1 decrypt blobs under
`LegacyPolicy::AllowXorFallback`. In that mode,
`unpack_bytes_with_policy` (`common/src/lib.rs`) has a branch where an
AEAD-prefixed blob whose ChaCha20-Poly1305 authentication FAILS is
"decrypted" by the legacy XOR routine instead of erroring. The XOR of AEAD
ciphertext is deterministic garbage, and it is returned as `Ok`. This exists
to rescue genuine legacy v1 blobs whose first plaintext byte happens to equal
the AEAD prefix byte (~1/256 of legacy blobs), so it cannot simply be deleted.
The dangerous case is real content flowing to disk unverified: ciphertext
hashes are checked before decryption, but nothing re-checks plaintext after
it, so a wrong-key or edge-corrupted read can materialize garbage file bytes.
Tree/snapshot objects are safe today (they use `Reject`, and corrupt JSON
fails loudly). This plan adds expected-plaintext-hash verification at the
file-content boundary where the caller already knows the hash, making silent
garbage impossible there while preserving the legacy rescue.

## Current state

- `common/src/lib.rs` — shared crypto helpers. The vulnerable function:
  - `common/src/lib.rs:592-626`:

    ```rust
    /// Decrypts packed blob (ChaCha20-Poly1305 or legacy XOR per policy).
    pub fn unpack_bytes(data: &[u8], password: &str, path: &str) -> Result<Vec<u8>> {
        unpack_bytes_with_policy(data, password, path, LegacyPolicy::AllowXorFallback)
    }

    /// Decrypt with an explicit legacy-blob policy (format v2 uses `Reject`).
    pub fn unpack_bytes_with_policy(
        data: &[u8],
        password: &str,
        path: &str,
        policy: LegacyPolicy,
    ) -> Result<Vec<u8>> {
        if data.first() == Some(&AEAD_PREFIX_BYTE) && data.len() > 13 {
            // ... AEAD decrypt ...
            match cipher.decrypt(nonce, &data[13..]) {
                Ok(plain) => return Ok(plain),
                Err(_) if policy == LegacyPolicy::AllowXorFallback => {
                    return Ok(crypt_bytes(data, password, path));   // <-- silent garbage
                }
                Err(_) => {
                    anyhow::bail!("wrong encryption key for this workspace (decryption failed)");
                }
            }
        }
        match policy {
            LegacyPolicy::Reject => anyhow::bail!(...),
            LegacyPolicy::AllowXorFallback => Ok(crypt_bytes(data, password, path)),
        }
    }
    ```

  - Existing unit tests to model new tests after: `common/src/lib.rs:1100-1140`
    (roundtrip, legacy recovery, Reject-policy error tests live here).
- Callers of `unpack_bytes_with_policy`:
  - `agent-core/src/objects.rs:429` — tree/snapshot object reads, passes
    `LegacyPolicy::Reject` (already safe; do not change).
  - `agent-core/src/large_file.rs:254,313` — chunk-manifest roots, pass
    `ctx.policy`; `agent-core/src/large_file.rs:426,474` — manifest/chunk
    opens, pass `LegacyPolicy::Reject`.
  - The file-content decrypt boundary is inside the staged materializer
    (`agent-core/src/sync_pass/materialize/`). The download pipeline already
    threads the expected value: `agent-core/src/sync_pass/download.rs:42`
    (`plaintext_hash: String` field) and `download.rs:196-227` where the
    materialized result returns `(materialized.plaintext_hash, ...)`.
- Repo conventions: errors use `anyhow` with `bail!` / `.with_context(...)`;
  hash helper is `feanorfs_common::hash_bytes(&[u8]) -> String`; result types
  are plain `Result<T>`; no `println!` outside CLI UI modules.

## Commands you will need

| Purpose | Command | Expected on success |
|---|---|---|
| Build | `cargo build --workspace --locked` | exit 0 |
| Common tests | `cargo test -p feanorfs-common --locked` | all pass |
| Agent-core tests | `cargo test -p feanorfs-agent-core --locked` | all pass |
| Client tests | `cargo test -p feanorfs-client --locked` | all pass |
| Lint | `cargo clippy --workspace --all-targets --locked -- -D warnings` | exit 0 |

## Scope

**In scope** (the only files you should modify):
- `common/src/lib.rs` (add verified variant + tests)
- `agent-core/src/sync_pass/materialize/*` and/or
  `agent-core/src/sync_pass/download.rs` (wire the expected hash at the
  file-content decrypt call site)

**Out of scope** (do NOT touch):
- `agent-core/src/objects.rs` — object reads are `Reject` and correct.
- `agent-core/src/large_file.rs` — chunk transport already authenticates
  manifests with `Reject` and verifies chunk ciphertext hashes.
- Any wire format, cache schema, or `LegacyPolicy` enum shape visible to
  other crates beyond adding one new function.
- Migration code (`client/src/migrate*`).

## Git workflow

- Branch: `advisor/001-aead-xor-fallback-integrity`
- Commit style: conventional commits, e.g. `fix(common): verify plaintext after legacy fallback decrypt`
- Do NOT push or open a PR unless the operator instructed it.

## Steps

### Step 1: Add a hash-verified decrypt variant in common

In `common/src/lib.rs`, next to `unpack_bytes_with_policy`, add:

```rust
/// Decrypt like [`unpack_bytes_with_policy`], then prove the result is the
/// expected plaintext. `expected_plaintext_hash` is the Blake3 hex digest of
/// the plaintext (as stored in cache entries). Use everywhere the caller
/// knows the expected hash; keep `unpack_bytes_with_policy` for callers that
/// do not (tree objects parse-validate instead).
pub fn unpack_bytes_verified(
    data: &[u8],
    password: &str,
    path: &str,
    policy: LegacyPolicy,
    expected_plaintext_hash: &str,
) -> Result<Vec<u8>> {
    let plain = unpack_bytes_with_policy(data, password, path, policy)?;
    if hash_bytes(&plain) != expected_plaintext_hash {
        anyhow::bail!(
            "decrypted content does not match its recorded hash (wrong encryption key or corrupted blob)"
        );
    }
    Ok(plain)
}
```

Note: this intentionally covers BOTH success paths — AEAD success AND the
XOR fallback at line 612-613 — because verification happens after
`unpack_bytes_with_policy` returns. That is what turns the silent-garbage
branch into a detected failure whenever the hash is known.

**Verify**: `cargo build -p feanorfs-common --locked` → exit 0.

### Step 2: Wire the expected hash at the file-content decrypt site

Locate the single place where downloaded file bytes are decrypted during
materialization: `rg -n 'unpack_bytes' agent-core/src/sync_pass/materialize/`.
At that call site the expected plaintext hash is available from the download
item (see `agent-core/src/sync_pass/download.rs:42` and the
`(plaintext_hash, ...)` tuple at `download.rs:196-227`). Replace the
`unpack_bytes_with_policy(...)` call with
`unpack_bytes_verified(..., &item_plaintext_hash)`, threading the hash
parameter through whatever function signature needs it. Change no behavior
when the hash matches; failures now bail with the message from Step 1.

**Verify**: `cargo test -p feanorfs-agent-core --locked` → all pass
(especially `hub_tests` and any materialization tests).

### Step 3: Add regression tests in common

Model after the existing crypto tests at `common/src/lib.rs:1100-1140`. Add:

1. `verified_rejects_garbage_from_failed_aead_fallback` — pack a blob with
   key A, attempt `unpack_bytes_verified` with key B and the ORIGINAL
   plaintext hash → must be `Err`, never `Ok(garbage)`.
2. `verified_still_rescues_legacy_prefix_collision` — craft a legacy XOR blob
   whose plaintext's first byte equals `AEAD_PREFIX_BYTE`; decrypt with the
   correct key and matching hash → must succeed (the rescue survives).
3. `verified_errors_on_hash_mismatch` — correct key, deliberately wrong
   expected hash → must be `Err` with the recorded message text.

**Verify**: `cargo test -p feanorfs-common --locked unpack_bytes` → 3 new
tests pass alongside existing ones.

## Test plan

- New tests listed in Step 3, in `common/src/lib.rs`'s existing test module.
- Structural pattern: existing `pack_bytes`/`unpack_bytes` roundtrip tests in
  the same module.
- Full gates: `cargo test --workspace --all-features --locked` → pass.

## Done criteria

- [ ] `unpack_bytes_verified` exists in `common/src/lib.rs` and is used at the
      file-content decrypt site under `agent-core/src/sync_pass/`
- [ ] `rg -n 'unpack_bytes_verified' common/src agent-core/src` shows the
      definition plus at least one production call site and three tests
- [ ] `cargo test --workspace --all-features --locked` exits 0
- [ ] `cargo clippy --workspace --all-targets --locked -- -D warnings` exits 0
- [ ] No files outside the in-scope list modified (`git status`)
- [ ] `plans/README.md` status row updated

## STOP conditions

Stop and report back if:

- The code at `common/src/lib.rs:592-626` no longer matches the excerpt
  above (drift).
- You cannot find exactly one obvious file-content decrypt site under
  `agent-core/src/sync_pass/materialize/` — do not guess among several.
- Wiring the hash requires changing a public signature outside
  `sync_pass/` (e.g. the `Runtime` facade or FFI surface).
- Any existing test asserts the OLD silent-fallback behavior for
  prefix-collision blobs without an expected hash.

## Maintenance notes

- If format-v1 support is ever dropped entirely, delete the
  `AllowXorFallback` branch and this wrapper collapses into a plain check.
- Reviewers should scrutinize: the hash comparison uses the same
  `hash_bytes` as upload-side sealing; the error message names both possible
  causes without leaking key material.
- Deferred: adding expected-hash plumbing to every remaining
  `AllowXorFallback` caller (large-file roots) — they operate on manifests
  whose own parse validation already fails loudly on garbage.
