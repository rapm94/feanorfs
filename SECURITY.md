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

### What FeanorFS protects

- **File contents at rest on the server** — All file bytes are encrypted on the client before upload using a symmetric XOR keystream derived from Blake3's Extendable Output Function (XOF), keyed by `(password, relative_path)`. The server stores only ciphertext blobs and encrypted Blake3 hashes. Without the password, the server cannot recover plaintext.

### What FeanorFS does NOT protect

- **Metadata leakage** — The server sees: file paths, file sizes, modification times, and encrypted hashes. An adversary with server access can observe the structure of your workspace, the number of files, their sizes, and when they change. **Paths are NOT encrypted.**
- **Ciphertext integrity** — The encryption scheme is an unauthenticated XOR stream cipher (Blake3 XOF). It does not use AEAD (AES-GCM, ChaCha20-Poly1305). An attacker who can modify blobs on the server could tamper with ciphertext, and the client has no built-in mechanism to detect tampering beyond hash verification of the encrypted bytes. **Do not use FeanorFS against a malicious or compromised server.**
- **Server API access** — The server has no authentication or authorization. Anyone with network access to port 3030 can list, upload, or download blobs and query sync state. **Run the server on a trusted network, behind a VPN, or bind to localhost.**
- **Password storage** — The encryption password is stored in plaintext in `.feanorfs/config.json`. Anyone with read access to your workspace directory can recover it. **Protect the workspace directory permissions.**
- **Side-channel attacks** — The server can observe access patterns (which blobs are downloaded, when, and by whom). There is no oblivious RAM or access pattern obfuscation.
- **Brute-force resistance** — The encryption key is derived directly from `blake3(password || path)` with no KDF stretching (no Argon2, no PBKDF2, no salt). A low-entropy password may be brute-forced offline by an attacker who obtains a ciphertext blob and knows the corresponding path. **Use a high-entropy password.**

### Security goals for future versions

The following are known gaps tracked for future work. Contributions are welcome:

1. **AEAD encryption** — Replace the raw XOR stream with ChaCha20-Poly1305 or AES-GCM to provide ciphertext integrity and authentication.
2. **KDF for password stretching** — Replace direct `blake3(password || path)` key derivation with Argon2id + salt to resist offline brute-force.
3. **Server authentication** — Add token-based or mTLS authentication to the server API.
4. **Path obfuscation** — Encrypt file paths in the metadata database so the server cannot observe workspace structure.

## Cryptographic primitives

| Component | Primitive | Usage |
|---|---|---|
| Hashing | Blake3 | Content-addressed storage keys, plaintext/encrypted file identification |
| Encryption | Blake3 XOF (XOR stream) | Symmetric encryption of file contents; key = `blake3(password ‖ path)` extended to file length |
| Key derivation | Direct Blake3 hash | No KDF, no salt, no stretching (see limitations above) |

Blake3 is a cryptographic hash function; its XOF mode produces a keystream. However, using it as a raw XOR stream cipher without authentication is not a standard encryption construction. The current design prioritizes simplicity and zero external crypto dependencies over defense against active attackers.

## Responsible disclosure

We follow responsible disclosure. Credit will be given to reporters in the release advisory unless they prefer to remain anonymous.
