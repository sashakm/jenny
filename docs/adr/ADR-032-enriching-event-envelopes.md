# ADR-032: Enriching Event Envelopes with Context

**Status:** Proposed
**Date:** 2026-11-25

## Context

An `EventEnvelope` carries an id, a name, a timestamp, the correlation id of the request
that raised it, free-form headers and the payload (ADR-006). Headers are carried all the
way: the outbox stores them in a `jsonb` column (ADR-016), the bus writes them as
topmesys headers, and `NatsGateway` copies them onto the JetStream message.

Nothing jenny builds sets one, though. `Raised::envelopes(cx)` and `Events::publish`
encode the event and call `correlated_with(cx)`, and the use case never sees the
envelope. `EventEnvelope::with_header` only helps a caller that builds envelopes by hand
and calls `EventPublisher::publish` itself, which is the path ADR-016 tells use cases
not to take.

What consumers ask for is not part of the domain fact:

- **who did it**: the principal that triggered the event, an app user or a service, and
  on whose behalf;
- the tenant;
- the trace context, which ADR-026 wants in event headers;
- the service and version that raised the event.

Today the only place for these is the payload. Every event type then repeats the field,
and request metadata enters the domain, where `Context` deliberately does not reach.

The envelope can only be changed safely in a few places. The `Context` exists only
between the use case and the write. After `insert_raising` or `save_raising` the envelope
is in the outbox, and the relay replays it later with no `Context`, keeping its id so
that a consumer can recognise a duplicate (ADR-016).

## Proposal

### Enrichers run where the envelope meets the `Context`

| Where | Has the `Context` | May change the envelope |
|---|---|---|
| `Events`, when the envelope is encoded | yes | headers, through enrichers |
| the store and its outbox | no | no: it stores what it is handed |
| `OutboxRelay` | no | no: it replays the envelope unchanged, under the same id |
| `EventBus`, `NatsGateway` | no | they add reserved and transport headers to the message, not to the envelope |

An enricher is registered on `Events`, which every raising path already goes through:
`change(…).raising(&events, cx)`, `add(…).raising(&events, cx)` and `Events::publish`.
`Change` and `RaisingAdd` apply the enrichers between `envelopes(cx)` and the store's
`*_raising` call. The outbox therefore stores the enriched envelope, and a replay
carries the same headers as the first delivery.

`JennyServer::events()` returns that `Events`, carrying the enrichers turned on in
configuration (below), instead of the bare `Arc<dyn EventPublisher>`. `main` adds its
own:

```rust,ignore
let events = server.events()?             // with what `messaging.events.headers` turns on
    .enriched_with(|cx: &Context, headers: &mut Headers| {
        if let Some(tenant) = cx.get::<TenantId>() {
            headers.insert("tenant", tenant.as_str());
        }
    });
```

The enrichers sit on `Events`, in the composition root, rather than in each use case,
because what an event carries is a deployment decision. A use case that wanted a header
of its own would also be the one place that forgets it.

The trait is small and synchronous:

```rust,ignore
pub trait Enrich: Send + Sync + 'static {
    fn enrich(&self, cx: &Context, headers: &mut Headers);
}
```

It is implemented for closures of that shape. It cannot fail: a requirement on the
`Context`, such as "a tenant must be known", belongs in the use case, before anything is
written. It is not async, because it runs once per event inside the write, and anything
that needs I/O belongs in the adapter that built the `Context`.

### Headers only

An enricher gets the headers, nothing else:

- not the id, which deduplication relies on (ADR-016);
- not the name, which routing relies on;
- not the timestamp or the correlation id, which describe the request, not the enricher;
- not the payload, which is what the domain raised. Changing it belongs in the domain.

The reserved names (`event-id`, `event-name`, `event-occurred-at`, `correlation-id`)
still cannot be overwritten, because the bus writes them after the envelope's own headers
(ADR-006).

### Who did it: subject and actor, as in RFC 8693

OAuth 2.0 Token Exchange (RFC 8693) names two parties. The **subject** (`sub`) is the
one on whose behalf a call is made. The **actor** (the `act` claim) is the party actually
making it, when that is someone else: a backend for frontend, or a service calling
another with a token exchanged for the user's. The built-in `Caller` enricher writes both:

- `caller-subject`: the principal's id (`Principal::id`). That is a token's `sub`, an
  API key's owner, or the id a client certificate maps to.
- `caller-actor`: `act.sub` from the principal's `act` attribute, only when there is
  one. Without `act` the subject acted itself, and RFC 8693 reads an absent `act` the
  same way.

Only the outermost `act` is carried. RFC 8693 treats nested `act` claims, the earlier
actors in a chain, as informational, and says they must not be used for access control.
They are left out rather than handed to consumers as if they meant more.

The JWT authenticator already copies a token's claims into the principal's attributes,
so a token that came from a token exchange carries `act`. With ADR-024's attribute
schema, `act` is a record attribute like any other, and has to be declared to reach
`Caller`.

The names have a `caller-` prefix because a NATS message already has a subject: its
routing address. A bare `subject` header next to it would be misread.

`Caller` writes nothing when there is no principal. That covers an unauthenticated
request, a scheduled task (whose `Context` has none), and an event raised during
inbound dispatch of an unauthenticated message (below). The headers only ever describe
an authenticated caller.

### Off until configured

No built-in enricher is on by default. A service may have no notion of a caller, or one
it does not want in its events. A principal id is also personal data, and a JetStream
stream keeps it for as long as its retention says, where it is hard to erase.

They are turned on in configuration, so the scaffold and operators can turn them on
without changing `main`:

```yaml
messaging:
  events:
    headers:
      caller: true    # caller-subject, and caller-actor when delegated
      source: true    # source-service, source-version
```

- `Source` writes the service name and version. The runtime knows both, but
  `with_version` usually comes after `events()` in `main`, so `Source` reads them when an
  event is raised, not when it is built.
- ADR-026's trace context becomes one more entry, writing `traceparent` and
  `tracestate`, instead of a mechanism of its own.

The built-ins are also public types (`Caller`, `Source`), for a service that wires
`Events` by hand.

The header names are constants in `jenny_core::port`, so a consumer written against
jenny does not spell them. They are a contract, like the payload.

### Inbound: restored only over an authenticated connection

When inbound dispatch lands (ADR-006), it builds the consumer's `Context` from the
message, and the correlation id from `correlation-id`. It restores `caller-subject` and
`caller-actor` only when the consumer's NATS connection is authenticated (ADR-031). Over
an anonymous connection anyone could have published the message, so the headers are
dropped and the `Context` has no caller.

An authenticated connection still says less than an authenticated request. NATS knows
who published, and the consumer only knows that NATS let them. The headers are therefore
as trustworthy as the NATS permissions on the subject, and an operator who relies on them
restricts who may publish there.

### Alternatives considered

- **The actor in the payload.** That is right when the actor is part of the domain fact,
  such as `approved_by` on an approval, and the domain models it there. As the general
  mechanism it was rejected: every event type would repeat the field, and the domain
  would read the `Context`.
- **Typed `subject` and `actor` fields on `EventEnvelope`.** They are easier to discover,
  but they need outbox columns, a mapping in every transport, and the same again for the
  next field, the tenant. Headers already survive every hop.
- **One `actor` header**, the principal's id. It cannot tell a user acting for themselves
  from a service acting for that user, and consumers that audit need to know which.
- **Enriching at the gateway.** The gateway sees every event that leaves the process, but
  it has no `Context`, and events committed through the outbox arrive there long after
  the request.

## Open questions

- Does a restored caller become the consumer's `Principal`, so that the consumer's Cedar
  policies apply to the user the event was raised for? Or is it a value of its own in
  the `Context`, so that a header is never mistaken for a verified credential? The answer
  meets ADR-024's provenance: a principal restored from a header would be a third kind,
  next to credentials and claims.

## Consequences

- Who triggered an event, and on whose behalf, reaches every listener and every consumer
  of the stream without changing a domain event or a use case.
- ADR-026's trace context and ADR-024's attributes get a place to go.
- `JennyServer::events()` returns `Events` instead of `Arc<dyn EventPublisher>`:
  breaking. The scaffold and the example drop their `Events::new(…)`.
- Header names become part of a service's event contract, and need the same care for
  stability as payloads.
- Each header adds bytes to every outbox row and every JetStream message.
- The `Events` a use case holds now matters. A second `Events` built from the raw
  publisher, without the enrichers, would quietly drop the headers.
