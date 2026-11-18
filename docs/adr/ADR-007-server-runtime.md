# ADR-007: The Server Runtime and Composition Root

**Status:** Accepted — with `persistence`, the runtime also builds, prepares and checks a Postgres pool (ADR-015), and stops the outbox relay after the HTTP drain and before the broker, closing the pool last (ADR-016)
**Date:** 2026-08-24

## Context

Every jenny service needs the same startup: read configuration, bring telemetry up, build
the driven adapters, wire them into use cases, hand those to the driving adapters, expose
health endpoints, run scheduled work, serve, and shut down cleanly. Without a runtime,
every service reassembles that by hand and each one gets a slightly different subset
right.

ADR-001 also identified exactly one place allowed to know every layer. That place needs a
name and a shape.

## Decision

### `JennyServer` is the composition root

```rust,ignore
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut server = JennyServer::from_args()?;                   // config + telemetry

    let store = Arc::new(Memory::<Note>::new());                  // driven adapter
    let events = Events::new(server.events()?);                   // the in-process bus
    let notes = Notes::new(store.clone(), events);                // application

    let rest = RestAdapter::new("my-notes")?                      // driving adapter
        .mount(adapter::inbound::rest::routes(notes))?;

    server.with_version(jenny::service_version!())
        .register(rest)?
        .register_health_check(DatabaseCheck::new("notes-db", pool))
        .run()
        .await
}
```

Read it top to bottom and you have the entire dependency graph. Every arrow points
inward, and if something is not wired here it does not exist.

### No injection container

`ServerContext` hands adapters their configuration section and any shared resources
**while wiring**, and then has no further role. After `main` returns, use cases own their
`Arc`s outright.

This is the difference from a DI container and it is the point: a missing dependency is a
compile error or a startup failure in one readable function, not a runtime surprise on
the first request that needs it. A container buys convenience in exchange for moving
wiring errors from compile time to run time and making the graph something you trace
rather than read. For a hand-wired binary that trade is not worth taking.

### Configuration: file, then environment, then flags

```
config.yaml  →  JENNY__SERVER__ADDRESS  →  --address
```

That order is the deployment order: a file for defaults you commit, environment variables
for what differs per environment, flags for what differs per run.

The framework interprets exactly three sections — `server`, `telemetry` and `messaging`.
Everything else passes through untyped — an adapter deserialises its own `applications.<name>` section
into whatever type it likes. **The framework imposes no schema on service configuration**,
because it cannot know one and a wrong guess would be in the way forever.

An explicitly named config file that is missing is an error; the conventional
`config.yaml` simply may not exist.

### Bind and serve are separate steps

`bind()` assembles the router and binds the socket without serving. `ServerParts` then
exposes `local_addr()`, a `ServerHandle` for graceful shutdown, and `serve()`.

The reason is testability: binding port `0` and asking the listener what it got removes
the fixed-port race that otherwise makes integration tests flaky under parallel runs. The
example service's end-to-end test does exactly this, against the same `build` function
`main` uses — so what is tested is the real composition root rather than a rebuilt
approximation of it.

`run()` is `bind().serve()` plus a handler for `SIGINT` and, on Unix, `SIGTERM`. The
generated Dockerfile runs the service as PID 1, where the kernel installs no default
handler, so without an explicit `SIGTERM` handler `docker stop` and Kubernetes waited out
their grace period and killed the process: no drain, no flush.

### Shutdown is a sequence, not a switch

On a signal or `ServerHandle::shutdown`:

1. `/health/ready` answers 503 with `"status": "stopping"`; `/health/live` stays up.
2. The server keeps accepting for `server.shutdown_delay_seconds` (default `0`). On
   Kubernetes the pod leaves the load balancer *while* it is told to stop, not before,
   so a few seconds of delay (around five) keeps late requests from being refused.
3. The listener closes; in-flight requests finish. The scheduler starts no more runs and
   waits for those in progress.
4. Both are bounded together by `server.shutdown_timeout_seconds` (default `30`, what
   Kubernetes waits before `SIGKILL`); what is still running then is abandoned.
5. The broker stops (draining its listeners) and telemetry flushes.

### The framework's endpoints get their own prefix

Health, metrics and OpenAPI live under `server.framework_path`, `/j` by default, while
REST adapters mount under `server.api_path`, `/api`. Two namespaces rather than one, so
the framework can never claim a route the service wanted — and so an operator can move
either without touching code. `/j` is short because it appears in every probe URL in
every manifest.

### Health is three endpoints, not one

| Path | Question | Asked by |
|---|---|---|
| `GET /j/health/live` | is the process alive? | the restart policy |
| `GET /j/health/ready` | should it get traffic? | the load balancer |
| `GET /j/health/status` | what is running in here? | a human |

The live/ready split is the one that matters operationally. A service whose database is
down is *live* — restarting will not help — but not *ready*. Conflating them turns a
dependency outage into a restart loop.

Readiness is the server's started flag plus the registered `HealthCheck`s, each under a
2-second timeout, because a probe that hangs is worse than one that fails. With no checks
registered, ready means started. Checks run **concurrently**: they are independent I/O,
and six two-second timeouts in series would make a readiness probe take twelve seconds to
fail.

`/j/health/status` also reports the framework's version and the service's own. The
framework cannot read the latter for you — `env!` expands where it is written, which for
framework code is the framework's manifest — so `jenny::service_version!()` is a macro
that expands in *your* binary.

### What checks itself, and what you register

The runtime auto-registers checks for the things it owns: the OTLP endpoint it was
configured with (a TCP reachability probe) and the in-process event bus it started.

It does **not** auto-register your database pool, and this is where the no-container
decision has a visible cost. Reaching the pool would need either a DI container or a
`sqlx` dependency in the runtime. Both are worse than one line in `main`:

```rust,ignore
server.register_health_check(DatabaseCheck::new("notes-db", pool.clone()))
```

So the framework ships ready-made checks in the crates that own the resources —
`jenny-persistence`, `jenny-messaging` — and the runtime merely aggregates them. Being
honest about that boundary is better than magic that works for a pool and mysteriously
does not for anything else.

### The metrics endpoint can be locked

`server.metrics.auth` adds HTTP Basic authentication to the scrape endpoint. Off by
default, because in most deployments the endpoint is only reachable from inside the
cluster; available because that stops being true at some point in every cluster's life.
See ADR-005 for the comparison and redaction details.

### Cross-cutting layers apply to everything

Body limit, tracing and the problem-shaped 404/405 fallbacks are applied above every
route, including the fallbacks themselves, so a 404 has the same shape as any other
failure (ADR-005).

Outermost of all is `RequestIdLayer`. It keeps an inbound `x-request-id` that is a UUID
(so a gateway's correlation id survives the hop) and mints one otherwise, then puts that
one id everywhere: the request span, the `Context` a handler extracts, the response's
`x-request-id`, and the `instance` (`urn:uuid:…`) of every problem document produced
while serving the request, through a task-local that `Problem` reads. Before, `Ctx`
minted a fresh id on each extraction when no header came in, and `instance` was never
set at all.

The body limit is enforced when a handler *reads* the body, which is axum's semantics: a
handler that ignores the body will answer a request that carries an oversized one. Worth
knowing, and documented on the setting.

### Scheduled tasks are a driving adapter

`ScheduledTask` is a cron-scheduled `Task`, driven by the runtime as a background tokio
task. A `Task` implementation should be as thin as a REST handler — the clock is just
another thing that invokes a use case. Each firing is spawned separately, so a slow run
never delays another task's schedule, and a failed run is logged without stopping the
schedule. A firing that comes due while the previous run of the same task is still going
is skipped with a warning, not run beside it.

Slots are wall-clock times but delays are monotonic: the scheduler reads the wall clock
once and advances it with tokio's clock. A timer that fires a moment early can therefore
never yield the same slot twice, slots missed while the process was suspended are
skipped rather than fired in a burst, and a next slot computed a moment too late is due
now instead of an error that used to end the whole scheduler.

## Consequences

- Every jenny service has the same binary shape, and the example is a working template
  for it.
- Integration tests boot the real service on an ephemeral port and shut it down cleanly.
- `jenny-server` is the only crate depending on more than `jenny-core`, which is exactly
  the privilege ADR-001 grants the composition root.
- Wiring is manual. That is the intended cost: more lines in `main`, in exchange for a
  graph you can read.
