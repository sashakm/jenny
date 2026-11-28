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
| **Driven (outbound) port** | [`Store<A>`] for aggregates (extended by your own port when the domain needs its own queries), `Blobs` for files, plus `EventPublisher`, `Authenticator`, `Authorizer` |
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
        let note = self.notes.add(note, created).raising(&self.events, cx).await?;
        Ok(note.id())
    }

    pub async fn archive(&self, id: NoteId, cx: &Context) -> DomainResult<()> {
        self.notes.change(&id, Note::archive).raising(&self.events, cx).await?;
        Ok(())
    }

    pub async fn edit(&self, id: NoteId, cmd: EditNote, expected: Option<Version>,
                      cx: &Context) -> DomainResult<()> {
        self.notes
            .change(&id, |note| note.edit(cmd.title, cmd.body))
            .expecting(expected)                // If-Match: 412 when it moved on
            .raising(&self.events, cx)
            .announcing_changes()               // also `note.updated`, with the patch
            .await?;
        Ok(())
    }
}
```

`.raising(…)` publishes what the domain method returned *with* the write. With a
Postgres store and `persistence.postgres.outbox: true`, the aggregate and its events are
committed in one transaction and a relay forwards the events to the bus, so a crash can
lose neither one without the other; any other store publishes right after the write.

The aggregate derives serde and `Aggregate`, and its value objects deserialise through
their constructors — so a store needs no mapping code, and stored data that breaks an
invariant fails to load instead of becoming a state the domain would never build.

And `main` is the dependency graph, readable top to bottom:

```rust,ignore
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut server = JennyServer::from_args(jenny::service!())?; // config + telemetry

    let store: Arc<NoteStore> = match server.postgres()? {        // driven adapter
        Some(postgres) => Arc::new(postgres.documents::<Note>().queryable(QUERYABLE)),
        None => Arc::new(Memory::<Note>::new().queryable(QUERYABLE)),
    };
    let events = Events::new(server.events()?);       // the in-process bus (`messaging`)
    let notes = Notes::new(store, events);                            // application

    let rest = RestAdapter::new("my-notes")?                     // driving adapter
        .accept_json_patch()?
        .mount(adapter::inbound::rest::routes(notes))?;           // mounts at /api

    server.register(rest)?.run().await
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
adapter directions, memory, Postgres or NATS from configuration, attachments in memory or
an object store, links between notes in memory or an AGE graph, and an end-to-end test
that boots the actual composition root (`just test-postgres`, `just test-nats`,
`just test-objects` and `just test-age` run it against each backend too).

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

## Storage

Use cases hold a `Store<A>`, and which one they get is a line in the composition root.
`Memory<A>` needs nothing; with the `persistence` feature and
`persistence.postgres.url` set, `PgDocuments<A>` keeps each aggregate as one `jsonb`
row. Both take the same allowlist, and neither needs mapping code, because the aggregate
derives serde:

```rust,ignore
const QUERYABLE: [(&str, ValueKind); 2] = [("title", ValueKind::Text), ("archived", ValueKind::Bool)];

let notes: Arc<dyn Store<Note>> = match server.postgres()? {
    Some(postgres) => Arc::new(postgres.documents::<Note>().queryable(QUERYABLE)),
    None => Arc::new(Memory::<Note>::new().queryable(QUERYABLE)),
};
```

```yaml
persistence:
  postgres:
    url: postgres://notes:secret@db/notes   # better: JENNY__PERSISTENCE__POSTGRES__URL
    max_connections: 10
    outbox: true      # events committed with their write, relayed to the bus (`messaging`)
```

The runtime does the rest before it binds its socket. It runs your migrations
(`postgres.migrate(sqlx::migrate!())`), creates the document tables under an advisory
lock so replicas can start together, and adds `postgres` to `/j/health/ready`. Every
save is a compare-and-swap on the version; listings filter, sort and page inside the
database. For data other systems query relationally, `postgres.table::<Note>("notes")`
is the same store over a table of your own with a column per field.

Services that already run NATS can keep aggregates in a JetStream key-value bucket
instead: with the `messaging` feature and `messaging.nats.url` pointing at a NATS with
JetStream enabled (`nats-server -js`), `server.nats()` hands out a connection and
`nats.kv::<Session>()` a store in the bucket `<namespace>-session`, created before the
server starts. Versions, conditional writes and listings behave exactly as in memory —
listing reads every key, so it suits hundreds of aggregates, not millions.

That connection is the service's only one: `nats.gateway("my-notes", "my-notes.>")`
forwards events to JetStream over it, with the same credentials. `messaging.nats`
secures it and says who the service is (ADR-031): a client certificate first, or a
`.creds` file, an NKey, a user and password, or a token.

```yaml
messaging:
  nats:
    url: tls://nats-1:4222,tls://nats-2:4222
    refresh_interval: 30              # seconds between looks at the files below
    tls:                              # present, even empty: TLS on every server
      ca: /etc/nats-tls/ca.crt        # default: the system's roots
      cert: /etc/nats-tls/tls.crt     # mTLS; the server maps the subject to a user
      key: /etc/nats-tls/tls.key
      # handshake_first: true         # for a server with `handshake_first`
      # server_name: nats.internal    # when the URL names an IP address
    auth:                             # at most one of:
      creds: /etc/nats/service.creds  # user JWT + NKey seed
      # nkey: …                       # JENNY__MESSAGING__NATS__AUTH__NKEY
      # user: notes                   # with password: JENNY__MESSAGING__NATS__AUTH__PASSWORD
      # token: …                      # JENNY__MESSAGING__NATS__AUTH__TOKEN
```

Certificates, keys and `.creds` are paths, read again at each connect. The runtime
looks at them every `refresh_interval` seconds and reconnects once a rotated set is
complete — the key belongs to the certificate, the `.creds` file holds a seed — so
cert-manager can renew them under a running service. Whenever `url` is set, the runtime
connects before it binds, so a server that refuses the service stops it at startup with
the server's reason. For credentials from elsewhere (Vault, a sidecar),
`server.nats_options(|options| …)` adjusts or replaces the `ConnectOptions` before
`server.nats()`.

Files are not aggregates, so they have a port of their own: `Blobs` — put (also only
if absent, or only if unchanged since an entity tag), get, list by path, delete, and
signed URLs that let a browser upload or download directly. With the `objects` feature,
`objects.url` picks the store and `server.objects()` hands it out:

```yaml
objects:
  url: s3://attachments/notes        # AWS S3, or anything S3-compatible
  options:                           # object_store's option names, over AWS_*
    endpoint: http://localhost:9000
    conditional_put: etag            # only if the service writes conditionally
```

On AWS the URL is enough; credentials come from the role, as for any AWS SDK. Over S3
the conditional writes are refused until `conditional_put: etag` turns them on: a
server that ignored their headers would overwrite instead, and nothing could tell.
`file:///var/lib/blobs` and `memory://` need no infrastructure, and
`jenny::memory::MemoryBlobs` keeps the same contract in tests. A blob that passes
through the service is bounded by `server.body_limit` (256 KiB by default); a signed URL
is the way around it for anything larger. The example keeps note attachments this way
(ADR-018).

A graph is for questions no store answers — what is connected to what — so its port is
your domain's own, and its adapter queries Apache AGE in the same Postgres. With the
`graph` feature, `postgres.graph("social")` hands out a graph the runtime creates before
serving. Queries are static Cypher: values are bound, and labels come from an
allowlist, because Cypher cannot take them as parameters. Each row decodes with serde:

```rust,ignore
let social = postgres.graph("social").identifiers(["KNOWS", "FOLLOWS"]);
let names: Vec<String> = social
    .cypher("MATCH (:Person {id: $id})-[:{{relation}}]->(f:Person) RETURN f.name")
    .bind("id", 7)
    .identifier("relation", "KNOWS")
    .fetch_all()
    .await?;
```

The example keeps the links between notes this way, or in memory without a database
(ADR-019).

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

`/j/health/status` reports the framework version, your service's name and version (what
`jenny::service!()` read from your package, unless `service_name` / `service_version`
override it), and a check per dependency. The runtime registers checks
for what it owns — the OTLP endpoint, the in-process bus, the Postgres pool it built from
`persistence.postgres`, the NATS connection from `messaging.nats`, the object store
from `objects.url`. A pool you build yourself is one line:

```rust,ignore
server.register_health_check(DatabaseCheck::new("reporting-db", pool.clone()))
```

A REST adapter mounts at `server.api_path` unless it called `.at("/v2")`, which wins —
an operator moving the convention should not silently relocate a path a developer meant
specifically.

Configuration is layered file → env (`JENNY__SERVER__ADDRESS`) → flags. The framework
reads only `server`, `telemetry`, `messaging`, `persistence` and `objects`; your
`applications.<name>` section passes through untyped, so no schema is imposed on it.

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
allowlist. Parsing rejects anything that is not a dotted identifier, and every store takes
the allowlist — each path with the `ValueKind` it holds, so `priority:gt:banana` is a 400
— before it will list anything. For a hand-written Postgres port, the SQL translator
takes it as a *required* argument; there is no API that skips it:

```rust,ignore
let columns = Columns::new("id", [                   // `id` breaks ties, so pages
    ("title",      "title",      ValueKind::Text),     // never overlap or skip rows
    ("created_at", "created_at", ValueKind::Timestamp),
]);
let sql = SqlCriteria::build(&criteria, &columns)?;   // identifiers from `columns`,
let statement = format!("SELECT … FROM notes{}", sql.tail());  // values bound, not
                                                     // interpolated
```

Every listing is paged: `page_size` defaults to 50 and may not exceed 1000, so in
Postgres a growing table never turns one request into a full scan. (The NATS store
reads every key to list; see above.)

## Crates

| Crate | Ring | Role |
|---|---|---|
| `jenny` | — | facade: feature-gated re-exports |
| `jenny-core` | domain + ports | `Aggregate`, `DomainError`, `Validate`, `Principal`, `Urn`, `Context`, `Store`, `Blobs`, `Events`, the outbound ports, `Memory`, `MemoryBlobs`, tasks |
| `jenny-rest` | driving adapter | axum/tower: `RestAdapter`, `Routes`, `Problem`, auth layers |
| `jenny-messaging` | both | `EventBus` (driven), `MessagingAdapter` (driving), the `Nats` connection (`NatsConnectionConfig`: TLS, credentials), NATS gateway, `NatsKv` store |
| `jenny-persistence` | driven adapter | Postgres: `Postgres` pool, `PgDocuments` and `PgTable` stores, `OutboxRelay`, criteria to SQL, Apache AGE graphs (`graph`) |
| `jenny-objects` | driven adapter | `ObjectBlobs`: `Blobs` over S3 and compatibles, local files or memory |
| `jenny-security` | driven adapter | JWT/API-key authentication, Cedar authorization |
| `jenny-server` | composition root | runtime, config, health, hosting; the Postgres pool, the NATS connection (`nats_options`) and the object store it builds |
| `jenny-macros` | — | `Dto`, `QueryFormat`, `Aggregate`, `DomainEvent`, `#[use_cases]` |
| `cargo-jenny` | — | `cargo jenny` scaffolding |

Features: `rest`, `server` (implies `rest`), `messaging`, `persistence`, `graph` (implies
`persistence`), `objects`, `security`, `openapi`, `swagger-ui`, `telemetry-otlp` /
`telemetry-prometheus` / `telemetry-loki`, `full`.

## Events

Aggregates raise events, and a use case hands them to the write that persists the
change — `change(…).raising(&events, cx)` — which publishes them through the
[`EventPublisher`] port. Behind that port the in-process bus fans one event
out to many independent listeners — an audit writer, a NATS forwarder, a mailer — each
its own small outbound effect. Nothing depends on listeners, so adding one is additive.

With the `messaging` feature the runtime starts and stops the bus for you, so wiring is
two calls in `main`:

```rust,ignore
server.subscribe(AuditLogWriter::new(pool.clone()))?;   // one listener
let events = server.events()?;                          // the port, for use cases
```

Saving and then publishing is two writes, and a crash between them loses the event.
With a Postgres store and `persistence.postgres.outbox: true` they are one: the events
are committed in the same transaction as the change, into `jenny_outbox`, and a relay the
runtime runs forwards them to the bus — in order, at least once, with
`FOR UPDATE SKIP LOCKED` so replicas never forward the same row (ADR-016). The use cases
do not change; any other store publishes right after the write.

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

## Roadmap

What comes next is written up as ADRs with the status *Proposed*: the need is agreed, the
design is open for discussion, and each one lists its open questions.

| ADR | What | Builds on |
|---|---|---|
| **Day 2** | | |
| [ADR-032](docs/adr/ADR-032-enriching-event-envelopes.md) | event headers filled from the request's `Context`, such as the caller's subject and actor (RFC 8693) | ADR-006, ADR-016 |
| [ADR-020](docs/adr/ADR-020-management-interface.md) | `/j` on a management listener of its own; each endpoint group configurable | ADR-007 |
| [ADR-021](docs/adr/ADR-021-protobuf-contracts.md) | Rust-first protobuf contracts from `#[derive(Dto)]`, over gRPC, binary HTTP and JSON | ADR-005, ADR-011 |
| [ADR-022](docs/adr/ADR-022-streaming-sse-and-websocket.md) | server-sent events and WebSocket | ADR-006 inbound dispatch |
| [ADR-023](docs/adr/ADR-023-credentials-signed-keys-algorithms-mtls.md) | signed API keys, a key-algorithm policy, mTLS | ADR-011 |
| [ADR-024](docs/adr/ADR-024-identity-attributes.md) | identity attributes, their sources and provenance | ADR-023 |
| [ADR-025](docs/adr/ADR-025-scopes-and-delegated-policies.md) | resource scopes and delegated Cedar policies | ADR-011, ADR-024 |
| [ADR-026](docs/adr/ADR-026-observability-day-two.md) | meters, framework instrumentation, log modes | ADR-008 |
| [ADR-027](docs/adr/ADR-027-container-image-and-helm-chart.md) | a container image and Helm charts from the scaffold | ADR-009, ADR-020 |
| [ADR-029](docs/adr/ADR-029-clustered-deployments.md), step 1 | leases, so a scheduled task runs once, not once per replica | ADR-007 |
| **After day 2** | | |
| [ADR-028](docs/adr/ADR-028-rust-frontends-with-dioxus.md) | Rust frontends with Dioxus, and a typed `jenny-client` | most of the above |
| [ADR-030](docs/adr/ADR-030-server-side-wasm-components.md) | server-side WebAssembly components | ADR-023, ADR-025 |
| **Milestone 3.0** | | |
| [ADR-029](docs/adr/ADR-029-clustered-deployments.md) | clustered deployments: Raft-replicated state, then several services | everything |

The day-2 work goes in this order: ADR-032 (ADR-031, authenticating to NATS, is built),
then ADR-020, then ADR-026 with ADR-029's leases, then
ADR-023 (its key-algorithm policy first), then ADR-027, ADR-024 and ADR-025, and finally
ADR-021 and ADR-022.

Smaller gaps in what is already built are listed at the end of each accepted ADR, under
*Not built yet*.

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
