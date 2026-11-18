# ADR-012: Aggregates, Versioned Stores and Use Cases

**Status:** Accepted — implemented
**Date:** 2026-11-18

## Context

The application layer was the most verbose part of a jenny service, and measuring the
scaffolded resource showed where it came from:

- **Generic ports spread outward.** `Notes<R, P: ?Sized>` made every REST handler
  `async fn x<R: NoteRepository, P: EventPublisher + ?Sized>`, and every route
  registration a turbofish. It bought nothing: the ports are `#[async_trait]`, which
  boxes every call's future under static dispatch too.
- **No shape shared by every use case**, so every cross-cutting concern was written per
  method: a 25-line `publish` helper (encode, correlate, log), `.ok_or_else(not_found)`,
  load → change → save → publish spelled out for every command. It is also why the
  `jenny.usecase.*` metrics of ADR-008 were never built: there was nowhere to put them.
- **Storage code in the wrong ring.** Each resource carried a 40-line in-memory query
  engine in `application/`, and a 130-line in-memory repository whose only job was
  mapping an aggregate to a row type and back.

Three requests on the roadmap need the same missing piece. JSON Patch support is a
read-modify-write over HTTP, which loses concurrent updates without a version to check.
Short command use cases need load-change-save written once. And "concrete support" for
Postgres, a document store and a key-value store needs a shared interface for adapters
to implement, or each backend can only ship helpers. All three need the framework to
know what an aggregate is and which version of it a caller last saw.

ADR-004 rejected a generic `Repository<T, Id>`, kept serde out of the domain, and made
static dispatch the default; ADR-003 kept a `UseCase` trait for when indirection is
needed. This ADR revises all four.

## Decision

### `Store<A>`: identity and versions, nothing else

`jenny_core::port::Store<A: Aggregate>` is the one storage port the framework ships:

```rust,ignore
#[async_trait]
pub trait Store<A: Aggregate>: Send + Sync + 'static {
    async fn find(&self, id: &A::Id) -> DomainResult<Option<Loaded<A>>>;
    async fn insert(&self, aggregate: A) -> DomainResult<Loaded<A>>;     // Conflict if present
    async fn save(&self, aggregate: Loaded<A>) -> DomainResult<Loaded<A>>; // compare-and-swap
    async fn remove(&self, id: &A::Id) -> DomainResult<A>;                 // NotFound if absent
    async fn list(&self, criteria: &Criteria) -> DomainResult<Vec<A>>;
}
```

It is not the repository ADR-004 rejected, for three reasons:

- **Domain queries keep the domain's language.** A domain that needs
  `archived_before(cutoff)` declares `trait NoteRepository: Store<Note>` in
  `domain/port/` and holds an `Arc<dyn NoteRepository>`; trait upcasting (stable since
  Rust 1.86) lets the same value be used as a `dyn Store<Note>`. The generic part covers
  only what is identical for every aggregate.
- **Load and save by identity is the repository's collection semantics**, not storage
  vocabulary leaking in. There is no `update_where`, no column, no query builder.
- **The version is what the generic trait does that hand-written ports did not.** Every
  write is conditional on the version the aggregate was read at, in every adapter, so a
  slow writer can never silently undo a fast one. That is not optional: the failure it
  prevents is silent data loss.

The version lives in `Loaded<A>`, which a store hands out and takes back. It dereferences
to the aggregate and never enters the domain model. A `version` field on every aggregate
was the alternative. It would put a storage concern into every domain type, and returning
a `(A, Version)` pair would make every caller thread the version through by hand.

`list` takes `Criteria`, and each adapter enforces its own allowlist, as ADR-010
requires of anything that turns client-supplied paths into a query.

### `StoreExt::change` is the command use case

```rust,ignore
let archived = self.notes.change(&id, Note::archive).await?;
self.events.publish(&archived.outcome, cx).await;
```

`change` loads, applies a domain method, and compares the aggregate's encoding before and
after. It saves only when something changed, so repeating a request writes nothing and
leaves the version where it was. `Changed<A, T>` carries what the method returned
(usually the event it raised), the new `Loaded<A>`, and the difference as `Changes`, an
RFC 6902 JSON Patch.

`Changed::updated()` turns that patch into a `<kind>.updated` event (`{ id, version,
changes }`) for listeners that mirror state, such as an audit log, a search index or a
read model, rather than react to what the domain said happened. The patch is computed
over the stored form, so it costs nothing extra and needs no comparison code per
aggregate.

A lost race surfaces as `Conflict`. Retrying inside `change` was considered and left out:
it would need `Fn` rather than `FnOnce`, and it would hide contention the caller may want
to see.

### Serde on aggregates and events

Aggregates and domain events derive `Serialize`/`Deserialize`. This reverses ADR-004's
"serde at both boundaries and in neither centre" for those two kinds of type.

The reason is the turn-key goal. A generic store needs an encoding for every aggregate,
and the encoding written by hand was a row type plus two conversions per aggregate per
store. Each event also needed its own `EventPayload` impl. That mapping was the bulk of
the remaining boilerplate, and it would have multiplied with every backend.

Invariants survive it. Value objects deserialise through their smart constructors
(`#[serde(try_from = "String", into = "String")]`), so stored data that breaks a rule
fails to load, and `Loaded::decode` reports that as `Internal`: the data is corrupt, which
is not the caller's fault. That is the same guarantee the hand-written `TryFrom<Row>`
gave.

The cost is real and accepted. Renaming an aggregate field changes the stored document
and the event payload, and the change shows up in the domain's diff rather than in an
adapter's. A service that must decouple its storage shape can still write its own
adapter with its own mapping, since `Store` is only a trait. A `Codec<A>` seam with
hand-written mappings as the default was considered and rejected: about 30 lines per
aggregate, in exchange for flexibility the trait already allows.

Serde is the only exception. `sqlx::FromRow`, `utoipa::ToSchema` and their like stay out
of `domain/`.

### Use cases are methods; `#[use_cases]` observes them

The `UseCase` trait is deleted. Nothing used it outside its own documentation. A
concrete, non-generic use-case struct already lets every driving adapter share it without
erasure, and the decorator role the trait was kept for now belongs to the attribute below.

Ports are held as `Arc<dyn …>`. This reverses the "static dispatch is the default" of
ADR-003 and ADR-004, for the reason in the context: `async_trait` boxes either way, and a
generic parameter costs a type parameter on everything that touches the use case.

`#[jenny::use_cases]` on an inherent `impl` block wraps the body of every `pub async fn`
in it:

```rust,ignore
jenny::port::use_case::__private::observe("notes.archive", Some(cx.request_id()), async move {
    let __jenny_outcome: DomainResult<()> = { /* the body, unchanged */ };
    __jenny_outcome
}).await
```

The span is `use_case`, with `otel.name`, `use_case`, `request_id` (taken from a `&Context`
parameter when there is one) and `outcome`, which is `"ok"` or `DomainError::kind()`. The
annotated binding keeps `?` and `return` converting exactly as they did. Private and
synchronous methods are helpers and are left alone; `#[use_case(skip)]` and
`#[use_case(name = …)]` adjust single methods.

The macro generates no trait, struct, port or registration, so ADR-001's rule that
macros do not hide boundaries holds. It is the same kind of thing as
`#[tracing::instrument]`, and the dependency graph is still the struct's fields.

Alternatives considered:

| Alternative | Why not |
|---|---|
| Use-case parameters resolved by type (axum/Bevy style) | Needs a type-keyed provider, which is a DI container |
| A command bus dispatching on the command type | Runtime dispatch hides the wiring |
| A declarative `resource! { … }` generating use cases and routes | Generates adapters; `cargo jenny` already covers "generate it once" |
| An aggregate framework in the cqrs-es style | Brings event sourcing with it |
| A `#[use_case]` attribute turning functions into structs | Gives up the grouping a service struct gives CRUD, and multiplies wiring in `main` |

### `jenny.usecase.duration`

Each finished use case reports to a process-wide `UseCaseObserver`, installed with
`observe_use_cases`. jenny-server's telemetry installs one once a meter provider exists.
It records a `jenny.usecase.duration` histogram in seconds, with the bucket boundaries
OpenTelemetry recommends for HTTP durations and the attributes `use_case` and `outcome`.

This replaces the `jenny.usecase.total` / `errors` / `duration` trio ADR-008 designed.
Counts and error rates fall out of the histogram's count per outcome, and one instrument
cannot disagree with itself.

A hook rather than the OpenTelemetry API in jenny-core keeps the core free of an
observability dependency, and it is independent of log filtering. A metrics bridge
reading `tracing` events would have lost measurements whenever `RUST_LOG` filtered the
event out. A process-wide hook is the same arrangement as the `tracing` subscriber: being
measured is the backend's business, not something each use case is handed.

### The smaller pieces

- `#[derive(Aggregate)]` takes the identity from the field named `id`, or one marked
  `#[aggregate(id)]`. The kind defaults to the type name in snake case.
- `#[derive(DomainEvent)]` names an event after its type (`NoteCreated` →
  `note.created`), or after `#[event(name = …)]`.
- `Events` wraps `Arc<dyn EventPublisher>`. `publish(&event, cx)` encodes as JSON,
  correlates with the request, and logs a failure rather than returning it, since the
  state change it reports has already been committed (ADR-004's dual-write note stands).
- `jenny_core::memory::Memory<A>` is an in-memory `Store`. It holds JSON documents, not
  aggregates, so every test goes through the same encoding a database store uses.
  Nothing is queryable until `.queryable([...])` says so: a filter is also a question
  about the data, and `filter=password_hash:begins:a`, answered one character at a time,
  discloses a field the API never returns. It is a `HealthCheck` too.
- `jenny_core::testing::RecordedEvents` is the event double every service used to write
  for itself.
- `HealthCheck` is implemented for `Arc<T>`, so the store a use case holds is the value
  the runtime checks.

## Consequences

- A command use case is two or three lines. The scaffolded resource dropped its
  outbound adapter entirely, and the example's application layer shrank to about a
  quarter of its previous size.
- REST handlers and route tables carry no type parameters.
- Every use case is traced and measured without anyone writing it.
- Optimistic concurrency is uniform across every store adapter, which is what HTTP
  `ETag`/`If-Match` support will be built on.
- Aggregate field names are now part of the stored format and of the `<kind>.updated`
  payload. Renaming one is a migration, and it shows up in the domain's diff.
- Domain modules import `serde`. The review rule of ADR-001 becomes "`jenny::domain`,
  `jenny::port` and `serde`, nothing else".
- A service that needs storage the generic store cannot express writes its own adapter
  for its own port, as before. The difference is that it now does so only when it needs
  to.

## Not built yet

- Database, document and key-value stores (ADR-013).
- Mapping `Version` to `ETag`/`If-Match` and telling a stale write apart from a domain
  conflict over HTTP. Both are decided with the JSON Patch mode of the REST adapter.
