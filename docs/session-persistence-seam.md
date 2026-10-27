# Flint logical session persistence seam

Status: characterization (pre-decision). This document describes what logical
session lifecycle persists today, how persistence failure behaves, and where the
smallest seam sits between lifecycle behaviour and SQLite. It is a starting
point for implementation or an architectural grill, not an interface
commitment. It records no decision.

The companion [`domain-model.md`](domain-model.md) describes the current model,
[`invocation-policy.md`](invocation-policy.md) the invocation policy, and
[`runtime-request-seam.md`](runtime-request-seam.md) the request seam; this
document narrows to one question: **can lifecycle behaviour be separated from
SQLite details?**

Constraint from the originating issue: preserve restart, adoption, deployment
pinning, drift handling, and cleanup behaviour.

## 1. Two vocabularies

Lifecycle and persistence do not share a state set:

| | States |
| --- | --- |
| `SessionEntry` (`session.rs:320-356`) | `Starting`, `Ready`, `Stopping`, `Stopped`, `Failed` |
| `StoredSessionState` (`session_store.rs:17-38`) | `Ready`, `Stopped` |

`Starting`, `Stopping` and `Failed` are **in-memory only**. There is no stored
representation of "starting", "stopping", or "cleanup pending", and the table
enforces this: `CHECK (state IN ('ready','stopped'))` (`session_store.rs:305`).
A crash during provisioning or a failed cleanup therefore leaves the store at
the *last successful* write, not at the lifecycle state.

## 2. The record and the table

`SessionRecord` (`session_store.rs:41-53`) is the persistence DTO: the
`SessionKey` (runtime ARN, qualifier, session id), the pin
(`catalog_generation`, `image`, `image_id`), `volume_name`, four timestamps,
`state`, `last_error`, `last_stop_token`.

The table flattens the key into three columns (`session_store.rs:294-308`),
keyed `(runtime_arn, qualifier, runtime_session_id)`, with an index on `state`.
`SCHEMA_VERSION = 1` (`session_store.rs:15`); `migrate` creates or accepts it
and rejects anything newer (`session_store.rs:285-320`).

`SessionStore` itself is a **concrete struct** wrapping
`Arc<Mutex<rusqlite::Connection>>` (`session_store.rs:55-59`). There is no
trait: `SessionManagerInner.store` is a `SessionStore` field
(`session.rs:199`), constructed by `with_store(backend, store: SessionStore,
interval)` (`session.rs:448`).

## 3. The persistence operations lifecycle requires

Six operations cover everything lifecycle does: `upsert`, `mark_ready`,
`mark_stopped`, `touch`, `load_all`, and `path` (logging only). There are
**15 production call sites** — 13 in `session.rs`, 2 in `lib.rs`:

| # | Call site | Operation | Enclosing function | Transition |
| --- | --- | --- | --- | --- |
| 1 | `session.rs:702` | `upsert` | `acquire` | reserve logical session (writes `Stopped`) |
| 2 | `session.rs:813` | `mark_ready` | `acquire` | provisioning succeeded |
| 3 | `session.rs:822` | `mark_stopped` | `acquire` | provisioning failed |
| 4 | `session.rs:789` | `touch` | `acquire` | healthy `Ready` reuse |
| 5 | `session.rs:966` | `mark_stopped` | `acquire` | stop-during-provisioning cleanup |
| 6 | `session.rs:405` | `mark_stopped` | `finish_stop` | stop cleanup succeeded |
| 7 | `session.rs:1064` | `mark_stopped` | `stop` | idempotent stop of `Stopped` |
| 8 | `session.rs:1085` | `mark_stopped` | `stop` | idempotent stop of `Failed` (no compute) |
| 9 | `session.rs:261` | `mark_stopped` | `StartingTransitionGuard::drop` | provisioning task aborted |
| 10 | `session.rs:1351` | `touch` | `reap_once` | `HealthyBusy` activity refresh |
| 11 | `session.rs:1532` | `touch` | `SessionLease::drop` | activity on lease release |
| 12 | `session.rs:596` | `upsert` | `adopt` | startup adoption (writes `Ready`) |
| 13 | `session.rs:492` | `mark_stopped` | `restore_stopped` | startup restore of a stopped record |
| 14 | `lib.rs:110` | `load_all` | `production_router` | startup load |
| 15 | `lib.rs:68` | `path` | `production_router` | startup logging |

`mark_ready` has one production caller; `load_all` one; `upsert` two; `touch`
three; `mark_stopped` seven. No other module touches the store.

## 4. Lifecycle to persistence transitions

| Lifecycle event | Persistence write |
| --- | --- |
| acquire, unknown key | `upsert` `Stopped` — reserve before compute |
| `backend.start` `Ok` | `mark_ready` |
| `backend.start` `Err` | `mark_stopped(…, Some(message), None)` |
| healthy `Ready` reuse | `touch` |
| stop cleanup succeeded (`finish_stop`) | `mark_stopped(…, None, client_token)` |
| stop cleanup failed | **no write** — the store keeps its last state |
| lease drop / `HealthyBusy` refresh | `touch` |
| reaper idle / max-lifetime / unhealthy reap | via `finish_stop` → `mark_stopped` |
| `StartingTransitionGuard::drop` (abort) | `mark_stopped(…, Some("…interrupted"), None)` |
| startup adoption | `upsert` `Ready` |
| startup restore of a stopped record | `mark_stopped` (rewrites the record's own fields) |
| startup drift quarantine | **no write** |

Two details worth noting: `update_state` does **not** touch the pin columns
(`session_store.rs:249` sets only state, `compute_started_at`, `last_activity`,
`last_error`, `last_stop_token`), and `upsert`'s `DO UPDATE` leaves
`created_at_unix_millis` unchanged (`session_store.rs:113-122`). And
`restore_stopped` (`session.rs:486-513`) rewrites the record with the values it
just read — a no-op write.

## 5. Persistence failure cases

Every store error becomes `SessionError::State` via a blanket `#[from]`
(`session.rs:1541`), mapped to **500 `InternalServerException`** with the
message `"runtime session state is unavailable"` (`edge.rs:773-776`); the raw
error, which may embed SQLite text, is only logged. Lifecycle cannot
distinguish a SQLite failure from `MissingSession`, `InvalidState`, or
`LockPoisoned`.

There are four distinct failure behaviours:

| Behaviour | Call sites | Observable result |
| --- | --- | --- |
| **Log only** | `session.rs:261`, `1351`, `1532` | in-memory state stands; the write is lost silently |
| **Park for retry** | `session.rs:405` (`finish_stop`), `813/822` and `966` (acquire cleanup) | entry becomes `Failed { container: Some(_) }` with `"session cleanup must be retried: …"`; container left running; caller sees 424 `session_stop_failed` |
| **Abort startup** | `lib.rs:110`, `session.rs:492`, `596` | `StartupError`; process does not start; an already-running adopted container is left running |
| **Surface as 500** | `session.rs:702`, `789`, `1064`, `1085` | `SessionError::State` → 500 |

Two of these are inconsistent in ways worth surfacing:

- **The `stop` idempotent arms strand the entry.** At `session.rs:1057` the
  entry is removed from the map, then `mark_stopped(...)?` runs; on error the
  `?` returns before the re-insert at `session.rs:1070`, so the entry is gone
  from `entries` while the store still holds the row. The session then becomes
  invisible to `pinned_runtime` and a later acquire provisions fresh compute.
- **A lost `touch` is invisible.** Three call sites log and continue, so a
  persistently failing store silently stops recording activity while lifecycle
  keeps reaping on its in-memory timestamps.

The acquire paths are the deliberate ones: `session.rs:825-872` distinguishes a
failed `mark_ready` (stop the container, or park it for retry) from a failed
`backend.start`, and `session.rs:960-1004` hands the cleanup error to a
concurrent `stop()` through a shared outcome.

## 6. Restart and restore

The startup sequence in `production_router` (`lib.rs:59-182`):

```text
SessionStore::open(&state_path)          lib.rs:67
DockerSessionBackend::connect_with_registry  lib.rs:80  (reconcile → adoptable)
take_adopted_sessions()                  lib.rs:104
store.load_all()                         lib.rs:110
SessionManager::with_store(...)          lib.rs:111  (also starts the reaper)
for each record:
    registry.resolve_stored(arn, qualifier)   lib.rs:117
    sessions.validate_record(deployment, record)  lib.rs:125
    on drift:  stop adopted container, restore_drifted   lib.rs:142-156
    else if adopted container for key:  sessions.adopt(...)      lib.rs:160-165
    else:  sessions.restore_stopped(...)                        lib.rs:169-171
remaining adopted entries (no record) → adopt   lib.rs:174-180
```

Adoption takes precedence over restore purely by the presence of an adopted
container; the record's `state` is not consulted. `restore_drifted` writes
nothing to SQLite — the quarantine lives in the in-memory `drifted` map.

## 7. Adoption

`DockerSessionBackend::reconcile` (`docker.rs:982-1010`) lists managed
containers, filters to owned candidates, and calls `adopt` (`docker.rs:1028`).
Adoption validates, in order: container running; required labels present;
`resolve_stored` succeeds; **the three pin labels match**
(`docker.rs:1062-1076`); endpoint and creation timestamp valid; ping healthy
(3 attempts). A mismatch is `AdoptionError::Invalid` and the container is
removed; a transient inspect failure aborts startup.

`SessionManager::adopt` (`session.rs:570-619`) then builds a `Ready` record
(`catalog_generation`/`image`/`image_id` from the deployment, `volume_name`,
`created_at = compute_started = now - container.age`), `upsert`s it, and inserts
`SessionEntry::Ready`. A duplicate registration is a `RetryableConflict`.

## 8. Deployment pinning and drift

The pin is the `catalog_generation` + `image` + `image_id` triple on the record.
`validate_record` (`session.rs:467-484`) compares it, plus the key's ARN and
qualifier, against the resolved runtime; any mismatch is
`SessionError::DeploymentDrift`. A drifted record is quarantined in
`drifted: Mutex<HashMap<SessionKey, String>>` (`session.rs:202`) by
`restore_drifted` (`session.rs:515-538`), and `acquire` checks `drifted` first
(`session.rs:661-669`). **The stored record is not modified** — the stale pin
stays on disk and the block is in memory only. Docker applies the same three
checks at adoption, so a container that does not match is never adopted.

## 9. Cleanup

A failed cleanup is `SessionEntry::Failed { container: Some(_) }`
(`session.rs:349-355`). **It is not persisted.** The store keeps the last
successful state — for a `Ready`→stop cleanup failure that is `Ready`, because
`finish_stop` writes `mark_stopped` only when `stopped.is_ok()`
(`session.rs:401-408`). The message `"session cleanup must be retried: …"` and
the parked container exist only in memory. The reaper collects
`Failed { container: Some(_) }` (`session.rs:1288-1301`) and retries through
`stop_by_key` → `finish_stop`, which finally writes `mark_stopped` with
`client_token = Some("lifecycle-reaper")` on success.

So the store cannot answer "is cleanup pending?" — only the process that parked
it can, and only until it restarts.

## 10. Where SQLite details leak

| Leak | Where |
| --- | --- |
| The store is a concrete field, not a port | `session.rs:199`, `session.rs:448` |
| Lifecycle imports store types directly | `session.rs:18` (`SessionRecord`, `SessionStore`, `SessionStoreError`, `StoredSessionState`) |
| Every persistence error collapses to one opaque variant | `session.rs:1541` (`State(#[from] SessionStoreError)`) |
| SQLite error type in the public error | `session_store.rs:393-394` (`Sqlite(#[from] rusqlite::Error)`) |
| `MissingSession` semantics never matched by lifecycle | `session_store.rs:228-234`, `261-265` |
| The filesystem path leaks into startup logging | `session_store.rs:89-91`, `lib.rs:68` |
| Failure injection is a SQLite trigger | `session_store.rs:144-155` |

Everything genuinely SQLite-specific — WAL, `synchronous`, `busy_timeout`,
`foreign_keys`, the schema, migrations, the `CHECK` — is already confined to
`session_store.rs`. The leak is not the SQL; it is the **type**, the **error
bridge**, and the **failure-injection mechanism**.

## 11. Deletion test

The issue asks to confirm lifecycle behaviour does not depend on SQLite
details. Apply the test to the store:

- **Delete the store abstraction and inline SQLite into `session.rs`.** The
  schema, migrations, WAL configuration, the `CHECK`, and `rusqlite` types would
  reappear inside the lifecycle state machine, and the 13 call sites would
  interleave SQL with transition guards. Complexity reappears; the store earns
  its keep.
- **Delete the store as a *type* and keep the SQL.** Then lifecycle would hold a
  `Connection` and every test would need a real or in-memory database. The
  current `in_memory()` (`session_store.rs:62-70`) already shows the cost: the
  only way to inject a failure is a **SQLite trigger**
  (`fail_ready_writes_for_test`, `session_store.rs:144-155`), which couples the
  test to the adapter.

So the finding is: the store's **interface** (six operations) is already the
right shape, and lifecycle behaviour does *not* depend on SQL semantics — but it
does depend on the store being a **concrete, un-fakeable type** whose errors are
opaque and whose failure injection is SQLite-specific.

## 12. The seam and adapter strategy

The seam is the six-operation boundary that already exists — `upsert`,
`mark_ready`, `mark_stopped`, `touch`, `load_all`, `path` — lifted into a
**port** that lifecycle depends on, with the SQLite store as one adapter:

```text
        SessionManager            state machine · guards · reaper · leases
              │  6 operations, typed errors
              ▼
      ┌──────────────────┐
      │  session state   │        the seam
      │  store (port)    │
      └──────────────────┘
        │              │
        ▼              ▼
   SQLite adapter   in-memory adapter
   (production)     (tests, with deterministic failure injection)
```

What this buys, mapped to the acceptance criteria:

- **Deterministic failure injection.** A failing in-memory adapter can fail a
  *named operation* (`mark_ready`, `touch`, `upsert`, …) on the *n*-th call,
  instead of installing a trigger that only covers `mark_ready`. Every one of
  the four behaviours in §5 becomes testable.
- **Typed errors.** A port error can distinguish "not found" from "unavailable"
  from "corrupt", so lifecycle can decide instead of collapsing to a 500 — which
  is what would fix the stranded-entry inconsistency in §5.
- **SQLite stays where it belongs.** WAL, migrations, the schema and the
  `CHECK` stay inside the adapter; lifecycle sees records and errors.

What must **not** move behind the seam: the state machine and its guards, the
reaper and TTL decisions, leases, drift quarantine, and adoption policy. The
port stores *facts*; it does not decide.

Whether the port is one trait or two (reads vs writes), and whether
`SessionRecord`/`StoredSessionState` are the port's types or private to the
adapter, is deliberately not decided here — §14 lists the questions.

## 13. Behaviour-preserving verification

The refactor is behaviour-preserving, so the primary evidence is that the
existing tests stay green **unchanged**:

| Concern | Existing evidence |
| --- | --- |
| Ready → stop → restore | `session.rs:1753` (restore + drift + quarantine) |
| Pinning | `session.rs:1817` (`Arc::ptr_eq` across stop) |
| Drift via HTTP | `edge.rs:1355` (424 `session_deployment_drift`, new session still 200) |
| Record written on invoke | `edge.rs:1448` (`load_all` shows the generated id) |
| Persistence failure → retry | `session.rs:2010` (`fail_ready_writes_for_test`) |
| Cleanup retry | `session.rs:2066`, `session.rs:2107`, `session.rs:2264` |
| Reaper transitions | `session.rs:2214`, `session.rs:2297` |
| Aborted tasks | `session.rs:1928` (transition guards) |
| Store round-trip | `session_store.rs:478`, `session_store.rs:438`, `session_store.rs:462` |
| Adoption | `docker.rs:2462` (`#[ignore]`, real Docker) |

New tests the port should enable — one per failure behaviour, driven through a
faulty adapter rather than a SQLite trigger:

- each of `upsert` / `mark_ready` / `mark_stopped` / `touch` fails in turn, and
  the observable result matches §5 exactly (log, park, abort, or 500);
- a failure on the `stop` idempotent arms does **not** strand the entry
  (pinning the fix, if that inconsistency is ruled a bug);
- a `touch` failure does not change reaping decisions (the in-memory timestamps
  still govern);
- restart after a failed `mark_ready` leaves the container parked and retryable;
- `load_all` failure aborts startup with the same message;
- a `Stopped` record survives a round trip through the adapter unchanged.

## 14. Open questions for the grill

1. **One port or two?** Reads (`load_all`) and writes (`upsert`, `mark_ready`,
   `mark_stopped`, `touch`) have different failure semantics. One trait, or a
   read port and a write port?
2. **Are the record types the port's or the adapter's?** `SessionRecord` and
   `StoredSessionState` currently live in the SQLite module. Do they move to the
   port, or does the adapter map to lifecycle types?
3. **Does the port own the pin?** `validate_record` compares record fields to
   the runtime. Should the port expose a "matches" query, or stay dumb and let
   lifecycle compare?
4. **Is the stranded-entry inconsistency a bug to fix here**, or out of scope
   for a behaviour-preserving refactor? It is the one place where a persistence
   failure changes in-memory behaviour.
5. **Should "cleanup pending" be persistable?** §9 shows the store cannot answer
   it. Adding a third stored state is a behaviour change, not a refactor — is it
   wanted?
6. **Drift is in-memory only.** Should a drifted record be rewritten in the
   store, or is quarantine-on-restart enough?
7. **`restore_stopped`'s no-op write** (§4) — keep it, or make restore
   read-only?

## 15. What this document does not do

It does not select an interface, name final types, or change code. Acceptance
criterion 6 — "record the outcome of review or grilling before selecting an
interface" — is deliberately unmet until a maintainer reviews this.
