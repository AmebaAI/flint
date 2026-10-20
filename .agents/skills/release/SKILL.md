---
name: release
description: Cut a Flint release by dispatching the Release workflow, which mints the version with momo and opens a release pull request for the owner to review and merge. Use when asked to release Flint.
license: GPL-3.0
---

# Cut a Flint release

Use Jujutsu only. Never invoke Git directly.

Releasing is owner-controlled. The `Release` workflow mints the version with momo and opens a `release/vX.Y.Z` pull request; merging that PR tags the release and publishes the images. This skill dispatches the workflow and reports back — it never bumps the version, edits the changelog, tags, merges, or publishes anything itself.

## Preconditions

- `CHANGELOG.md` has at least one non-empty entry under `## [Unreleased]`; `momo status` reports the version it would mint.
- The working copy is clean and the latest mainline is fetched.
- No Flint release pull request is already open.

## Dispatch the release

1. Confirm there is something to release: run `momo status` and read the `→ next:` version.
2. Dispatch the workflow from the default branch: `gh workflow run release.yml -R AmebaAI/flint`.
   - Add `-f dry_run=true` only when the release process itself changed; it mints locally and stops before committing, pushing, or releasing.
   - To recover or republish an existing release, pass `-f version=<x.y.z> -f commit=<sha>`.
3. Watch it: `gh run watch -R AmebaAI/flint`.
4. Report the release pull request the run opens. The owner reviews and merges it; the merge tags the release, builds the multi-platform GHCR images, and creates the GitHub Release.

Never bump `Cargo.toml`, edit the changelog by hand, create a tag, or merge the release pull request.

## Safety and completion

Do not merge the release pull request, enable auto-merge, or create the tag. Finish by reporting the run URL, the version momo derived, and the release pull request URL.
