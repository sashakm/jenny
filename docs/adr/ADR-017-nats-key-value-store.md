# ADR-017: The NATS Key-Value Store

**Status:** Accepted — implemented
**Date:** 2026-11-18

## Context

ADR-013 chose NATS JetStream KV as the key-value store, because jenny already depends on
`async-nats` and many services that use the bus run NATS anyway. The user framed it as
"first and foremost a generic datastore": a `Store<A>` like the others, not a cache
API. Three things about JetStream KV do not line up with `Store<A>` as Postgres
implements it:

- **Revisions are per bucket.** The revision of a key's first write is the bucket's
  next sequence number — 57, say — not 1. `Version` has so far counted writes per
  aggregate from 1, and HTTP `ETag`s, `Updated` events and tests rely on that.
- **There is no query language.** A bucket can list its keys and nothing else.
- **Wiring is synchronous, connecting is not.** `async_nats::connect` is a future, and
  `build` runs before anything is served.

## Decision

### Versions are the store's own; revisions only guard the write

Each value is the aggregate's document beside its version:

```json
{ "version": 3, "document": { "id": "…", "user": "ada", "visits": 9 } }
```

`insert` is the bucket's `create`, which fails when the key exists (`Conflict`), at
version 1. `save` reads the entry, refuses when its version is not the one the aggregate
was read at, and writes version + 1 with `update(key, value, revision)`, the bucket's
compare-and-swap on the revision it just read. The version check covers the time since
the use case loaded the aggregate; the revision check covers the moment between that
read and the write. `remove` checks the same way and deletes with
`delete_expect_revision`.

Using the revision as the version would have saved a read per write. It was rejected
because `Version` would then mean something different per store, and an aggregate's
first `ETag` would be `"57"`.

The bucket is created with `history: 1`: the store keeps its own versions, so the
bucket needs only the latest value.

### Listing is in memory, with the in-memory store's rules

`list` reads every key, sorts them (so ties keep a stable order), loads each value, and
hands the documents to `jenny_core::memory::Queryable`. That is the allowlist and
evaluator `Memory` uses, now public for exactly this. Filtering, sorting, paging and
the `ValueKind` checks are therefore identical to `Memory`, and swapping between them
changes nothing a client can observe.

That costs one read per key. It is right for what a key-value store is chosen for —
sessions, preferences, the state of a process, hundreds of aggregates — and wrong for
millions, which is what Postgres is for. The type's documentation says so.

### Ids must be keys

NATS keys are letters, digits and `-/_=.`, not starting or ending with `.`. UUIDs and
numbers qualify. An id that does not is an internal error on first use. It means the
aggregate does not suit this store, and silently escaping ids would make keys
unreadable to anyone inspecting the bucket with the `nats` CLI.

### The connection is lazy, and the runtime prepares it

`Nats::new(url, prefix)` builds nothing. The client and each bucket's handle are made on
first use, behind `OnceCell`s. `Nats::kv::<A>()` registers a bucket named
`<prefix>-<kind>`. The prefix keeps services that share a NATS out of each other's
data. `prepare` connects and opens or creates every registered bucket.

In the runtime, `messaging.nats.url` makes `JennyServer::nats()` return a handle, whose
prefix is the same namespace the bus uses (`messaging.namespace`, else the service
name). `bind` prepares it before binding the socket and registers a `nats` readiness
check, the same as Postgres in ADR-015. Without a URL, `nats()` is `None`, and the
example falls back to memory.

The store lives in `jenny-messaging`, beside the gateway that already speaks NATS. A
separate crate for one adapter would have added a dependency edge and a feature for no
gain.

### Tests run where a NATS is

As with Postgres: `JENNY_TEST_NATS_URL` set means they run, each in a bucket of its own
that it deletes afterwards. Unset, they say so and pass; unset under `CI`, they fail.
`just test-nats` starts a disposable `nats:2-alpine -js`, and runs the store's tests,
the runtime's, and the example's whole end-to-end suite against a bucket. CI starts the
same container in a step, because a service container cannot be given the `-js` flag.

## Consequences

- A service that runs NATS gets a durable store from one setting, with the same version
  semantics, the same conditional writes and the same listings as every other store.
- A save is a read and a write, where Postgres needs one statement.
- Listing scales with the bucket, not the page.
- There is no outbox for this store. `raising` publishes right after the write, as with
  `Memory`. A KV write and a message cannot share a transaction in JetStream.

## Not built yet

- Watching a bucket (`watch`) as a source of change events for other services.
- Time-to-live per key, which JetStream supports and sessions would want.
