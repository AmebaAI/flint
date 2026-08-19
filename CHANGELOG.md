# Changelog

All notable changes to Flint will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/).

## [Unreleased]

### Added

### Changed

- Distributed release images across native AMD64 and ARM64 runners, added
  reusable Docker dependency and GitHub Actions caches, and removed redundant
  verification builds.

### Deprecated

### Removed

### Fixed

### Security

- Pinned the release container build and runtime base images by digest.

## [0.1.0] - 2026-08-19

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
- Added SQLite-backed logical session state and persistent per-session Docker
  volumes that survive compute replacement and Flint restarts.

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
- Added steady-state runtime health polling. `HealthyBusy` preserves background
  work, while idle, expired, or repeatedly unhealthy compute is stopped without
  deleting the logical session.
- Defaulted the Docker resource owner to `flint`; operators can still override
  it with `AGENTCORE_RUNTIME_OWNER` when running multiple instances.
- Updated the example runtime with a disk-backed invocation counter that
  demonstrates session storage surviving compute replacement.

### Fixed

- Initial Docker discovery failures now stop startup before reconciliation so
  existing owned session containers remain untouched.
- Killed or otherwise unavailable session containers are replaced on the next
  invocation instead of remaining cached as unavailable compute.
- Persisted sessions with stale deployment pins now block only their own resume
  instead of preventing Flint from starting with the current runtime catalog.

### Security

- Runtime containers are created from immutable image identities, image
  requests are intersected with operator allowlists, and active sessions remain
  pinned to their original deployment snapshot.
