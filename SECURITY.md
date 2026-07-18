# Security Policy

## Supported versions

FeanorFS is pre-1.0 software. Security fixes are applied only to the latest
release and `main`; there are no LTS branches.

## Reporting a vulnerability

**Do not open a public GitHub issue for security vulnerabilities.**

To report a security issue, email **raulapuigbo@gmail.com** with:

1. A description of the vulnerability and its impact.
2. Steps to reproduce (proof of concept, if possible).
3. Affected components (`common`, `server`, `client`).
4. Any suggested mitigations.

You will receive an acknowledgement within 72 hours. If the vulnerability is confirmed, a fix and advisory will be published in [CHANGELOG.md](CHANGELOG.md) and a new release will be tagged.

Please do not disclose the vulnerability publicly until a fix has been released.

## Verifying release artifacts

FeanorFS release binaries are built by [cargo-dist](https://github.com/axodotdev/cargo-dist) in GitHub Actions (`.github/workflows/release.yml`). The universal macOS package and DMG are built by `.github/workflows/tray-release.yml`, while native Linux and signed Windows installer products are built by `.github/workflows/desktop-release.yml`; both run only after the cargo-dist release succeeds and the requested tag resolves to the same commit.

The macOS workflow builds arm64 and x86_64 without secrets, combines them into
one universal CLI and `FeanorFS.app`, signs both with Developer ID Application
using hardened runtime and trusted timestamps, and signs the package with
Developer ID Installer. It notarizes and staples `FeanorFS-macOS.pkg`, wraps
that exact package in `FeanorFS-macOS.dmg`, then notarizes and staples the disk
image. Gatekeeper must accept both layers and the mounted package must match the
published package byte-for-byte. Missing credentials or any rejected assessment
stops the release before upload; there is no unsigned fallback.

Before packaging, the privileged workflow runs the Developer ID-signed CLI in
an isolated workspace without credential-store overrides. It requires the
workspace config to contain only a random `fsc1-…` reference, verifies the
corresponding `com.feanorfs.credentials` item exists and is readable through
macOS Keychain, deletes that test credential, and binds the public smoke record
to the packaged CLI SHA-256. An unsigned or ad-hoc-signed binary must fail this
gate.

The primary Unix installer inspects the latest release assets. On macOS it uses
the universal package only when both package and checksum are present, then
requires checksum, Developer ID Installer, and Gatekeeper verification before
installation. If an older release has no package it installs only the
cargo-dist CLI and reports that limitation; a listed but incomplete or invalid
package fails closed.

On Linux x86-64 and ARM64, the same installer prefers a release `.deb` on
Debian/Ubuntu, `.rpm` on Fedora/RHEL, or `.pkg.tar.zst` on Arch/Manjaro. It
verifies the checksum, package name, native architecture, and absence of install
scripts before invoking the system package manager. Package metadata declares
the required desktop libraries, and a listed native package with a missing or
invalid checksum fails closed. The exact-content tar product remains available
for custom prefixes and is rejected when runtime linkage is missing.

On Windows, the normal Inno Setup EXE and both embedded executables must carry
valid Authenticode signatures. CI proves exact installed hashes, per-user PATH
integration, and uninstall before publication. The PowerShell fallback accepts
only the expected checksummed two-executable bundle and verifies both
signatures. Azure Artifact Signing is mandatory; there is no unsigned desktop
fallback.

**Current release status:** v0.5.0 contains the attested CLI but no trusted
desktop artifacts. The first consumer macOS release must include the universal
DMG/package, accepted notarization JSON, signed-Keychain smoke record, and
verification evidence described above. The first Windows desktop release must
likewise include the Authenticode-signed setup EXE and verification evidence.

Release artifacts, evidence, and installer scripts also receive a **GitHub
Artifact Attestation** (SLSA build provenance via Sigstore). An attestation is
provenance, not a code signature: it proves the file was produced by an
official workflow from a specific commit. A tampered artifact fails
verification even if its download URL looks correct.

Trusted tags additionally build the amd64/arm64 opaque relay OCI image from
`Dockerfile.relay`, publish an SBOM and BuildKit provenance, and attach a GitHub
attestation to its immutable digest. The image runs as UID/GID 10001, supports a
read-only root filesystem, and generates authentication only inside its
persistent data volume. Its HTTP port must stay behind a public-TLS reverse
proxy; see [docs/deploy-relay.md](docs/deploy-relay.md).

Attestations are not retroactive. Older releases that predate this pipeline must
be built from source for equivalent provenance.

### Verify with GitHub CLI (recommended)

Install [GitHub CLI](https://cli.github.com/) (`gh`), download the artifact you intend to run, then:

```bash
gh attestation verify feanorfs-client-x86_64-apple-darwin.tar.xz \
  --repo rapm94/feanorfs
```

For a relay image:

```bash
gh attestation verify \
  oci://ghcr.io/rapm94/feanorfs-relay:<version> \
  --repo rapm94/feanorfs
```

Use the filename you downloaded (`*.tar.xz`, `*.zip`, `*.dmg`, `*.pkg`,
`*.deb`, `*.rpm`, `*.pkg.tar.zst`, `*.exe`,
`feanorfs-client-installer.sh`, or `feanorfs-client-installer.ps1`). This
includes `FeanorFS-macOS.pkg` and its notarization/verification evidence.
Success prints the linked workflow run and commit; failure means do not run the
binary.

List attestations for a release tag:

```bash
gh attestation download --repo rapm94/feanorfs <tag>
```

### Verify without piping the install script

If you prefer not to `curl | sh`, download the artifact for your platform from [GitHub Releases](https://github.com/rapm94/feanorfs/releases) and verify the attestation above. On macOS, open `FeanorFS-macOS.dmg` and its package. On Linux, install the matching `.deb`, `.rpm`, or `.pkg.tar.zst`. On Windows, run the signed setup EXE. The install scripts are convenience wrappers around the same attested artifacts.

### Verify with checksums

Each release also ships `*.sha256` checksum files. After download:

```bash
shasum -a 256 -c feanorfs-client-x86_64-apple-darwin.tar.xz.sha256
```

Checksums detect accidental corruption; attestations additionally bind the file to the CI build that produced it.

### Verify native Linux packages

For a Debian-family x86-64 package (use the matching architecture filename):

```bash
sha256sum -c FeanorFS-linux-x86_64.deb.sha256
gh attestation verify FeanorFS-linux-x86_64.deb --repo rapm94/feanorfs
dpkg-deb -f FeanorFS-linux-x86_64.deb Package Architecture Depends
```

`Package` must be `feanorfs`, the architecture must match the machine, and the
dependencies must include GTK 3, Ayatana AppIndicator 3, libxdo, and the XDG
desktop portal. For an RPM-family system:

```bash
sha256sum -c FeanorFS-linux-x86_64.rpm.sha256
gh attestation verify FeanorFS-linux-x86_64.rpm --repo rapm94/feanorfs
rpm -qp --queryformat '%{NAME} %{ARCH}\n' FeanorFS-linux-x86_64.rpm
rpm -qp --scripts FeanorFS-linux-x86_64.rpm
```

The identity must be `feanorfs` with the matching native architecture, and the
script query must be empty.

For Arch/Manjaro:

```bash
sha256sum -c FeanorFS-linux-x86_64.pkg.tar.zst.sha256
gh attestation verify FeanorFS-linux-x86_64.pkg.tar.zst --repo rapm94/feanorfs
bsdtar -xOf FeanorFS-linux-x86_64.pkg.tar.zst .PKGINFO
```

`pkgname` must be `feanorfs`, `arch` must match the machine, and the
dependencies must name GTK 3, Ayatana AppIndicator, xdotool, the XDG desktop
portal, and zenity.

### Verify the Windows installer

In PowerShell:

```powershell
(Get-FileHash -Algorithm SHA256 FeanorFS-windows-x86_64-setup.exe).Hash
Get-AuthenticodeSignature FeanorFS-windows-x86_64-setup.exe | Format-List Status,StatusMessage
gh attestation verify FeanorFS-windows-x86_64-setup.exe --repo rapm94/feanorfs
```

Compare the hash with `FeanorFS-windows-x86_64-setup.exe.sha256`. Signature
status must be `Valid`; an absent, unknown, or untrusted signer is a hard
failure.

### Verify Apple signatures and notarization on macOS

Before installation, verify the universal disk image and package:

```bash
shasum -a 256 -c FeanorFS-macOS.dmg.sha256
gh attestation verify FeanorFS-macOS.dmg --repo rapm94/feanorfs
xcrun stapler validate -v FeanorFS-macOS.dmg
spctl --assess --type open --context context:primary-signature --verbose=4 FeanorFS-macOS.dmg
shasum -a 256 -c FeanorFS-macOS.pkg.sha256
gh attestation verify FeanorFS-macOS.pkg --repo rapm94/feanorfs
pkgutil --check-signature FeanorFS-macOS.pkg
spctl --assess --type install --verbose=4 FeanorFS-macOS.pkg
xcrun stapler validate -v FeanorFS-macOS.pkg  # when Xcode tools are installed
```

`pkgutil` must show a `Developer ID Installer` authority. `spctl` must report
`source=Notarized Developer ID`, and `stapler` must validate the embedded
ticket. After installation, `codesign --display --verbose=4 /usr/local/bin/feanorfs`
and the equivalent command for
`/Applications/FeanorFS.app` must show a `Developer ID Application` authority,
a TeamIdentifier, hardened runtime flags, and a trusted timestamp. Compare this
with `FeanorFS-macOS.verification.txt`; the corresponding notarization JSON
must have status `Accepted`.

## CI and supply-chain controls

- CI tests the core workspace on Linux, macOS, and Windows. The cross-platform
  tray has native build, Clippy, test, and product/payload gates on all three;
  Linux also builds verified `.deb`, `.rpm`, and `.pkg.tar.zst` packages.
- Rust 1.88 is the declared and continuously tested minimum supported version.
- `cargo-deny` blocks known advisories, yanked crates, unapproved licenses, and
  untrusted dependency sources. A scheduled run catches newly published
  advisories even when the repository is idle.
- Main-branch CI builds the hardened relay image, exercises its actual runtime,
  and uses Trivy to block fixed high/critical vulnerabilities in the complete
  Linux image before a trusted tag can publish it.
- CodeQL scans Rust and GitHub Actions. `actionlint` validates workflow syntax
  and embedded shell, while `zizmor` audits repository-owned workflows.
- Repository-owned actions are pinned to immutable commit SHAs. Cargo-dist's
  generated `release.yml` remains generator-owned and is never patched by hand.
- Dependabot covers Cargo, npm, Docker base images, and GitHub Actions. Version updates use a
  cooldown; security updates are not intentionally delayed.
- `release-plz` runs only after CI succeeds on a trusted `main` push. Cargo-dist
  then builds and attests the tag, and both desktop workflows verify that the
  tag resolves to the exact release commit before uploading their artifacts.
- Apple signing and notarization secrets are available only to their dedicated
  steps. Decoded keys live under the ephemeral runner directory, private keys
  are imported into a temporary keychain, and an unconditional cleanup step
  deletes both files and keychain.

### Build from source

For maximum assurance, clone the tag and build locally:

```bash
git clone https://github.com/rapm94/feanorfs.git
cd feanorfs
git checkout <tag>
cargo build --release --bin feanorfs
```

No binary from GitHub Releases is involved.

## Threat model

Full analysis: [docs/threat-model.md](docs/threat-model.md). Open security work is owned and gated in [TODO.md](TODO.md).

### What FeanorFS protects

- **File contents and names at rest on a format-v3 hub** — File blobs, trees,
  and snapshot objects are sealed with ChaCha20-Poly1305 AEAD before upload.
  The hub stores opaque ciphertext objects, heads, and reachability manifests;
  it cannot recover plaintext or format-v3 filenames without the workspace key.
- **Ciphertext integrity (AEAD blobs)** — Tampered ciphertext fails authentication on decrypt. The client also re-hashes downloaded ciphertext against the expected `encrypted_hash` before decrypting.
- **Authenticated transport** — `feanorfs serve` enables Rustls HTTPS and a
  generated persistent bearer token by default. Private hubs deliver only their
  public CA through authenticated `fnh1`/`fnr1` capabilities; clients retain
  normal certificate and hostname verification.
- **LAN invite delivery** — Single-use pairing uses SPAKE2, then
  ChaCha20-Poly1305 with explicit key confirmation. mDNS carries only public
  rendezvous data, never the pairing secret, server token, or E2EE key.
- **Off-LAN invite delivery** — `fnp2` retains the same SPAKE2/AEAD/key-confirmation
  exchange over an outbound-only WSS rendezvous. The relay receives a random
  128-bit public session ID and bounded opaque frames, never the 80-bit secret,
  invite, hub token, workspace ID, or E2EE key. It is disabled by default.
- **Opaque private-hub relay** — `start --relay` gives an owned private hub a
  random 256-bit route stored in atomic private hub-local state and outbound WSS workers. Remote clients relay the
  existing Rustls byte stream while retaining the CA-bound hostname for SNI.
  The relay sees route and traffic metadata but not the bearer token, workspace
  ID, API paths, object names, or tunneled bytes. Inner TLS and bearer auth fail
  closed under substitution; queues, concurrency, frames, bytes, and duration
  are bounded.
- **Private-hub identity recovery** — `feanorfs serve recovery export` seals the
  hub CA and bearer token with Argon2id and XChaCha20-Poly1305. Offline import
  validates the CA/key pair and uses a durable fence so partial restores cannot
  start the hub.
- **Private-hub identity rotation** — `feanorfs serve recovery rotate` requires
  the hub to be stopped, writes an encrypted backup outside the data directory,
  replaces both CA and token behind the same durable fence, and preserves opaque
  storage. Every client must authenticate the replacement `fnh1` capability;
  old trust and credentials fail closed.
- **Workspace capability recovery** — `feanorfs recovery export` seals the full
  portable workspace capability with Argon2id and XChaCha20-Poly1305 in an
  atomic private file. Import authenticates and validates the capability before
  creating workspace/global state, then delegates to the ordinary `start`
  lifecycle. CLI prompts and the tray's bounded stdin pipe keep passphrases and
  decrypted capabilities out of argv, environment variables, and logs. The kit
  does not contain file blobs and cannot recover an unavailable or erased hub.

### What FeanorFS does NOT protect

- **Traffic metadata** — A format-v3 hub still observes ciphertext sizes,
  object counts, hash equality, retention, and request timing. Legacy formats
  expose flat path metadata until migration.
- **Legacy XOR blobs (v1 workspaces)** — Unmigrated workspaces still decrypt pre-AEAD blobs via an unauthenticated XOR stream. Run `feanorfs migrate` to format v3, which rejects non-AEAD blobs and encrypts workspace structure. Do not sync unmigrated workspaces against untrusted servers.
- **Historical weak workspace keys** — New format-v2/v3 create and link paths accept only canonical 256-bit recovery keys and reject human passphrases before writing configuration. A legacy format-v1 workspace may still contain a human-chosen key so it can be read for migration; use `feanorfs migrate --rekey` to replace it while resealing the workspace.
- **Local account compromise** — Unattended sync resolves the workspace key and
  server token from macOS Keychain for signed releases, Windows Credential
  Manager, or Linux Secret Service. Config JSON contains only a random
  reference. Unsigned macOS/source builds and unavailable stores fall back to
  atomic Unix `0700`/`0600` files.
  Malware running as the logged-in user may still use that user's credential
  APIs, read the working tree, or capture decrypted data.
- **No process sandbox** — Agent workspaces isolate files, not processes.
  Commands run by an agent retain the logged-in user's operating-system access.
- **Rollback availability** — Immutable history preserves recovery paths, but a
  malicious or corrupted hub can withhold objects, deny service, or attempt to
  present an older reachable head. Clients warn on observed regressions; this is
  not an external transparency log.
- **Pairing assurance** — SPAKE2 usage has focused protocol tests but has not
  received an independent cryptographic audit.

### Open security work

Ownership, dependencies, and release evidence for legacy-crypto retirement,
hosted identity/default relay work, and independent review are tracked only in
[TODO.md](TODO.md).

## Cryptographic primitives

| Component | Primitive | Usage |
|---|---|---|
| Hashing | Blake3 | CAS blob keys, plaintext/encrypted file identification |
| Encryption (new blobs) | ChaCha20-Poly1305 AEAD | `pack_bytes` / `unpack_bytes`; deterministic SIV-style nonce for CAS stability |
| Encryption (legacy, decrypt-only) | Blake3 XOF XOR stream | Pre-AEAD blobs until `feanorfs migrate` |
| Key derivation | Blake3 with length-prefix domain separation | `blake3(domain ‖ len ‖ key ‖ len ‖ path)` — no salt, no KDF stretching |
| Hub transport | Rustls TLS 1.2/1.3 + bearer token | Public PKI or capability-pinned private hub CA; token generated by default |
| Pairing | SPAKE2 (Ed25519 group) + ChaCha20-Poly1305 | PAKE-authenticated invite transfer with AEAD and key confirmation over secret-free mDNS (`fnp1`) or bounded WSS rendezvous (`fnp2`) |
| Opaque relay | WebSocket carrying an inner Rustls stream | Random 256-bit route provides reachability; the existing private CA and bearer token still authenticate the hub end to end |
| Hub recovery/rotation | Argon2id + XChaCha20-Poly1305 | Passphrase-hardened authenticated backup, restore, and crash-safe replacement of the private CA and bearer token |
| Workspace recovery kit | Argon2id + XChaCha20-Poly1305 | Passphrase-hardened authenticated encryption of the complete portable workspace capability; restore enters the normal `start` path |

## Responsible disclosure

We follow responsible disclosure. Credit will be given to reporters in the release advisory unless they prefer to remain anonymous.
