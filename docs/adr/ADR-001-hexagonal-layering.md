# ADR-001: Hexagonal Layering and the Dependency Rule

**Status:** Accepted — the domain import rule and the module contents amended by ADR-012 (serde in `domain/`, `Store`, no `UseCase`); telemetry lives in `jenny::server::telemetry` (ADR-008)
**Date:** 2026-08-24

## Context

jenny is a framework for building webservices that stay changeable. The failure mode it
exists to prevent is the familiar one: a service where the database schema, the HTTP
route table and the business rules have grown into each other, so that changing any of
the three means touching all of them.

Hexagonal architecture is the answer we want, but the name is used loosely enough that
"we're hexagonal" communicates almost nothing. This ADR pins down what it means here,
concretely enough to review against.

## Decision

### Dependencies point inward

The domain depends on nothing external. Infrastructure depends on the domain, never the
reverse. This is the rule the rest of the framework exists to serve.

```
        ┌──────────── composition root ────────────┐
        │  ┌──────── driving adapters ────────┐    │
        │  │  rest · grpc · messaging · task  │    │
        │  │     ┌──── application ────┐      │    │
        │  │     │   ┌── domain ──┐    │      │    │
        │  │     │   │  model     │    │      │    │
        │  │     │   │  event     │    │      │    │
        │  │     │   │  service   │    │      │    │
        │  │     │   └────────────┘    │      │    │
        │  │     │      use cases      │      │    │
        │  │     └─────────────────────┘      │    │
        │  └──────────────────────────────────┘    │
        │  driven adapters: persistence · eventbus │
        └──────────────────────────────────────────┘
```

### It is a design rule, not a crate boundary

The project is intentionally one workspace, and a jenny *service* is intentionally one
crate. We are not enforcing the rule with `cargo` dependency edges.

That choice is deliberate and has a cost, so it is worth stating why. Splitting a
service into `domain`/`application`/`adapter` crates does buy compiler-enforced
layering, but it also buys: orphan-rule friction every time a `From` impl needs to span
two of them, a workspace to maintain per service, slower incremental builds, and a
strong incentive to put things in the wrong crate to avoid the ceremony. For a service
of the size jenny targets, the enforcement is not worth those. The rule is upheld by
**code review**, and the framework's job is to make violations obvious rather than
impossible.

### The framework's module tree encodes the rings

`jenny-core` is laid out so that an import states which ring it belongs to:

| Module | Ring | Contents |
|---|---|---|
| `jenny::domain` | the hexagon | `DomainError`, `Validate`, `Principal`, `Urn`, `DomainEvent` |
| `jenny::port` | the boundary | `UseCase`, `Context`, the outbound port traits |
| `jenny::task` | driving adapter | the clock |
| `jenny::telemetry` | driven adapter | logs, traces, metrics |

Nothing under `domain` or `port` pulls in a database driver, an HTTP or gRPC framework,
or a broker client. So the review rule has a mechanical form: **a `domain/` module may
import from `jenny::domain` and `jenny::port`, and nothing else from the framework.** A
`use jenny::rest::…` in a domain file is a violation you can see without knowing the
domain.

### The expected service layout

```
src/
  domain/
    model/          entities, value objects, newtypes
    event/          domain event definitions — not dispatch
    service/        cross-aggregate rules, fine-grained authorization
    port/           outbound traits the domain owns
  application/      use cases; collects raised events, hands them to EventPublisher
  adapter/
    inbound/        rest · grpc · messaging · task · health · metrics
    outbound/       persistence · storage · eventbus · telemetry · security
  main.rs           composition root
```

Two deviations from the usual diagrams, both forced and both worth naming:

- **`inbound`/`outbound`, not `in`/`out`.** `in` is a Rust keyword. `mod r#in` is legal
  and unpleasant, and `crate::adapter::r#in::rest` would appear in every import in the
  service. Not a trade worth making for two saved characters.
- **No `domain/port/in/`.** Inbound ports are covered in ADR-003; the short version is
  that a use case is a function until it demonstrably needs to be a trait, and a
  directory for a thing that usually should not exist invites creating it.

### Where each concern lives

| Concern | Ring |
|---|---|
| Input **format** validation | inbound adapter — the `TryFrom<Dto> for Command` is the validation |
| Business / invariant validation | domain — `Validate`, or a newtype that cannot hold a bad value |
| Credential extraction | inbound adapter (per protocol) |
| Credential verification | driven adapter, behind the `Authenticator` port |
| Coarse endpoint authorization | inbound adapter, via the `Authorizer` port |
| Data-dependent authorization | domain service, via `Context::authorize` — written once, enforced on every transport (ADR-011) |
| Protocol status mapping | inbound adapter |
| Wiring | composition root |

The authorization split is the one most often got wrong. "May this caller `POST` here at
all" is an endpoint property and belongs to the adapter. "May this caller edit *this*
note" depends on the data, so putting it in the REST handler means re-implementing it in
the gRPC handler and forgetting it in the message consumer.

### DTOs are per-protocol and not shared

Each inbound adapter owns its own wire types. What makes REST, gRPC and a message
consumer interchangeable drivers of the same behaviour is that each converts into the
same **domain command** — the convergence point is the command, not a shared DTO.

Sharing DTOs across protocols looks like deduplication and is not: it couples the JSON
shape to the protobuf shape, so neither can evolve without the other, and it drags
serde derives toward the domain.

### Macros do not hide boundaries

Derives and framework macros for mechanical boilerplate are fine. Custom proc-macros
that generate ports, adapters or wiring are not. If the dependency graph is only
visible after macro expansion, the rule above stops being reviewable, which is the only
enforcement mechanism we have.

## Consequences

- Reviewers have a concrete, checkable rule rather than an aesthetic preference.
- A service is one crate, so `From`/`TryFrom` impls across layers need no newtype
  gymnastics, and incremental builds stay fast.
- Nothing stops a determined author from importing `sqlx` into `domain/`. That is the
  accepted cost of the single-crate decision; the framework makes it visible, review
  catches it.
- The framework can add adapters without touching the core, because adapters depend on
  ports and ports live inside.
