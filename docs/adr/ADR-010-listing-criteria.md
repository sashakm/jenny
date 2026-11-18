# ADR-010: Generic Listing Criteria

**Status:** Accepted
**Date:** 2026-08-24

## Context

Every collection endpoint wants the same three things: a page, an order, and a way to
narrow the result. Written per resource that is a lot of near-identical code, and each
copy is an opportunity to get the injection-sensitive part wrong.

The awkward part is that "narrow by an attribute the client names" is inherently
stringly-typed — the client has to be able to say *which* attribute — while the thing it
eventually reaches is a query language. That is the whole problem this ADR is about.

## Decision

### One vocabulary, in the core

`Criteria` — a `Page`, a list of `Sort`s and a list of `Filter`s — lives in
`jenny_core::domain::query`, not in the REST adapter. A gRPC adapter would produce the
identical thing, and a repository should accept it once rather than once per transport.

### The wire spelling

Compact, repeatable parameters:

```
?page=2&page_size=20
&sort=-created_at&sort=title
&filter=status:eq:active&filter=title:contains:milk
```

- A sort is `field`, `-field`, or `field:asc|desc`.
- A filter is `path:check:value`, or `path:value` when the check is `eq`. Checks are
  `eq`, `begins`, `ends`, `contains`, `gt`, `st` (`lt` is accepted as an alias, because
  it is what most people type).

Two parsing decisions worth recording:

- **The split is bounded** — at most two colons — so a value may contain colons of its
  own. `at:eq:12:30:00` filters `at` for the literal `12:30:00`.
- **A three-part filter whose middle token is not a check is an error**, not a guess.
  Guessing would turn a typo into a filter that silently matches the wrong rows, which is
  the worst possible failure mode for a query language.

RSQL/FIQL and JSON:API bracket syntax were both considered. RSQL buys AND/OR grouping
nobody asked for at the cost of a grammar to maintain and heavy percent-encoding; bracket
syntax needs a deep-object parser that `serde_urlencoded` does not have. The compact form
maps directly onto the model and needs neither.

### Injection is handled in three places, not one

A filter path is attacker-controlled and, unlike a value, **cannot be a bound
parameter** — SQL parameters cannot stand in for identifiers. So:

1. **Parsing** rejects anything that is not a dotted identifier path. Quotes, whitespace,
   semicolons and wildcards never get past the front door, whatever happens downstream.
   That holds for every way in: `Sort` and `Filter` deserialise through the same
   validating constructors, so a `Criteria` read from a JSON body or a message is
   checked exactly like one parsed from a query string.
2. **An allowlist** decides which attributes are queryable at all.
   `Criteria::ensure_allowed` is the in-memory form; `jenny-persistence` takes a
   `Columns` table that doubles as allowlist and path-to-column translation.
3. **Values are bound, never interpolated**, and `LIKE` wildcards inside them are
   escaped — otherwise a filter of `%` quietly matches every row in the table.

The allowlist is a **required argument** of `SqlCriteria::build`. There is deliberately
no API that turns criteria into SQL without one, because an optional safety control is
one that gets skipped under deadline.

`Columns` also carries a type per column, so `age:gt:banana` is a 400 at the edge with a
useful message rather than a 500 from the database, and comparisons against non-text
columns get the cast they need.

sqlx 0.9 requires `AssertSqlSafe` around any statement built at runtime, which is exactly
the right question to be asked; the documentation answers it where the wrapper appears.

### Every listing is paged, in a total order

A `Page` without a size is `DEFAULT_PAGE_SIZE` (50) entries, and no size exceeds
`MAX_PAGE_SIZE` (1000); the REST parser answers a `page_size` outside `1..=1000` with
400 rather than silently clamping it. `SqlCriteria` therefore always emits `LIMIT` and
`OFFSET`. An unbounded default would turn a growing table into a full scan per request,
shipped over the wire.

Offset paging is only correct over a total order: rows tied on the requested sort key
can otherwise land on two pages or on none, because the database may order ties
differently per query. `Columns::new` therefore takes the table's key column as a
required first argument, and `SqlCriteria` ends every `ORDER BY` with it (unless the
client already sorted by it). Cursor pagination would avoid the problem altogether, and
remains the better choice for very large or fast-changing tables; it is not built.

### When not to use this

Generic criteria trade type safety for openness. Where the set of useful queries is small
and known, a typed domain query (`NoteQuery { archived: Option<bool> }`) is better: it
cannot express a nonsense filter, needs no allowlist, and reads like the domain. This
vocabulary is for genuinely open-ended list endpoints, and the documentation says so.

## Consequences

- A collection endpoint gets paging, sorting and filtering by adding one extractor
  argument, and the same `Criteria` reaches the repository whatever transport it came in
  on.
- Every path a client can name is checked twice — shape at parse time, membership at the
  allowlist — before anything reaches storage.
- The allowlist is a small amount of per-repository configuration. That is the deliberate
  cost of letting clients name attributes at all, and it is the piece that must not be
  optional.
- Filter semantics are per-adapter: the in-memory implementation compares strings, the
  SQL one delegates to the database's collation. Anything relying on exact ordering
  semantics across both should use a typed query instead.
