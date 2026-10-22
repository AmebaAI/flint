# Flint invocation policy

Status: characterization (pre-decision). This document describes where
invocation policy lives today, so a maintainer can decide where to concentrate
it. It is a starting point for implementation or an architectural grill, not an
interface commitment. It records no decision.

The companion [`domain-model.md`](domain-model.md) describes the current model;
this document narrows to one question: **what is policy, what is coordination,
and where is the smallest seam between them?**

Constraint from the originating issue: separating policy from coordination must
not change externally visible invocation behaviour.

## 1. The production invocation path

The production path is `edge.rs` → `session.rs` → `proxy.rs` / `docker.rs`:

```text
POST /runtimes/{arn}/invocations
  edge::invoke_agent_runtime            edge.rs:138
    validate_runtime_headers            edge.rs:660
    runtime_session_id                  edge.rs:685
    resolve_session_runtime             edge.rs:602   (pinned deployment, else catalog)
    authorize_request                   edge.rs:207
    SessionManager::acquire             session.rs:644 (SessionLease)
    RuntimeProxy::invoke                proxy.rs:58   → forward  proxy.rs:77
      reqwest send → container POST {invocation_path}
```

`RuntimeProxy` forwards the body to the container and streams the response back;
the `SessionLease` is moved into the response stream so it lives as long as the
body (`proxy.rs:219`). Two sibling operations share the machinery: agent-card
(`proxy.rs:63`, same `forward`) and command (`edge.rs:321` →
`docker.rs::execute_command` → `command.rs::event_stream_body`).

The test-only engine under `#[cfg(test)] mod runtime` (`lib.rs:33-34`) is a
**separate** invocation model — retry, cost limits, outcome validation,
diagnostics — and is not part of production. See §8.

## 2. Policy inventory (production)

### 2.1 Retry

Production does **not** retry an invocation. Every retry loop is around
something other than the forwarded request:

| What is retried | Where | Budget |
| --- | --- | --- |
| Health ping before reusing `Ready` compute | `session.rs:751-759` | 3 attempts, 100 ms |
| Adopt ping at startup | `docker.rs:1093-1102` | 3 attempts, 250 ms |
| Container remove | `docker.rs:1500-1523` | 3 attempts, 100 ms, 404 = success |
| Abort-path container cleanup (`CreatedContainerGuard`) | `docker.rs:86-106` | 3 attempts, 100 ms |
| Discovery refresh | `docker.rs:718-766` | interval + event, 250 ms debounce |

`const MAX_CONSECUTIVE_HEALTH_FAILURES: u8 = 3` (`session.rs:21`).

### 2.2 Timeout

| Timeout | Where | Value |
| --- | --- | --- |
| Invocation duration (send + each response chunk) | `proxy.rs:116-117`, `173`, `227` | `limits.max_duration_seconds`, default 60 |
| Container startup / readiness | `docker.rs:1305-1310` (poll 100 ms at `docker.rs:1301`) | `lifecycle.startup_timeout_seconds`, default 60 |
| Health HTTP client | `docker.rs:854-857` | connect 1 s, total 2 s |
| Proxy HTTP client connect | `proxy.rs:51-54` | 2 s |
| TCP health probe (MCP) | `docker.rs:1686-1690` | 500 ms |
| Idle / maximum session lifetime | `session.rs:1340`, `session.rs:1274-1275` | 900 s / 28 800 s |
| Command execution | `docker.rs:1405-1409` | `command.timeout_seconds`, default 30 |
| Health reaper interval | `config.rs:14`, `session.rs:621-642` | `health_check_interval`, default 5 s |

Resolved as `LifecyclePolicy` (`catalog.rs:320`) and `ProxyLimits`
(`catalog.rs:412`).

### 2.3 Cancellation

One `CancellationToken` per session entry; each lease gets a child token
(`session.rs:782`). `SessionManager::stop` cancels the entry and moves it to
`Stopping` (`session.rs:1107`). The proxy observes the token in three places —
request streaming (`proxy.rs:130-138`), send (`proxy.rs:170-172`), response
streaming (`proxy.rs:222-229`) — and returns `ProxyError::Cancelled`. A
transport failure instead **invalidates the lease**
(`proxy.rs:185-187` → `session.rs:1504-1510`), which stops the compute. Dropping
a lease decrements `active_requests` (`session.rs:1513-1536`).

### 2.4 Cleanup

| Case | Behaviour |
| --- | --- |
| Explicit stop / idle / max-lifetime / repeated-unhealthy | remove container (`docker.rs:1500`), persist `Stopped` |
| Volumes | **never removed** — `remove` (`docker.rs:1500-1523`) deletes only the container; the per-session volume from `ensure_session_volume` (`docker.rs:1157-1188`) is retained |
| Stop failure | entry parked in `Failed` holding the container, message `session cleanup must be retried: …` (`session.rs:419-430`) |
| Failed-cleanup retry | reaper collects `Failed { container: Some(_) }` and retries (`session.rs:1288-1301`) |
| Startup failure | container removed; a second failure is appended to the error (`docker.rs:1316-1324`) |
| Persistence failure during acquire | container stopped, entry recorded `Failed` (`session.rs:825-872`, `960-1004`) |

### 2.5 Outcome validation

Production judges a forwarded invocation **only by HTTP status**:
`if !status.is_success()` → `ProxyError::RuntimeClient(status)` with the body
drained and deliberately not forwarded (`proxy.rs:193-204`). There is no body,
schema, or exit-code validation on the invocation path.

Compute health is judged separately, not per invocation: `http_endpoint_health`
accepts `Healthy`/`HealthyBusy` (`docker.rs:1657-1674`); MCP uses TCP readiness
(`docker.rs:1676-1695`).

### 2.6 Cost limits

**None in production.** No resolved policy carries a cost field —
`ProxyLimits`, `ResourcePolicy`, `CommandPolicy`, `LifecyclePolicy`
(`catalog.rs:298-417`) — and nothing on the invocation path observes spend. The
only cost concept is test-only (`runtime.rs:91`, `388-397`).

### 2.7 Diagnostics

- Structured `tracing` at startup (`lib.rs:68-137`), proxy failure
  (`proxy.rs:186`), Docker lifecycle (`docker.rs:103`, `399-449`, `749-757`),
  and session transitions (`session.rs:774`, `942`, `1353`, `1533`).
- `GET /_local/health` → `RuntimeHealth` (`lib.rs:229-236`): runtime source,
  count, discovery status (`ready`/`degraded`), last successful refresh, last
  error.
- Rich per-invocation diagnostics (attempts, containers removed, outcomes) are
  **test-only** (`runtime.rs:109-137`, `lib.rs:207-213`).

## 3. Coordination inventory

Coordination is concentrated in `session.rs` + `session_store.rs`:

- `SessionManagerInner` (`session.rs:197-204`): the backend, the store, the
  `entries` map behind a `Mutex`, a `drifted` map, and a generation counter.
- `SessionEntry` (`session.rs:320`): `Starting | Ready | Stopping | Stopped |
  Failed`, each carrying a `generation`, plus `active_requests`,
  `consecutive_health_failures`, timestamps and the resolved TTLs.
- Transition guards that restore state if a task is aborted
  (`StartingTransitionGuard` `session.rs:206`, `StopTransitionGuard`
  `session.rs:278`).
- The lifecycle reaper: an interval task (`session.rs:621-642`) running
  `reap_once` (`session.rs:1251-1362`).
- `Notify` for stop completion; `mpsc` channels for command events
  (`session.rs:64`) and steering; semaphores for command concurrency
  (`docker.rs:806`).
- SQLite persistence of `Ready`/`Stopped` (`session_store.rs:17-38`).
- The runtime registry behind an `RwLock` (`catalog.rs:90-93`).

`proxy.rs` participates in coordination only by holding a lease and reading its
cancellation token; `edge.rs` is stateless in production.

## 4. Seams today

**`SessionBackend` — the compute seam** (`session.rs:91-117`):

```rust
trait SessionBackend {
    fn volume_name(&self, key) -> String;
    async fn start(&self, key, runtime, cancellation) -> Result<SessionContainer, String>;
    async fn ping(&self, container) -> SessionHealth;
    async fn stop(&self, container) -> Result<(), String>;
    async fn execute_command(&self, container, command, timeout, cancellation)
        -> Result<CommandExecution, String>;
}
```

One production adapter, `DockerSessionBackend` (`docker.rs:1527`); four test
adapters (`InvocationSessionBackend` `session.rs:133`, `FakeBackend`
`session.rs:1586`, `EndpointBackend` `proxy.rs:384`, `ProxySessionBackend`
`edge.rs:1313`). The coordinator never touches bollard directly.

**`RuntimeProxy` — policy and transport fused.** `forward` (`proxy.rs:77-284`)
owns header allowlisting, the deadline, request/response/chunk limits,
cancellation, the status check, and lease invalidation, all inline with the
`reqwest` call. There is no trait.

**`ContainerRuntime` — a second, test-only seam** (`runtime.rs:52`, impl
`docker.rs:412`). Not reachable from production.

**The command path** splits differently: framing in `command.rs`, policy
(timeout, concurrency, cancellation, output limits) in
`docker.rs::execute_command`.

## 5. Deletion test

**Applied to a proposed policy module.** Delete it and does complexity vanish
or reappear? Today the *same* policy — a deadline, cancellation, byte limits,
and a success judgement — is implemented twice, once per transport: the HTTP
forward (`proxy.rs:116-204`) and Docker exec (`docker.rs:1339-1510`). Delete a
policy module that concentrates these and both implementations grow it back,
plus diagnostics that are currently implicit. It earns its keep **provided** it
holds behaviour, not constants: a module that only re-exports
`ProxyLimits`/`LifecyclePolicy` is a pass-through and fails the test.

**Applied to the existing `SessionBackend` seam.** Delete it and `SessionManager`
would call bollard directly: Docker specifics (container names, labels, volumes,
exec, ping) would move into the coordinator, and the four test adapters would
have to fake Docker instead of a compute lifecycle. Complexity reappears in the
coordinator and across the tests, so the seam earns its keep.

The consequential finding: **`SessionBackend` is a *compute* seam, not a
*policy* seam.** Its axis of variation is the compute provider; invocation
policy does not vary with the provider. Hanging policy (request limits, outcome
validation) on `SessionBackend` would force every compute adapter to know about
HTTP request policy. That is the wrong seam for policy.

## 6. The smallest seam that concentrates policy

Policy is *invariant* across operations; transport is what *varies*. The
smallest seam therefore sits **between the operation policy and the operation
transport**, above the compute seam, and already has more than one adapter:

```text
        SessionManager            coordination: lease, state machine, reaper
              │  (lease + cancellation)
              ▼
      ┌──────────────────┐
      │  attempt policy  │        deadline · limits · cancellation ·
      │  (the seam)      │        success judgement · diagnostics
      └──────────────────┘
        │            │
        ▼            ▼
   HTTP forward   Docker exec        transport adapters (≥ 2 today)
```

Two adapters exist today (HTTP forward; Docker exec), so this is a real seam,
not a hypothetical one — the codebase-design rule ("one adapter is a
hypothetical seam; two means a real one") is satisfied without inventing a
provider.

What stays **local to coordination** (and must not move behind the seam): the
lease and its lifetime, `active_requests`, the `SessionEntry` state machine and
generations, transition guards, the reaper, TTL decisions, cleanup-retry
parking, and SQLite. The seam may *report* an outcome to coordination (a
transport failure invalidates the lease, as `proxy.rs:185-187` does today) but
must not own the lease or the state.

A candidate interface is deliberately not fixed here; §8 lists the questions
that decide its shape. The characterization only asserts *where* it goes and
*why it is minimal*.

## 7. Behaviour-preserving verification

The refactor is behaviour-preserving, so the primary evidence is that the
existing tests stay green **unchanged** — they are the current behaviour's
specification. The relevant ones already exist:

| Concern | Existing evidence |
| --- | --- |
| Timeout | `session.rs` TTL tests; proxy duration is exercised via `edge.rs:1499` and `proxy.rs` tests |
| Cancellation | `proxy.rs:645` (stop cancels a slow error body); `session.rs:1872` (dropping a response releases the lease); `edge.rs` cancel test |
| Cleanup failure | `session.rs:2010`, `session.rs:2066`, `session.rs:2107`, `session.rs:2264` |
| Outcome validation | `proxy.rs:564` (non-2xx maps without leaking the body) |
| Limits | `proxy.rs:615` (streaming request limit) |
| Reuse / reprovision | `session.rs:1672`, `session.rs:1696`, `session.rs:1726`; `edge.rs:1499` |
| End-to-end SDK | `tests/aws_sdk_compat.rs`, `tests/containerized.rs` |

New tests the seam should add — one per policy concern, driven through the seam
rather than the transport:

- **retry**: assert an invocation is attempted exactly once on failure (pinning
  the *absence* of retry, so a future retry policy is a visible change);
- **timeout**: an attempt past its deadline yields the timeout outcome, and no
  second attempt;
- **cancellation**: a cancelled attempt yields the cancelled outcome and the
  transport is abandoned;
- **cleanup failure**: an attempt whose compute cannot be released reports the
  failure to coordination without silently succeeding;
- **invalid outcome**: a non-success outcome is surfaced without leaking the
  runtime body;
- **cost limits**: assert **no** cost outcome is produced today (there is no
  production cost policy), so introducing one is an explicit, test-visible
  decision.

Because the seam is an interface, these are unit tests at the seam with a fake
transport — the same technique the compute seam already uses (`FakeBackend`).

## 8. Open questions for the grill

1. **What is the test-only engine for?** `runtime.rs` holds retry, attempt
   timeout, cleanup timeout, cost limits, outcome validation and diagnostics.
   Production has none of these except timeouts. Is the intent to *retire*
   `runtime.rs`, or to *move* its policies into production? The issue's list of
   concerns implies the latter, but "without changing externally visible
   invocation behaviour" forbids adding retries or cost outcomes. Which is it?
2. **Retry semantics.** If retry is ever added, what is retryable (transport
   failure only, or a non-success outcome too), and does it re-acquire compute
   through coordination or reuse the lease? This decides whether retry sits
   above or inside the seam.
3. **Cost limits.** With no production cost policy, is the seam expected to
   host a cost outcome now (unused) or later? An unused policy path is a
   speculative seam.
4. **Outcome validation depth.** Today it is a status check. Should the seam own
   a pluggable validator (per protocol), or stay a status check and keep
   protocol semantics in the proxy?
5. **Diagnostics destination.** Tracing (production) or a bounded in-memory
   record (test-only today)? This decides whether the seam records or only
   reports.
6. **Command parity.** Should the Docker exec path adopt the same seam, or is
   parity a non-goal? Adoption is what makes the seam real rather than
   hypothetical.

## 9. What this document does not do

It does not select an interface, name types, or change code. Acceptance
criterion 5 — "record the outcome of review or grilling before selecting an
interface" — is deliberately unmet until a maintainer reviews this.
