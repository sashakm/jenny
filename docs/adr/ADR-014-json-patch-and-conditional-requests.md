# ADR-014: JSON Patch and Conditional Requests

**Status:** Accepted — implemented; `change_at(id, expected, f)` is now `change(id, f).expecting(expected)` (ADR-016)
**Date:** 2026-11-18

## Context

A REST adapter so far was plainly representational: `GET` returns a resource, `PUT`
replaces it. Services also want RFC 6902 JSON Patch, so a client can change one field
without sending the whole resource back.

A patch is a read-modify-write on the server, and two things make that harder than it
looks:

- **Lost updates.** Two clients read version 3; both patch; the second write silently
  undoes the first. A patch makes this more likely than a `PUT`, because it is cheap to
  send without re-reading. HTTP's answer is `ETag` and `If-Match`, which need a version
  to be meaningful. ADR-012 put one on every stored aggregate.
- **Which fields a patch may touch.** A patch applies to the representation a client
  read, and that includes fields the server owns: an `id`, an `archived` flag. If a
  patch to one of those succeeds without effect, the client believes something that is
  not true. Patch paths are client-supplied, like filter paths (ADR-010), and need the
  same kind of allowlist.

The decision taken with the roadmap was that `PATCH` is derived automatically, not
written per resource.

## Decision

### Two modes, chosen on the adapter

`RestAdapter::accept_json_patch()` switches the adapter from plainly representational
to accepting JSON Patch. In that mode, every route template with both a `GET` and a
`PUT` also answers `PATCH`, unless it declares a `PATCH` or a raw `route(…)` of its own.
Without it, `PATCH` is `405`.

The call must come before `mount`, and fails afterwards. Resources mounted earlier were
built without the derived routes, and an adapter that patched some resources and not
others depending on call order would be a surprise to everyone. Deferring route
construction until the end would have broken the "layers wrap what was mounted before
them" rule the adapter already has.

A derived `PATCH` is listed with the resource's endpoints, so `/health/status` shows it.

### A patch is a `PUT` computed on the server

The derived handler:

1. requires `Content-Type: application/json-patch+json`, else `415` with
   `Accept-Patch`, and a valid RFC 6902 document, else `400`;
2. calls the resource's own `GET` for the current representation and its `ETag`; an
   `If-Match` that does not match that tag under strong comparison is `412`;
3. applies the patch with the `json-patch` crate; a failed `test` operation is `409`,
   and any other failure to apply is `422`, as RFC 5789 §2.2 describes;
4. calls the resource's own `PUT` with the result and `If-Match: <the GET's ETag>`, and
   returns its response.

So a patch passes exactly the validation, use case, domain rules and authorization
inside the use case that a `PUT` does. Nothing about it is written twice.

The `GET` and `PUT` are called as `MethodRouter`s, not by routing a new request through
the resource's `Router`. Routing again would append to the `MatchedPath` axum already
set and merge a second copy of the path parameters into the request. The private
`UrlParams` type cannot be removed from the extensions, so routing again cannot be made
correct. To keep resource-level layers in force, `Routes` keeps a copy of each `GET` and
`PUT` and applies every `Routes::layer` call to the copies too. Since axum's
`Router::layer` layers each method router the same way, a patch reaches the `GET` and
`PUT` through exactly the layers a direct request would.

The forwarded requests keep the client's extensions: the principal, the request id,
the resource URN, the matched path and its parameters. They drop the body headers and
the client's own preconditions. The precondition has been checked against the `GET`,
and its `ETag` is what the `PUT` receives.

### Fields a patch may not change

The patch applies to what `GET` returns. Fields a `PUT` does not take are ignored when
the result is deserialised, which is serde's default, and a patch changing one would
otherwise be a silent no-op.

The forwarded `PUT` therefore carries the JSON Pointers the patch changed, taken from a
diff of the representation before and after rather than from the operations' own paths,
so an operation that changes nothing does not count. `jenny_rest::Json` deserialises such a body through
`serde_ignored`. If a field it ignored is, contains, or lies inside a changed path, the
request is refused with `422` before the handler runs: "`/archived` cannot be changed
through this resource".

The `PUT` body type is the allowlist: what it takes may be patched, and nothing else
can be. That follows ADR-010's rule for client-supplied paths, and it costs nothing per
resource. It needs two conventions, which the example and the scaffold follow:

- **A `PUT` accepts the `GET` representation**, ignoring what it does not use.
  `#[serde(deny_unknown_fields)]` on a `PUT` body would refuse every patch.
- **A `PUT` reads its body through `jenny_rest::Json`.** A handler using `axum::Json`
  cannot do the check. The derived handler notices after the fact and logs a warning,
  because by then the write has happened.

Declaring the editable type per resource, `editable::<NoteRequest>(path)`, was the
alternative. It would make the allowlist explicit, at the cost of a line per resource
and of not being automatic, which was the point.

### `ETag` and `If-Match` are store versions

- `Versioned<T>` wraps a response body and sends `ETag: "<version>"`, on success only.
  An error is not a representation, and tagging one would let a client match it.
- `IfMatch(Option<Version>)` extracts the precondition. It is `None` when the header is
  absent or `*`, and `Some(v)` for a single strong tag.
- Anything else is `412` before the handler runs: a weak tag (never equal under strong
  comparison), a tag that is not a version, or a list of several. That is the safe answer
  to a precondition that cannot be evaluated against one compare-and-swap. Ignoring it
  would let the write through.
- The use case receives the version and calls `StoreExt::change_at(id, expected, f)` or
  `Store::remove(id, expected)`. The comparison is the store's compare-and-swap, so
  there is no window between "the version matched" and "the write happened".
- `DomainError::Stale` is the failure: `412` over HTTP, `FAILED_PRECONDITION` over gRPC.
  It is separate from `Conflict`, which stays `409` for a duplicate or for losing a race
  the caller set no precondition on. `412` without a precondition in the request would
  be wrong. `change_at` turns a lost compare-and-swap into `Stale` when an expectation
  was given, since the caller's view is out of date either way.

Putting the expected version into `Context` was considered, so use-case signatures would
not change. It was rejected because a use case that changes two aggregates would not say
which one the precondition guards. An explicit `expected` argument does.

A precondition is honoured only by handlers that take `IfMatch`. RFC 9110 says a server
must evaluate `If-Match`, and a handler that does not take it does not. The example and
the scaffold take it on every write.

## Consequences

- An adapter in JSON Patch mode answers `PATCH` for every replaceable resource without a
  line of handler code, and a patch is validated and authorized exactly like a `PUT`.
- Lost updates are refused, not resolved: a stale patch or `PUT` is `412`, and the
  client re-reads.
- The adapter's authorization gate sees a derived `PATCH` as action `PATCH` on the route
  template. Its inner `GET` and `PUT` pass only the resource's own layers, not the
  adapter-wide gate again, so a policy that grants `PUT` must grant `PATCH` as well for
  patches to be allowed.
- A patch costs one `GET` and one `PUT` inside the process, plus serialising the
  representation in both directions.
- `If-Match: *` on a resource that does not exist answers `404` from the `GET`, not the
  `412` RFC 9110 specifies. Distinguishing the two would need the derived handler to
  special-case one status.

## Not built yet

- JSON Merge Patch (RFC 7396, `application/merge-patch+json`), which the same machinery
  could serve with the same allowlist.
- `PATCH` operations in the OpenAPI document. Following ADR-005, the document is
  supplied rather than inferred. The `json-patch` crate's `utoipa` support is pinned to
  utoipa 5 while jenny is on 6, so the schema would have to be written by hand.
- A `PUT` that reports the change it made (the patch between the stored forms), and an
  option to require `If-Match` on every write (`428 Precondition Required`).
