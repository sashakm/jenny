# ADR-005: The HTTP Adapter — axum and tower

**Status:** Accepted
**Date:** 2026-08-24

## Context

REST is the first driving adapter jenny ships, and it sets the pattern the others follow.
Two things had to be decided: which HTTP stack to build on, and what a service author
actually writes to expose a resource.

gRPC is on the roadmap. That is not a detail — it is the single biggest input into the
first decision.

## Decision

### axum, over tower and hyper

Three reasons, in order of weight.

**Middleware is shared with gRPC.** tonic is built on tower and hyper. A `tower_http`
layer — CORS, tracing, timeouts, request ids, compression — applied to the REST adapter
applies unchanged to a future gRPC adapter. Both can even be served from one hyper
server. On a stack with its own middleware model, every cross-cutting concern gets
written twice and drifts.

**The composition root reads like a dependency graph.** axum builds one `Router`, cloned
cheaply per connection, on plain multi-threaded tokio. Stacks that use a per-thread
runtime need an app *factory* that re-runs on every worker, which forces every adapter to
be `Clone` and re-registered per thread — machinery that shows up in `main` and has
nothing to do with the service being built.

**Standard types.** Requests and responses are `http` crate types and handlers are
`tower::Service`s, so anything in that ecosystem drops in without an adaptor.

Building directly on tower and hyper was considered and rejected: routing, path
parameters, body handling and typed extraction are precisely what axum adds, and
hand-rolling them is a large amount of undifferentiated work. axum *is* the thin layer,
and it still exposes `tower::Layer` for everything else.

`jenny-rest` re-exports `axum` and `tower_http` so services need one dependency rather
than three, and reaching for raw axum is expected rather than an escape hatch.

### A resource is a function that returns routes

There is no trait to implement:

```rust,ignore
pub fn routes(notes: Arc<Notes>) -> Routes {
    Routes::new("notes")
        .describe("Note lifecycle")
        .get("/notes", list)
        .post("/notes", create)
        .get("/notes/{id}", fetch)
        .with_state(notes)
}
```

`Routes` records the method/path pairs for introspection and wraps an `axum::Router`.
`.route(path, method_router)` is the escape hatch to raw axum for anything the shorthands
do not cover.

A controller *trait* was the alternative: `urn()`, `path()`, `resources()`, implemented
per resource and stored behind `Arc<dyn …>`. It was rejected because it is ceremony
without payoff. A resource is a route table plus handlers; a trait adds an impl block,
type erasure and a vtable to express that, and ADR-003's rule — a trait only when the
indirection is needed — applies here too. The declarative metadata a trait would have
carried (description, labels) is a builder call instead.

### The adapter owns its identity

```rust,ignore
RestAdapter::new("my-notes")?          // mounts at `server.api_path`, default /api
    .mount(rest::notes::routes(notes))?
```

The name does three jobs: it roots the `Urn` of every mounted resource, it keys the
`applications.<name>` configuration section, and it is what `/health/status` reports.
There is no separate container type between the adapter and its routes — a level of
nesting that would exist only to be passed through.

The mount path resolves explicit `.at(…)` → `server.api_path` → `/api`. **Code beats
configuration**, which is the opposite of the file → env → CLI precedence used for
values, and deliberately so: an operator changing `api_path` is moving the *convention*,
while a developer who wrote `.at("/v2")` meant that path specifically. A config file
silently relocating it would be baffling to debug.

Because a configured default can create a collision that was not there at registration
time — an adapter explicitly at `/x` and `api_path: /x` — the duplicate check runs again
after defaults are applied. Otherwise the second adapter's routes would simply never
match.

### Listings are a first-class extractor

`Listing` parses paging, sorting and filtering into the core's transport-neutral
`Criteria` (ADR-010). It is a hand-written extractor rather than `Query<Criteria>`
because `sort` and `filter` repeat, and `serde_urlencoded` keeps only one value per key
— the second `sort=` would be silently dropped. Parameters it does not recognise are
left alone, so a handler can take both `Listing` and its own `Query<T>`.

### `#[derive(Dto)]`, for inbound DTOs only

A DTO's constraints would otherwise be written twice: once as validation, once as an
OpenAPI schema, and they drift. One attribute set now drives both.

Scoped to adapter DTOs on purpose. ADR-003 rejected a validation derive for *domain
invariants* — rules interesting enough to be worth writing resist declarative syntax —
and that reasoning still holds. It does not hold for format checks on a wire type, which
are exactly the mechanical ones. The domain's answer stays the smart constructor, which
makes the invalid state unrepresentable rather than merely detected; a derive there would
compete with the better pattern.

The macro operates entirely within one ring and generates nothing that crosses a
boundary, so ADR-001's rule against macros hiding architecture is intact. The OpenAPI
half is gated on the *macro crate's* feature, decided when `jenny-macros` compiles —
emitting `#[cfg(feature = "openapi")]` into the caller would silently vanish in any crate
that happens not to define a feature by that name.

### Operational endpoints get a namespace and a lock

Health, metrics and OpenAPI live under `server.framework_path` (`/j`), away from
`server.api_path`, so the framework can never take a route the service wanted.

The metrics endpoint optionally requires HTTP Basic credentials from configuration.
Metrics leak more than people expect — route names, tenant counts, error rates, deploy
times — and "only reachable from inside the cluster" stops being true at some point in
every cluster's life. Both halves are compared in constant time against SHA-256 digests,
so a length mismatch leaks no timing either, and the password is redacted from the
config type's `Debug`.

Basic auth sends its credential in every request, base64-encoded, which is not
encryption. The documentation says so where the setting is defined.

Each mounted resource gets its `Urn` stamped onto every request it serves, so a
`RouteId` extractor can identify the resource without re-deriving it from the path.

### Failures are RFC 9457 problem documents

```json
{ "type": "urn:jenny:problem:conflict", "title": "Conflict",
  "status": 409, "detail": "note is already archived",
  "instance": "8f14e45f-ea1a-4a2c-9a1e-2d4c1f0b3a77" }
```

`Problem` converts from `DomainError`, so a handler `?`s straight through a use-case call
and the status mapping happens at the boundary:

```rust,ignore
async fn create(...) -> ApiResult<(StatusCode, Json<Uuid>)> {
    let id = notes.create(dto.try_into()?, &cx).await?;
    Ok((StatusCode::CREATED, Json(id.as_uuid())))
}
```

Both `?` operators — the `TryFrom` and the use case — go through the same conversion.

Choices worth recording:

- **RFC 9457, not a bespoke envelope.** Clients that already understand problem details
  need no jenny-specific handling.
- **`type` is a URN, not `about:blank`.** The failure kind stays machine-readable without
  clients parsing prose, and it echoes the `Urn` identity used elsewhere.
- **`instance` is the request id** — the same id in the logs and traces, so a user
  quoting an error message is quoting something greppable.
- **Success responses are the payload, not an envelope.** Wrapping every success in
  `{status, message}` doubles the nesting for no gain; the status is already in the
  status line.
- **5xx details are logged, not sent.** "connection to db-primary.internal refused" is
  for operators. 4xx details *are* sent, because the caller made a mistake and needs to
  know which.

`jenny_rest::Json` is a newtype over `axum::Json` whose rejections are also `Problem`s,
and the runtime installs problem-shaped 404 and 405 fallbacks. The result is that every
failure a client can provoke has one content type and one shape.

### Security layers take ports, not mechanisms

`Authentication::required(Arc<dyn Authenticator>)` lifts credentials off the
`Authorization` header, verifies them through the port, and puts the `Principal` into
request extensions where the `Ctx` extractor finds it. `Authentication::optional` allows
anonymous callers **but still rejects credentials that are present and invalid** —
silently downgrading a bad token to anonymous access is how authentication bypasses
happen.

`Authorization::new(Arc<dyn Authorizer>)` gates on the HTTP method as the action and the
request path as the resource. It reads the path from `OriginalUri`, because
`Router::nest` rewrites the URI to strip the mount prefix — a layer inside an adapter
mounted at `/api` would otherwise be handed `/orders` and be asked to match it against
rules written for `/api/orders`. There is a regression test for exactly this.

Unknown `Authorization` schemes are an authentication *failure*, not a silent pass, so a
typo in a scheme name can never be mistaken for an anonymous request.

CORS defaults are provided but **no origins are set** — that is a security decision the
framework has no business making for a service.

### The OpenAPI document is supplied, not inferred

`RestAdapter::with_openapi` takes the document from your own `#[derive(OpenApi)]` type,
and the runtime merges every adapter's and serves it. It is not inferred from `Routes`,
because that would mean either annotating every handler or guessing at request and
response shapes — and a guessed API document is worse than none, since clients trust it.

Swagger UI's assets are compiled in rather than fetched from a CDN, so the page works on
an air-gapped cluster and under a restrictive CSP.

## Consequences

- A gRPC adapter can be added beside REST and inherit the middleware stack.
- Exposing a resource is a function call; there is no framework trait in the way.
- All HTTP failures — handler, extractor, routing — share one shape.
- A collection endpoint gets paging, sorting and filtering by adding one argument.
- Services are exposed to axum's API and its release cadence. Accepted: hiding it behind
  a jenny-shaped façade would cost more than it saves, and the re-export makes the
  coupling explicit.
- Per-request HTTP metrics and W3C trace-context propagation are not implemented yet
  (ADR-008).
