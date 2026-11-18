# ADR-003: Inbound Ports — Use Cases and the Invocation Context

**Status:** Accepted — the `UseCase` trait and the static-dispatch default superseded by ADR-012
**Date:** 2026-08-24

## Context

A use case is the unit of application behaviour a driving adapter invokes. Two questions
have to be answered before anything else in the framework can be designed:

1. Is a use case a trait, or a function?
2. How does ambient information about the call — who is asking, which request this is,
   what a middleware resolved — reach it without dragging transport concepts inward?

Get the second wrong and every use-case signature ends up mentioning a framework
envelope type, which puts the framework inside the hexagon.

## Decision

### A use case is a function until it needs to be a trait

The default is a plain function:

```rust,ignore
pub fn create_note<R: NoteRepository>(
    repo: &R,
    cmd: CreateNote,
    cx: &Context,
) -> DomainResult<NoteId>
```

Static dispatch, no allocation, no trait to keep in sync, trivially testable with a fake
repository. Most inbound ports never need to be more than this, which is why there is no
`domain/port/in/` directory in the expected layout (ADR-001) — a directory for a thing
that usually should not exist is an invitation to create it.

`jenny::UseCase` exists for when the indirection is actually needed: several driving
adapters that must not know the concrete type, a heterogeneous set stored behind
`Arc<dyn …>`, or a decorator (retry, cache, audit) that should wrap any use case
uniformly.

```rust,ignore
#[async_trait]
pub trait UseCase: Send + Sync + 'static {
    type Input: Send;
    type Output: Send;
    type Error: Into<DomainError> + Send;

    async fn invoke(&self, input: Self::Input, cx: &Context) -> Result<Self::Output, Self::Error>;
}
```

`Input` is a domain command, not a DTO. `Error` must convert into `DomainError` so that
adapters always have a status mapping, but a use case may return something narrower and
let the conversion happen at the boundary.

### Ambient context travels beside the input, not around it

`Context` carries the request id, the authenticated `Principal`, labels, and a
type-keyed extension bag. It is passed **by reference, as a second argument**.

The alternative — wrapping the payload in a framework envelope, `invoke(req:
Request<Input>) -> Result<Response<Output>, E>` — was rejected. It costs a wrap at every
call site and an unwrap at every use case, it puts a framework type in the signature of
every piece of application behaviour, and it makes the response side carry metadata that
in practice nobody reads. Keeping the two separate means a use case's signature is
domain command in, domain value out, with one borrowed framework type alongside.

Because every driving adapter builds the same `Context`, a use case written against it
works unchanged whether the call arrived over HTTP, off a queue, or from the clock.

`Context::require_principal()` returns `DomainError::Unauthenticated` when there is
nobody — the idiomatic first line of a use case that is only meaningful for a known
caller, and the mechanism by which fine-grained authorization (ADR-001) can live in the
domain without knowing about HTTP.

### The failure vocabulary

`DomainError` has six variants, chosen because they are the categories every transport
can express:

| Variant | HTTP | gRPC |
|---|---|---|
| `NotFound` | 404 | `NOT_FOUND` |
| `Invalid` | 400 | `INVALID_ARGUMENT` |
| `Unauthenticated` | 401 | `UNAUTHENTICATED` |
| `Forbidden` | 403 | `PERMISSION_DENIED` |
| `Conflict` | 409 | `ABORTED` |
| `Internal` | 500 | `INTERNAL` |

`Conflict` earns its place: without it, "this note is already archived" and "that title
is taken" have to be reported as `Internal`, which tells the caller a 500 for something
they could fix.

There is no machine-readable error-code vocabulary. A code catalogue is only worth
having if consumers actually branch on it, and one invented up front reliably drifts
from the codes that get used. Errors carry a variant and a human-readable detail; if a
machine-readable contract is needed later it can be added deliberately, against real
consumers.

### Validation is split by who owns the rule

- **Format** validation belongs to the inbound adapter, as the `TryFrom<Dto> for
  Command` impl. It is per-protocol by construction.
- **Business** validation belongs to the domain: `Validate`, or better, a newtype whose
  smart constructor makes the invalid state unrepresentable, so no downstream code has
  to re-check.

`ValidationErrors` collects every problem rather than short-circuiting, because a caller
fixing a form wants the whole list. It converts into `DomainError::Invalid`.

There is no derive macro for **domain** validation. Rules interesting enough to be worth
writing are usually interesting enough to resist a declarative attribute syntax, and the
plain-Rust version is readable and debuggable.

Format checks on an adapter DTO are the opposite — mechanical, and otherwise written
twice because the OpenAPI schema wants the same bounds. `#[derive(Dto)]` covers those;
see ADR-005 for why the split falls exactly there.

## Consequences

- Use-case signatures stay domain-shaped; the only framework type in them is a borrowed
  `Context`.
- Static dispatch is the path of least resistance, and `Arc<dyn UseCase<…>>` is
  available when the composition root wants it.
- Adapters get a total mapping from `DomainError` to their protocol's statuses, and a
  use case may still define a richer error type of its own.
- There is no response-side metadata channel. If a use case needs to influence protocol
  headers, it returns that in its `Output` type, where it is visible.
