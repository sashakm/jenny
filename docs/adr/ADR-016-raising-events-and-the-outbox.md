# ADR-016: Raising Events with a Write, and the Transactional Outbox

**Status:** Accepted — implemented
**Date:** 2026-11-18

## Context

A use case that changes an aggregate and publishes what the domain raised makes two
writes: the store, then the bus. ADR-004 accepted that and recorded the way out: an
outbox, written in the same transaction as the state change and drained by a relay.
Until now, the shape of the use case made that way out impossible to take without
rewriting every use case:

```rust,ignore
let archived = self.notes.change(&id, Note::archive).await?;   // write 1
self.events.publish(&archived.outcome, cx).await;              // write 2
```

The store never saw the event, so it could not commit it. If the process died between
the two lines, the change was kept and the event was lost. If the bus was down, the
event was logged and dropped.

Two ways to let the store see the events were weighed:

1. **The use case hands the events to the write.** The change call takes the event
   publisher, and the store commits the events with the aggregate if it can, or
   publishes them right after if it cannot.
2. **The aggregate records its events**, in a pending list the store drains on save,
   as in many DDD codebases.

The second changes every domain type and hides the events in aggregate state. Today's
domain methods return what they raised, which is explicit and easy to test. The first
keeps that, and the decision was to take it.

## Decision

### The change call carries its events

`StoreExt::change` returns a `Change` that can be awaited on its own, or after saying
more:

```rust,ignore
self.notes
    .change(&id, |note| note.edit(title, body))
    .expecting(expected)          // the version the caller read; Stale otherwise
    .raising(&self.events, cx)    // publish what the domain method returned
    .announcing_changes()         // and `note.updated` with the patch
    .await?;
```

- `raising` publishes the change function's outcome. The `Raised` trait is implemented
  for every `DomainEvent + Serialize` and for an `Option`, a `Vec` and `()` of them, so
  a domain method returns whatever it naturally raises.
- `announcing_changes` adds the `<kind>.updated` event carrying the JSON Patch. That
  answers the "publish what changed" need from ADR-012 without the use case comparing
  anything.
- Events are encoded before the write. An event that cannot be encoded is a bug, and the
  change is not saved without it.
- A change that fails, in the domain or on a lost compare-and-swap, publishes nothing.
  A change that changes nothing writes nothing, and its events are published as they
  are, because there is no write to commit them with.

`StoreExt::change_at` is removed in favour of `.expecting(…)`. Two ways to say the same
thing are one too many.

`StoreExt::add(aggregate, raised)` is the creating counterpart. It returns an `Add` that
can only be awaited after `.raising(…)`, because an add that forgot its events would
lose them silently. `Store::insert` stays for aggregates that raise nothing.

### The port lets a store take the events

`Store` gains `insert_raising` and `save_raising`. Each takes the encoded events and
returns the ones it did not keep, which the caller then publishes. The default keeps
none, which is exactly the save-then-publish behaviour, so `Memory` and any hand-written
store work unchanged. A store with an outbox overrides them.

### Postgres keeps an outbox

`Postgres::with_outbox(publisher)` turns it on for every store the handle hands out,
whenever they were handed out. `insert_raising` and `save_raising` then run the
aggregate's write and one `INSERT` per event into `jenny_outbox` in one transaction, and
keep every event.

```sql
CREATE TABLE IF NOT EXISTS jenny_outbox (
    position       bigserial   PRIMARY KEY,
    id             uuid        NOT NULL,
    name           text        NOT NULL,
    occurred_at    timestamptz NOT NULL,
    correlation_id uuid,
    headers        jsonb       NOT NULL DEFAULT '{}',
    payload        bytea       NOT NULL,
    recorded_at    timestamptz NOT NULL DEFAULT now()
)
```

The table is created in `prepare` with the document tables, under the same advisory
lock. `Postgres::outbox_schema()` gives the statement for a migration when
`create_tables` is off.

### The relay forwards, at least once

`OutboxRelay` takes up to 100 rows `ORDER BY position … FOR UPDATE SKIP LOCKED`,
publishes them in order, deletes the ones published and commits. A publish that fails
ends the batch there: the rows from that one on stay, still in order, for the next pass.

- **Several replicas** each run a relay, and `SKIP LOCKED` keeps them off each other's
  rows. A test drains 250 events with two relays at once and sees each exactly once.
- **Delivery is at least once.** A relay that publishes and dies before its commit
  forwards those rows again after a restart. `EventEnvelope::id` is recorded and is the
  same on every attempt, so a listener that must not act twice can deduplicate on it.
- **Order** holds within a relay. Across replicas each takes the rows the others have
  not locked, so a strict global order is not promised.
- **Latency:** a commit in the same process wakes the relay through a `Notify`, so its
  events go out at once. Rows another replica wrote are picked up by a one-second poll.
  `LISTEN/NOTIFY` would shorten that, at the cost of a dedicated connection per replica,
  and is not needed yet.

### The runtime runs it from configuration

`persistence.postgres.outbox: true` makes `JennyServer::postgres()` enable the outbox
with the in-process bus from `events()`. The bus is where listeners, and through the
NATS gateway other services, already receive events. Without the `messaging` feature
there is no bus, and the setting is a wiring error at startup, not a silent no-op; a
service with its own publisher calls `with_outbox` itself.

The relay starts in `bind`, so the first request's events go out at once. In `serve` it
is stopped after the HTTP drain, so it forwards what the last requests committed, and
before the broker, because that is where it forwards to. The pool closes last.

The scaffold turns the outbox on when a project has both `persistence` and `messaging`.

## Consequences

- With Postgres and the outbox, a change and its events are committed together or not
  at all. ADR-004's documented gap is closed.
- Use cases read as one statement per command, and the same code is correct against
  `Memory` in tests and Postgres in production.
- Listeners must tolerate an event arriving twice after a crash, and in a different
  order across replicas. They already had to tolerate missing events. Trading that for
  duplicates is the point.
- Events from a Postgres write reach the bus a moment after the request returns, not
  during it. Tests that assert on the bus right after a request have to wait for the
  relay.
- `change_at` callers move to `.expecting(…)`.

## Not built yet

- `remove` raising events, for a "deleted" event committed with the deletion.
- An outbox for document stores other than Postgres. MongoDB (ADR-013) can do the same
  within a multi-document transaction.
- `LISTEN/NOTIFY` to wake relays in other replicas.
