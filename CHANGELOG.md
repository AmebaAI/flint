# Changelog

All notable changes to Flint are documented here, newest first, managed by
[momo](https://github.com/Jarvvski/momo).

Unreleased changes live under `## [Unreleased]`, grouped by kind:

- `### Breaking` - an incompatible change
- `### Added` - new functionality, backwards compatible
- `### Fixed` - a backwards-compatible bug fix

Record a change with `momo break|add|fix "<description>"`; momo owns the heading
and the section it lands in. `momo status` derives the next version from the
unreleased changes and the latest tag, and the release workflow runs `momo
release` and tags the release, so **do not bump the version by hand**. See
[docs/releasing.md](docs/releasing.md).

Flint is pre-1.0, so the minor position is the breaking slot (Cargo's `^0.2.0`
means `>=0.2.0, <0.3.0`): `break` and `add` both raise the minor, `fix` raises
the patch. That is momo's zero-aware rule. A 1.0.0 release needs the owner's
explicit approval.

Sections released before the momo adoption use Keep a Changelog headings
(`Changed`, `Deprecated`, `Removed`, `Security`); they are preserved as history
and carry no meaning now.

## [0.2.2] - 2026-10-27

### Fixed

- Update `ctutils` to 0.4.6 and `tokio` to 1.54.1 to address RUSTSEC-2026-0385 and RUSTSEC-2026-0386.

## [0.2.1] - 2026-10-21

### Breaking

### Added

### Fixed

- Update `rustls` to 0.23.46 to address RUSTSEC-2026-0285 and RUSTSEC-2026-0346 through RUSTSEC-2026-0349.
- Update the `debian:bookworm-slim` runtime base image to pick up the patched `libpcre2-8-0` (CVE-2026-86145, CVE-2026-89157, CVE-2026-89161).
- Upgrade the runtime image's Debian packages during the build so security fixes published after the pinned base image are installed, addressing the `perl` CVEs CVE-2026-13221, CVE-2026-42496, CVE-2026-8376, CVE-2026-42497, CVE-2026-48962, CVE-2026-57432, and CVE-2026-57433.
- Add Dependabot configuration for Cargo, GitHub Actions, and Docker base images so these updates arrive automatically.
- Update webpki-roots to 1.0.10 and rustls-webpki to 0.103.16 to address RUSTSEC-2026-0381 and RUSTSEC-2026-0382.

## [0.2.0] - 2026-08-20

### Added

### Changed

- Docker runtime names now default to the image name when omitted from labels or
  catalog entries.
- **Breaking:** Docker-discovered runtime images now use separate name and
  protocol labels instead of the JSON runtime descriptor label.
- Updated the Compose example to use the latest published GHCR image instead
  of building Flint locally.
- Distributed release images across native AMD64 and ARM64 runners, added
  reusable Docker dependency and GitHub Actions caches, and removed redundant
  verification builds.

### Deprecated

### Removed

### Fixed

- Release jobs now create or verify the Git tag before starting platform image
  builds, so tag permission failures stop before expensive compilation.

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

[0.2.2]: https://github.com/AmebaAI/flint/compare/v0.2.1...v0.2.2
[0.2.1]: https://github.com/AmebaAI/flint/compare/v0.2.0...v0.2.1
[0.2.0]: https://github.com/AmebaAI/flint/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/AmebaAI/flint/releases/tag/v0.1.0
