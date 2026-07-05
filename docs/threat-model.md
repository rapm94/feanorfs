# Threat Model

This document provides a detailed security analysis of FeanorFS. For the policy on reporting vulnerabilities, see [../SECURITY.md](../SECURITY.md).

## System model

```
┌─────────────────────────────────────────────────────────────┐
│  Trusted zone (client machine)                              │
│                                                             │
│  ┌───────────────┐   ┌─────────────────┐                    │
│  │  feanorfs CLI │   │ .feanorfs/      │                    │
│  │  (encrypt/    │   │  config.json    │ ← plaintext pass   │
│  │   decrypt)    │   │  local_cache.db │ ← plaintext hashes │
│  └───────┬───────┘   └─────────────────┘                    │
│          │                                                  │
└──────────┼──────────────────────────────────────────────────┘
           │ encrypted blobs + metadata (HTTPS not enforced)
           │
┌──────────┼──────────────────────────────────────────────────┐
│          ▼          Untrusted zone (server)                  │
│  ┌───────────────┐   ┌─────────────────┐                    │
│  │  Axum server  │   │ server-data/    │                    │
│  │  (opt. token) │   │  db.sqlite      │ ← paths, sizes,    │
│  │               │   │  blobs/<hash>   │   encrypted hashes │
│  └───────────────┘   └─────────────────┘                    │
└─────────────────────────────────────────────────────────────┘
```

## Assets

1. **File contents (plaintext)** — the data being synced. Confidentiality is the primary security goal.
2. **Encryption password** — the secret that protects file contents.
3. **File metadata** — paths, sizes, modification times, hashes. Partial confidentiality expected (paths are not encrypted).
4. **Local cache integrity** — `local_cache.db` and `config.json` on the client. Tampering could cause desync or data loss.

## Adversary model

### Adversary A: Passive server operator

**Can observe:** All encrypted blobs, all metadata (paths, sizes, mtimes, encrypted hashes), all API requests.

**Goal:** Recover plaintext file contents.

**Result:** **Defended against** (with caveats). Without the password, the server cannot decrypt blobs. The XOR keystream is derived from `blake3(password ‖ path)`, which the server does not know. However:
- A weak password can be brute-forced offline if the attacker knows any `(path, plaintext)` pair (they can verify a password guess by checking if `crypt_bytes(plaintext, guess, path) == stored_ciphertext`). **Use a high-entropy password.**
- The server sees file paths in cleartext. If the path itself reveals sensitive information (e.g., `credentials/bank-passwords.txt`), that information is exposed.

### Adversary B: Active server operator (malicious or compromised)

**Can observe and modify:** All blobs, all metadata, all API responses.

**Goal:** Alter decrypted file contents on the client, inject malicious files, or recover plaintext.

**Result:** **Partially defended against.**
- **Ciphertext tampering (new blobs)**: New blobs are sealed with ChaCha20-Poly1305 AEAD (`pack_bytes`). Tampered ciphertext fails authentication on decrypt. Additionally, the client re-hashes downloaded ciphertext against the expected `encrypted_hash` before decrypting.
- **Legacy downgrade (v1 workspaces only)**: Format v2 workspaces reject non-AEAD blobs. Unmigrated v1 workspaces still fall back to the legacy XOR stream on decrypt — run `feanorfs migrate`. Removing the XOR path entirely is [SEC-6](roadmap.md). Do not sync unmigrated workspaces against untrusted servers.
- **Metadata lies**: a malicious server can still lie in `SyncResponse` (supply a hash matching substituted ciphertext); AEAD limits this to replaying validly-encrypted blobs for that same path.
- **Replay attacks**: The server can replay an older version of a blob (since blobs are content-addressed, old versions are still valid by hash). The client has no version counter or nonce to detect this.
- **Metadata manipulation**: The server can lie about `SyncResponse` — claiming no downloads are needed, or injecting fake file states. The client trusts the server's metadata.

### Adversary C: Network attacker (MITM)

**Can observe and modify:** All HTTP traffic between client and server.

**Goal:** Recover plaintext, inject malicious updates, or disrupt sync.

**Result:** **NOT defended against.** The server listens on plain HTTP (port 3030). There is no TLS. A network attacker can:
- Read all metadata and encrypted blobs (same exposure as Adversary A).
- Modify blobs in transit (same exposure as Adversary B).
- Inject fake API responses.
- **Mitigation**: Run the server behind a TLS-terminating reverse proxy (nginx, Caddy) or on localhost only. A VPN also mitigates.

### Adversary D: Local attacker on client machine

**Can observe:** All files in the workspace directory, including `.feanorfs/config.json` (plaintext password) and `.feanorfs/local_cache.db` (plaintext hashes).

**Goal:** Recover the encryption password, then decrypt all server-side blobs.

**Result:** **NOT defended against.** The password is stored in plaintext in `config.json`. Anyone with read access to the workspace can recover it. **Protect workspace directory permissions (`chmod 700 .feanorfs/`).**

### Adversary E: Offline brute-force attacker

**Can observe:** A ciphertext blob and the corresponding file path (e.g., from a server backup leak).

**Goal:** Recover the password and/or plaintext.

**Result:** **Partially defended against.** The key derivation is `blake3(password ‖ path)` with no salt, no KDF, no stretching. Blake3 is fast (~1 GB/s), making brute-force attempts very cheap. A password with &lt; 80 bits of entropy is vulnerable. **Use a high-entropy password (≥ 20 random characters, or a Diceware passphrase with ≥ 6 words).**

## Cryptographic analysis

### Current encryption construction (AEAD, new blobs)

```
key       = blake3("feanorfs-aead-v1" ‖ len(password) ‖ password ‖ len(path) ‖ path)
nonce     = blake3("feanorfs-aead-nonce-v1" ‖ key ‖ len(data) ‖ data)[..12]
blob      = 0x01 ‖ nonce ‖ ChaCha20-Poly1305(key, nonce, plaintext)
```

**Properties:**
- **Authenticated**: tampered ciphertext fails the Poly1305 tag check on decrypt.
- **Deterministic (SIV-style)**: the nonce is derived from the key and plaintext, so the same `(password, path, plaintext)` always produces the same blob. This is required for content-addressed storage (the encrypted hash must be stable across clients) and for cheap change detection. Since each `(key, nonce)` pair only ever seals one plaintext, nonce reuse is not a concern.
- **Path-dependent** with length-prefix domain separation: `(password="ab", path="cdef")` and `(password="abc", path="def")` derive different keys.

**Known properties / weaknesses:**
1. **Determinism leaks reverts**: identical plaintext at the same path always yields identical ciphertext, so the server can observe "this file returned to a previous state." Accepted trade-off — see roadmap non-fix note.
2. **Legacy XOR fallback (v1 only)**: unmigrated workspaces still decrypt pre-AEAD blobs via XOR. Run `feanorfs migrate`; removal of XOR decrypt is [SEC-6](roadmap.md).
3. **Fast key derivation**: single Blake3 pass — v2 workspaces require 64-hex generated keys (256-bit CSPRNG).

### Legacy construction (pre-AEAD blobs, decrypt-only)

```
keystream  = Blake3_XOF(len(password) ‖ password ‖ len(path) ‖ path)
ciphertext = plaintext XOR keystream
```

Unauthenticated and malleable — an attacker who knows plaintext at a position can substitute arbitrary content by XORing the difference. Retained only so pre-AEAD blobs remain readable until `feanorfs migrate` re-seals them.

### Hash usage

- `plaintext_hash = blake3(plaintext_bytes)` — stored only in the local cache. Never sent to the server.
- `encrypted_hash = blake3(ciphertext_bytes)` — the CAS blob key. Sent to the server. Used to verify upload integrity (server-side) and download integrity (client re-hashes ciphertext before decrypt).
- The server verifies `blake3(uploaded_bytes) == claimed_hash` on upload, rejecting mismatches with HTTP 400.

## Risk summary

| Risk | Severity | Mitigation | Status |
|---|---|---|---|
| Plaintext leakage via weak password | High | Use the auto-generated 64-hex key; planned: reject human passphrases (SEC-7) | User responsibility |
| Legacy XOR decrypt path (unmigrated v1 workspaces) | Medium | `feanorfs migrate` to format v2; then remove XOR path ([SEC-6](roadmap.md)) | Open |
| Ciphertext tampering (AEAD blobs) | — | ChaCha20-Poly1305 authentication | Implemented |
| No TLS (network MITM) | High | Run behind reverse proxy or VPN; future: native TLS support | Known limitation |
| No server authentication | Medium | Run with `--token`; clients send `Authorization: Bearer` | Implemented (optional) |
| Password stored in plaintext | Medium | `chmod 700 .feanorfs/`; future: OS keychain integration | Known limitation |
| Metadata leakage (paths, sizes) | Medium | Future: path encryption, size padding | Known limitation |
| No KDF / brute-force resistance | Medium | Use high-entropy password; future: Argon2id + salt | Known limitation |
| Replay attacks (old blob versions) | Low | Future: version counters or monotonic sequence numbers | Known limitation |

## Planned security improvements

See [roadmap.md](roadmap.md):

1. **Remove legacy XOR decrypt** (SEC-6) — after all workspaces run `feanorfs migrate`.
2. **Native TLS** — optional TLS on the Axum server; until then, reverse proxy.
3. **Path obfuscation** — encrypt file paths in metadata.

## Process isolation (agents)

FeanorFS's agent workspaces provide **data isolation, not process sandboxing.**

**What IS isolated (data):** each agent works in its own copy under `.feanorfs/agents/<name>/`; nothing reaches the main folder or the server without going through `agent land`'s three-way diff. Conflicted paths are blocked from sync with all versions preserved (on disk and as immutable content-addressed blobs). An honest agent cannot make a mess.

**What is NOT isolated (processes):** `agent run` sets the child's working directory — nothing else. The child inherits the full filesystem (absolute-path writes escape the agent folder), network access, and environment variables. An agent that pushes to another repo, exfiltrates secrets, or runs a malicious `build.rs` is not stopped by FeanorFS.

**Composition rule:** run untrusted or highly autonomous agents inside a sandboxed harness (Claude Code / Cursor sandboxes, containers, `sandbox-exec`, firejail) pointed at the agent folder. FeanorFS contains honest mistakes; the sandbox contains everything else.

**Reconciliation trust:** conflict resolution is performed by a consumer (human or LLM), never by FeanorFS. Resolution requires explicit `conflicts keep`; all versions remain recoverable. LLM merges should be verified in a spawned agent workspace before `conflicts keep --file`. Open agent edge-case tests: [roadmap.md](roadmap.md).
