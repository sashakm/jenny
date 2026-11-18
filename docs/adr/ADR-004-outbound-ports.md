# ADR-004: Outbound Ports

**Status:** Accepted — "no generic repository", serde and static dispatch superseded by ADR-012; "storage has no port" by ADR-013; the runtime registers the readiness check of a pool it built (ADR-015); the dual-write gap is closed by the outbox (ADR-016)
**Date:** 2026-08-24

## Context

Outbound ports are the half of hexagonal architecture that actually inverts the
dependency: the domain declares what it needs, and infrastructure supplies it. If the
framework gets this wrong — by shipping ports that are too generic, or by not shipping
the ones that are genuinely universal — the inversion either does not happen or happens
in a shape nobody wants.

The question is therefore not "what should the framework abstract" but "which of these
traits does the framework know enough to write".

## Decision

### Repositories belong to your domain, not to the framework

jenny ships **no** `Repository<T, Id>` trait, and this is the most consequential
decision in this ADR.

A generic repository can only speak in generic terms: `find_by_id`, `save`, `delete`,
over `T: Entity`. But the point of an outbound port is that it is phrased in the
domain's language — `NoteRepository::archived_before(cutoff) -> Vec<Note>`, returning
your aggregate and failing with your errors. A generic trait cannot express that, so
services end up either fighting it or bypassing it, and either way the abstraction has
made things worse. Worse, a generic repository leaks storage vocabulary (ids, entities,
CRUD) back across the boundary it exists to defend.

So: **repository ports are declared in `domain/port/`, by you.** What jenny ships is the
machinery a driven adapter needs on the *other* side of that trait — statement building,
row mapping, and error conversions so a repository failure arrives already phrased as a
`DomainError`.

```
domain/port/                    NoteRepository            (trait — yours)
     ▲
adapter/outbound/persistence/   PgNoteRepository          (impl — uses jenny-persistence)
```

`jenny-persistence` provides `From<RepositoryError> for DomainError` mapping a lookup
miss to `NotFound` and everything else to `Internal` — a dropped connection is not the
caller's mistake and must not be reported as one.

### The framework ships four ports, because their shape does not vary

| Port | Driven adapter |
|---|---|
| `EventPublisher` | `adapter/outbound/eventbus` — in-process bus, broker, outbox relay |
| `Authenticator` | `adapter/outbound/security` — JWT/JWKS verification, API-key lookup |
| `Authorizer` | `adapter/outbound/security` — a policy engine; jenny ships Cedar (ADR-011) |
| `HealthCheck` | any driven adapter reporting whether it is usable |

These are universal in a way a repository is not: "turn credentials into a principal" and
"publish this event" mean the same thing in every application.

`HealthCheck` is a port for a structural reason rather than a philosophical one. The
things worth checking — a connection pool, a broker client, a remote endpoint — live in
driven adapters, and `jenny-persistence` must be able to answer for its own pool without
depending on the runtime. Putting the trait in the runtime would have inverted that
edge; putting it in the core lets each adapter implement it and lets the runtime merely
aggregate.

All four are object-safe, so the composition root can choose between `Arc<dyn …>` and a
monomorphised generic without the port changing. That is a wiring decision, not a design
one.

### Events cross the boundary as encoded envelopes

`DomainEvent` — the trait domain event types implement — requires only a name:

```rust,ignore
pub trait DomainEvent: Send + Sync + 'static {
    fn name(&self) -> &str;   // "note.created"
}
```

No `Serialize` bound. Requiring one would put serde on domain types, which ADR-001
rules out. Instead `EventPublisher` takes an `EventEnvelope` — a name, a timestamp, and
an already-encoded payload — and the **application layer** builds it:

```rust,ignore
let envelope = EventEnvelope::json(event.name(), &payload)?;
self.events.publish(envelope).await;
```

That places encoding exactly one ring out from the domain, mirroring where decoding
happens on the way in (`TryFrom<Dto> for Command`, ADR-003). The symmetry is the
argument: serde lives at both boundaries and in neither centre.

### Aggregates raise, the application publishes

An aggregate method that changes state returns the event it raised:

```rust,ignore
pub fn archive(&mut self) -> DomainResult<NoteArchived>
```

It does not publish. Publishing needs an outbound port, a runtime, and knowledge of
whether the state change was actually committed — none of which an aggregate has. The
application layer persists first, then publishes.

### Dual writes are acknowledged, not solved yet

`repository.save()` followed by `publisher.publish()` is two writes. If the process dies
between them, state and events diverge.

jenny does **not** ship a transactional outbox. The decision is to start direct and
document the upgrade path: when consistency actually requires it, put an outbox behind
`EventPublisher` — write the event into an outbox table inside the same transaction as
the state change, and let a relay drain it to the bus. Because the application layer only
ever sees the port, that change touches the composition root and the driven adapter, and
nothing else.

Shipping an outbox now would mean shipping a schema, a migration story and a relay
process for a problem many services do not have. The port is the part that has to be
right up front, and it is.

### Storage has no port

Object stores and filesystems vary too much — streaming, presigned URLs, multipart,
consistency — for a useful common trait. Declare one in `domain/port/` shaped to what
your domain actually needs.

## Consequences

- Repository ports read like the domain, because the domain wrote them.
- Swapping Postgres for an in-memory store is a one-line change in the composition root;
  the example service demonstrates exactly this.
- Test doubles for the shipped ports are trivial — implement the trait, record what
  arrives.
- Encoding failures and publish failures are visible in the application layer, which is
  where the decision about how to handle them belongs.
- Until an outbox exists, event delivery is at-most-once with respect to a crash between
  the two writes. Documented rather than hidden.
