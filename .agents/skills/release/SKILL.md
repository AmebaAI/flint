---
name: release
description: Prepare a Flint release pull request by computing the SemVer bump from Unreleased changelog entries, updating the manifest and changelog, and opening a guarded release PR. Use when asked to release Flint.
license: GPL-3.0
---

# Prepare a Flint release

Use Jujutsu only. Never invoke Git directly. This skill prepares and pushes a release PR; it never merges, tags, publishes packages, or creates a GitHub Release locally.

## Preconditions

- The working copy is clean before starting.
- The latest mainline is fetched and the release is based on it.
- A root `Cargo.toml` package named `flint` exists. Until source migration supplies it, stop and report that release preparation is unavailable.
- `CHANGELOG.md` contains at least one non-empty entry under `## [Unreleased]`.
- No existing release tag or GitHub Release uses the computed version.
- No other open Flint release PR is being prepared.

## Compute the version

Read all populated `Unreleased` categories and compute the highest required bump:

- `Fixed` and `Security` only: PATCH
- Any `Added`, `Changed`, or `Deprecated`: MINOR
- `Removed` or an explicit `**Breaking:**` entry: MINOR while the current version is `0.x`
- At `1.x` and later, `Removed` or explicitly breaking entries: MAJOR
- A `0.x` to `1.0.0` transition always requires explicit owner approval

Use the manifest version as the version of record. If the current version is `0.0.0`, the first feature release is `0.1.0`; a fixes-only first release is `0.0.1`. If no populated entries exist, stop.

## Prepare the release PR

1. Update the root package version in `Cargo.toml`.
2. Refresh the matching root package version in `Cargo.lock` without changing dependency resolution unnecessarily.
3. Move populated `Unreleased` entries into `## [X.Y.Z] - YYYY-MM-DD`, preserving categories and order.
4. Recreate an empty `## [Unreleased]` section with Added, Changed, Deprecated, Removed, Fixed, and Security headings.
5. Run `mise run check`. Before source migration, stop at the precondition instead of claiming success.
6. Review the full diff and verify the version, changelog heading, lockfile, and release metadata agree.
7. Describe the change with `jj describe -m "Release Flint vX.Y.Z"` and a concise body explaining the computed bump.
8. Create `release/vX.Y.Z` with `jj bookmark create`.
9. Push it with `jj git push -b release/vX.Y.Z`.
10. Resolve the repository from `jj git remote list` and open a ready-for-review PR with `gh pr create -R <owner/repo>`.

The PR title must be `Release Flint vX.Y.Z`. Its body must include the release notes and the reason for the PATCH, MINOR, or MAJOR bump. Never merge, enable auto-merge, enqueue a merge queue, or create the tag locally.

## Safety and completion

The release PR must be the only release mutation. The merged-release GitHub Actions workflow performs validation, tagging, GitHub Release creation, and GHCR publication. Finish by checking `jj status` and report the PR URL. Never add co-author trailers.
