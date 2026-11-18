# ADR-011: Cedar Authorization

**Status:** Accepted
**Date:** 2026-11-18

## Context

`jenny-security` used to ship its own RBAC: resources were glob patterns over concrete
request paths, roles mapped HTTP methods to those patterns, and `inheritedRoles` merged
one role into another. It had three structural problems that were not fixable by
patching:

- **Globs over paths over-grant.** `glob`'s `*` matches `/`, so `/api/orders/*[0-9]`
  also matched `/api/orders/x/payments/9`, and `/api/orders*` matched
  `/api/orders-admin/users`. Any pattern language over concrete paths has this risk,
  because the path is attacker-chosen text.
- **It failed open on configuration mistakes.** A pattern that did not compile or a
  rule naming an unknown resource was silently dropped, and inheritance was resolved in
  one order-dependent pass, so a role inheriting from an inheriting role could miss
  permissions or keep stale ones.
- **It could only answer the coarse question.** The `Authorizer` port took
  `(principal, action: &str, resource: &str)`, which is enough for "may this caller
  `POST` here", but not for "may this caller archive *this* note". That question was left
  to hand-written domain code with no shared vocabulary.

Writing a better rule language would have meant owning a policy engine. Cedar is one: a
small, analysable language with a Rust implementation, default-deny, `forbid` overriding
`permit`, entity hierarchies, attribute conditions, and schema validation of policies.

## Decision

### The port speaks the principal/action/resource/context shape, not Cedar

```rust,ignore
pub trait Authorizer: Send + Sync + 'static {
    async fn authorize(&self, principal: &Principal, request: &AccessRequest)
        -> Result<(), AuthorizationError>;
}

AccessRequest::new("archive", Resource::new("Note", 42).with_attribute("owner", "u-7"))
    .with_context("reason", "cleanup");
```

`AccessRequest` and `Resource` live in `jenny_core::port::outbound`. A `Resource` has a
kind, an id, parents (`within`) and JSON attributes; the request adds an action and a
context map. Every policy engine has this shape, so the domain can ask without importing
one. Exposing `cedar_policy` types from the port was rejected: it would put an
infrastructure crate into `jenny-core`, which ADR-001 forbids.

### One port for the coarse and the fine-grained question

`jenny-rest`'s `Authorization` layer asks the coarse question for every request and, on
success, attaches the authorizer to the request so that `Ctx` puts it into the
`Context`. A use case asks the fine-grained one after loading the data:

```rust,ignore
cx.authorize(&AccessRequest::new("archive", Resource::new("Note", note.id())
    .with_attribute("owner", note.owner()))).await?;
```

`Context::authorize` fails closed: no principal is `Unauthenticated`, a denial is
`Forbidden`, and no attached authorizer is `Internal`, because a use case that asks must
never get "allowed" by omission. Other driving adapters attach one with
`Context::with_authorizer`. The ADR-001 split still holds: data-dependent rules are
asked by the domain. What changed is that they are written as policies, next to the
endpoint rules, instead of as `if` statements.

### The HTTP gate asks about route templates in a URN tree

For `GET /api/orders/2341` on `Routes::new("orders")`, mounted on
`RestAdapter::new("shop")`:

| | |
|---|---|
| action | `Action::"GET"` |
| resource | `Route::"/api/orders/{id}"` in `Resource::"urn:shop:orders"` in `Adapter::"urn:shop"` |
| context | `{ "path": "/api/orders/2341" }` |

The resource is the matched **template** (axum's `MatchedPath`), never the concrete
path, so a rule is an exact match or an `in` over the hierarchy jenny already has: the
URNs from ADR-005. That removes pattern matching from authorization entirely. Using the
concrete path with Cedar's `like` was considered and rejected, because `like`'s `*` also
crosses `/`, which is the original bug.

`Routes` stamps its URN from a layer *inside* any layer applied to the adapter, so the
authorization layer could not see it. `RestAdapter::into_router` therefore installs a
`RouteTable` (template → resource URN) as the outermost layer of the adapter.

A request without a principal is answered 401 rather than 403: "may an unknown caller do
this" has no useful answer, and the client's remedy is to authenticate.

### `CedarAuthorizer` maps the request onto Cedar entities

| Cedar | Built from |
|---|---|
| `principal` | `User::"<id>"`, member of `Role::"<r>"` for each of `Principal::roles()`; attributes are the claims plus `id` |
| `action` | `Action::"<action>"` |
| `resource` | `<kind>::"<id>"`, member of its parents, with its attributes |
| `context` | the request's context |

`in_namespace("Shop")` qualifies all of them (`Shop::User`). Static entities (a role
hierarchy, action groups) are loaded once in Cedar's JSON format and win over a
request-contributed entity with the same uid, which is what replaces `inheritedRoles`:
`Role::"CustomerSupport"` declared in `Role::"Dispatch"` keeps that membership whoever
holds it.

### A schema is optional but changes three things

`with_schema` validates every policy in strict mode at startup, so a misspelled entity
type or attribute fails the deployment instead of silently never matching. From then on
requests, contexts and entities are checked against it, and the principal carries only
the claims the schema declares for `User`. Copying every claim would make each request
fail schema conformance on the first undeclared one. A schema is not required, because
a service starting with five policies should not need to write one first.
`docs/data/cedar/http.cedarschema` describes what the HTTP gate sends.

Without a schema, attribute values are converted literally: a JSON object becomes a
record, never an entity reference, and values Cedar has no type for (null, fractions)
are dropped.

### Failures deny

Anything that keeps a request from being evaluated (an unrepresentable id, a request
the schema rejects) denies and logs at `error`. Cedar skips a policy whose evaluation
errors. For a `permit` that only withholds a grant, but a skipped `forbid` grants more
than the policies say, so `CedarAuthorizer` denies whenever a `forbid` errored. Policy
templates are rejected at load, because nothing in jenny links them.

## Consequences

- No pattern language over attacker-controlled paths remains anywhere in authorization.
- Endpoint rules and data rules are written in one language, reviewed together, and
  validated against one schema.
- Policies are named by their `@id` annotation, or `<file>#<n>`, in logs.
- A service needs to know Cedar. The reference schema and `docs/data/cedar/shop.cedar`
  are the starting point, and `jenny-rest/tests/cedar_test.rs` runs them end to end.
- `jenny-security` depends on `cedar-policy` (MSRV 1.89, below the workspace's 1.94).
- Static entities are loaded once. Changing the role hierarchy means a restart.

## Not built yet

- Reloading policies and entities without a restart.
- Entity references as resource attributes (`resource.owner == principal`), which would
  need a way to mark an attribute as a reference in the engine-neutral `Resource`.
- Anonymous principals, so a policy could grant an endpoint to unauthenticated callers;
  public routes currently skip the `Authorization` layer instead.
