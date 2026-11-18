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
                       store · event publisher · storage
                                    │
                            adapter/outbound         ← infrastructure
                    postgres · eventbus · object store
```

## Ports are traits, adapters are impls

| Term | In jenny |
|---|---|
| **Driving (inbound) port** | a use case — a method on a plain struct, observed by `#[jenny::use_cases]` |
| **Driving adapter** | `Routes` (REST), `MessagingAdapter`, `Task` — translates a protocol into a use-case call |
| **Driven (outbound) port** | [`Store<A>`] for aggregates (extended by your own port when the domain needs its own queries), plus `EventPublisher`, `Authenticator`, `Authorizer` |
| **Driven adapter** | the implementation: an in-memory or database store, the event bus, JWT verification |
| **Composition root** | `JennyServer` — the only place that knows every layer |

Use cases hold their ports as `Arc<dyn Trait>`. Every port is `#[async_trait]`, which
boxes each call's future whatever the dispatch, so a generic parameter would buy one
saved vtable lookup and cost a type parameter on every handler.

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
  application/      use cases; load, change, save through Store, publish what was raised
  adapter/
    inbound/        driving: rest · grpc · messaging · task
    outbound/       driven: persistence · storage · eventbus · telemetry · security
  main.rs           composition root: hand-wired, no DI container
```

Two notes on that tree. `in`/`out` are the usual diagram labels but `in` is a Rust
keyword, so `inbound`/`outbound` it is. And there is no `domain/port/in/` — an inbound
port is a method signature, and nothing more is needed.

### What goes where

| Concern | Belongs in |
|---|---|
| Input **format** validation | inbound adapter — the `TryFrom<Dto> for Command` *is* the validation |
| Business / invariant validation | the domain — [`Validate`], or a newtype that cannot hold a bad value |
| Credential extraction | inbound adapter (differs per protocol) |
| Credential **verification** | driven adapter, behind [`Authenticator`] |
| Coarse endpoint authorization | inbound adapter, via [`Authorizer`] |
| Data-dependent authorization | the domain, via `Context::authorize` — written once, enforced on every transport |
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
    State(notes): State<Notes>,
    cx: Ctx,
    Json(dto): Json<NoteRequest>,
) -> ApiResult<(StatusCode, Json<Uuid>)> {
    let id = notes.create(dto.try_into()?, &cx).await?;
    Ok((StatusCode::CREATED, Json(id.as_uuid())))
}

/// `Listing` is paging, sorting and filtering, parsed and validated.
async fn list(
    State(notes): State<Notes>,
    listing: Listing,
    cx: Ctx,
) -> ApiResult<Json<Vec<NoteResponse>>> {
    Ok(Json(notes.list(&listing, &cx).await?.iter().map(Into::into).collect()))
}

pub fn routes(notes: Notes) -> Routes {
    Routes::new("notes")
        .get("/notes", list)
        .post("/notes", create)
        .get("/notes/{id}", fetch)
        .with_state(notes)
}
```

A use case takes a domain command plus the invocation [`Context`] — no envelope to
unwrap, and the same signature whether the call arrived over HTTP, off a queue, or
from the clock. `Store::change` is load, apply a domain method, save with a version
check, in one call:

```rust,ignore
// application/notes.rs
#[derive(Clone)]
pub struct Notes {
    notes: Arc<dyn Store<Note>>,
    events: Events,
}

#[jenny::use_cases]                         // a span and a duration metric per method
impl Notes {
    pub async fn create(&self, cmd: CreateNote, cx: &Context) -> DomainResult<NoteId> {
        let (note, created) = Note::create(cmd.title, cmd.body);
        let note = self.notes.insert(note).await?;
        self.events.publish(&created, cx).await;
        Ok(note.id())
    }

    pub async fn archive(&self, id: NoteId, cx: &Context) -> DomainResult<()> {
        let archived = self.notes.change(&id, Note::archive).await?;
        self.events.publish(&archived.outcome, cx).await;
        Ok(())
    }
}
```

The aggregate derives serde and `Aggregate`, and its value objects deserialise through
their constructors — so a store needs no mapping code, and stored data that breaks an
invariant fails to load instead of becoming a state the domain would never build.

And `main` is the dependency graph, readable top to bottom:

```rust,ignore
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut server = JennyServer::from_args()?;          // config + telemetry up

    let store = Arc::new(Memory::<Note>::new().queryable(QUERYABLE)); // driven adapter
    let events = Events::new(server.events()?);       // the in-process bus (`messaging`)
    let notes = Notes::new(store.clone(), events);                    // application

    let rest = RestAdapter::new("my-notes")?                     // driving adapter
        .mount(adapter::inbound::rest::routes(notes))?;           // mounts at /api

    server.with_version(jenny::service_version!())
        .register(rest)?
        .register_health_check(store)
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

### Two REST modes

An adapter is plainly representational unless told otherwise: `PUT` replaces, `PATCH` is
`405`. With `.accept_json_patch()?` it also takes RFC 6902 JSON Patch on every route that
has a `GET` and a `PUT` — derived from those two, so there is no `PATCH` handler to write.
A patch is applied to what `GET` returns and handed to `PUT`, so it gets the same
validation and use case; changing a field the `PUT` ignores is `422`, not a silent no-op.

Writes are conditional when a client wants them to be. `Versioned` sends a store version
as the `ETag`, `IfMatch` hands `If-Match` to the use case, and the store's
compare-and-swap refuses a stale write with `412`:

```rust,ignore
async fn update(State(notes): State<Notes>, Path(id): Path<Uuid>,
                IfMatch(expected): IfMatch, cx: Ctx, Json(dto): Json<NoteRequest>)
    -> ApiResult<StatusCode>
{
    notes.edit(NoteId::from_uuid(id), dto.try_into()?, expected, &cx).await?;
    Ok(StatusCode::NO_CONTENT)
}
```

## Scaffolding

```sh
cargo install cargo-jenny
cargo jenny new my-notes --features persistence,messaging
cargo jenny add resource order
```

`new` writes a project that compiles and passes `cargo test` immediately — the full
ring structure, a starter resource, `Cargo.toml`, `config.yaml`, `justfile`,
`Dockerfile`, a `compose.yaml` carrying only the services your features actually need,
and an end-to-end test that boots the real composition root.

`add resource` stubs a vertical slice across every ring — aggregate, store port, use
cases, REST routes, tests — inserts the `mod` declarations, and prints the wiring
snippet rather than editing `src/lib.rs`, because the composition root is the one file
worth reading top to bottom.

## What the runtime gives you

`JennyServer` loads configuration (YAML → environment → CLI), brings telemetry up,
mounts every registered adapter on one axum server, drives scheduled tasks, and shuts
down gracefully on `SIGTERM` or `SIGINT`: readiness turns 503 first, the listener stays
open for `server.shutdown_delay_seconds` so a load balancer can catch up, then in-flight
requests and scheduled runs get `server.shutdown_timeout_seconds` to finish.

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
  "instance": "urn:uuid:8f14e45f-ea1a-4a2c-9a1e-2d4c1f0b3a77" }
```

`instance` is the request id: the same id as the response's `x-request-id`, the request
span in your logs and traces, and `Context::request_id()`. An inbound `x-request-id`
UUID is kept, so a gateway's id survives the hop. A failed `Validate` lists each issue in
an `errors` member (`[{ "field": "title", "message": "must not be empty" }]`). Details of
5xx failures, with their whole cause chain, are logged rather than sent.

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
let columns = Columns::new("id", [                   // `id` breaks ties, so pages
    ("title",      "title",      ColumnKind::Text),    // never overlap or skip rows
    ("created_at", "created_at", ColumnKind::Timestamp),
]);
let sql = SqlCriteria::build(&criteria, &columns)?;   // identifiers from `columns`,
let statement = format!("SELECT … FROM notes{}", sql.tail());  // values bound, not
                                                     // interpolated
```

Every listing is paged: `page_size` defaults to 50 and may not exceed 1000, so a
growing table never turns one request into a full scan.

## Crates

| Crate | Ring | Role |
|---|---|---|
| `jenny` | — | facade: feature-gated re-exports |
| `jenny-core` | domain + ports | `Aggregate`, `DomainError`, `Validate`, `Principal`, `Urn`, `Context`, `Store`, `Events`, the outbound ports, `Memory`, tasks |
| `jenny-rest` | driving adapter | axum/tower: `RestAdapter`, `Routes`, `Problem`, auth layers |
| `jenny-messaging` | both | `EventBus` (driven), `MessagingAdapter` (driving), NATS gateway |
| `jenny-persistence` | driven adapter | Postgres/sqlx query and mapping helpers |
| `jenny-security` | driven adapter | JWT/API-key authentication, Cedar authorization |
| `jenny-server` | composition root | runtime, config, health, hosting |
| `jenny-macros` | — | `Dto`, `QueryFormat`, `Aggregate`, `DomainEvent`, `#[use_cases]` |
| `cargo-jenny` | — | `cargo jenny` scaffolding |

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

Every envelope has its own id and can be correlated with the request that raised it
(`EventEnvelope::json(…)?.correlated_with(&cx)`). The bus carries both as message
headers, and the NATS forwarder passes them on, the id as `Nats-Msg-Id` so JetStream
drops a replayed duplicate.

## Authorization

The `Authorizer` port asks one question, *may this principal do this action on this
resource*, and `jenny-security` answers it with [Cedar](https://www.cedarpolicy.com)
policies. The REST `Authorization` layer asks it for every request, about the matched
route template inside its resource and adapter, never about a path pattern:

```cedar
permit (principal in Role::"Dispatch", action in Action::"read",
        resource in Resource::"urn:shop:orders");

permit (principal, action == Action::"archive", resource is Note)
  when { resource.owner == principal.id };
```

The second rule depends on the note, so the use case asks it once the note is loaded,
and it holds on every transport:

```rust,ignore
cx.authorize(&AccessRequest::new("archive",
    Resource::new("Note", note.id()).with_attribute("owner", note.owner()))).await?;
```

`CedarAuthorizer::from_path("policies/")?.with_schema_file("schema.cedarschema")?`
validates every policy at startup, so a misspelled type fails the deployment.
[`docs/data/cedar`](docs/data/cedar) has a schema for the HTTP gate and an example
policy set; ADR-011 has the reasoning.

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

[`Store<A>`]: https://docs.rs/jenny/latest/jenny/port/trait.Store.html
[`Context`]: https://docs.rs/jenny/latest/jenny/port/struct.Context.html
[`Validate`]: https://docs.rs/jenny/latest/jenny/domain/validation/trait.Validate.html
[`Authenticator`]: https://docs.rs/jenny/latest/jenny/port/outbound/trait.Authenticator.html
[`Authorizer`]: https://docs.rs/jenny/latest/jenny/port/outbound/trait.Authorizer.html
[`EventPublisher`]: https://docs.rs/jenny/latest/jenny/port/outbound/trait.EventPublisher.html
