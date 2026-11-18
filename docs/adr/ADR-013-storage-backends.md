# ADR-013: Storage Backends

**Status:** Accepted — not implemented
**Date:** 2026-11-18

## Context

jenny is opinionated about messaging and had nothing concrete for storage beyond helpers
for writing a Postgres repository by hand. Services are expected to ask for a relational
database first, a document store soon after, a key-value store, an object store, and in
some cases a graph.

ADR-012 makes this tractable: `Store<A>` is a shared interface for adapters to implement,
and aggregates are serde-encodable, so a generic adapter needs no per-aggregate mapping
code. What remains is choosing backends and drivers, and being honest about which of
them are worth depending on.

The goal is turn-key in a specific sense. Enabling a backend takes a feature and a
configuration section. Ports specific to a domain are still implemented by the service,
but the generic case needs nothing beyond `Store<A>`.

## Decision

### Every backend has the same shape

- A cargo feature on the facade, off by default (ADR-002).
- A configuration section the runtime reads to build the client or pool.
- A `Store<A>` implementation where the backend can honour its contract, including the
  version check on every save.
- A `HealthCheck` the runtime registers, so readiness reflects the dependency.
- Migrations or bucket provisioning at startup, where the backend has them.

### Postgres, through sqlx — first

A `PgStore<A>` in two modes:

- **Document** — one table per kind with `id`, `version` and a `jsonb` body. No mapping
  code at all. This is the default document store too, because it adds no
  infrastructure and shares transactions with everything else in the database.
- **Table** — a mapped table, for data that other systems query relationally, with the
  `Columns` allowlist from ADR-010 doubling as the mapping.

Saves are `UPDATE … WHERE id = $1 AND version = $2`. The transactional outbox that ADR-004
deferred is built here, because a store and an outbox in the same transaction is exactly
what Postgres can offer and the others cannot.

### MongoDB, through the official driver

`mongodb` (Apache-2.0, maintained by MongoDB Inc.) for when a dedicated document database
is asked for. Version checks are `update_one({ _id, version }, …)`, and zero documents
matched is a stale write. The same driver reaches FerretDB 2 (the Mongo wire protocol
over Postgres with Microsoft's MIT-licensed DocumentDB extension) and the managed
Mongo-compatible services, so one adapter covers several deployments.

The driver's MSRV (1.94.1 at 3.10) sits one patch above jenny's declared 1.94, which is
resolved when the adapter is built.

### NATS JetStream KV

The key-value store, because jenny already depends on `async-nats` for messaging and
the in-process bus forwards to NATS. JetStream KV has per-key revisions,
`update(key, value, revision)` as a compare-and-swap, `create` that only succeeds if the
key is absent, and watch streams. A revision is a `Version`, so `Store<A>` maps onto it
directly. It is also the natural home for the generic uses of a key-value store: caching,
idempotency keys, leases.

### Object storage, through the S3 API

There is a port for this now, which reverses ADR-004's "storage has no port". The
argument there was that object stores vary too much for one trait. `object_store` from
the Apache Arrow project has since shown that a common shape works: put, get, list,
delete, conditional puts and signed URLs. jenny-core gets a small object-storage
(`Blobs`) port, and an adapter implements it over `object_store`. Its in-memory and
local-filesystem backends make tests and development need no infrastructure.

The S3 API is the integration, not MinIO. The MinIO community edition stopped shipping
binaries in October 2025, went into maintenance mode in December 2025, and has had a
read-only repository since April 2026, with known vulnerabilities that will not be fixed
upstream. Existing MinIO installations, AWS S3, Garage, SeaweedFS, Ceph RGW and
Cloudflare R2 all speak S3, so a MinIO-specific client would bind services to the one
implementation with no future.

### Graph: Apache AGE

openCypher inside Postgres (Apache-2.0), queried through sqlx. It shares the pool, the
health check, migrations and transactions with the relational store, so a graph write
and the outbox can commit together. Its releases follow Postgres majors (1.8 is PG18
only), and managed availability varies by provider.

A graph does not fit `Store<A>`, because its value is in traversals. The graph port
stays specific to the domain. What jenny provides is the client, `agtype` decoding,
and a Cypher builder with bound parameters and an allowlist of labels and relationship
types. Labels cannot be bound parameters, which is the same injection class as SQL
identifiers in filter paths (ADR-010) and gets the same answer.

### Considered and not chosen

| Option | Why not |
|---|---|
| Neo4j through `neo4rs` | The mainstream graph choice, but the driver is a community (neo4j-labs) project with 0.9 long in release candidates, Neo4j Community is GPLv3, and AGE reuses infrastructure jenny already has |
| Kùzu | Archived in October 2025; forks exist but are young |
| SurrealDB, ArangoDB | Capable multi-model databases under the Business Source License |
| FalkorDB | SSPL |
| A MinIO-specific client | See above |
| Valkey/Redis | Good for caching and rate limits, but a conditional write needs `WATCH`/`MULTI` or a script; deferred until a service needs it |
| redb / fjall (embedded) | Useful for single-node services; deferred, since `Memory<A>` covers tests and development |

## Consequences

- A service gets a working store from a feature and a configuration section, and keeps
  the use cases unchanged when it moves from `Memory` to a database.
- Postgres carries the most weight: relational, document, graph and the outbox in one
  database, one pool and one transaction.
- Each backend is a dependency jenny has to keep current, which is why the list is short
  and every entry has a reason.
- Optimistic concurrency is identical across backends, so the HTTP layer can map
  `Version` to `ETag` once.

## Not built yet

All of it, in this order: Postgres (document and table modes, pool and migrations, then
the outbox), MongoDB, NATS KV, object storage, AGE.
