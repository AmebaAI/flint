# Flint

> Lights the fire without the Firecracker.

Flint is a thin, local-focused emulator for [AgentCore](https://aws.amazon.com/bedrock/agentcore/) from AWS, written in Rust. It is intended as a development utility for running and testing AgentCore-oriented workflows locally.

## Status

Flint is implemented as a standalone Rust package with AgentCore Runtime-compatible routes, Docker-backed sessions, native and container connectivity, opaque protocol proxying, runtime commands, and immutable Docker-discovered runtime snapshots.

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

Build the labeled fixture image and start the discovery-based example:

```sh
mise run fixture:build
export AGENTCORE_RUNTIME_OWNER=flint-example
docker compose -f compose.example.yml up --build --wait
```

`AGENTCORE_RUNTIME_OWNER` is required. Choose a stable value unique to this
Flint deployment so restarts can adopt its sessions without colliding with
another instance. The default Compose profile mounts only the Docker socket:

```yaml
services:
  flint:
    image: flint:local
    ports:
      - "127.0.0.1:${FLINT_PORT:-35469}:8080"
    environment:
      PORT: "8080"
      AGENTCORE_RUNTIME_SOURCE: docker
      AGENTCORE_RUNTIME_IMAGES: flint-runtime-fixture:local
      AGENTCORE_RUNTIME_OWNER: ${AGENTCORE_RUNTIME_OWNER:?required}
      FLINT_CONNECTIVITY_MODE: container
      FLINT_DOCKER_NETWORK: flint-agentcore
      FLINT_RUNTIME_ENV_ALLOWLIST: FLINT_FIXTURE_ALLOWED,FLINT_FIXTURE_UNSET
      FLINT_FIXTURE_ALLOWED: fixture-allowed
      FLINT_FIXTURE_UNAPPROVED: must-not-forward
      FLINT_RUNTIME_HEADER_ALLOWLIST: x-flint-invocation-id
    volumes:
      - /var/run/docker.sock:/var/run/docker.sock
    networks:
      - flint-agentcore

networks:
  flint-agentcore:
    name: flint-agentcore
```

### Endpoints

| Client | Endpoint |
| --- | --- |
| Host process | `http://localhost:35469` |
| Compose sibling | `http://flint:8080` |
| Flint health check | `http://127.0.0.1:35469/_local/health` |

The host port is configurable with `FLINT_PORT`. AgentCore SDK clients should set
an AgentCore Runtime endpoint URL appropriate to where the client runs.

### AgentCore runtime contracts

Runtime ports and paths are fixed by the AgentCore service contract and cannot
be overridden in Flint configuration:

| Protocol | Port | Invocation | Health | Agent card |
| --- | ---: | --- | --- | --- |
| `HTTP` | 8080 | `POST /invocations` | `GET /ping` | None |
| `MCP` | 8000 | `POST /mcp` | None | None |
| `A2A` | 9000 | `POST /` | `GET /ping` | `GET /.well-known/agent-card.json` |
| `AGUI` | 8080 | `POST /invocations` | `GET /ping` | None |

Images must bind their protocol port on `0.0.0.0`. Flint supports HTTP JSON/SSE,
MCP streamable HTTP, A2A JSON-RPC, and AG-UI SSE through the AgentCore Runtime
API. WebSocket proxying at `/ws` is not currently implemented.

Hosted AgentCore requires ARM64 images. Flint intentionally accepts images for
the local Docker host architecture so development does not require emulation.
It still enforces the hosted service's protocol ports and paths.

### Docker image discovery

Docker discovery is the default runtime source. A runtime image opts in with the
`ai.ameba.flint.runtime.descriptor` image label. Its strict JSON value contains
only runtime settings that AgentCore exposes:

```json
{
  "name": "example",
  "protocol": "HTTP",
  "environmentVariables": ["MODEL"],
  "lifecycleConfiguration": {
    "idleRuntimeSessionTimeout": 900,
    "maxLifetime": 28800
  }
}
```

Only `name` and `protocol` are required in an image label. Valid protocols are
`HTTP`, `MCP`, `A2A`, and `AGUI`. Each marked image declares one runtime and each
runtime has only the `DEFAULT` qualifier.

By default Flint scans all marked local images. Set
`AGENTCORE_RUNTIME_IMAGES` to a comma-separated exact allowlist of image
references when only selected images should be trusted. Images must already be
present; Flint does not pull them or manage registry credentials. It resolves
each selected runtime to an immutable image identity before creating a
container.

`environmentVariables` contains host environment variable names. The operator
must separately approve every forwarded name with
`FLINT_RUNTIME_ENV_ALLOWLIST`. A requested variable that is unapproved, unset,
or empty is omitted and produces a warning when that runtime starts. Values are
captured in the immutable registry snapshot, so a changed value applies to new
sessions after the catalog is refreshed.

Lifecycle values are seconds. Flint follows AgentCore's microVM defaults of 900
seconds idle and 28,800 seconds maximum lifetime. Both values must be from 60 to
28,800 seconds and idle time must not exceed maximum lifetime. Flint currently
models microVM lifecycle only. AgentCore Instances, including their 14-day
maximum, will require explicit compute configuration in a future version.

Initial discovery must succeed before Flint reconciles owned sessions. If it
fails, startup stops and existing owned containers are left untouched. After a
successful start, Flint refreshes discovery after Docker image events and on
the `FLINT_DISCOVERY_REFRESH_SECONDS` interval. Invalid refreshes leave the
last known-good snapshot active and report `degraded` with diagnostics from
`/_local/health`. Existing sessions keep their original immutable deployment;
new sessions use the latest valid snapshot.

### Local runtime identity

Runtime names become runtime IDs. Flint derives ARNs instead of storing them in
the catalog. Unsigned requests use region `us-east-1` and account
`000000000000`. Signed requests use the SigV4 credential region, and a 12-digit
access-key ID becomes the account ID. Other access-key IDs use
`000000000000`. An ARN or `accountId` that conflicts with this request identity
is rejected. Requests that identify a runtime by name instead of a full ARN
must include the derived `accountId` query parameter.

Derived ARNs use this form:

```text
arn:aws:bedrock-agentcore:<region>:<account>:runtime/<name>
```

### Explicit JSON catalog mode

JSON catalogs remain available as an explicit compatibility source. They use the
same minimal settings plus the image reference:

```json
{
  "runtimes": [
    {
      "name": "example",
      "image": "example-agent:local",
      "protocol": "HTTP",
      "environmentVariables": ["MODEL"],
      "lifecycleConfiguration": {
        "idleRuntimeSessionTimeout": 900,
        "maxLifetime": 28800
      }
    }
  ]
}
```

Only `name`, `image`, and `protocol` are required. Start catalog mode with:

```sh
AGENTCORE_RUNTIME_SOURCE=catalog AGENTCORE_RUNTIME_CATALOG=./config/runtime-catalog.example.json AGENTCORE_RUNTIME_OWNER=flint-native cargo run
```

### Process-wide runtime policy

Set `FLINT_CONNECTIVITY_MODE=container` and `FLINT_DOCKER_NETWORK` when Flint
runs in a container. Flint and spawned runtimes must share that named, local
bridge network. Runtime ports are not published to the host. Special Docker
modes such as `host`, `none`, `bridge`, `default`, and `container:<id>` are
rejected, and host-gateway access is disabled.

Connectivity, environment approval, and `FLINT_RUNTIME_HEADER_ALLOWLIST` apply
to every runtime in both catalog and discovery modes. Resource limits, runtime
command controls, authentication behavior, and proxy limits use Flint's fixed
trusted-local defaults and are not runtime catalog settings.

### Trusted-local security boundary

Discovery mode intentionally enables permissive authentication and runtime
commands for local development. Runtime images and every user with access to
the Docker daemon must therefore be trusted. Mounting `/var/run/docker.sock`
grants effectively host-level Docker control. Flint is not a production
isolation boundary.

After creating the `flint-agentcore` network and loading a labeled runtime
image, the published image can be started directly:

```sh
docker run --rm --name flint --network flint-agentcore -p 127.0.0.1:35469:8080 -e AGENTCORE_RUNTIME_OWNER=flint-example -e AGENTCORE_RUNTIME_SOURCE=docker -e FLINT_CONNECTIVITY_MODE=container -e FLINT_DOCKER_NETWORK=flint-agentcore -v /var/run/docker.sock:/var/run/docker.sock ghcr.io/amebaai/flint:latest
```

For local lifecycle validation, run `mise run test:docker` for native Docker
checks or `mise run test:container` for the full discovery-based Compose path.

Published releases use the multi-platform image
`ghcr.io/amebaai/flint` and provide SemVer tags plus `latest`.

## License

Flint is available under the [GNU General Public License v3.0](LICENSE). Flint
is an independent project and is not affiliated with, sponsored by, or endorsed
by AWS.

See [CHANGELOG.md](CHANGELOG.md) for release history.
