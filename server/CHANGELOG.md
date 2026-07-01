# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.1.0](https://github.com/rapm94/feanorfs/releases/tag/v0.1.0) - 2026-07-01

### Added

- *(client)* workspace conflict detection and sync-all scanner
- agent workspaces, library API, and quality-pass fixes
- *(client)* align token nomenclature, improve watch logs, and update documentation
- *(server)* support token/API key authentication, custom port, and custom data directory
- *(server)* implement bearer token authentication and mDNS advertisement
- sort active workspaces and clean up workspace query output format
- *(server)* add active workspace query database operation and GET route
- *(server)* integrate tracing and tower-http log subscriber
- *(server)* update axum blob server and DB modules
- *(server)* implement axum blob storage server and sqlite metadata DB

### Other

- document release-plz and cargo-dist release flow
- *(install)* align binstall and install script with cargo-dist
- workspace conflicts, shared sync delta, and CLI split
- update onboarding for setup, attach, and mirror_state
- OSS quality pass — dedupe binary, fix docs, add deny CI
- sync integration harness and quality-pass follow-ups
- add binary release workflows, cargo-binstall configurations, and install script
- update documentation, guides, and cargo workspace configurations
- update documentation to reference feanorfs and document workspaces command
- *(deps)* add tracing and tracing-subscriber dependencies
- expand documentation, architecture, threat model, and community guidelines
