# FeanorFS release TODO

This is the only authoritative list of open work required to publish and prove
the seamless native desktop installers. Shipped work belongs in `CHANGELOG.md`.
Do not add unrelated product ideas here.

## Founder tasks

These require account ownership or representative human acceptance. Never
commit credentials or paste them into issues, logs, or chat.

### F1. Enable the trusted macOS DMG release

- [ ] Create or renew Developer ID Application and Developer ID Installer
  identities and an App Store Connect notarization API key.
- [ ] Store the documented `APPLE_*` values only as GitHub Actions secrets.
- [ ] Run `.github/workflows/tray-release.yml` for the immutable release tag and
  give the AI only the workflow URL and tag.

Done when GitHub contains the universal `.dmg` and `.pkg`, checksums,
attestations, notarization results, and verification evidence; Gatekeeper
accepts both containers; and the signed CLI passes the redacted Keychain smoke.

### F2. Enable the trusted Windows EXE release

- [ ] Configure Azure Artifact Signing with GitHub OIDC.
- [ ] Store `AZURE_CLIENT_ID`, `AZURE_SUBSCRIPTION_ID`, and `AZURE_TENANT_ID`
  only as GitHub Actions secrets, and set the documented signing account,
  endpoint, and certificate-profile repository variables.
- [ ] Run `.github/workflows/desktop-release.yml` for the same immutable release
  tag and give the AI only the workflow URL and tag.

Done when the CLI, tray, and installer `.exe` all have valid Authenticode
chains and the signed installer/product smokes and GitHub attestations pass.

### F3. Accept the clean-machine onboarding experience

Blocked by F1 and F2.

- [ ] On a normal macOS account, install from the DMG and confirm the menu-bar
  chooser offers **Start Mirroring**, **Join Another Computer**, and **Not
  Now** without Terminal.
- [ ] On a normal Windows account, run the installer EXE and confirm the same
  tray-first choices, Start-menu entry, CLI PATH, login persistence, and clean
  uninstall.
- [ ] On Debian/Ubuntu, Fedora/RHEL, and Arch/Manjaro, install the native package
  and confirm the tray opens and survives logout/login.

Done when the founder records the tested OS versions and either accepts the
flow or reports a reproducible defect without sharing workspace secrets.

## AI tasks

### AI-1. Close the macOS installer gate

Blocked by F1.

- [ ] Verify the tag and release target are identical.
- [ ] Verify universal architectures, Developer ID signatures, package and DMG
  notarization/stapling, exact payload, checksums, attestations, Keychain smoke,
  and tray-first installation.
- [ ] Fix reproducible packaging defects and rerun the same immutable tag.

### AI-2. Close the Windows installer gate

Blocked by F2.

- [ ] Verify Authenticode on both binaries and the installer EXE, exact payload
  hashes, checksum, attestation, PATH/uninstall behavior, Credential Manager,
  Task Scheduler, TLS, doctor, MCP, and tray-first installation.
- [ ] Fix reproducible packaging defects and rerun the same immutable tag.

### AI-3. Close the final desktop acceptance gate

Blocked by F3, AI-1, and AI-2.

- [ ] Review the founder's clean-machine results and reproduce every defect.
- [ ] Confirm the published `.dmg`, `.exe`, `.deb`, `.rpm`, and
  `.pkg.tar.zst` are the exact verified assets and no unsigned macOS/Windows
  fallback exists.
- [ ] Confirm no token, E2EE key, pairing capability, recovery passphrase,
  private route, CA private material, filename, or file content appears in
  process arguments, environment variables, logs, discovery, or release
  evidence outside its explicitly encrypted form.

Done when every supported installer passes and the founder accepts the normal
user flow.
