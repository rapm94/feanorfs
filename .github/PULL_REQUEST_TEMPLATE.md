## Summary

<!-- Brief description of what this PR changes and why. -->

## Related issue

<!-- Link the issue this PR addresses: Closes #123 -->

## Type of change

- [ ] Bug fix (non-breaking change which fixes an issue)
- [ ] New feature (non-breaking change which adds functionality)
- [ ] Breaking change (fix or feature that would cause existing behavior to not work as expected)
- [ ] Documentation update
- [ ] Security-relevant change (encryption, auth, key handling)
- [ ] Refactor (no functional change)

## Checklist

- [ ] `cargo fmt --all -- --check` passes
- [ ] Core Clippy passes (`--workspace --exclude feanorfs-tray --all-targets --all-features`)
- [ ] Core tests pass (`--workspace --exclude feanorfs-tray --all-features`)
- [ ] macOS tray checks pass if `tray/` changed
- [ ] `cargo deny check` passes if dependencies changed
- [ ] `CHANGELOG.md` updated under `## [Unreleased]` (if user-facing change)
- [ ] `SECURITY.md` and/or `docs/threat-model.md` updated (if security-relevant change)
- [ ] `README.md` and `docs/` updated (if user-facing behavior changed)
- [ ] Tests added for new functionality or bug fixes

## Notes for reviewer

<!-- Anything the reviewer should pay attention to: tricky logic, security implications, breaking changes, migration steps. -->
