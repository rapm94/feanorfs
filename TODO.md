# FeanorFS TODO

This is the only authoritative list of open FeanorFS work. It contains only
tasks required to finish the consumer desktop product, hosted connectivity,
public SDK distribution, and legacy-crypto retirement. Shipped work belongs in
`CHANGELOG.md`, not here. Speculative features and trigger-only ideas are not
TODOs.

## Founder tasks

These tasks require accounts, credentials, product decisions, operating
authority, or representative users. Never commit credentials or paste them
into issues, logs, or chat.

### F1. Enable and prove the signed macOS release

- [ ] Create or renew Developer ID Application and Developer ID Installer
  identities and an App Store Connect notarization API key.
- [ ] Store the following only as GitHub Actions secrets:
  `APPLE_DEVELOPER_ID_P12_BASE64`, `APPLE_DEVELOPER_ID_P12_PASSWORD`,
  `APPLE_DEVELOPER_ID_INSTALLER_P12_BASE64`,
  `APPLE_DEVELOPER_ID_INSTALLER_P12_PASSWORD`, `APPLE_NOTARY_ISSUER_ID`,
  `APPLE_NOTARY_KEY_ID`, and `APPLE_NOTARY_KEY_P8_BASE64`.
- [ ] Run `.github/workflows/tray-release.yml` for a real immutable `v*` tag.
- [ ] Give the AI the workflow URL and tag. Do not send certificate material or
  secret values.

Done when the release contains the stapled universal package, checksum,
notarization result, verification report, Keychain smoke report, and GitHub
attestation; Gatekeeper accepts it and the report binds the signed CLI hash to
the Keychain test.

### F2. Enable and prove the signed Windows release

- [ ] Configure Azure Artifact Signing and its GitHub OIDC trust.
- [ ] Store `AZURE_CLIENT_ID`, `AZURE_SUBSCRIPTION_ID`, and `AZURE_TENANT_ID` as
  GitHub Actions secrets.
- [ ] Set `AZURE_ARTIFACT_SIGNING_ACCOUNT_NAME`,
  `AZURE_ARTIFACT_SIGNING_ENDPOINT`, and
  `AZURE_ARTIFACT_SIGNING_CERTIFICATE_PROFILE` as GitHub Actions variables.
- [ ] Run `.github/workflows/desktop-release.yml` for the same immutable release
  tag used for the desktop release.
- [ ] Give the AI the workflow URL and tag, without credentials.

Done when both Windows executables have valid Authenticode signatures, the
exact two-file bundle passes checksum and architecture checks, the signed
product smoke proves Credential Manager plus Task Scheduler lifecycle, and the
published artifacts have GitHub attestations.

### F3. Define and provision the hosted product

- [ ] Choose the production domain, cloud, region, budget, support address, and
  incident owner for the hosted account service and default relay.
- [ ] Choose the login method. A mainstream hosted identity provider with email
  magic-link or passkey support is preferred; FeanorFS must not store login
  passwords.
- [ ] Approve the recovery policy: the service may store only
  client-encrypted workspace capabilities and must never receive plaintext
  E2EE keys, hub bearer tokens, recovery passphrases, filenames, or file bytes.
- [ ] Approve retention, deletion, abuse/rate-limit, privacy, and availability
  policies before inviting external users.
- [ ] Provision DNS, TLS, runtime, database, secret manager, monitoring, backup,
  and staging/production environments. Give the AI scoped deployment access
  through the platform secret manager or GitHub environment, never raw secrets
  in the repository.

Done when the decisions and environments required by AI-3 exist and the
founder has approved a production-readiness checklist.

### F4. Enable the first public Node SDK release

- [ ] Confirm ownership of the `@feanorfs` npm organization and all six package
  names produced by `bindings/ts/`.
- [ ] Configure npm trusted publishing for the repository workflow. Use
  `NPM_TOKEN` only if trusted publishing cannot be used.
- [ ] Approve the first public SDK version and immutable release tag.
- [ ] Give the AI the workflow URL and tag; never send an npm token.

Done when the facade and five native packages are public with matching
versions and npm provenance, and a clean consumer install passes the packed
SDK smoke test.

### F5. Collect legacy-format migration evidence

- [ ] Ask representative existing users on macOS, Linux, and Windows to run
  `feanorfs --json doctor --migration-report` after updating.
- [ ] Collect only the aggregate report. Do not request workspace paths,
  identifiers, endpoints, credential references, routes, keys, tokens, or
  capabilities.
- [ ] Migrate every reported format-v1 workspace with `feanorfs migrate` or
  `feanorfs migrate --rekey` and collect a new aggregate report.
- [ ] Define and approve the retirement threshold, including minimum sample
  size, platform coverage, observation period, and rollback plan.
- [ ] Give the sanitized aggregate evidence and approval to the AI.

Done when the approved representative evidence contains no format-v1
workspace and explicitly authorizes AI-5. A single local report is not enough.

### F6. Commission hosted security review and approve production launch

Blocked by AI-3.

- [ ] Select an independent application-security reviewer with PAKE, E2EE,
  OAuth/OIDC, WebSocket, and cloud-infrastructure experience.
- [ ] Give the reviewer the hosted threat model, API contract, staging access,
  source revision, deployment manifest, and AI-3 evidence without production
  credentials or user data.
- [ ] Require written findings covering account takeover, encrypted-vault
  substitution/rollback, device revocation, relay abuse and denial of service,
  metadata leakage, logging, tenant isolation, deletion, backup/restore, and
  dependency/supply-chain risk.
- [ ] Accept or reject every finding explicitly and require all critical/high
  findings to be fixed and retested.
- [ ] Approve the production go/no-go only after the review, load test,
  backup/restore drill, incident drill, and privacy/deletion checks pass.

Done when the signed review closure and founder launch approval are recorded
without publishing sensitive infrastructure details.

## AI tasks

The AI owns implementation, automated verification, documentation, and
evidence review. It must stop rather than weaken signing, TLS, authentication,
credential, or encryption requirements.

### AI-1. Close the macOS release gate

Blocked by F1.

- [ ] Inspect the complete workflow and public release evidence.
- [ ] Verify the tag resolves to the released commit; both binaries are
  universal and Developer ID signed; the installer is Developer ID Installer
  signed, notarized, stapled, and Gatekeeper accepted; payload files are exact;
  and the signed CLI passed the redacted Keychain reload smoke.
- [ ] Fix reproducible workflow or packaging defects and rerun the same gate.
- [ ] Update release documentation only after all evidence passes.

### AI-2. Close the Windows release gate

Blocked by F2.

- [ ] Inspect the native Windows and Azure signing jobs plus public artifacts.
- [ ] Verify both binaries' Authenticode chains, exact bundle contents,
  checksums, architecture, Credential Manager redaction/reload, interactive
  tray registration, background hub/workspace tasks, TLS, doctor, MCP, and
  stop/resume behavior.
- [ ] Fix reproducible workflow or product defects and rerun the signed gate.
- [ ] Never publish or label unsigned Windows binaries as the desktop product.

### AI-3. Build and validate hosted accounts plus the default relay

Blocked by F3.

- [ ] Write the hosted threat model and API contract before implementation.
  Preserve the existing dumb-storage boundary and inner TLS; the relay must see
  only bounded routing/session metadata and opaque bytes.
- [ ] Implement browser-based login for the CLI/tray without putting session
  codes or tokens in argv, environment variables, logs, URLs, or local
  plaintext config.
- [ ] Implement an account vault that stores only versioned,
  client-encrypted workspace capabilities, supports explicit device removal
  and account deletion, and fails closed when local OS credential storage is
  unavailable after migration.
- [ ] Make the hosted relay the zero-configuration off-LAN fallback while
  preserving direct LAN discovery first and self-hosted relay overrides.
- [ ] Add bounded rate limits, quotas, expiry, abuse controls, health checks,
  backup/restore, and secret-free metrics. Capability-bearing routes and inner
  tunnel bytes must never enter logs or traces.
- [ ] Add staging end-to-end tests for first device, second-device join,
  offline/reconnect, reboot/login persistence, credential revocation, account
  deletion, relay denial, malformed frames, and recovery. FeanorFS must never
  auto-merge conflicts.
- [ ] Produce the threat model, test evidence, deployment inventory, and review
  brief required by F6; fix and retest every accepted finding.

### AI-4. Publish and verify the Node SDK

Blocked by F4.

- [ ] Run the existing trusted-tag package assembly, packed-package tests, and
  provenance-enabled publish workflow.
- [ ] Verify the facade selects the correct package on macOS x64/arm64, Linux
  x64/arm64, and Windows x64 without downloading code at runtime.
- [ ] Install from the public registry in a clean project and run the real SDK
  smoke path.
- [ ] Record the public package/version links and remove any temporary token
  fallback after trusted publishing succeeds.

### AI-5. Retire legacy XOR only after field evidence

Blocked by F5.

- [ ] Review the sanitized aggregate evidence against the founder-approved
  threshold. Stop if any format-v1 workspace, unreadable registry, malformed
  config, unsupported format, or insufficient sample remains.
- [ ] Remove `LegacyPolicy`, legacy XOR decryption, the unsafe default password,
  and format-v1 compatibility branches without changing format-v3 ciphertext
  identity or deterministic CAS behavior.
- [ ] Keep migration diagnostics understandable for users who skipped the
  required upgrade path; never silently reinterpret old ciphertext.
- [ ] Add rejection, tamper, migration-boundary, rollback, and mixed-version
  tests; update the threat model, compatibility documentation, and release
  notes.

### AI-6. Run the final consumer release audit

Blocked by AI-1, AI-2, AI-3, and F6. AI-4 and AI-5 may ship independently when
their gates are satisfied.

- [ ] From clean macOS, Linux, and Windows machines, verify installer to tray
  onboarding, **Start Mirroring**, **Join Another Computer**, **Not Now**,
  login/reboot persistence, LAN and off-LAN pairing, idle convergence,
  recovery, diagnostics/repair, update awareness, conflict preservation, and
  reversible stop/resume.
- [ ] Verify no key, token, invite, pairing capability, recovery passphrase,
  private route, filename, or file content appears in process arguments,
  environment variables, logs, traces, crash reports, discovery, or hosted
  storage outside its explicitly encrypted form.
- [ ] Require signed/checksummed/attested artifacts and green release evidence
  on every platform; do not substitute a local development build.
- [ ] Publish the supported-platform matrix, security limitations, recovery
  responsibilities, and operator runbook only after the evidence is green.

## Completion order

1. F1 → AI-1 and F2 → AI-2 can run in parallel.
2. F3 → AI-3 → F6 completes and authorizes the hosted consumer product.
3. F4 → AI-4 completes public SDK distribution independently.
4. F5 → AI-5 remains gated until representative migration evidence exists.
5. AI-6 is the consumer launch decision.
