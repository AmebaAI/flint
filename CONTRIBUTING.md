# Contributing to Flint

Thank you for contributing to Flint.

## Before opening a change

- Discuss substantial behavior or public configuration changes in an issue.
- Keep changes focused and avoid unrelated cleanup.
- Add user-visible changes to the Unreleased section of `CHANGELOG.md`.
- Report security issues privately as described in `SECURITY.md`.

## Local setup

Flint uses [mise](https://mise.jdx.dev/) for its toolchain and task runner.

```sh
mise install
mise run check
```

Docker-backed changes must also pass:

```sh
mise run test:docker
mise run test:container
mise run ci
```

The Docker checks require a trusted local Docker daemon. Do not run untrusted
pull-request code against a shared or production Docker daemon.

## Pull requests

Pull requests should:

- Explain the user-visible behavior and motivation.
- Include focused tests for changed behavior.
- Preserve compatibility with the pinned official AgentCore SDK tests.
- Pass all required checks.
- Resolve review conversations before merge.

The project uses squash merges and requires an approving review from the code
owners.

## License

By contributing, you agree that your contribution is licensed under the
GNU General Public License v3.0 only.
