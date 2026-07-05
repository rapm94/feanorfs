# Security Policy

## Supported versions

FeanorFS is pre-1.0 software (`0.1.x`). Security fixes are applied only to the latest `main` branch. There are no LTS releases yet.

## Reporting a vulnerability

**Do not open a public GitHub issue for security vulnerabilities.**

To report a security issue, email **raulapuigbo@gmail.com** with:

1. A description of the vulnerability and its impact.
2. Steps to reproduce (proof of concept, if possible).
3. Affected components (`common`, `server`, `client`).
4. Any suggested mitigations.

You will receive an acknowledgement within 72 hours. If the vulnerability is confirmed, a fix and advisory will be published in [CHANGELOG.md](CHANGELOG.md) and a new release will be tagged.

Please do not disclose the vulnerability publicly until a fix has been released.

## Threat model

Full analysis: [docs/threat-model.md](docs/threat-model.md). Open backlog: [docs/roadmap.md](docs/roadmap.md) (SEC-6, etc.).

### What FeanorFS protects

- **File contents at rest on the server** — New blobs are sealed with ChaCha20-Poly1305 AEAD (`pack_bytes`) before upload. Keys are derived per path from the workspace encryption key. The server stores only ciphertext and encrypted Blake3 hashes. Without the key, the server cannot recover plaintext.
- **Ciphertext integrity (AEAD blobs)** — Tampered ciphertext fails authentication on decrypt. The client also re-hashes downloaded ciphertext against the expected `encrypted_hash` before decrypting.
- **Optional server auth** — Run `feanorfs serve --token <TOKEN>` to require Bearer auth on all API routes.

### What FeanorFS does NOT protect

- **Metadata leakage** — The server sees file paths, sizes, modification times, and encrypted hashes. Paths are not encrypted.
- **Legacy XOR blobs (v1 workspaces)** — Unmigrated workspaces still decrypt pre-AEAD blobs via an unauthenticated XOR stream. Run `feanorfs migrate` to format v2, which rejects non-AEAD blobs. Do not sync unmigrated workspaces against untrusted servers.
- **No TLS by default** — HTTP on port 3030. Use a reverse proxy or VPN for internet deployments.
- **Password storage** — Encryption keys and server tokens are stored in plaintext in `.feanorfs/config.json` and `~/.feanorfs/global.json`. Protect directory permissions.
- **Brute-force resistance** — Key derivation is a single Blake3 pass with no KDF stretching. v2 workspaces require 64-hex generated keys (256-bit CSPRNG). Human passphrases remain weak if used manually.
- **Replay of old blob versions** — Content-addressed storage allows the server to serve an older valid blob for a path.

### Security goals for future versions

1. **Remove legacy XOR decrypt** (SEC-6) — after all workspaces migrate to format v2.
2. **Native TLS** on the Axum server (or document reverse-proxy as the only supported internet path).
3. **Path obfuscation** — encrypt paths in server metadata.
4. **OS keychain** for stored keys/tokens.

## Cryptographic primitives

| Component | Primitive | Usage |
|---|---|---|
| Hashing | Blake3 | CAS blob keys, plaintext/encrypted file identification |
| Encryption (new blobs) | ChaCha20-Poly1305 AEAD | `pack_bytes` / `unpack_bytes`; deterministic SIV-style nonce for CAS stability |
| Encryption (legacy, decrypt-only) | Blake3 XOF XOR stream | Pre-AEAD blobs until `feanorfs migrate` |
| Key derivation | Blake3 with length-prefix domain separation | `blake3(domain ‖ len ‖ key ‖ len ‖ path)` — no salt, no KDF stretching |

## Responsible disclosure

We follow responsible disclosure. Credit will be given to reporters in the release advisory unless they prefer to remain anonymous.
