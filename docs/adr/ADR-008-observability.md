# ADR-008: Observability

**Status:** Accepted — use-case metrics built (ADR-012); HTTP metrics and trace propagation not implemented
**Date:** 2026-08-24

## Context

jenny targets production services, so structured logging, distributed tracing and metrics
are requirements rather than extras. The constraint that shapes the design is that
service authors should instrument their code without learning, or depending on, whichever
backend is deployed.

## Decision

### `tracing` is the instrumentation API; the subscriber is a wiring decision

All framework code and all service code instruments with `tracing` — `info!`, `error!`,
`#[instrument]`. Nothing outside the telemetry module names an exporter.

Which backends receive that is decided once, at startup, from configuration. Service
authors never depend on OpenTelemetry directly. This is the same inversion as everywhere
else in the framework: instrumentation is a port, exporters are adapters.

### Each backend must work alone

Not a design decision so much as a scar. The subscriber composition was gated on
`all(telemetry-otlp, telemetry-loki)`, so a service enabling only OTLP built its exporter
layers and then dropped them — it compiled, tested green under `--all-features`, and
exported nothing. Enabling only `telemetry-loki` or only `telemetry-prometheus` did not
compile at all.

`--all-features` cannot catch this class of bug by construction. `just features` now
builds each backend and each pair on its own (ADR-002).

### Backends

| Signal | Backend | Feature |
|---|---|---|
| Logs | stdout, pretty or JSON | always |
| Logs + traces | OTLP → OTel Collector | `telemetry-otlp` |
| Logs | direct Loki push | `telemetry-loki` |
| Metrics (pull) | Prometheus `/metrics` | `telemetry-prometheus` |
| Metrics (push) | OTLP → Collector | `telemetry-otlp` |

The **recommended production path is OTLP to an OpenTelemetry Collector**, which fans out
to whatever is downstream and provides buffering, retry and sampling without the service
knowing. Direct Loki push and Prometheus scraping exist as simpler alternatives when
there is no Collector.

A note, because the requirement is often phrased this way: **metrics do not go to Loki.**
Loki ingests log streams, not time series. Prometheus (or Mimir/Thanos for long-term
storage) is where metrics go; both are usually read through Grafana.

### Configuration and lifetime

The `telemetry` section of the config file deserialises directly into `TelemetryConfig`,
and the runtime initialises the stack during bootstrap — before anything else runs, so no
instrumentation is lost to a late subscriber.

`init()` returns a `TelemetryHandle` that must be held for the process lifetime. Dropping
it flushes pending spans, log records and metric points. The runtime holds it and drops
it after the server stops, so a graceful shutdown does not lose the last few seconds of
telemetry — which is exactly the window that matters when diagnosing a shutdown.

### Correlation

Every driving adapter builds a `Context` with a request id (ADR-003). The REST adapter
reuses an inbound `x-request-id` when there is one, so an id set by a gateway or an
upstream service survives the hop, and mints one otherwise.

That same id is the `instance` field of a `Problem` (ADR-005). A user quoting an error
message is quoting something greppable in the logs.

### Not implemented

Recorded here rather than left as a surprise:

- **Framework metrics.** `jenny.usecase.duration`, `jenny.usecase.total`,
  `jenny.usecase.errors`, and per-adapter HTTP metrics (`http.server.request.duration`,
  `http.server.active_requests`) are designed but not built.
- **W3C trace-context propagation.** Extracting `traceparent` from inbound requests and
  injecting it into outbound ones is not wired. `tower_http`'s tracing layer gives
  per-request spans today, but they are not linked into a distributed trace across
  service boundaries.

Both are adapter-level concerns that can be added without touching `jenny-core`, which is
why neither blocks the layering work.

## Consequences

- Services instrument with one API and choose backends in configuration.
- `jenny-server` carries the OTel SDK and `tracing-subscriber`; `jenny-core`, which the
  domain depends on, carries only `tracing`. The exporters are feature-gated, so a
  service that logs to stdout does not compile OTLP.
- Every request gets an id before anything else sees it (`RequestIdLayer`): kept from an
  inbound `x-request-id` UUID, echoed in the response, recorded on the request span,
  used as the `Context` request id and as the `instance` of every problem document.
- Telemetry is up before the first line of service code runs and flushed after the last.
- Distributed traces do not yet span service boundaries; single-service tracing works.
