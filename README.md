# Flint

> Lights the fire without the Firecracker.

Flint is a thin, local-focused emulator for [AgentCore](https://aws.amazon.com/bedrock/agentcore/) from AWS, written in Rust. It is intended as a development utility for running and testing AgentCore-oriented workflows locally.

## Status

Flint is implemented as a standalone Rust package with AgentCore Runtime-compatible routes, Docker-backed sessions, native and container connectivity, opaque protocol proxying, runtime commands, and an immutable runtime catalog.

## Development

Flint uses [mise](https://mise.jdx.dev/) for its toolchain and task runner:

```sh
mise install
mise run fmt
mise run check
mise run publishLocal
mise run fixture:build
mise run test:docker
mise run test:container
```

The ordinary `check` task runs formatting, Clippy, build, and non-Docker
checks. Docker lifecycle checks are opt-in because they require a local Docker
daemon and the fixture image.

## Container usage

Build the deterministic fixture image and start the example Compose service
with the secret-free Compose catalog:

```sh
mise run fixture:build
export AGENTCORE_RUNTIME_OWNER=flint-example
docker compose -f compose.example.yml up --build --wait
```

`AGENTCORE_RUNTIME_OWNER` is required. Choose a stable value that is unique to
this Flint deployment so restarts can adopt its sessions without colliding with
another instance.

The essential Compose configuration is:

```yaml
services:
  flint:
    image: flint:local
    build:
      context: .
      dockerfile: Dockerfile
    ports:
      - "127.0.0.1:${FLINT_PORT:-35469}:8080"
    environment:
      PORT: "8080"
      AGENTCORE_RUNTIME_CATALOG: /etc/flint/runtime-catalog.json
      AGENTCORE_RUNTIME_OWNER: ${AGENTCORE_RUNTIME_OWNER:?required}
      RUST_LOG: ${RUST_LOG:-info}
    volumes:
      - /var/run/docker.sock:/var/run/docker.sock
      - "${FLINT_RUNTIME_CATALOG:-./config/runtime-catalog.compose.example.json}:/etc/flint/runtime-catalog.json:ro"
    networks:
      - flint-agentcore
    healthcheck:
      test: [CMD, curl, --fail, --silent, "http://127.0.0.1:8080/_local/health"]
      interval: 2s
      timeout: 2s
      retries: 30

networks:
  flint-agentcore:
    name: ${FLINT_DOCKER_NETWORK:-flint-agentcore}
```

### Endpoints

| Client | Endpoint |
| --- | --- |
| Host process | `http://localhost:35469` |
| Compose sibling | `http://flint:8080` |
| Flint health check | `http://127.0.0.1:35469/_local/health` |
| Runtime container | `http://<runtime-container-name>:8080` |

The host port is configurable with `FLINT_PORT`. AgentCore SDK clients should set
an AgentCore Runtime endpoint URL appropriate to where the client runs. The
Compose catalog defaults to the `CONTAINER` qualifier, so unqualified requests
and requests that explicitly select `CONTAINER` use the shared Docker network.

### Runtime catalog topology

Flint uses separate catalogs for the two supported topologies:

- `config/runtime-catalog.example.json` contains the native `DEFAULT`
  deployment for a Flint process running directly on the host.
- `config/runtime-catalog.compose.example.json` contains the `CONTAINER`
  deployment used by `compose.example.yml`.

A containerized Flint service must use `connectivity.mode: container`, with
`dockerNetwork` matching `FLINT_DOCKER_NETWORK`. Flint and spawned runtime
containers then share that named network, and Flint reaches runtimes by Docker
container name without publishing runtime ports to the host. Do not combine
native and container deployments in one service catalog because only one
topology is reachable from a given Flint process.

To use a custom `FLINT_DOCKER_NETWORK`, copy the Compose catalog, change its
`dockerNetwork` to the same value, and pass that file through
`FLINT_RUNTIME_CATALOG`. Changing only the Compose environment variable
intentionally fails startup preflight.

### Docker authority and startup policy

Catalog images must already be present in the Docker daemon before Flint starts.
Flint does not pull images or manage registry credentials. Startup fails with
an image reference when any catalog image is unavailable.

Container connectivity accepts named, local bridge networks only. Special
Docker modes such as `host`, `none`, `bridge`, `default`, and `container:<id>`
are rejected. Startup also verifies that every configured network exists and
that the Flint container is attached to it. The example does not add a
host-gateway mapping. Set `addHostGateway: true` only when a trusted deployment
explicitly requires host access.

For local lifecycle validation, run `mise run test:docker` for native Docker
checks or `mise run test:container` for the full Compose topology and startup
mismatch check. The service needs the Docker socket to create and manage runtime
containers. Mounting `/var/run/docker.sock` grants effectively host-level Docker
control, so use the example only in a trusted local environment. This profile is
not a production isolation boundary.

Published releases use the multi-platform image
`ghcr.io/<repository-owner>/flint` and provide SemVer tags plus `latest`.

## License

Flint is available under the [GNU General Public License v3.0](LICENSE). Flint
is an independent project and is not affiliated with, sponsored by, or endorsed
by AWS.

See [CHANGELOG.md](CHANGELOG.md) for release history.
