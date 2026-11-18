# ADR-015: The Postgres Document Store and the Runtime's Pool

**Status:** Accepted — document and table modes implemented; the outbox not implemented
**Date:** 2026-11-18

## Context

ADR-013 chose Postgres as the first storage backend, with a document mode, a table mode
and an outbox. The goal is that a service gets a working store from configuration
alone. This ADR settles how the document mode, the first of the three, meets the
runtime: where the pool comes from, who creates the tables and when, how listings are
allowlisted, and how a database-backed test runs in a workspace whose CI had no
database.

## Decision

### The runtime owns the pool, built from `persistence.postgres`

With the `persistence` feature, `ServerConfig` gains a `persistence.postgres` section
(`PostgresConfig`): `url`, pool sizes, an acquire timeout, and `create_tables`. The `url`
carries the password, so `Debug` redacts it, error messages never repeat it, and the
documentation steers it into `JENNY__PERSISTENCE__POSTGRES__URL` rather than a committed
file.

`JennyServer::postgres()` returns `Option<Postgres>`: a lazily connecting pool when a
`url` is configured, `None` otherwise. Laziness makes the pool available during wiring,
which is synchronous and runs before any database needs to be up, the same way
`events()` makes the bus available early. `None` is what makes "it only needs
configuration" work. The example and the scaffold build a Postgres store when there is a
pool and a `Memory` store when there is not, from one allowlist. The same binary passes
its tests in memory and serves from a database once a URL is set.

A pool the runtime built is a pool it manages. `bind()` calls `Postgres::prepare()`
before binding the socket and registers a `postgres` readiness check. `serve()` closes
the pool after the bus has stopped, because listeners may still have been writing. So
a database that is down, or a migration that fails, stops the service at startup and is
never discovered by the first request. ADR-004's note that "the runtime cannot register
a database check for you" no longer holds for a pool the runtime built. A pool a service
builds itself still registers its own check.

jenny-server depends on jenny-persistence behind the feature. ADR-002 allows exactly
this: the composition root may depend on adapters.

### `prepare` creates the document tables, safely from several replicas

`Postgres::documents::<A>()` registers the table its store needs, named after
`Aggregate::KIND` or given explicitly with `documents_in`. `prepare` runs the service's
migrations first (`Postgres::migrate(sqlx::migrate!())`, embedded at compile time), then
every `CREATE TABLE IF NOT EXISTS` in one transaction under
`pg_advisory_xact_lock(hashtext('jenny.prepare'))`. The lock is needed because
concurrent `IF NOT EXISTS` statements from replicas starting together can still
collide on the type catalogue.

The framework creating tables was weighed against requiring a migration for each one. A
document table's shape is the framework's format, not the service's schema, so the
framework is the right owner. Where the schema is owned by migrations alone,
`create_tables: false` turns it off, and `PgDocuments::schema()` gives the statement to
put in one.

### The document table

```sql
CREATE TABLE IF NOT EXISTS "note" (
    id         text        PRIMARY KEY,
    version    bigint      NOT NULL CHECK (version > 0),
    document   jsonb       NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now()
)
```

- The key is the `Display` of `Aggregate::Id`, as `text`, so every id type fits one
  shape.
- `save` is `UPDATE … WHERE id = $1 AND version = $2 RETURNING version`. When it matches
  nothing, one more query tells "removed" (`NotFound`) from "saved by someone else"
  (`Conflict`). That query runs only on the failure path.
- `remove` with an expected version is a single
  `DELETE … WHERE id = $1 AND ($2 IS NULL OR version = $2)`, so the check and the
  deletion are atomic.
- Table names are plain lower-case identifiers, checked when the store is built (a
  panic, since the name is a constant in code) and always quoted.

### One allowlist type for every store

Criteria filtering needs to know what an attribute holds: a database must cast, and a
client sending `priority:gt:banana` deserves a 400. The kind used to be
`jenny-persistence`'s `ColumnKind`, and the in-memory store had an untyped list of paths
instead. That made swapping stores more than a one-line change and let `Memory` silently
match nothing where Postgres refused.

`ValueKind` now lives in `jenny_core::domain::query`, with the value checks and
the check-applicability rules. `ColumnKind` is removed. `Memory::queryable`,
`PgDocuments::queryable` and `Columns` all take `(path, ValueKind)`. A document store
turns each allowlisted dotted path into `(document #>> '{a,b}')`, cast to the kind's SQL
type. Paths are written into the statement only from this allowlist, which is code, and
are checked to be dotted field names. Client-supplied values are bound parameters, as
ADR-010 requires everywhere.

Semantics still differ slightly between stores, as ADR-010 anticipated: text equality is
case-sensitive in Postgres and not in memory, and ordering follows the database's
collation.

### The table mode reads and writes through Postgres's own record conversion

`Postgres::table::<A>("notes")` is a `PgTable<A>` over a table the service creates in a
migration: a column per top-level field of the aggregate, same names, plus
`version bigint`. Rows are read with `to_jsonb(t)` and written with
`jsonb_populate_record(NULL::notes, $1)`, so Postgres converts between the aggregate's
serde form and the column types, and there is still no mapping code. The id is matched
through the same record conversion, so a `uuid` or `integer` key works without the store
knowing its type.

The column list comes from `information_schema.columns`, read in `prepare` after the
migrations, so a missing table or `version` column stops the service at startup. An
aggregate field with no column is refused on the first write: storing the rest and
dropping it would be silent data loss. Columns the aggregate does not have
(`created_at DEFAULT now()`) are left to the database and do not count as changes.
Identifiers come from the catalogue or from code, and are always quoted.

### Database tests run where a database is

Tests that need Postgres read `JENNY_TEST_POSTGRES_URL`:

- **Set:** they run, each in a table of its own, which they drop afterwards.
- **Unset:** they say so and pass, so `just test-all` keeps working without docker.
- **Unset under `CI`:** they fail. A suite that quietly skips its database tests in the
  one place that must run them would report green for code it never exercised.

`just test-postgres` starts a disposable Postgres 18 on a free port and runs the
store's tests, the runtime's, and the example's whole end-to-end suite against it. CI
runs a Postgres service for its test job, plus a step running the example against it.

## Consequences

- A service moves from memory to Postgres by setting one environment variable. The use
  cases, the allowlist and the tests are unchanged.
- Readiness reflects the database without the service registering anything.
- Table creation and migrations happen before traffic, under a lock, on every start.
  That costs one round trip per document table, and it is the price of never serving
  against a missing table.
- Filtering on a document path is not indexed unless someone adds an expression index.
  The `PgDocuments` documentation shows how.
- CI now needs a database service for its test job.

## Not built yet

- The transactional outbox, which needs a store write and its events in one
  transaction.
- Migrations in the scaffold: a generated project can call `Postgres::migrate`, but
  ships no `migrations/` directory yet.
