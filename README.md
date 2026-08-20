# Flint

> Lights the fire without the Firecracker.

Flint runs AWS Bedrock AgentCore Runtime containers on your local Docker host.
Point an AgentCore client at Flint, invoke a labeled runtime image, and test the
same request flow without deploying to AWS.

Flint is built for trusted local development. It is not a production sandbox.

## Quickstart

You need Docker with Docker Compose. Start by building the included TypeScript
HTTP runtime image:

```sh
docker build \
  --tag flint-runtime-example \
  examples/runtime
```

The label in [`examples/runtime/Dockerfile`](examples/runtime/Dockerfile) names
this runtime `example`. Flint uses that name as the runtime ID, so the request
below targets `/runtimes/example/invocations`.

Start Flint with [`compose.example.yml`](compose.example.yml):

```sh
docker compose \
  -f compose.example.yml \
  up --build --wait
```

Call Flint's AgentCore Runtime API route and capture the generated session ID:

```sh
FLINT_HEADERS=$(mktemp)

curl --fail-with-body \
  --dump-header "$FLINT_HEADERS" \
  --request POST \
  --header 'content-type: application/json' \
  --data '{"prompt":"hello"}' \
  'http://127.0.0.1:35469/runtimes/example/invocations?accountId=000000000000'

FLINT_SESSION_ID=$(
  awk -F ': *' \
    'tolower($1) == "x-amzn-bedrock-agentcore-runtime-session-id" {
       sub(/\r$/, "", $2)
       print $2
     }' \
    "$FLINT_HEADERS"
)
rm "$FLINT_HEADERS"
test -n "$FLINT_SESSION_ID"
```

The first response includes the new counter value:

```json
{"message":"hello from flint-runtime-example","count":1}
```

This local request is unsigned, so Flint uses region `us-east-1` and account
`000000000000`. Like AWS, Flint accepts either a full runtime ARN or a runtime ID
with an `accountId`; this example uses the readable runtime ID form. Because the
request omits the session header, Flint creates a logical session and returns its
ID in the `x-amzn-bedrock-agentcore-runtime-session-id` response header.

Use the captured ID to invoke the same logical session again:

```sh
curl --fail-with-body \
  --request POST \
  --header 'content-type: application/json' \
  --header "x-amzn-bedrock-agentcore-runtime-session-id: ${FLINT_SESSION_ID}" \
  --data '{"prompt":"hello again"}' \
  'http://127.0.0.1:35469/runtimes/example/invocations?accountId=000000000000'
```

The second response has `"count":2`. The example stores this counter in the
session's `/workspace` volume.

Kill its current container to simulate lost compute:

```sh
FLINT_CONTAINER_ID=$(
  docker ps \
    --quiet \
    --filter 'label=agentcore.emulator.managed=true' \
    --filter "label=agentcore.emulator.runtime-session-id=${FLINT_SESSION_ID}"
)
test -n "$FLINT_CONTAINER_ID"
docker kill "$FLINT_CONTAINER_ID"
```

Invoke the same session again:

```sh
curl --fail-with-body \
  --request POST \
  --header 'content-type: application/json' \
  --header "x-amzn-bedrock-agentcore-runtime-session-id: ${FLINT_SESSION_ID}" \
  --data '{"prompt":"after restart"}' \
  'http://127.0.0.1:35469/runtimes/example/invocations?accountId=000000000000'

FLINT_REPLACEMENT_ID=$(
  docker ps \
    --quiet \
    --filter 'label=agentcore.emulator.managed=true' \
    --filter "label=agentcore.emulator.runtime-session-id=${FLINT_SESSION_ID}"
)
test -n "$FLINT_REPLACEMENT_ID"
test "$FLINT_REPLACEMENT_ID" != "$FLINT_CONTAINER_ID"
```

Flint replaces the container, remounts the session volume, and returns
`"count":3`. A request without the session header creates a different logical
session with a new volume, so its first response starts again at `"count":1`.
Without an explicit kill or stop, healthy idle compute remains running until the
default 900-second idle timeout.

Flint starts the `example` container on the first request and forwards the body
to the container's `POST /invocations` endpoint. AgentCore SDK clients can use
`http://127.0.0.1:35469` as their endpoint URL and call the same API route.

Stop the runtime session before shutting Flint down:

```sh
curl --fail-with-body \
  --request POST \
  --header "x-amzn-bedrock-agentcore-runtime-session-id: ${FLINT_SESSION_ID}" \
  'http://127.0.0.1:35469/runtimes/example/stopruntimesession?accountId=000000000000'

docker compose -f compose.example.yml down
```

## Runtime images

Flint discovers local Docker images with a runtime protocol label:

```dockerfile
LABEL ai.ameba.flint.runtime.protocol="HTTP"
```

The optional runtime name label overrides the image-name default:

```dockerfile
LABEL ai.ameba.flint.runtime.name="example"
```

Optional runtime labels configure requested environment variables and lifecycle
values:

```dockerfile
LABEL ai.ameba.flint.runtime.environment-variables="MODEL,API_KEY" \
    ai.ameba.flint.runtime.lifecycle.idle-runtime-session-timeout="900" \
    ai.ameba.flint.runtime.lifecycle.max-lifetime="28800"
```

The environment-variable label is a comma-separated list. Missing optional
labels use Flint's defaults. If the name label is absent, Flint uses the final
repository component of the image name, without its tag or digest, as the local
runtime ID. For example, `ghcr.io/acme/my-runtime:latest` becomes
`my-runtime`.

Every runtime has one qualifier, `DEFAULT`.

AgentCore fixes the port and routes for each protocol. Flint does not let image
metadata override them.

| Protocol | Port | Invocation | Health | Agent card |
| --- | ---: | --- | --- | --- |
| `HTTP` | 8080 | `POST /invocations` | `GET /ping` | None |
| `MCP` | 8000 | `POST /mcp` | TCP readiness | None |
| `A2A` | 9000 | `POST /` | `GET /ping` | `GET /.well-known/agent-card.json` |
| `AGUI` | 8080 | `POST /invocations` | `GET /ping` | None |

The health column describes the container endpoint Flint polls internally. It is
not exposed as a `/runtimes/{runtime}/ping` data-plane route. Use
`GET /_local/health` to check Flint itself.

The image must bind its protocol port on `0.0.0.0`. It must also create the
configured session storage path and make it writable by UID and GID `10001`.
Flint accepts images built for the local Docker host, even though hosted
AgentCore requires ARM64. HTTP JSON and SSE are supported; WebSocket proxying at
`/ws` is not.

## Configuration

Docker discovery is the default. Images must already exist in the local Docker
daemon; Flint never pulls them. The main settings are:

- `AGENTCORE_RUNTIME_OWNER`: Docker resource ownership namespace, default
  `flint`. Keep it stable across restarts, and override it when multiple Flint
  instances share one Docker daemon.
- `AGENTCORE_RUNTIME_IMAGES`: optional comma-separated image allowlist.
- `FLINT_CONNECTIVITY_MODE` and `FLINT_DOCKER_NETWORK`: use `container` and a
  named local bridge when Flint runs in Docker.
- `FLINT_RUNTIME_ENV_ALLOWLIST`: host variable names that runtime descriptors
  may request. Missing, empty, and unapproved values are not forwarded.
- `FLINT_RUNTIME_HEADER_ALLOWLIST`: custom request headers Flint may forward.
- `FLINT_STATE_PATH`: SQLite state path. Native runs default to the XDG state
  directory. The Compose example uses `/var/lib/flint/flint.sqlite3`.
- `FLINT_SESSION_STORAGE_MOUNT_PATH`: persistent session volume mount, default
  `/workspace`.
- `FLINT_HEALTH_CHECK_INTERVAL_SECONDS`: control-plane health interval, default
  5 seconds.

For file-based configuration, set `AGENTCORE_RUNTIME_SOURCE=catalog` and use
[`config/runtime-catalog.example.json`](config/runtime-catalog.example.json).
Catalog entries use the descriptor fields plus `image`; `name` is optional and
defaults to the final repository component of the image name.

## Identity and sessions

Unsigned requests use region `us-east-1` and account `000000000000`. Signed
requests take their region from the SigV4 scope. A 12-digit access-key ID becomes
the account ID; other access keys use the local default. Flint rejects an ARN or
`accountId` that conflicts with the derived identity.

Lifecycle values use `idleRuntimeSessionTimeout` and `maxLifetime`. Both accept
60 to 28,800 seconds and default to 900 and 28,800 seconds respectively. Flint
models AgentCore microVM lifecycles, not AgentCore Instances.

Flint stores logical session IDs and deployment pins in SQLite. It creates an ID
when an invocation omits the session header and returns that ID in the response.
The session remains valid after idle shutdown, maximum lifetime, an unhealthy
container, or `StopRuntimeSession`.

Each session has a persistent Docker volume mounted at `/workspace` by default.
When compute stops, Flint keeps the volume. The next invocation starts a fresh
container with the same session ID, immutable deployment pin, and volume. If
the current catalog no longer matches that pin, Flint blocks only that session
from resuming. Flint continues serving new sessions from the current catalog and
never stores runtime environment values or credentials in SQLite.

For HTTP, A2A, and AG-UI runtimes, Flint polls the fixed `GET /ping` endpoint.
`HealthyBusy` keeps background work alive. `Healthy` compute stops after its idle
timeout. Three unreachable, malformed, or unsuccessful health responses stop the
compute. MCP continues to use TCP readiness because its contract has no health
route.

Session volumes are not deleted automatically. Remove them from Docker manually
when their data is no longer needed. Invalid catalog refreshes leave the last
valid catalog active and report degraded health at `/_local/health`.

## Security boundary

Flint allows permissive local authentication and runtime commands. Only use
runtime images you trust. Anyone who can access the mounted Docker socket has
host-level Docker control.

Runtime containers run without published ports, host-gateway access, or extra
Linux capabilities. Their persistent session volume is writable by the runtime
and is not size-bounded by Flint. These limits reduce accidents, but they do not
make Flint a production isolation boundary.

## Development

Flint uses [mise](https://mise.jdx.dev/) for its Rust toolchain and project tasks:

```sh
mise install
mise run check
mise run test:docker
mise run test:container
mise run publishLocal
```

The Docker tests require a running Docker daemon. See
[CONTRIBUTING.md](CONTRIBUTING.md) before sending a change and
[SECURITY.md](SECURITY.md) for vulnerability reporting.

## License

Flint is available under the [GNU General Public License v3.0](LICENSE). It is
an independent project and is not affiliated with, sponsored by, or endorsed by
AWS.
