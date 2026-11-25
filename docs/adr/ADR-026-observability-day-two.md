# ADR-026: Observability for Day 2 — Meters, Instrumentation, Log Modes

**Status:** Proposed
**Date:** 2026-11-20

## Context

What ADR-008 built:

- `tracing` as the one instrumentation API;
- logs to stdout (pretty or JSON), over OTLP to a collector, or pushed to Loki;
- traces over OTLP;
- metrics pulled by Prometheus or pushed over OTLP;
- use-case metrics, through the `observe_use_cases` hook that `jenny-server` installs
  after the meter provider.

What it did not:

- **Meters for services.** ADR-008 keeps OpenTelemetry out of service code, and jenny
  offers nothing in its place. A service that wants a counter of its own has no
  supported way to make one.
- **Framework instrumentation** beyond use cases: HTTP metrics are designed but not
  built, and the stores, the outbox, the bus, authentication and tasks report nothing.
- **Trace context across boundaries.** `traceparent` is neither extracted nor injected,
  so a trace stops at the service's edge, and at the outbox.
- **Log modes per sink.** One `log_format` and one `debug` flag apply to every sink at
  once. Human-readable logs on stdout and full-fidelity records to the collector cannot
  be had together.

## Proposal

### Meters as a jenny primitive

`jenny::metrics`: counters, histograms, up-down counters and gauges, named under the
service's meter. They follow the pattern `observe_use_cases` already uses. `jenny-core`
defines the handles and a process-wide recorder hook, and `jenny-server` installs the
OTel-backed recorder once the meter provider exists. Two things follow:

- the core takes no OpenTelemetry dependency;
- an instrument made before telemetry starts still records once it has. One made from
  the global provider before then would stay a no-op for good.

The `metrics` crate (metrics-rs) was considered and not chosen: services would have one
metric API and the framework another. Units are required. Names follow OpenTelemetry's semantic
conventions where one exists.

### Instrument what the framework owns

| Area | Metrics | Spans |
|---|---|---|
| REST | `http.server.request.duration`, `http.server.active_requests`, body sizes, by method, route template and status | per request (exists), named by route template |
| gRPC (ADR-021) | `rpc.server.duration` | per call |
| streams (ADR-022) | open streams, messages, drops | per stream |
| Postgres | pool size, idle, waiting; query duration | per query, statement name only, never values |
| outbox | relay lag (age of the oldest undelivered row), delivered, failed | write → relay → publish, linked |
| NATS KV, objects, graph | operation duration and failures | per operation |
| event bus | published, delivered, handler failures | per delivery |
| authentication, authorization | failures by reason; denials by policy id | — |
| tasks | run duration, failures, time of last success | per run |
| runtime | build info (service and framework version) | — |

Labels come from bounded sets only: route templates, policy ids, store names. Never ids
or raw paths: one label from user input is enough to make a metrics backend fall over.
Histograms carry exemplars, the trace id of a sampled request, so a slow bucket leads to
a trace.

### Trace context everywhere

W3C `traceparent` and `tracestate` are extracted on REST, gRPC and streams, written into
event headers by an enricher (ADR-032), stored with outbox rows, and restored by the
relay. A trace then spans request → commit → relay → consumer.

### Log modes per sink

```yaml
telemetry:
  logs:
    stdout: { format: pretty, level: info }  # pretty | compact | json
    otlp:   { level: debug }                 # every record, with span context
```

Each sink gets its own filter, in `RUST_LOG` syntax, and stdout its own format. A
human-readable line carries a short trace id, so a line on a terminal leads to its
trace. OTLP can be sent over HTTP/protobuf as well as gRPC (`otlp.protocol`), because
some proxies and agents only pass HTTP.

## Open questions

- Keep `telemetry-loki`? With OTLP through Alloy to Loki as the documented path, direct
  push is a second way to do the same thing. Proposal: remove it.
- Head sampling in jenny, or leave sampling to the collector? Proposal: a ratio setting
  here, and tail sampling at the collector.
- Do a scheduled task's runs start their own traces, or link to the one that scheduled
  them?

## Consequences

- Every metric above needs a cardinality review when it is added, not after the bill.
- `telemetry.prometheus_port` goes with ADR-020, and the `telemetry` section changes
  shape: breaking.
