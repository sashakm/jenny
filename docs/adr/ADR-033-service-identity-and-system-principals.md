# ADR-033: Service Identity and System Principals

**Status:** Accepted — implemented
**Date:** 2026-11-28

## Context

Enriching events with who raised them (ADR-032) and consuming events from JetStream
(ADR-006) both need two things jenny did not have.

**A name for the service.** It existed only as `telemetry.service_name`, a telemetry
setting that also, by fallback, named the bus namespace, the NATS buckets and the
service in `/health/status`. The version came from `JennyServer::with_version`, usually
called after `events()` in `main`, so nothing wired earlier could read it. JetStream
consumers need a stable, durable name; event headers need the service and its version;
both need them at wiring time.

**A principal for the service itself.** A scheduled task built its own `Context::new()`
with no principal, and an inbound handler would have had the same problem. A use case
that calls `cx.authorize(…)` then fails with `Unauthenticated`, or the task skips
authorization altogether. Writing a Cedar policy for "the nightly cleanup may archive
notes" was impossible, because there was nobody to name.

The obvious shortcut, a well-known id such as `system`, is unsafe. `CedarAuthorizer`
maps every principal to `User::"<id>"`, and a principal's id is whatever its credential
says: any JWT whose `sub` is `system` would hold the service's own permissions.

## Decision

### `jenny::service!()` and a top-level `service_name`

`jenny::service!()` (in `jenny-core`, re-exported by the facade) expands where it is
written to a `Service { name, version }` from `CARGO_PKG_NAME` and `CARGO_PKG_VERSION`,
for the same reason `service_version!()` is a macro: `env!` in framework code would read
the framework's manifest. `JennyServer::from_args(service)` and
`from_parsed_args(args, service)` take it, so the identity exists before anything is
wired.

Configuration overrides it with the new top-level keys `service_name` and
`service_version` (`JENNY__SERVICE_NAME`, `JENNY__SERVICE_VERSION`).
`ServerConfig::identify` resolves the pair once, and the result is the default for:

- `telemetry.service_name` / `telemetry.service_version`, now optional; set, they rename
  the service in telemetry only;
- `messaging.namespace`;
- `/health/status`'s `service` and `service_version`;
- consumer names, `source-*` headers and system principals.

An empty name after resolution is a startup error, because all of those are built from
it. `with_version` and `DEFAULT_EVENT_NAMESPACE` are gone: the macro always supplies a
version and a name.

Rejected: keeping the name in `telemetry` and reading it from there. It conflated "what
traces call the service" with "what the service is", and one cannot be renamed without
the other.

### `PrincipalKind`: the service is a different kind of principal

`Principal` gets a kind, `PrincipalKind::{User, System}`. `Principal::system(service,
component)` is the only constructor of a system principal; every authenticator and
`PrincipalBuilder::build` make users. The kind is `#[serde(skip)]`, so deserialising a
principal (a JWT's claims, a stored API-key record) always yields a user, whatever it
claims, and the serde shape did not change.

A system principal's `id()` is `<service>/<component>`. It displays, and is named as a
caller (`Principal::subject`, the `caller-subject` header), as
`System[<service>/<component>]`. The component is a free string: `tasks` for scheduled
tasks and `events` for inbound handlers today, others as adapters appear. Roles come
from `system.roles` in configuration (default empty), set with `Principal::with_roles`.

Rejected: an id convention such as `System[…]` alone. The kind exists because ids are
forgeable; a convention would be one more string a token could carry.

### Cedar: the entity type `System`

`CedarAuthorizer` maps a system principal to `System::"<service>/<component>"`
(`SYSTEM_TYPE`) and a user to `User::"<id>"` as before. Roles are `Role` parents for both.
With a schema, `System` keeps the attributes the schema declares for it, and has to be
declared for a system principal to be authorized at all; anything else cannot be
evaluated and denies (ADR-011). A test shows that users with the ids `my-notes/tasks` and
`System[my-notes/tasks]` match no policy written for `System::"my-notes/tasks"`.

Rejected: per-component roles in configuration. Cedar can already address
`System::"my-notes/tasks"` directly, so a policy is the place for that.

### Tasks get a `Context`

`Task::execute(&self)` becomes `execute(&self, cx: &Context)`. `TaskScheduler` builds a
fresh `Context` for every run: a new request id, the label `task=<name>`, and what it was
told with `running_as` and `with_authorizer`. The runtime makes that
`System[<service_name>/tasks]` with `system.roles`, and the authorizer given to the new
`JennyServer::with_authorizer`. A task is then a driving adapter like a REST handler: it
passes `cx` to the use case, and `cx.authorize(…)` gets a real answer.

`JennyServer::with_authorizer` is needed because the runtime had no authorizer of its own:
REST adapters carry theirs in their `Authorization` layer. It must come before
`handle(…)`, whose dispatchers take the authorizer when they are built.

## Consequences

- Policies can grant the service itself exactly what it needs, by component, and no
  credential can borrow that grant.
- Tasks and event handlers call use cases the same way requests do, with a principal and
  an authorizer.
- Breaking: `from_args` / `from_parsed_args` / `from_config` take a `Service`;
  `with_version` is removed; `Task::execute` takes `cx`; `telemetry.service_name` is
  optional.
- A Cedar schema that is to admit system principals must declare `System`.
- Serialising a system principal drops its kind, so it cannot round-trip as one. That is
  intended: the kind is never taken from data.
