# ADR-002: Workspace Decomposition and Feature Gating

**Status:** Accepted
**Date:** 2026-08-24

## Context

ADR-001 says a jenny *service* is one crate. The *framework* is a different question: it
ships a domain core, several transport adapters, persistence helpers, security
mechanisms and a runtime. A service that exposes REST and nothing else should not
compile sqlx, `async-nats` and `jsonwebtoken` to get there.

So the framework is a workspace, and the split is drawn along the rings from ADR-001
rather than along topics.

## Decision

### The crates

| Crate | Ring | Contains |
|---|---|---|
| `jenny-core` | domain + ports | `Aggregate`, `DomainError`, `DomainEvent`, `Changes`, `Validate`, `Principal`, `Credentials`, `Urn`, `Label`, `Criteria`; `Store`/`StoreExt`, `Events` with `Headers`/`Enrich` (`Caller`, `Source`), `Context` and its inbound extensions, `PrincipalKind`, `EventPublisher`, `Authenticator`, `Authorizer`, `Lease`/`HeldLease`/`TaskLease`; `Memory`, `Queryable`, `Blobs`, `MemoryBlobs`, `MemoryLease`; tasks; `service!()` |
| `jenny-rest` | driving adapter | axum/tower: `RestAdapter` (plain or JSON Patch mode), `Routes`, `Problem`, `Ctx`, `Versioned`/`IfMatch`, auth and CORS layers |
| `jenny-messaging` | both | `EventBus` (driven), `MessagingAdapter`, the `inbound` dispatch core and `consumer::JetStreamConsumer` (driving), NATS gateway, `NatsKv` store, `NatsLease` |
| `jenny-persistence` | driven adapter | Postgres: the pool, `PgDocuments` and `PgTable` stores, `PgLease`, the outbox and its relay, criteria to SQL, Apache AGE graphs (feature `graph`) |
| `jenny-objects` | driven adapter | `ObjectBlobs`: the `Blobs` port over `object_store` (S3 and compatibles, files, memory) |
| `jenny-security` | driven adapter | JWT/JWKS and API-key authentication, Cedar authorization |
| `jenny-server` | composition root | runtime, the service identity, configuration, telemetry, the framework's endpoint groups (`server.endpoints`) on the API or a management listener (`server.management`), hosting; bus handlers, JetStream consumers, system principals for tasks and handlers, task leases (`tasks.lease`, `lease()`) |
| `jenny-macros` | — | `#[derive(Dto)]`, `#[derive(QueryFormat)]`, `#[derive(Aggregate)]`, `#[derive(DomainEvent)]`, `#[use_cases]` |
| `cargo-jenny` | — | `cargo jenny` — scaffolding (ADR-009) |
| `jenny` | — | facade of feature-gated re-exports |

### The dependency graph

```
jenny (facade)
├── jenny-core                      (always)
├── jenny-macros                    (always; proc-macro only)
├── jenny-rest        → jenny-core
├── jenny-messaging   → jenny-core
├── jenny-persistence → jenny-core, jenny-macros
├── jenny-objects     → jenny-core
├── jenny-security    → jenny-core
└── jenny-server      → jenny-core, jenny-rest
                       → jenny-messaging   (feature `messaging`)
                       → jenny-persistence (feature `persistence`)
                       → jenny-objects     (feature `objects`)

cargo-jenny                         (standalone binary; depends on none of the above)
```

Every arrow points at `jenny-core` or at `jenny-server`. Two properties fall out of
that, and both are load-bearing:

- **No adapter depends on another adapter.** `jenny-rest` does not depend on
  `jenny-persistence` or `jenny-security`. It converts `DomainError` into HTTP
  statuses; `jenny-persistence` converts its own failures into `DomainError`. The two
  meet in the core and never see each other. An inbound adapter that knew about an
  outbound one would be an arrow pointing outward.
- **Only `jenny-server` depends on more than the core.** It is the composition root, and
  ADR-001 grants it exactly that privilege.

`jenny-core` keeps a deliberately small dependency set — `tokio`, `tokio-util`,
`futures`, `async-trait`, `serde`, `serde_json`, `json-patch`, `uuid`, `chrono`,
`thiserror`, `anyhow`, `tracing`, `cron` and `bytes`. No web framework, no
database driver, no broker client, and no exporter: telemetry setup (the OTel SDK,
`tracing-subscriber`, the backends) lives in `jenny-server`, the only crate that starts it.

### Features

```toml
default = []
rest         = ["dep:jenny-rest"]
server       = ["dep:jenny-server", "rest"]     # the runtime hosts REST adapters
messaging    = ["dep:jenny-messaging", "jenny-server?/messaging"]
persistence  = ["dep:jenny-persistence", "jenny-server?/persistence"]
graph        = ["persistence", "jenny-persistence/graph"]
objects      = ["dep:jenny-objects", "jenny-server?/objects"]
security     = ["dep:jenny-security"]
openapi      = ["jenny-macros/openapi", "jenny-rest?/openapi", "jenny-server?/openapi"]
swagger-ui   = ["openapi", "jenny-server?/swagger-ui"]
telemetry-otlp / telemetry-prometheus / telemetry-loki   # each implies `server`
full         = ["rest", "server", "messaging", "persistence", "graph", "objects", "security", "openapi"]
```

Nothing is on by default. Compile time and binary size stay proportional to what a
service actually uses. `openapi` is opt-in because `utoipa` is a real dependency;
`swagger-ui` is separate again because its vendored assets cost roughly two megabytes of
binary, which is a different order of decision.

### Every combination is compiled, not just `--all-features`

`just features` builds each feature set on its own. This is not ceremony: `--all-features`
cannot catch a `#[cfg]` that only holds when two features are enabled together, and the
telemetry backends were broken in exactly that way — with only `telemetry-otlp` on, the
subscriber composition was gated on `all(otlp, loki)`, so the OTLP layers were built and
then dropped. It compiled and tested green under `--all-features` while exporting
nothing for anyone who enabled one backend. Two neighbouring bugs (`telemetry-loki`
alone, and `telemetry-prometheus` alone) did not even compile.

### The facade mirrors the rings

`jenny` re-exports `domain`, `port` and `task` as modules, plus the
everyday items flat so ordinary code reads `use jenny::{Context, DomainError};`.
Adapters appear as `jenny::rest`, `jenny::messaging`, … behind their features.

Both paths exist on purpose: `jenny::domain::DomainError` when the ring matters to the
reader, `jenny::DomainError` when it does not.

`jenny-rest` additionally re-exports `axum` and `tower_http`, so a service that wants
`axum::extract::Path` or a `tower_http` layer does not have to add and version-match
those crates itself.

### Every crate is published, on one version

crates.io only accepts dependencies it can resolve, so the facade can ship only if the
crates behind it do. All of them are published, and all on the same version:
`[workspace.package] version` is the one number, and release-plz bumps it as a single
`version_group`. Services depend on `jenny` alone; the other crates are on crates.io
because the facade needs them there, not as separate entry points, and their API is
whatever the facade re-exports. One version also means `cargo-jenny` can scaffold
against its own version number and be right.

### Not created yet

- **`jenny-grpc`.** The design is settled — ADR-005 chose axum precisely so a tonic
  adapter can share middleware with REST — but nothing is built. It gets its own ADR
  when work starts.

## Consequences

- A REST-only service depends on `jenny` with `features = ["server"]` and never
  compiles a database driver.
- The rule "no adapter depends on another adapter" is a reviewable property of the
  manifests, not just of the source.
- `jenny-macros` stays internal; `#[derive(Dto)]` reaches users through the facade and
  `#[derive(QueryFormat)]` through `jenny-persistence`.
- `cargo-jenny` depends on none of the framework crates. It emits text, and coupling the
  generator to the thing it generates would mean a breaking change in the framework
  breaks the tool that is supposed to help you adopt it.
- Adding a transport means adding a crate that depends on `jenny-core` and a feature on
  the facade. Nothing existing changes.
