# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.1.0](https://github.com/rapm94/feanorfs/releases/tag/v0.1.0) - 2026-07-01

### Added

- *(client)* workspace conflict detection and sync-all scanner
- *(client)* add setup and attach onboarding with mirror wording
- *(client)* add mirror_state to --json status and sync results
- agent workspaces, library API, and quality-pass fixes
- *(client)* align token nomenclature, improve watch logs, and update documentation
- *(client)* add server authentication, connect command, and automatic discovery
- *(client)* implement connection profile caching and credentials config
- sort active workspaces and clean up workspace query output format
- *(client)* implement workspaces command and no-watch option for sync
- *(client)* integrate logging and trace client-side watch updates
- *(client)* modularize commands and watcher, refine local client logic
- *(client)* implement CLI sync client, API transport, local cache, and watcher

### Other

- document release-plz and cargo-dist release flow
- *(install)* align binstall and install script with cargo-dist
- workspace conflicts, shared sync delta, and CLI split
- update onboarding for setup, attach, and mirror_state
- OSS quality pass — dedupe binary, fix docs, add deny CI
- sync integration harness and quality-pass follow-ups
- add binary release workflows, cargo-binstall configurations, and install script
- update documentation, guides, and cargo workspace configurations
- wrap long tracing macro arguments and update log prefixes
- update documentation to reference feanorfs and document workspaces command
- *(deps)* add tracing and tracing-subscriber dependencies
- expand documentation, architecture, threat model, and community guidelines
