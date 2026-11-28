# ADR-020: A Dedicated Management Interface

**Status:** Accepted — implemented
**Date:** 2026-11-20 (accepted 2026-11-28)

## Context

Everything the framework serves lived under `server.framework_path` (`/j`), on the same
listener as the API (ADR-007): health, metrics, the OpenAPI document and Swagger UI.
Keeping that traffic apart was a matter of policy: an ingress rule that does not route
`/j`, Basic auth on the scrape, a Cedar policy. One ingress that forwards `/` exposes
probes and metrics to the internet, and metrics leak more than people expect (ADR-005).

The tooling around a service already assumes the split. Kubelet probes, a kube-prometheus
`ServiceMonitor` and a `NetworkPolicy` are all simpler when operational traffic has a
port of its own that no `Ingress` ever names. Then the separation holds because the API
port does not serve `/j`, not because every rule in front of it is right.

Other limitations were:

- `telemetry.prometheus_port` suggested a port of its own, but it only switched the
  exporter on; `/metrics` was mounted under `/j` on the main listener. A setting named
  after a port that binds none is a trap.
- The OpenAPI document was served whenever an adapter supplied one, and Swagger UI
  whenever the `swagger-ui` feature was on. Neither could be switched off by
  configuration.
- The request trace layer wrapped every route, so every probe and every scrape was a
  request span and an access-log line, at one per few seconds per replica.

## Decision

### A second listener, opt-in

`server.management.address` (`[::]:8081`; port `0` picks a free one). Unset, everything
is on one listener, which suits `cargo run` and tests. Set, `JennyServer::bind` builds two
routers and binds two sockets, and the framework's endpoints move to the management
listener unless configured otherwise. The API router then holds the REST adapters only,
and answers `/j/…` with the usual 404 problem document, whatever sits in front of it.
`ServerParts::management_addr()` reports the resolved address, as `local_addr()` does for
the API; startup logs both.

The port in docs and examples is **8081**, not 9000: the scaffold's `just run-objects`
already uses `localhost:9000` for its S3-compatible store.

The paths stay the same on both: `/j/health/ready` on the management port, too. Serving
at the root of the management port (`/metrics`, `/healthz`), as some tools default to,
was considered and rejected. Two path layouts depending on one setting is how a probe
ends up broken in exactly one environment, and every tool takes a path.

### Each endpoint group is configured on its own

`server.endpoints.<group>` decides, per group, whether it is served (`enabled`), where
(`listener: api | management`), at which path (`path`), and whether it is access-logged
(`log_requests`). A key left out keeps the group's default:

| Group | Routes (default path) | `enabled` | `log_requests` |
|---|---|---|---|
| `probes` | `<fp>/health/live`, `<fp>/health/ready` | true | false |
| `status` | `<fp>/health/status`: versions, every check, its timing and error | true | false |
| `metrics` | `<fp>/metrics`, the Prometheus scrape | true (with `telemetry-prometheus`) | false |
| `openapi` | `<fp>/openapi.json` | true (with `openapi`, when an adapter supplies a document) | true |
| `swagger` | `<fp>/swagger`, Swagger UI | true (with `swagger-ui`) | true |
| `public_status` | `<fp>/health/public`: `up`, `degraded` or `down` | **false** | false |

`<fp>` is `server.framework_path`. Without `management` every group defaults to the API
listener; with it, to the management listener.

```yaml
server:
  management:
    address: "[::]:8081"
  endpoints:
    openapi: { listener: api, path: /docs/openapi.json }
    swagger: { enabled: false }
    public_status: { enabled: true, listener: api, path: /status }
    metrics:
      auth: { username: prometheus, password: change-me }
```

`path` replaces the default path outright, `framework_path` included. For `status`,
`metrics`, `openapi` and `public_status` it is the full path of the one route; for
`probes` and `swagger` it is a prefix (`<path>/live` and `<path>/ready`; the UI under
`<path>`). A group can thus be served on the API listener at a path of its own, so an
API's documentation, or a status page for its users, can be public while probes and
metrics stay internal.

Each group is a `Router` of its own, wrapped in `TraceLayer` only when it logs requests,
and merged into the router of its listener (`crates/jenny-server/src/endpoints.rs`
decides where; `server.rs` assembles). Both routers get `RequestIdLayer` outermost, the
`not_found` / `method_not_allowed` problem fallbacks and `DefaultBodyLimit`.

What cannot work stops the service in `bind`, before anything connects or binds, with an
error naming the key at fault:

- a group on `listener: management` while `server.management` is unset;
- two routes clashing on one listener: two groups, or a group inside a REST adapter's
  mount (an adapter mounted at the root claims only its own routes);
- `swagger` enabled while `openapi` is disabled, or the two on different listeners;
- a `path` with `{…}`, `*`, a `:` segment or `//`, which axum reads as a parameter or
  refuses, and `swagger` at `/`, which would claim every path.

### `metrics.auth` moves under `endpoints`

The scrape's Basic credentials are `server.endpoints.metrics.auth`, beside the rest of
the group's settings, rather than staying at `server.metrics.auth` with the group
configured elsewhere. Probes stay unauthenticated, as kubelet presents no credentials.

### Swagger UI depends on the `openapi` group

Swagger UI is configured with the `openapi` group's URL only and no longer serves a copy
of the document. Keeping its own copy would have meant that switching `openapi` off
leaves the document online at a second path. Hence the startup errors above: without
the group, or on another listener, the UI would load nothing.

### No access log for probes and scrapes, by default

`probes`, `status`, `public_status` and `metrics` produce no request span and no
access-log line; `log_requests: true` restores both. They still pass through
`RequestIdLayer`, so the response carries an `x-request-id`, and the framework's own
handlers that log (the scrape, when encoding fails) put that id in the line themselves.
A `HealthCheck` a service registers runs inside a probe without a span, so its own log
lines carry no request id unless `probes` (or `status`) logs requests; making the log
formatter read the request id without a span is left to ADR-026. The REST adapters, the
documentation groups and the 404 fallback are traced as before.

### `public_status` reports readiness, coarsened

`status` is not fit for the public: it names every check and reports its error, which
describes the infrastructure behind the service. `public_status` says how the service is,
and nothing about why. It answers `{"status": …}` and nothing else:

- `up`, HTTP 200: ready (started, not stopping, every check up);
- `degraded`, HTTP 200: started and not stopping, at least one check failing;
- `down`, HTTP 503: not started yet, or stopping.

A liveness-only variant, so a public page never shows a dependency's outage, was
rejected: a page that says "up" while every request fails is worse than one that says
"degraded".

### Lifecycle

Both sockets are bound before `health.mark_started()`; if either bind fails (a port in
use), `bind` returns an error naming `server.address` or `server.management.address`, and
no listener serves. On shutdown:

1. readiness turns false;
2. `shutdown_delay`;
3. the API listener closes and drains;
4. the outbox relay, the broker, the NATS connection and the Postgres pool stop, in
   ADR-016's order;
5. the management listener closes last, before telemetry is dropped.

During steps 2–4 the management listener keeps answering, so a load balancer or kubelet
sees `/health/ready` report `stopping` with a 503 rather than a refused connection. The
deadline that bounded the drain (`shutdown_delay + shutdown_timeout`) also bounds the
management listener's own drain.

### What else changed

Removed outright, with no compatibility shim or special error, because nothing has been
released: `server.health`, `server.metrics` (`MetricsConfig`),
`TelemetryConfig::prometheus_port` and `with_prometheus_port`. The Prometheus exporter is
on whenever `telemetry-prometheus` is; where it is served is the `metrics` group's
business. An old key is now an unknown key, ignored like any other.

### Left to other decisions

- Whether the management listener is on by default in the Helm chart is ADR-027's call;
  the generated `config.yaml` leaves it commented out, so `cargo run` keeps one port.
- gRPC health and reflection become groups like these with ADR-021.
- A client certificate on the management listener comes with ADR-023's mTLS.

### Alternatives considered

- Two path layouts (root-level `/metrics`, `/healthz` on the management port): see above.
- Keeping `server.metrics.auth` where it was, and a compatibility error for removed keys:
  both keep a second place to look for one group's settings, for users who do not exist.
- Swagger UI keeping its own document copy: the document would outlive `openapi: false`.
- Splitting the listener and the per-group configuration into two changes: the second
  listener's defaults only make sense with the groups.

## Consequences

- An ingress cannot publish what is not on the port it routes to.
- Two ports to expose in the container image, the chart and the docs; the generated
  `Dockerfile` keeps `EXPOSE 8000` and says where the second one comes from.
- A probe or scrape no longer floods the access log, at the cost of having to turn
  `log_requests` on to see them.
- Breaking: `server.health`, `server.metrics` and `telemetry.prometheus_port` give way to
  `server.endpoints`; Swagger UI no longer serves its own copy of the document.
