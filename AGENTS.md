## Versioning & changelog

Versioning and the changelog are momo's job (pinned in `mise.toml`; see [`docs/releasing.md`](docs/releasing.md)). The version of record is the latest `v<x.y.z>` tag, and it is release-owned: an ordinary change never moves it, and no one bumps `Cargo.toml` by hand.

A user-visible change is recorded with momo, which owns the changelog heading and the section it lands in:

- `momo break "<description>"` - an incompatible change
- `momo add "<description>"` - new functionality, backwards compatible
- `momo fix "<description>"` - a backwards-compatible bug fix

`momo status` derives the next version from the unreleased changes and the latest tag, and `mise run check` runs it as the changelog gate. Flint is pre-1.0, so the minor position is the breaking slot: `break` and `add` both raise the minor, `fix` raises the patch. Never bump to 1.0.0 (or any MAJOR) without the owner's explicit approval. Skip pure internal refactors.

Releases are cut by dispatching the `Release` workflow, which mints the version and opens a release pull request - see [`docs/releasing.md`](docs/releasing.md).

## Agent skills

### Issue tracker

Issues and specs live in GitHub Issues, accessed with the `gh` CLI. See `docs/agents/issue-tracker.md`.

### Domain docs

This is a single-context repo with root `CONTEXT.md` and ADRs under `docs/adr/`. See `docs/agents/domain.md`.
