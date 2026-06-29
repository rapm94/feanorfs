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
- **Ciphertext tampering**: The encryption scheme is an unauthenticated XOR stream (Blake3 XOF). There is no MAC/AEAD. On download, the client re-hashes the ciphertext and compares it to the expected `encrypted_hash` from sync metadata before decrypting — this catches blob substitution when metadata is honest, but a malicious server can still lie in `SyncResponse` (supply a hash that matches substituted ciphertext). **Do not use FeanorFS against an untrusted server.**
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

### Encryption construction

```
keystream = Blake3_XOF(blake3(password_bytes ‖ path_bytes), length = len(data))
ciphertext = plaintext XOR keystream
```

**Properties:**
- **Symmetric**: `decrypt = encrypt` (XOR is self-inverse).
- **Deterministic**: Same `(password, path, plaintext)` always produces the same ciphertext. This is required for content-addressed storage (the encrypted hash must be stable across clients).
- **Path-dependent**: The keystream is different for each file path, so identical plaintext files at different paths produce different ciphertexts (and different encrypted hashes).

**Weaknesses:**
1. **No authentication**: There is no MAC. Ciphertext can be modified without detection. An attacker who knows the plaintext at a given position can replace it with arbitrary content by XORing the difference.
2. **No nonce**: The keystream for a given `(password, path)` is always the same. This is acceptable for content-addressed storage (identical content must produce identical ciphertext), but it means the scheme is vulnerable to known-plaintext attacks if the same key is reused for different data at the same path (which does not happen in the current design).
3. **Fast key derivation**: `blake3(password ‖ path)` is computed in nanoseconds. No brute-force resistance beyond the password's own entropy.

### Hash usage

- `plaintext_hash = blake3(plaintext_bytes)` — stored only in the local cache. Never sent to the server.
- `encrypted_hash = blake3(ciphertext_bytes)` — the CAS blob key. Sent to the server. Used to verify upload integrity (server-side) and download integrity (client re-hashes ciphertext before decrypt).
- The server verifies `blake3(uploaded_bytes) == claimed_hash` on upload, rejecting mismatches with HTTP 400.

## Risk summary

| Risk | Severity | Mitigation | Status |
|---|---|---|---|
| Plaintext leakage via weak password | High | Use high-entropy password (≥ 20 chars) | User responsibility |
| Ciphertext tampering (no AEAD) | High | Use trusted server only; future: migrate to ChaCha20-Poly1305 | Known limitation |
| No TLS (network MITM) | High | Run behind reverse proxy or VPN; future: native TLS support | Known limitation |
| No server authentication | Medium | Run with `--token`; clients send `Authorization: Bearer` | Implemented (optional) |
| Password stored in plaintext | Medium | `chmod 700 .feanorfs/`; future: OS keychain integration | Known limitation |
| Metadata leakage (paths, sizes) | Medium | Future: path encryption, size padding | Known limitation |
| No KDF / brute-force resistance | Medium | Use high-entropy password; future: Argon2id + salt | Known limitation |
| Replay attacks (old blob versions) | Low | Future: version counters or monotonic sequence numbers | Known limitation |

## Planned security improvements

1. **AEAD encryption** — Replace raw XOR stream with ChaCha20-Poly1305. The deterministic key derivation can be preserved by deriving the AEAD key from `blake3(password ‖ path)` and using a fixed nonce (acceptable since each `(key, nonce)` pair is only ever used for one plaintext).
2. **Password stretching** — Replace direct `blake3(password ‖ path)` with `Argon2id(password, salt=path)` to add brute-force resistance. The salt being the path preserves per-path key uniqueness.
3. **Native TLS** — Add optional TLS to the Axum server (`axum-server` + `rustls`) and HTTPS support to the client's `reqwest::Client`.
4. **Path obfuscation** — Encrypt file paths in the metadata using the same password-derived key, so the server only sees opaque path identifiers.
