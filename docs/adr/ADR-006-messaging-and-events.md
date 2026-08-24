# ADR-006: Messaging and the In-Process Event Bus

**Status:** Accepted — inbound dispatch not implemented
**Date:** 2026-08-24

## Context

Messaging shows up on both sides of the hexagon, and conflating them is the usual
mistake:

- **Outbound**, when a domain event needs to reach whatever cares about it.
- **Inbound**, when an arriving message should drive a use case.

They are different problems. The outbound side is about fan-out and delivery guarantees;
the inbound side is about acknowledgement, retry and poison messages.

## Decision

### One publish, many effects

The application layer publishes a domain event exactly once, through the
`EventPublisher` port (ADR-004). Behind that port an in-process bus fans it out to every
listener whose subscription matches:

```
 application ──publish──▶ EventPublisher (port)
                               │
                         EventBus (driven adapter)
                               ├──▶ audit log writer
                               ├──▶ NatsGateway ──▶ JetStream
                               └──▶ notification sender
```

Each listener is its own small outbound effect. Listeners depend on event definitions;
nothing depends on listeners. Adding one is additive, and the use case that raised the
event never learns it happened — which is the property that makes this worth having over
calling the three effects directly.

`EventBus` is backed by `topmesys`, an in-process broker with topic-pattern
subscriptions. Event names are namespaced into topics (`note.created` published on a bus
namespaced `my-notes` routes as `my-notes.note.created`), so a listener subscribes with
`my-notes.note.*`.

### The runtime owns the broker's lifecycle

With the `messaging` feature, `JennyServer` starts the broker and stops it on shutdown.
Two calls in `main` are the whole wiring:

```rust,ignore
server.subscribe(AuditLogWriter::new(pool.clone()))?;   // a listener
let events = server.events()?;                          // the port, for use cases
```

`events()` has to be available *during wiring*, because use cases need the publisher
before anything is served — so the broker starts on first use rather than at `bind()`.
Stopping it on shutdown waits for listeners still handling an event, so a graceful
shutdown does not drop side effects that were already in flight.

The namespace defaults to `messaging.namespace`, then `telemetry.service_name`, then
`"jenny"`. That last fallback is not decoration: topics may not contain empty segments,
so an unset service name would otherwise produce `.note.created` and make every publish
fail at runtime. A test covers it.

`topmesys` is re-exported from `jenny-messaging`, because subscribing a listener means
implementing its `EventConsumer` trait — that makes it part of this crate's public
surface whether or not we like it, and a re-export is more honest than making every
service add the dependency itself.

### Why a bus rather than a broker by default

The in-process bus is the default because most fan-out is in-process, and because it
keeps the port's contract honest without requiring infrastructure to run in tests.
Getting events *out* of the process is then one listener — `NatsGateway` forwards
matching events to JetStream — rather than a different mechanism.

Delivery is in-process, one event to many independent listeners, each triggering a
distinct side effect. A listener that fails is logged and the event discarded; it does
not fail the publisher or the other listeners. That is the right default for effects
that are genuinely independent, and it is why the dual-write discussion in ADR-004
matters: neither the bus nor the publisher makes events durable.

### Inbound dispatch is declared, not implemented

`MessagingAdapter` is the messaging counterpart to `RestAdapter`: a named driving
adapter, rooted at a subject prefix, declaring which subjects the service consumes. The
runtime records and logs them at startup and reports them on `/health/status`.

The dispatch path — consume, build a `Context`, convert the payload into a domain
command, invoke a use case, ack or nack — is **not built**. Shipping declaration-only is
a deliberate choice rather than an omission: the shape of the declaration is what other
things (configuration, status reporting, the future CLI's scaffolding) need to be stable,
and it is stable now. The dispatch behind it can land without that shape changing.

When it does land, the design it has to satisfy is already fixed by the rest of the
framework:

- an inbound message converts into the **same domain command** a REST DTO converts into
  (ADR-001), so a use case cannot tell which arrived;
- the consumer builds the same `Context` every other driving adapter builds (ADR-003),
  with the correlation id taken from message headers where the broker has them;
- failure handling — ack-and-log, nack for retry, dead-letter — is configurable per
  subscription rather than baked in, because the right answer depends on whether the
  effect is idempotent.

### `topmesys` is isolated

The dependency lives in `jenny-messaging` and reaches the application layer only as the
`EventPublisher` port. Replacing it, or putting a broker client or an outbox relay behind
that port instead, is a change in the composition root.

## Consequences

- Fan-out costs one `publish` call at the point where the event was raised.
- The bus needs no setup code: enabling the feature is enough, and the runtime reports
  its health on `/j/health/status` because it is a dependency the runtime owns.
- Listeners are independently testable and independently addable.
- Events are not durable across a crash between the state change and the publish; ADR-004
  records the outbox upgrade path.
- A service cannot yet be *driven* by messages. `MessagingAdapter` declares intent and
  the runtime warns at startup that dispatch is not running, so this is loud rather than
  silent.
