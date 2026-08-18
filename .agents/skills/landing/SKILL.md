---
name: landing
description: Land a Flint change through Jujutsu while maintaining the Unreleased changelog and opening a ready-for-review pull request. Use when asked to land a Flint change.
license: GPL-3.0
---

# Land a Flint change

Use Jujutsu only. Never invoke Git directly.

## Preconditions

1. Inspect `jj status`, `jj log`, and the complete diff.
2. Keep the change focused and do not modify unrelated work.
3. Run the repository's verification task with `mise run check` when the Rust source and manifest exist. Before source migration, report that the check is unavailable rather than inventing a passing result.

## Changelog

For a user-visible change, add a concise entry under `CHANGELOG.md` -> `## [Unreleased]` in the appropriate category:

- `Added` for new behavior
- `Changed` for changed behavior, including an explicit `**Breaking:**` marker when incompatible
- `Deprecated` for behavior scheduled for removal
- `Removed` for removed behavior
- `Fixed` for bug fixes
- `Security` for security fixes

Describe what users observe, not implementation details. Do not bump the Cargo version during an ordinary landing. Skip changelog edits for pure refactors, tests, formatting, and non-behavioral maintenance.

## Landing

After verification succeeds:

1. Review the final diff and confirm the changelog entry is included when required.
2. Describe the current change with `jj describe -m` using a short imperative subject and a concise body. Never add co-author trailers.
3. Create an `adam/<descriptive-slug>` bookmark with `jj bookmark create`.
4. Push that bookmark with `jj git push -b <bookmark>`.
5. Resolve the repository from `jj git remote list`; pass the explicit repository to `gh`.
6. Open a ready-for-review pull request with `gh pr create -R <owner/repo>`.

The PR must explain the user-visible change and verification performed. Never merge, enable auto-merge, enqueue a merge queue, or create a release from an ordinary landing.

Finish with `jj status`. If it is not clean after the landing operation, stop and report the remaining changes.
