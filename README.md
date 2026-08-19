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

Set an owner name and start Flint with
[`compose.example.yml`](compose.example.yml):

```sh
export AGENTCORE_RUNTIME_OWNER=flint-quickstart
export FLINT_SESSION_ID=10000000-0000-0000-0000-000000000001

docker compose \
  -f compose.example.yml \
  up --build --wait
```

Call Flint's AgentCore Runtime API route:

```sh
curl --fail-with-body \
  --request POST \
  --header 'content-type: application/json' \
  --header "x-amzn-bedrock-agentcore-runtime-session-id: ${FLINT_SESSION_ID}" \
  --data '{"prompt":"hello"}' \
  'http://127.0.0.1:35469/runtimes/example/invocations?accountId=000000000000'
```

This local request is unsigned, so Flint uses region `us-east-1` and account
`000000000000`.

The example runtime returns:

```json
{"message":"hello from flint-runtime-example"}
```

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

Flint discovers local Docker images with an
`ai.ameba.flint.runtime.descriptor` label. The example image uses the smallest
valid descriptor:

```json
{
  "name": "example",
  "protocol": "HTTP"
}
```

The runtime name becomes its local runtime ID. Every runtime has one qualifier,
`DEFAULT`.

AgentCore fixes the port and routes for each protocol. Flint does not let image
metadata override them.

| Protocol | Port | Invocation | Health | Agent card |
| --- | ---: | --- | --- | --- |
| `HTTP` | 8080 | `POST /invocations` | `GET /ping` | None |
| `MCP` | 8000 | `POST /mcp` | TCP readiness | None |
| `A2A` | 9000 | `POST /` | `GET /ping` | `GET /.well-known/agent-card.json` |
| `AGUI` | 8080 | `POST /invocations` | `GET /ping` | None |

The image must bind its protocol port on `0.0.0.0`. Flint accepts images built
for the local Docker host, even though hosted AgentCore requires ARM64. HTTP
JSON and SSE are supported; WebSocket proxying at `/ws` is not.

## Configuration

Docker discovery is the default. Images must already exist in the local Docker
daemon; Flint never pulls them. The main settings are:

- `AGENTCORE_RUNTIME_OWNER`: required, and should stay stable across restarts.
- `AGENTCORE_RUNTIME_IMAGES`: optional comma-separated image allowlist.
- `FLINT_CONNECTIVITY_MODE` and `FLINT_DOCKER_NETWORK`: use `container` and a
  named local bridge when Flint runs in Docker.
- `FLINT_RUNTIME_ENV_ALLOWLIST`: host variable names that runtime descriptors
  may request. Missing, empty, and unapproved values are not forwarded.
- `FLINT_RUNTIME_HEADER_ALLOWLIST`: custom request headers Flint may forward.

For file-based configuration, set `AGENTCORE_RUNTIME_SOURCE=catalog` and use
[`config/runtime-catalog.example.json`](config/runtime-catalog.example.json).
Catalog entries use the descriptor fields plus `image`.

## Identity and sessions

Unsigned requests use region `us-east-1` and account `000000000000`. Signed
requests take their region from the SigV4 scope. A 12-digit access-key ID becomes
the account ID; other access keys use the local default. Flint rejects an ARN or
`accountId` that conflicts with the derived identity.

Lifecycle values use `idleRuntimeSessionTimeout` and `maxLifetime`. Both accept
60 to 28,800 seconds and default to 900 and 28,800 seconds respectively. Flint
models AgentCore microVM lifecycles, not AgentCore Instances.

A running session keeps its resolved image and configuration across discovery
refreshes. Invalid refreshes leave the last valid catalog active and report
degraded health at `/_local/health`.

## Security boundary

Flint allows permissive local authentication and runtime commands. Only use
runtime images you trust. Anyone who can access the mounted Docker socket has
host-level Docker control.

Runtime containers run without published ports, host-gateway access, or extra
Linux capabilities. These limits reduce accidents, but they do not make Flint a
production isolation boundary.

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
