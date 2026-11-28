# ADR-006: Messaging and the In-Process Event Bus

**Status:** Accepted — implemented, inbound dispatch included (2026-11-28); events committed with a Postgres write reach the bus through the outbox relay (ADR-016); `messaging.nats.url` also backs key-value stores (ADR-017)
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

### Inbound dispatch: typed handlers, from the bus and from JetStream

A service consumes events through typed handlers. A handler is a driving adapter: it gets
the payload decoded as JSON into its own DTO, converts it into the **same domain command**
a REST DTO converts into (ADR-001), and calls the use case, which cannot tell which
arrived. `jenny_messaging::inbound::Handler<M>` (`async fn handle(&self, message: M, cx:
&Context)`) is the trait; a closure works too, taking the `Context` by value, because
stable Rust cannot prove `Send` for a closure's future that borrows it.

One dispatch core (`jenny_messaging::inbound::Dispatcher`) serves both sources. It builds
the `Context` every other driving adapter builds (ADR-003): the request id from the
`correlation-id` header when it is a UUID, the server's authorizer, an `InboundEvent`
(id, name, occurred-at, attempt) for idempotency, the caller from `caller-subject` /
`caller-actor` when the transport is trusted, and the principal: the service itself,
`System[<service>/events]`, unless `principal: envelope` restores the caller (ADR-032,
ADR-033). What a handler raises is therefore correlated with the request that started it.

**The in-process bus.** `JennyServer::handle(pattern, handler)` subscribes a
`BusHandler` for a topic pattern; the raw `subscribe(EventConsumer)` stays. Bus headers
are always trusted. A failure or an undecodable payload is logged with the event id and
name, and not redelivered: the bus is in memory, and a retry belongs to a broker.

**JetStream.** `MessagingAdapter::stream("SHOP")` names the stream and `.on(binding,
handler)` binds a subject; `.subscribe(binding)` still only declares one. At `bind`, the
runtime creates or updates one durable pull consumer per adapter, named
`<service_name>-<adapter>`, with the handled subjects as filter subjects, over the shared
NATS connection (ADR-031). It never creates the stream; a missing one is a startup error
naming it. What becomes of a message is configurable under `messaging.inbound`, with
overrides per full subject in `messaging.inbound.subscriptions` (a list, since keys cannot
contain dots), because the right answer depends on whether the effect is idempotent:

| The handler | The message |
|---|---|
| succeeded | ack |
| undecodable payload, `Invalid`, `NotFound`, `Forbidden`, `Unauthenticated` | term, with a warning |
| `Internal`, `Stale`, `Conflict` | nak, delayed by `backoff` (seconds, default `[1, 5, 30]`, the last repeating), until `max_deliver` (default 5); then term |

`DomainError` has no `Unavailable` variant, so "unavailable" failures arrive as
`Internal`; a new variant has to be classified in `Outcome::is_retryable`, whose
exhaustive match makes that a compile error rather than an oversight.

Before any term, an optional `dead_letter` subject gets the payload and headers (minus
the `Nats-*` ones, so the copy is not deduplicated against the original), plus
`dead-letter-reason`, `dead-letter-subject` and `dead-letter-deliveries`. It is published
to JetStream and its acknowledgement awaited, so the subject must be captured by a
stream; if that fails, the message is nak'd rather than lost. A handler that runs long
sends a progress ack every half `ack_wait` (default 30 seconds), so it is not
redelivered meanwhile. jenny counts deliveries itself (the consumer's
own `max_deliver` is unlimited), so nothing is dropped without going through that path.
`max_in_flight` (default 1) is how many messages a consumer holds: at 1, messages are
handled in stream order as long as they succeed; more trades order for throughput.

Delivery is at least once, and jenny keeps no dedup store: a handler whose effect is not
idempotent records the `InboundEvent` id.

Consumers start once the listener is bound, so no message is acked by a service whose
startup then fails. At shutdown, they stop fetching as soon as it begins, finish the
messages they are handling and settle them, nak any they pulled but had not started,
beside the HTTP drain; then the relay, the broker, the NATS
connection and the pool close, in that order (ADR-016, ADR-031). `/health/status` lists
each consumer with its stream, filter subjects and pending count.

Other brokers and creating or configuring streams are out of scope.

### Event metadata travels as headers

An `EventEnvelope` carries an `id` (fresh per event, kept across an outbox replay), its
timestamp, the `correlation_id` of the request that raised it (`correlated_with(&cx)`)
and free-form headers. They are envelope fields rather than extra arguments to
`EventPublisher::publish`, so an outbox can store and replay an envelope unchanged; a
`Context` parameter was rejected for exactly that reason, since a relay has none.

`EventBus` writes them as topmesys message headers (`event-id`, `event-name`,
`event-occurred-at`, `correlation-id`; constants in `jenny_messaging::headers`), after
the envelope's own headers so that those cannot overwrite the reserved names.
`NatsGateway` copies every header onto the JetStream message and sends the event id as
`Nats-Msg-Id`, so JetStream deduplicates a replayed event within its duplicate window.
Headers on `EventMessage` arrived in topmesys 0.3.1; before that the bus dropped
everything but the name and payload, the timestamp included.

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
