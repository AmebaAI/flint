# Releasing Flint

Status: current

How a Flint release is cut, what the versioning rules are, and how to recover
from a partial release.

## The versioning model

Versioning and the changelog are [momo](https://github.com/Jarvvski/momo)'s job.
The CLI is pinned in `mise.toml`; the configuration is
[`momo.toml`](../momo.toml).

The version of record is the **latest tag** (`v<x.y.z>`), and it is
**release-owned** - an ordinary change never moves it, and no one bumps the
`Cargo.toml` version by hand.

Every user-visible change is recorded with momo, which owns the changelog
heading and the section it lands in:

| Command | Heading | Meaning | Pre-1.0 slot | Post-1.0 slot |
| --- | --- | --- | --- | --- |
| `momo break` | `### Breaking` | an incompatible change | minor | major |
| `momo add` | `### Added` | new functionality, backwards compatible | minor | minor |
| `momo fix` | `### Fixed` | a backwards-compatible bug fix | patch | patch |

Flint is pre-1.0, so the minor position is the breaking slot: Cargo's `^0.2.0`
means `>=0.2.0, <0.3.0`, which makes `0.2.0 -> 0.3.0` a breaking change for a
dependent while `0.2.0 -> 0.2.1` is not. That is the same statement as the rule
that a MAJOR bump needs the owner's explicit approval - 1.0.0 is exactly where
`break` stops being a minor.

The next version is the latest tag plus the **strongest** recorded kind, by
momo's zero-aware rule:

```bash
momo status                  # what is unreleased, and the version it would mint
momo status --version-only   # just the version
```

`mise run check` runs `momo status` as its changelog validation: it fails on a
changelog momo cannot read (a duplicate `## [Unreleased]` heading, or an
Unreleased bullet under a Keep a Changelog heading momo does not read) and
passes when nothing is unreleased.

Only `Flint` is versioned; it is the only package this repository builds. The
images and the release page carry the same version.

## Cutting a release

`.github/workflows/release.yml`, dispatched by hand from the default branch.
Dispatch it with no inputs and the `mint-release` job derives the next version,
mints it with momo, and opens a `release/vX.Y.Z` pull request. `main` requires a
pull request, so the bump reaches it through that PR; merging the PR runs the
post-merge jobs, which tag the release, build the multi-platform images, and cut
the GitHub Release.

| Input | Default | Meaning |
| --- | --- | --- |
| `version` | *(empty)* | An existing release version to recover or republish; empty mints a new release |
| `commit` | *(empty)* | The merged commit SHA to publish (recovery only, with `version`) |
| `dry_run` | `false` | Mint locally and stop before committing, pushing, tagging, or releasing |

`workflow_dispatch` only runs from the default branch, so this workflow has to
be on `main` before it can be dispatched at all.

Dispatch the release:

```bash
gh workflow run release.yml
gh run watch
```

The `release` environment gates the post-merge job, and the release pull request
requires a code-owner review - that approval is the human gate on the version.

### Step order

`mint-release` (dispatch, no inputs):

1. Derive the version with `momo status --version-only`, refusing when nothing is
   unreleased, or when the tag already exists.
2. `momo release` mints it - it writes `CHANGELOG.md`, `.momo/version` and the
   `Cargo.toml` version scalar. `cargo metadata` then syncs `Cargo.lock`, because
   momo never writes the lock.
3. `mise run check`, then commit `Cargo.toml`, `Cargo.lock`, `CHANGELOG.md` and
   `.momo/version` on `release/vX.Y.Z` and open the pull request.

`validate-release-pr` runs the full gate on the pull request, including the
Docker and containerized SDK suites.

After merge (`prepare-release` -> `create-release-tag` -> `build-release-image`
-> `publish-release`):

4. Validate the merged revision, version and release order, then re-run the full
   gate.
5. Create or verify the `v<version>` tag at the merged commit.
6. Build the AMD64 and ARM64 images, push them by digest, and assemble the
   multi-platform manifest list.
7. Create the GitHub Release with the changelog section as its notes.

Three orderings are deliberate. The tag is created **before** the image builds,
so a tag permission failure stops before expensive compilation. The images are
built on native runners and only the digest list is published by the final job,
so a failed build leaves the tag without a release rather than a release
pointing at a missing image. The GitHub Release is created **last**, after the
images exist.

### What a release publishes

- The tag `v<version>` at the merged release commit.
- GHCR images `ghcr.io/amebaai/flint:<version>`, `:<major.minor>` and `:latest`,
  for `linux/amd64` and `linux/arm64`.
- A GitHub Release titled `Flint v<version>` whose notes are the changelog
  section.

### When to dry-run instead

`-f dry_run=true` derives the version, mints it with `momo release`, and prints
the diff, then stops before committing, pushing, tagging, or releasing. Nothing
is published.

That is worth an extra run only when the release *process* changed - a new step
in the workflow or a change to the packaging. For an ordinary release, dispatch
the real run: the pull-request review and the `release` environment are the gate,
and the step order already fails before anything irreversible.

```bash
gh workflow run release.yml -f dry_run=true
```

## Recovering from a partial release

| Failed at | State | Recovery |
| --- | --- | --- |
| Mint or pull request | nothing pushed, or the PR is open | re-dispatch, or push the PR by hand |
| Verification on the PR | PR open, nothing tagged | fix the failure and re-run the PR checks |
| Tag creation | release merged, no tag | re-dispatch with `version` and `commit` |
| Image build | tag pushed, no images | re-dispatch with `version` and `commit` |
| Manifest or release page | images pushed, no Release | re-dispatch with `version` and `commit` |

A tag is never reused for a different commit, so recovery is always re-dispatching
or running the step by hand - never re-minting the same version.

## Prerequisites

- A `release` environment on the repository. The merged-release job runs in it.
- `main` requires a pull request (with a code-owner review) and the required
  status checks, so the minted bump lands through the release pull request.
- `contents: write` for the mint job (to push the branch and open the PR),
  `contents: write` and `packages: write` for the publish jobs. The workflow
  declares these itself.

## Local equivalents

Everything the workflow runs is a command, so it can be exercised locally:

```bash
momo status                  # what is unreleased and the version it would mint
momo status --version-only   # just the version
momo break|add|fix "<text>"  # record a change
momo release --dry-run       # show what a release would write, and write nothing
momo release                 # mint the version (changelog + ledger + manifest)
mise run check               # the pre-land gate, including the changelog check
```

Entries released before the momo adoption use Keep a Changelog headings
(`Changed`, `Deprecated`, `Removed`, `Security`); they are history and carry no
meaning now.
