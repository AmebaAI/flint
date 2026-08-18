# Changelog

All notable changes to Flint will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/).

## [Unreleased]

### Added

- Migrated the standalone AgentCore Runtime emulator into the Flint Rust package.
- Added deterministic runtime fixture, native Docker lifecycle tests, and
  containerized SDK coverage.
- Added minimal runtime catalog examples and the process-wide
  `flint-agentcore` container network configuration.
- Added repository security, contribution, and code ownership policies.
- Added pull request CI for Rust, Docker, dependency, license, and container
  security checks.
- Added Docker image-label runtime discovery with event-driven refresh,
  periodic resync, degraded health diagnostics, and last-known-good snapshots.
- Added guarded multi-platform GHCR and GitHub Release automation.

### Changed

- Rebranded package, binary, health identity, runtime defaults, image names,
  and local tooling for Flint.
- Made Docker discovery the default runtime source while retaining JSON
  catalogs as an explicit compatibility mode.
- Reduced runtime declarations to name, image, protocol, environment names,
  and AgentCore lifecycle settings. Protocol ports and paths now follow the
  fixed HTTP, MCP, A2A, and AG-UI service contracts.
- Derived local runtime identity from Floci-style SigV4 region and account
  context, with `us-east-1` and `000000000000` unsigned defaults.

### Deprecated

### Removed

### Fixed

- Initial Docker discovery failures now stop startup before reconciliation so
  existing owned session containers remain untouched.

### Security

- Runtime containers are created from immutable image identities, image
  requests are intersected with operator allowlists, and active sessions remain
  pinned to their original deployment snapshot.
