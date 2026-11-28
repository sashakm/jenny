# ADR-032: Enriching Event Envelopes with Context

**Status:** Accepted — implemented
**Date:** 2026-11-25 (accepted 2026-11-28)

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

## Decision

### Enrichers run where the envelope meets the `Context`

| Where | Has the `Context` | May change the envelope |
|---|---|---|
| `Events`, when the envelope is encoded | yes | headers, through enrichers |
| the store and its outbox | no | no: it stores what it is handed |
| `OutboxRelay` | no | no: it replays the envelope unchanged, under the same id |
| `EventBus`, `NatsGateway` | no | they add reserved and transport headers to the message, not to the envelope |

An enricher is registered on `Events`, which every raising path already goes through:
`change(…).raising(&events, cx)`, `add(…).raising(&events, cx)` and `Events::publish`.
`RaisingChange` (with `announcing_changes`) and `RaisingAdd` apply the enrichers between
`envelopes(cx)` and the store's `*_raising` call, in registration order. The outbox
therefore stores the enriched envelope, and a replay carries the same headers, under the
same id, as the first delivery. `Events::deliver`, which hands on what a store returns
unsent, does not enrich: those envelopes were enriched when they were encoded.

`JennyServer::events()` returns that `Events`, carrying the enrichers turned on in
configuration (below), instead of the bare `Arc<dyn EventPublisher>`; the outbox relay
keeps the raw bus, so a replay is never enriched twice. `main` adds its own after the
configured ones:

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

- `caller-subject`: `Principal::subject`. For a user that is its id: a token's `sub`, an
  API key's owner, or the id a client certificate maps to. For the service itself, a
  system principal (ADR-033), it is `System[<service>/<component>]`.
- `caller-actor`: `act.sub` from the principal's `act` attribute, only when `act` is an
  object with a string `sub`. Without `act` the subject acted itself, and RFC 8693 reads
  an absent `act` the same way.

Only the outermost `act` is carried. RFC 8693 treats nested `act` claims, the earlier
actors in a chain, as informational, and says they must not be used for access control.
They are left out rather than handed to consumers as if they meant more.

The JWT authenticator already copies a token's claims into the principal's attributes,
so a token that came from a token exchange carries `act`. With ADR-024's attribute
schema, `act` is a record attribute like any other, and has to be declared to reach
`Caller`.

The names have a `caller-` prefix because a NATS message already has a subject: its
routing address. A bare `subject` header next to it would be misread.

`Caller` writes nothing when there is no principal, as for an unauthenticated request.
Scheduled tasks and inbound handlers do have one now, the service itself (ADR-033), so
what they raise names `System[<service>/tasks]` or `System[<service>/events]`, or the
restored caller under the `envelope` policy (below).

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

- `Source` writes the service name and version, which it reads when it is built:
  `jenny::service!()` gives the runtime both before anything is wired (ADR-033), so no
  lazy lookup is needed. A value that is empty is left out.
- ADR-026's trace context becomes one more entry, writing `traceparent` and
  `tracestate`, instead of a mechanism of its own.

The built-ins are also public types (`Caller`, `Source`), for a service that wires
`Events` by hand.

The header names are constants in `jenny_core::port`, so a consumer written against
jenny does not spell them. They are a contract, like the payload.

### Inbound: who a handler runs as

Inbound dispatch (ADR-006) builds the consumer's `Context` from the message: the request
id from `correlation-id` when it is a UUID, an `InboundEvent` (id, name, occurred-at,
delivery attempt) for idempotency, and the server's authorizer.

**The caller headers are trusted only over a trusted transport.** The in-process bus is
always trusted: only this process writes to it. A JetStream consumer trusts them when the
NATS server required the service to authenticate (`auth_required` in its `INFO`,
`Nats::is_authenticated`), because it then requires every publisher to authenticate too.
Over an anonymous connection anyone could have published the message, so the headers are
dropped. A trusted caller is attached as an `EventCaller { subject, actor }` extension.

A server with `no_auth_user` reports that it requires authentication but accepts
anonymous clients as that user; give that user no publish permission where caller headers
are trusted.

An authenticated connection still says less than an authenticated request. NATS knows
who published, and the consumer only knows that NATS let them. The headers are therefore
as trustworthy as the NATS permissions on the subject, and an operator who relies on them
restricts who may publish there.

**The open question, answered.** Whether a restored caller becomes the consumer's
`Principal` is a per-subscription policy, `messaging.inbound.principal` (and its
`subscriptions` overrides):

- `system`, the default: the handler runs as the service itself,
  `System[<service>/events]` (ADR-033). The caller is only a value in the context
  (`EventCaller`), so a header is never mistaken for a verified credential.
- `envelope`: the handler runs as a `User` with the id in `caller-subject`, and
  `act: { sub: caller-actor }` when there is one, so the consumer's policies apply to the
  user the event was raised for, and what it raises carries the same subject and actor
  on: a delegated chain survives a hop. It carries no other attributes and no roles, so
  role-based policies deny it, and a `RestoredCaller` marker in the context stands in for
  ADR-024's provenance until that exists. It falls back to the system principal when
  there is no `caller-subject`, or when the subject has the system form `System[…]`: the
  service itself is never restored from a header. `envelope` on a JetStream subscription
  over an anonymous connection is a startup error naming the subscription.

A restored principal is a thin user plus a marker rather than a third `PrincipalKind`,
because the kind is about who the principal is, and provenance (how much we believe it)
is ADR-024's question.

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

## Consequences

- Who triggered an event, and on whose behalf, reaches every listener and every consumer
  of the stream without changing a domain event or a use case.
- ADR-026's trace context and ADR-024's attributes get a place to go.
- A consumer can run under the policies of the user an event was raised for, opt-in, and
  never as a system principal named in a header.
- `JennyServer::events()` returns `Events` instead of `Arc<dyn EventPublisher>`:
  breaking. The scaffold and the example drop their `Events::new(…)`.
- Header names become part of a service's event contract, and need the same care for
  stability as payloads.
- Each header adds bytes to every outbox row and every JetStream message.
- The `Events` a use case holds now matters. A second `Events` built from the raw
  publisher, without the enrichers, would quietly drop the headers.
