# Flint runtime request seam

Status: characterization (pre-decision). This document describes how a request
flows through Flint today, where the broad deployment model leaks into modules
that need only a few of its facts, and where the smallest seam sits. It is a
starting point for implementation or an architectural grill, not an interface
commitment. It records no decision.

The companion [`domain-model.md`](domain-model.md) describes the current model
and [`invocation-policy.md`](invocation-policy.md) the invocation policy; this
document narrows to one question: **which facts should each module see?**

Constraint from the originating issue: the edge, authorization, catalog,
session, and proxy modules should stop learning unnecessary details of the
broad deployment model, without changing externally visible behaviour.

## 1. The request flow

Four routes under `/runtimes/{agent_runtime_arn}/…` (`edge.rs:98-116`):

| Route | Handler |
| --- | --- |
| `POST …/invocations` | `invoke_agent_runtime` (`edge.rs:138`) |
| `POST …/commands` | `invoke_agent_runtime_command` (`edge.rs:321`) |
| `POST …/stopruntimesession` | `stop_runtime_session` (`edge.rs:419`) |
| `GET …/invocations/.well-known/agent-card.json` | `get_agent_card` (`edge.rs:495`) |

Every handler repeats the same sequence:

```text
request_id()                       edge.rs:842
Query<RuntimeQuery>                qualifier, accountId
validate_runtime_headers()         edge.rs:660
runtime_session_id()               edge.rs:685   (required for command)
resolve_session_runtime()          edge.rs:602   → pinned_runtime, else resolve_runtime
authorize_request()                edge.rs:563   → auth::authorize
SessionManager::acquire()          session.rs:644  (invoke / command / card)
RuntimeProxy::invoke()             proxy.rs:58   → forward  proxy.rs:77
```

`resolve_session_runtime` (`edge.rs:602`) consults
`SessionManager::pinned_runtime` (`session.rs:540`) before the catalog, so an
existing session's pinned deployment wins over a refreshed catalog; only on a
miss does it call `resolve_runtime` (`edge.rs:635`) → `RuntimeRegistry::resolve`
(`catalog.rs:142`) → `RuntimeCatalog::resolve` (`catalog.rs:1021`).

The resolved value is `ResolvedRuntime` (`catalog.rs:189-214`) — **24 fields**
spanning four unrelated concerns:

| Concern | Fields |
| --- | --- |
| Identity | `runtime_arn`, `runtime_id`, `account_id`, `qualifier` |
| Image | `image`, `image_id`, `image_platform`, `image_entrypoint`, `image_command`, `image_environment`, `image_working_directory` |
| Policy | `protocol`, `environment`, `environment_warnings`, `resources`, `command`, `lifecycle`, `allowed_custom_headers`, `authentication`, `policy`, `connectivity`, `limits` |
| Pin | `catalog_generation` |

## 2. Request facts

The facts a request carries, where each is parsed, and who consumes it:

| Fact | Parsed at | Consumed by |
| --- | --- | --- |
| method | `parts.method` | `auth::authorize` (canonical request `auth.rs:347`) |
| URI / path | `OriginalUri`, `Path(runtime_identifier)` | `authorize` (canonical URI `auth.rs:348`); resolution |
| `qualifier`, `accountId` | `Query<RuntimeQuery>` (`edge.rs:120`) | resolution (`catalog.rs:1061`); pin check (`session.rs:552`) |
| session id | `runtime_session_id` (`edge.rs:685`) | `SessionKey` (`session.rs:24`); forwarded (`proxy.rs:104`); echoed |
| runtime user id | `authorize_request` (`edge.rs:572`) | policy context (`auth.rs:207`); forwarded (`proxy.rs:95`) |
| request id | `request_id` (`edge.rs:842`) | error and success headers |
| `Authorization` | `auth::local_identity` (`auth.rs:58`) | SigV4 verification (`auth.rs:125`) |
| `x-amz-date`, `x-amz-content-sha256`, `x-amz-security-token` | `auth.rs:450` | SigV4 verification |
| body | `to_bytes` or streaming `Body` | signature hash (`auth.rs:156`); JSON parse; forward |
| trace headers | `edge.rs:42` | echoed; forwarded (`proxy.rs:97`) |
| MCP headers | `edge.rs:40` | forwarded for MCP only (`proxy.rs:108`) |

A partial request-facts type already exists: `AuthorizationRequest`
(`auth.rs:43-50`) carries `method`, `uri`, `headers`, `body`, `action`,
`runtime_user_id`. It is the only named grouping of request facts today.

## 3. What each consumer actually reads

The deployment is threaded as `&ResolvedRuntime` / `Arc<ResolvedRuntime>`; the
number of its fields each consumer *reads* is small:

| Consumer | Fields read | # |
| --- | --- | --- |
| `edge::stop_runtime_session` | `limits.max_request_bytes` | 1 |
| `edge::get_agent_card` | `protocol` | 1 |
| `edge::invoke_agent_runtime` | `authentication.mode`, `limits.max_request_bytes` | 2 |
| `edge::invoke_agent_runtime_command` | `limits.max_request_bytes`, `command.enabled` | 2 |
| `session::SessionKey::new` | `runtime_arn`, `qualifier` | 2 |
| `session::stop` | `runtime_arn`, `qualifier` | 2 |
| `proxy::forward` | `protocol`, `allowed_custom_headers`, `limits` | 3 |
| `auth::authorize` | `authentication`, `runtime_arn`, `qualifier`, `policy` | 4 |
| `session::pinned_runtime` | `qualifier`, `account_id`, `runtime_arn`, `runtime_id` | 4 |
| `session::validate_record` | `runtime_arn`, `qualifier`, `catalog_generation`, `image`, `image_id` | 5 |
| `session::adopt` | `catalog_generation`, `image`, `image_id`, `lifecycle` ×2 | 5 |
| `session::acquire` | `runtime_arn`, `qualifier`, `catalog_generation`, `image`, `image_id`, `lifecycle` ×2 | 7 |
| `docker::DockerSessionBackend` | image, entrypoint, env, working dir, lifecycle, command, connectivity, resources, protocol, identity, generation, warnings | ~20 |

**Only `DockerSessionBackend` legitimately reads most of the model** — it is the
one module that builds a container from it. Every other consumer reads 1–7
fields. Three edge helpers read **none**: `authorize_request` (`edge.rs:563`),
`resolve_session_runtime` (`edge.rs:602`) and `resolve_runtime` (`edge.rs:635`)
are pure pass-throughs that forward the whole deployment.

## 4. Where the model leaks

1. **Pass-through signatures.** `authorize_request`, `resolve_session_runtime`
   and `resolve_runtime` take `&ResolvedRuntime` and read no field of it. Their
   signatures advertise a dependency that does not exist.
2. **Wide signatures for narrow reads.** `stop` needs two identity fields but
   takes the deployment; `get_agent_card` needs `protocol` alone.
3. **A per-request clone of all 24 fields.**
   `RuntimeCatalog::resolve` does `(**template).clone()` (`catalog.rs:1072`)
   then overwrites only `runtime_arn`, `account_id`, `qualifier`
   (`catalog.rs:1073-1076`).
4. **Loose request facts.** Each handler re-threads `headers`, `method`, `uri`,
   `body`, `request_id`, `session_id`, `runtime_user_id` as separate arguments,
   and `authorize_request` re-derives `runtime_user_id` from the headers
   (`edge.rs:572`) that the handler already holds.
5. **Duplicated sequence.** The parse → validate → resolve → authorize → acquire
   order is written four times, once per handler, with small variations
   (streaming vs buffered body, session id required or optional).

## 5. Pinning and drift

Not a leak, but the reason the whole deployment must survive the boundary:

- **The pin is `Arc<ResolvedRuntime>` on `SessionEntry`** (`session.rs:320-356`),
  exposed by `SessionEntry::runtime()` (`session.rs:359`).
  `pinned_runtime` returns the stored `Arc` (`session.rs:564`) and `acquire`
  swaps the stored runtime in for a `Stopped` entry (`session.rs:690-696`).
  Tests assert pointer identity, so the pin must stay a shared handle.
- **Drift keys** are `catalog_generation` plus `image`/`image_id`
  (`session.rs:472-476`); `DockerSessionBackend` compares container labels at
  adoption (`docker.rs:1062-1076`).
- **Ownership.** `session.rs` owns pinning and drift validation; `catalog.rs`
  owns generation; `docker.rs` observes at adoption; `edge.rs` only consults.
- `DockerSessionBackend.deployments` also retains `Arc<ResolvedRuntime>` per
  container (`docker.rs:805`) for post-adoption ping and exec.

## 6. Error mapping

All HTTP mapping is in `edge.rs`: `map_proxy_error` (`edge.rs:735`),
`map_session_error` (`edge.rs:771`), `resolve_runtime`'s `CatalogError` mapping
(`edge.rs:646`), and inline `AuthorizationError` → 403 (`edge.rs:595`,
`edge.rs:611`). Errors from four modules converge on one type, `RuntimeApiError`
(`edge.rs:847`). The mapping consumes error *values*, never the deployment —
another sign the deployment need not cross those boundaries.

## 7. Deletion test

**Applied to a proposed request-context module.** Imagine a module that a
handler builds once and passes down. Delete it and does complexity vanish or
reappear?

- A context that is only a **struct of fields** — method, uri, headers, body,
  ids, plus the deployment — is a **data bag**. Delete it and the fields become
  parameters again; nothing is lost. It fails the deletion test and should not
  be built.
- A context that **owns the parse-once sequence** — generate the request id,
  default or require the session id, validate headers, extract the runtime user
  id, resolve *or* pin the deployment — earns its keep: delete it and that
  sequence, currently written four times (§4.5), reappears in all four handlers.
  It passes **only if it hides the sequence, not merely the field list**.

**Applied to the broad deployment model.** Delete `ResolvedRuntime` and every
consumer would need the raw catalog plus the resolution rules; the type is
essential and stays. The leak is not the type's existence but the **width of
what crosses the boundary**: 24 fields cross into consumers that read 1–7.

## 8. The seam

The seam sits at the **edge-to-consumer boundary**: between "the request has
been resolved" and "each module consumes it". Today that boundary is crossed by
a wide deployment value and by loose request facts; the proposal is that it be
crossed by a context that hands each consumer **only the view it needs**.

```text
              edge handler
                   │  builds once: request facts + resolved pin
                   ▼
        ┌───────────────────────┐
        │  request context      │   parse · validate · resolve · pin
        │      (the seam)       │
        └───────────────────────┘
          │        │        │
          ▼        ▼        ▼
      identity   policy   image        narrow views
          │        │        │
     session    auth/proxy  docker
```

Views are named by the axis each consumer varies along, not by the module:

| View | Fields | Consumers |
| --- | --- | --- |
| identity | `runtime_arn`, `runtime_id`, `account_id`, `qualifier` | `SessionKey`, `stop`, `pinned_runtime` |
| image + pin | `catalog_generation`, `image`, `image_id` | `validate_record`, `adopt`, drift |
| authorization | `authentication`, `runtime_arn`, `qualifier`, `policy` | `auth::authorize` |
| forwarding | `protocol`, `allowed_custom_headers`, `limits` | `proxy::forward` |
| compute | the full model | `DockerSessionBackend` |

Two properties the seam must preserve, both from §5:

- the pin stays a shared `Arc<ResolvedRuntime>` (pointer identity is asserted);
- the pin wins over a refreshed catalog, so resolution stays where
  `resolve_session_runtime` is today.

What must **not** move behind the seam: `SessionManager`'s state machine,
leases, the reaper and persistence; `DockerSessionBackend`'s container
construction; and `edge.rs`'s `RuntimeApiError` mapping.

Whether this is **one** context or **two** narrower things — a request-facts
context and a set of deployment views — is deliberately not decided here; §10
lists the questions that decide it.

## 9. Behaviour-preserving verification

The refactor must not change observable behaviour, so the primary evidence is
that the existing tests stay green **unchanged**:

| Concern | Existing evidence |
| --- | --- |
| Permissive requests | `edge.rs:1081` (invoke by ARN and by id), `edge.rs:1499` |
| Signed requests | `edge.rs:1223` (real signature accepted, wrong secret denied) |
| Policy / user id | `edge.rs:1264` (per-user invoke permission) |
| Session reuse | `session.rs:1672`, `edge.rs:1499` (one container across two invokes) |
| Pinned deployment | `session.rs:1817` (`Arc::ptr_eq` across stop), `edge.rs:1355` (drift) |
| Streaming | `proxy.rs:615` (request limit), `proxy.rs:688` (drop releases lease) |
| Error mapping | `edge.rs:1118`, `edge.rs:1178`; `proxy.rs:564` (no body leak) |
| Header forwarding | `proxy.rs:503` (credentials and hop-by-hop not forwarded) |
| Restore / adopt / drift | `session.rs:1753`, `catalog.rs:1694` |

New tests the seam should add — one per fact, driven through the context rather
than the handler:

- a handler cannot observe a deployment fact its view does not expose
  (compile-time where possible);
- resolving twice in one request yields the same pin, and the pin wins over a
  catalog refresh mid-request;
- the request id and session id are generated once and are identical in the
  response header and the forwarded request;
- a body buffered for authorization is the same bytes forwarded (no re-read);
- an error from any module maps to the same status and code as today.

Because the seam is a boundary, these are unit tests at the seam plus the
existing end-to-end tests (`tests/aws_sdk_compat.rs`, `tests/containerized.rs`).

## 10. Open questions for the grill

1. **One context or two?** Request facts (method, uri, headers, body, ids) and
   deployment views are different axes. A single type risks the data bag §7
   warns about; two types risk two things to thread. Which?
2. **Does the context own resolution?** If `resolve_session_runtime` moves into
   the context, the pin-vs-catalog precedence moves with it — good locality, but
   it pulls the `SessionManager` into the context's construction.
3. **Views by module or by axis?** The table above groups by axis; a
   module-shaped view (`AuthorizationView`, `ForwardView`) is the alternative.
   Which reads better at the call site?
4. **The per-request clone** (`catalog.rs:1072`). Is narrowing worth a
   `RuntimeTemplate` that clones only identity, or is the clone immaterial at
   this scale?
5. **Streaming body.** The permissive fast path authorizes with `&[]`
   (`edge.rs:171`) while the buffered path authorizes the real bytes. Does the
   context model "authorized without the body" explicitly, or is that a policy
   detail left where it is?
6. **`SessionBackend::start`** takes `Arc<ResolvedRuntime>`. Is the compute view
   a new type, or does the backend keep the whole model (it is the one consumer
   that needs it)?

## 11. What this document does not do

It does not select an interface, name final types, or change code. Acceptance
criterion 6 — "record the outcome of review or grilling before selecting an
interface" — is deliberately unmet until a maintainer reviews this.
