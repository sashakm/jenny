# ADR-019: Graphs With Apache AGE

**Status:** Accepted — implemented
**Date:** 2026-11-19

## Context

ADR-013 chose Apache AGE for graphs, and decided that a graph gets no `Store<A>`: its
value is in traversals, so the port stays the domain's own, and jenny supplies a client,
`agtype` decoding, and Cypher with bound parameters and an allowlist of labels. What
that left open is how AGE is actually reached from sqlx, and it is less direct than a
table:

- **Cypher is a string inside SQL.** A query is the second argument of a function,
  `SELECT * FROM cypher('social', $$ MATCH … $$, $1) AS (r agtype)`. Anything spliced
  into that literal is injection, the same class ADR-010 deals with for filter paths.
- **Parameters go in one way only.** The third argument is a single `agtype` map, and
  AGE accepts it only as a bound parameter of a prepared statement. Labels and
  relationship types cannot be parameters at all.
- **Results are `agtype`**, a JSON superset with annotations after values
  (`{…}::vertex`, `[…]::path`, `2.5::numeric`) that sqlx knows nothing about, and
  `AS (…)` must list exactly the columns the `RETURN` produces.
- **Each session needs setting up.** Unless the server preloads the library, a session
  must `LOAD 'age'` before its first Cypher query, and a role that is not a superuser
  may not. And `ag_catalog` must be on the search path: AGE's rewritten queries, and
  even `create_graph`, look up its operators unqualified ("operator does not exist:
  graphid = graphid").

## Decision

### In jenny-persistence, behind a `graph` feature

The graph is part of `jenny-persistence`, not a crate of its own. It uses the same pool,
sqlx and `prepare`, and adds no dependency, so a separate crate would only split one
database across two. The `graph` feature (on the facade, implying `persistence`) keeps
the API out of services that do not want it.

`Postgres::graph(name)` is the only way to get a `Graph`. That is how `prepare` knows
every graph the service uses, the same way it knows every document table.

### Static text, bound values, allowlisted names

```rust,ignore
let social = postgres.graph("social").identifiers(["KNOWS", "FOLLOWS"]);

let names: Vec<String> = social
    .cypher("MATCH (:Person {id: $id})-[:{{relation}}]->(f:Person) RETURN f.name")
    .bind("id", 7)
    .identifier("relation", "KNOWS")
    .fetch_all()
    .await?;
```

- `cypher` takes a `&'static str`, so `format!` is not the easy way to build a query.
- `bind` serialises any `Serialize` value into the one parameter map, bound as `$1`.
- `identifier` fills a `{{placeholder}}` with a name from the graph's allowlist, and
  anything else is `DomainError::invalid`, a 400, because it most likely came from a
  client. Placeholders left unfilled are an internal error.
- The query sits in a dollar quote tagged `$jenny_cypher$`, and a query containing the
  tag is refused. The graph name is checked as a lower-case identifier when the graph is
  made, and a bad one panics, since it is a constant in the service's code.

A typed Cypher builder was considered and rejected. Cypher's surface is large, a builder
would be a second language to learn and would always lag behind AGE, and the dangerous
parts — values and names — are exactly what `bind` and `identifier` already cover.

### One column, decoded with serde

Every statement is `… AS (result ag_catalog.agtype)`. A query that returns several
things per row returns a map (`RETURN {name: f.name, since: r.since}`). Each row is
deserialised into whatever the caller asks for. A vertex arrives as
`{"id", "label", "properties"}`, an edge also has `start_id` and `end_id`, and a path
is an array of both.

The text form becomes JSON by dropping the annotations outside strings. `NaN` and the
infinities have no JSON form and are refused rather than guessed at. On the wire,
`agtype`'s binary format is a version byte (1) followed by that text, like `jsonb`'s.
Tests against AGE 1.8.0 confirm it in both directions. The type is declared as
`ag_catalog.agtype`, so it resolves whatever the search path. A result column reports
the bare `agtype` its search path found, and both names are accepted.

### Each transaction sets itself up

`fetch_all`, `fetch_optional` and `execute` run in a transaction of their own, and
`fetch_all_in` runs in the caller's. Either way the transaction starts with:

- `LOAD 'age'`, only when it is not in `shared_preload_libraries` or
  `session_preload_libraries` (looked up once per graph). A server that preloads AGE
  serves a least-privileged role. One that does not still works for a superuser.
- `ag_catalog` **prepended** to the search path with `set_config(…, true)`. That is local
  to the transaction, so a pooled connection goes back unchanged. And it is prepended,
  not replaced, so the caller's own schemas still resolve for the rest of the
  transaction.

`fetch_all_in` takes a `Transaction`, not a bare connection, because outside a
transaction a local setting silently does nothing, and the query would fail later with
an operator error. Setting the search path for every connection in the pool's
`after_connect` was rejected: it would apply to every store's query too.

### Graphs exist before serving

`Postgres::prepare` runs `CREATE EXTENSION IF NOT EXISTS age` and creates each missing
graph under the same advisory lock as the document tables, so replicas can start
together, when `create_tables` is on. With it off, `prepare` checks that the extension
and every graph exist, and fails startup naming `Graph::schema`, the SQL for a
migration. A database without AGE therefore stops the service at startup, not on the
first traversal. There is no separate readiness check, because the graph lives in the
database the `postgres` check already covers.

### Tested as shipped, and without the preload

The tests read `JENNY_TEST_AGE_URL`, and skip without it or fail under `CI`, like every
backend's. It names a database of its own, so the rest of the suite keeps running
against stock Postgres. `just test-age` runs the graph tests and the example twice
against `apache/age:release_PG18_1.8.0`: once preloaded, as the image ships, and once
started with a plain `postgres`, so the `LOAD` path runs too. CI does the same, with
a service container for the first and a container started by a step for the second.

### In the example and the scaffold

Notes link to other notes. `NoteLinks` is the domain's port, and
`adapter/outbound/links.rs` has two answers: `MemoryLinks` over a set, and
`GraphLinks`, which keeps one `Note` vertex per linked note (its id only, since the
note lives in its store) and a `LINKS_TO` edge per link. AGE has no uniqueness constraints,
so two concurrent `MERGE`s can both create the same link. `GraphLinks` therefore reads
with `DISTINCT` and deletes every match, which makes a duplicate harmless. Deleting a
note forgets its links after the note is gone, the same order attachments use
(ADR-018).

`cargo jenny new --features graph` implies `persistence`, runs the AGE image in
`compose.yaml` instead of `postgres:18-alpine`, and points at `postgres.graph(…)` in
the composition root.

## Consequences

- A graph is a feature and one line in the composition root, in the database the stores
  already use, created before serving.
- Graph writes can share a transaction with each other and with SQL of the service's
  own (`fetch_all_in`). They cannot yet share one with a store's save or its outbox
  events, so a use case that writes both orders its writes, as the example does.
- Concurrent `MERGE`s can duplicate. Adapters read with `DISTINCT`, or add a unique index
  on the label's table themselves.
- Writing a new label creates a table in the graph's schema, so the role that writes
  needs `CREATE` there, or the labels must be created in a migration.
- AGE follows Postgres majors (1.8 is PG18 only), and managed offerings vary, so a
  service that enables `graph` constrains where it can run.

## Not built yet

- Committing graph writes with a store's save and its outbox events in one transaction.
- Placeholders for anything but labels and relationship types. Variable-length bounds
  (`*1..3`) cannot be parameters in Cypher, so they stay in the query text.
- A test with a role that is not a superuser.
