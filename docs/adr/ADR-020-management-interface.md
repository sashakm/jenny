# ADR-020: A Dedicated Management Interface

**Status:** Proposed
**Date:** 2026-11-20

## Context

Everything the framework serves lives under `server.framework_path` (`/j`), on the same
listener as the API (ADR-007): health, metrics, the OpenAPI document and Swagger UI.
Keeping that traffic apart is a matter of policy today: an ingress rule that does not
route `/j`, `server.metrics.auth`, a Cedar policy. One ingress that forwards `/` exposes
probes and metrics to the internet, and metrics leak more than people expect (ADR-005).

The tooling around a service already assumes the split. Kubelet probes, a kube-prometheus
`ServiceMonitor` and a `NetworkPolicy` are all simpler when operational traffic has a
port of its own that no `Ingress` ever names. Then the separation holds because the API
port does not serve `/j`, not because every rule in front of it is right.

Other limitations today:

- `telemetry.prometheus_port` suggests a port of its own, but it only switches the
  exporter on; `/metrics` is mounted under `/j` on the main listener. A setting named
  after a port that binds none is a trap.
- The OpenAPI document is served whenever an adapter supplies one, and Swagger UI
  whenever the `swagger-ui` feature is on. Neither can be switched off by
  configuration.
- The request trace layer wraps every route, so every probe and every scrape is a
  request span and an access-log line, at one per few seconds per replica.

## Proposal

### A second listener, opt-in

`server.management.address` (`[::]:9000`, say). Unset, everything is on one listener,
which suits `cargo run` and tests. Set, `JennyServer::bind` binds both, and the framework's
endpoints move to the management listener unless configured otherwise. The API listener
then answers `/j/…` with 404, whatever sits in front of it.

The paths stay the same on both: `/j/health/ready` on the management port, too. Serving
at the root of the management port (`/metrics`, `/healthz`), as some tools default to,
was considered. Two path layouts depending on one setting is how a probe ends up broken
in exactly one environment, and every tool takes a path.

### Each endpoint group is configured on its own

Implementors and operators decide, per group, whether it is served, where, and at which
path:

| Group | Serves | Default with a management listener |
|---|---|---|
| `probes` | `health/live`, `health/ready` | management |
| `status` | `health/status`: versions, every check, its timing and error | management |
| `metrics` | the Prometheus scrape | management |
| `openapi` | the OpenAPI document | management |
| `swagger` | Swagger UI | management |
| `public_status` | `up`, `degraded` or `down`, nothing else | off |

```yaml
server:
  management:
    address: "[::]:9000"
  endpoints:
    openapi: { listener: api, path: /docs/openapi.json }
    swagger: { enabled: false }
    public_status: { enabled: true, listener: api, path: /status }
```

Every group can be switched off, `openapi` and `swagger` included. A group can be served
on the API listener at a path of its own, so an API's documentation, or a status page
for its users, can be public while probes and metrics stay internal.

`public_status` exists because `status` is not fit for the public. It names every check
and reports its error, which describes the infrastructure behind the service. The public
variant says how the service is, and nothing about why.

gRPC health and reflection (ADR-021) are groups like these.

### No access log for probes and scrapes, by default

`probes`, `status`, `public_status` and `metrics` produce no request span, no access-log
line and no trace. Each group has `log_requests: true` for operators who want them.
They still pass through the request-id layer, so a failure inside one is logged with an
id like any other.

### Lifecycle

A port conflict on either listener fails startup before anything serves. On shutdown,
readiness turns false first, the API drains, and the management listener stops last, so
a load balancer sees "not ready" for the whole drain instead of a refused connection.
The rest of the order (ADR-016's relay, the broker, the pool) is unchanged.

### What the management port trusts

Probes stay unauthenticated, as kubelet presents no credentials. `server.metrics.auth`
keeps working. The management listener can bind to a different interface than the API
(the pod IP only, or loopback for a sidecar), and with ADR-023 it can require a client
certificate.

`telemetry.prometheus_port` goes. The exporter is on when `telemetry-prometheus` is, and
where it is served is the `metrics` group's business.

## Open questions

- Should the scaffold turn the split on? Proposal: on in the Helm chart (ADR-027), off in
  the generated `config.yaml`, so `cargo run` keeps one port.
- Does `public_status` count as a readiness signal, or report only liveness, so a public
  page never shows a dependency's outage?

## Consequences

- An ingress cannot publish what is not on the port it routes to.
- Two ports to expose in the container image, the chart and the docs.
- Breaking: `server.health`, `server.metrics.enabled` and `telemetry.prometheus_port`
  give way to `server.endpoints`.
