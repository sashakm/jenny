# jenny

A pragmatic hexagonal webservice framework for Rust.

## The one rule

**Dependencies point inward.** The domain depends on nothing external. Infrastructure
depends on the domain, never the reverse.

jenny is a single workspace on purpose — the layering is a design rule, not a crate
boundary, and it is upheld by code review. What the framework does is make the
inward-pointing version the easy one to write: its own module tree encodes the ring
each item belongs to, so a misplaced dependency is visible in the `use` statement that
introduces it.

```
  HTTP ─┐
  gRPC ─┼─▶ adapter/inbound ──▶ use case ──▶ domain
  bus  ─┤     DTO -> command        │          entities · rules · events
  cron ─┘                           │
                                    ▼
                             outbound ports          ← traits the domain owns
                    repository · event publisher · storage
                                    │
                            adapter/outbound         ← infrastructure
                    postgres · eventbus · object store
```

## Ports are traits, adapters are impls

| Term | In jenny |
|---|---|
| **Driving (inbound) port** | a use case — usually a plain `async fn`; [`UseCase`] when a trait earns its keep |
| **Driving adapter** | `Routes` (REST), `MessagingAdapter`, `Task` — translates a protocol into a use-case call |
| **Driven (outbound) port** | a trait the domain declares: your `NoteRepository`, plus `EventPublisher`, `Authenticator`, `Authorizer` |
| **Driven adapter** | the implementation: sqlx, the event bus, JWT verification |
| **Composition root** | `JennyServer` — the only place that knows every layer |

Static dispatch is the default (`fn run<R: NoteRepository>(…)`); `Arc<dyn Trait>` is
there when the composition root wants to choose at runtime. Ports are ordinary traits,
so both work and neither is baked in.

## Project layout

The layout a jenny service is expected to have — mirrored exactly by
[`examples/basic`](examples/basic):

```
src/
  domain/
    model/          entities and value objects; invariants enforced by smart
                    constructors and types, so illegal states cannot be built
    event/          domain event definitions — no dispatch
    service/        cross-aggregate rules and fine-grained authorization
    port/           the outbound traits the domain owns
  application/      use cases; collects raised events and hands them to EventPublisher
  adapter/
    inbound/        driving: rest · grpc · messaging · task
    outbound/       driven: persistence · storage · eventbus · telemetry · security
  main.rs           composition root: hand-wired, no DI container
```

Two notes on that tree. `in`/`out` are the usual diagram labels but `in` is a Rust
keyword, so `inbound`/`outbound` it is. And there is no `domain/port/in/` — an inbound
port is a function signature until it needs to be more.

### What goes where

| Concern | Belongs in |
|---|---|
| Input **format** validation | inbound adapter — the `TryFrom<Dto> for Command` *is* the validation |
| Business / invariant validation | the domain — [`Validate`], or a newtype that cannot hold a bad value |
| Credential extraction | inbound adapter (differs per protocol) |
| Credential **verification** | driven adapter, behind [`Authenticator`] |
| Coarse endpoint authorization | inbound adapter, via [`Authorizer`] |
| Data-dependent authorization | the domain — written once, enforced on every transport |
| Protocol status mapping | inbound adapter — `DomainError` → HTTP or gRPC status |

DTOs are **per-protocol and deliberately not shared**. What makes REST, gRPC and a
message consumer interchangeable drivers is that each converts into the same domain
command — the convergence is on the command, not on a shared DTO.

## Quick start

```toml
[dependencies]
jenny = { version = "0.1", features = ["server"] }
```

A driving adapter is a function that returns routes. No trait to implement:

```rust,ignore
// adapter/inbound/rest.rs
use jenny::rest::{ApiResult, Ctx, Json, Listing, Routes, StatusCode};
use jenny::{Dto, Validate};

/// One set of constraints; `Validate` and the OpenAPI schema both come from it.
#[derive(serde::Deserialize, Dto)]
struct NoteRequest {
    #[dto(not_empty, max_len = 120, example = "Shopping")]
    title: String,
    #[dto(max_len = 10_000)]
    body: String,
}

/// The TryFrom is the input validation.
impl TryFrom<NoteRequest> for CreateNote {
    type Error = DomainError;
    fn try_from(dto: NoteRequest) -> Result<Self, Self::Error> {
        dto.validate()?;                                  // format rules
        Ok(CreateNote { title: NoteTitle::new(dto.title)?, body: dto.body })
    }
}

async fn create(
    State(notes): State<Arc<Notes>>,
    cx: Ctx,
    Json(dto): Json<NoteRequest>,
) -> ApiResult<(StatusCode, Json<Uuid>)> {
    let id = notes.create(dto.try_into()?, &cx).await?;
    Ok((StatusCode::CREATED, Json(id.as_uuid())))
}

/// `Listing` is paging, sorting and filtering, parsed and validated.
async fn list(
    State(notes): State<Arc<Notes>>,
    listing: Listing,
    cx: Ctx,
) -> ApiResult<Json<Vec<NoteResponse>>> {
    Ok(Json(notes.list(&listing, &cx).await?.into_iter().map(Into::into).collect()))
}

pub fn routes(notes: Arc<Notes>) -> Routes {
    Routes::new("notes")
        .get("/notes", list)
        .post("/notes", create)
        .get("/notes/{id}", fetch)
        .with_state(notes)
}
```

A use case takes a domain command plus the invocation [`Context`] — no envelope to
unwrap, and the same signature whether the call arrived over HTTP, off a queue, or
from the clock:

```rust,ignore
// application/notes.rs
pub async fn create(&self, cmd: CreateNote, cx: &Context) -> DomainResult<NoteId> {
    let (note, event) = Note::create(cmd.title, cmd.body);
    let id = note.id();
    self.repository.save(note).await?;      // outbound port
    self.publish(&event, cx).await;         // outbound port
    Ok(id)
}
```

And `main` is the dependency graph, readable top to bottom:

```rust,ignore
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut server = JennyServer::from_args()?;          // config + telemetry up

    let repository = Arc::new(PgNoteRepository::new(pool));      // driven adapter
    let events: Arc<dyn EventPublisher> = Arc::new(EventBus::new(bus, "my-notes"));
    let notes = Arc::new(Notes::new(repository.clone(), events)); // application

    let rest = RestAdapter::new("my-notes")?                     // driving adapter
        .mount(adapter::inbound::rest::routes(notes))?;           // mounts at /api

    server.with_version(jenny::service_version!())
        .register(rest)?
        .register_health_check(DatabaseCheck::new("notes-db", pool))
        .run()
        .await
}
```

Run the real thing:

```sh
cargo run --example basic --features server -- --config-path examples/basic/config.yaml

curl localhost:8000/j/health/ready
curl -X POST localhost:8000/api/notes -H 'content-type: application/json' \
  -d '{"title": "Shopping", "body": "milk"}'
curl 'localhost:8000/api/notes?sort=-title&filter=title:contains:shop&page_size=10'
```

[`examples/basic`](examples/basic) is the complete version — domain, use cases, both
adapter directions, a health check, and an end-to-end test that boots the actual
composition root.

## Scaffolding

```sh
cargo jenny new my-notes --features persistence,messaging
cargo jenny add resource order
```

`new` writes a project that compiles and passes `cargo test` immediately — the full
ring structure, a starter resource, `Cargo.toml`, `config.yaml`, `justfile`,
`Dockerfile`, a `compose.yaml` carrying only the services your features actually need,
and an end-to-end test that boots the real composition root.

`add resource` stubs a vertical slice across every ring — aggregate, repository port,
use cases, REST routes, tests — inserts the `mod` declarations, and prints the wiring
snippet rather than editing `main.rs`, because the composition root is the one file
worth reading top to bottom.

## What the runtime gives you

`JennyServer` loads configuration (YAML → environment → CLI), brings telemetry up,
mounts every registered adapter on one axum server, drives scheduled tasks, and shuts
down gracefully.

Its own endpoints live under `server.framework_path` (`/j`), namespaced away from
`server.api_path` (`/api`) so the framework can never collide with a route you wanted:

| Path | |
|---|---|
| `GET /j/health/live` | is the process alive — for the restart policy |
| `GET /j/health/ready` | should it get traffic — for the load balancer |
| `GET /j/health/status` | what is mounted, both versions, every dependency's state |
| `GET /j/metrics` | Prometheus scrape (`telemetry-prometheus`), optionally behind Basic auth |
| `GET /j/openapi.json` | the API description (`openapi`) |
| `GET /j/swagger` | a browsable UI, assets compiled in (`swagger-ui`) |

`/j/health/status` reports the framework version, your service's version (pass
`jenny::service_version!()`), and a check per dependency. The runtime registers checks
for what it owns — the OTLP endpoint, the in-process bus; your database pool is one
line, because reaching it otherwise would need a DI container:

```rust,ignore
server.register_health_check(DatabaseCheck::new("notes-db", pool.clone()))
```

A REST adapter mounts at `server.api_path` unless it called `.at("/v2")`, which wins —
an operator moving the convention should not silently relocate a path a developer meant
specifically.

Configuration is layered file → env (`JENNY__SERVER__ADDRESS`) → flags. The framework
reads only `server`, `telemetry` and `messaging`; your `applications.<name>` section
passes through untyped, so no schema is imposed on it.

Failures are [RFC 9457](https://www.rfc-editor.org/rfc/rfc9457) problem documents —
including 404s and 405s, so every error a client can provoke has one shape:

```json
{ "type": "urn:jenny:problem:conflict", "title": "Conflict",
  "status": 409, "detail": "note is already archived",
  "instance": "8f14e45f-ea1a-4a2c-9a1e-2d4c1f0b3a77" }
```

`instance` is the request id, which is the same id in your logs and traces. Details of
5xx failures are logged rather than sent.

## Listings

Collection endpoints take a `Listing` extractor: paging, sorting and filtering, parsed
and validated before a handler sees them.

```
GET /api/notes?page=2&page_size=20
              &sort=-created_at&sort=title
              &filter=status:eq:active&filter=title:contains:milk
```

`sort` and `filter` repeat. A sort is `field`, `-field` or `field:asc|desc`; a filter is
`field:check:value`, or `field:value` for equality, with checks `eq`, `begins`, `ends`,
`contains`, `gt` and `st`. The split is bounded, so a value may contain colons; a middle
token that is not a check is a 400 rather than a silent misparse.

**Filter paths are attacker-controlled**, so nothing executes one without an explicit
allowlist. Parsing rejects anything that is not a dotted identifier, and the persistence
translator takes the allowlist as a *required* argument — there is no API that skips it:

```rust,ignore
let columns = Columns::new([
    ("title",      "title",      ColumnKind::Text),
    ("created_at", "created_at", ColumnKind::Timestamp),
]);
let sql = SqlCriteria::build(&criteria, &columns)?;   // identifiers from `columns`,
let statement = format!("SELECT … FROM notes{}", sql.tail());  // values bound, not
                                                     // interpolated
```

## Crates

| Crate | Ring | Role |
|---|---|---|
| `jenny` | — | facade: feature-gated re-exports |
| `jenny-core` | domain + ports | `DomainError`, `Validate`, `Principal`, `Urn`, `UseCase`, `Context`, the outbound ports, tasks, telemetry |
| `jenny-rest` | driving adapter | axum/tower: `RestAdapter`, `Routes`, `Problem`, auth layers |
| `jenny-messaging` | both | `EventBus` (driven), `MessagingAdapter` (driving), NATS gateway |
| `jenny-persistence` | driven adapter | Postgres/sqlx query and mapping helpers |
| `jenny-security` | driven adapter | JWT/API-key authentication, RBAC |
| `jenny-server` | composition root | runtime, config, health, hosting |
| `jenny-macros` | — | derive macros (`Dto`, `QueryFormat`) |
| `jenny-cli` | — | `cargo jenny` scaffolding |

Features: `rest`, `server` (implies `rest`), `messaging`, `persistence`, `security`,
`openapi`, `swagger-ui`, `telemetry-otlp` / `telemetry-prometheus` / `telemetry-loki`,
`full`.

## Events

Aggregates raise events; the application layer publishes them after the state change,
through the [`EventPublisher`] port. Behind that port the in-process bus fans one event
out to many independent listeners — an audit writer, a NATS forwarder, a mailer — each
its own small outbound effect. Nothing depends on listeners, so adding one is additive.

With the `messaging` feature the runtime starts and stops the bus for you, so wiring is
two calls in `main`:

```rust,ignore
server.subscribe(AuditLogWriter::new(pool.clone()))?;   // one listener
let events = server.events()?;                          // the port, for use cases
```

Publishing after saving is two writes and can diverge on a partial failure. Start
direct; when consistency demands it, put a transactional outbox behind the same port —
write the event inside the state change's transaction and let a relay drain it. The
application layer does not change.

## Architecture decisions

[`docs/adr`](docs/adr) — start with
[ADR-001 (hexagonal layering)](docs/adr/ADR-001-hexagonal-layering.md).

## License

Licensed under either of

* Apache License, Version 2.0
   ([LICENSE-APACHE](LICENSE-APACHE) or http://www.apache.org/licenses/LICENSE-2.0)
* MIT license
   ([LICENSE-MIT](LICENSE-MIT) or http://opensource.org/licenses/MIT)

at your option.

### Contribution

Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in the work by you, as defined in the Apache-2.0 license, shall be
dual licensed as above, without any additional terms or conditions.

[`UseCase`]: https://docs.rs/jenny/latest/jenny/port/inbound/trait.UseCase.html
[`Context`]: https://docs.rs/jenny/latest/jenny/port/struct.Context.html
[`Validate`]: https://docs.rs/jenny/latest/jenny/domain/validation/trait.Validate.html
[`Authenticator`]: https://docs.rs/jenny/latest/jenny/port/outbound/trait.Authenticator.html
[`Authorizer`]: https://docs.rs/jenny/latest/jenny/port/outbound/trait.Authorizer.html
[`EventPublisher`]: https://docs.rs/jenny/latest/jenny/port/outbound/trait.EventPublisher.html
