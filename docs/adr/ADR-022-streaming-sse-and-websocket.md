# ADR-022: Streaming with Server-Sent Events and WebSocket

**Status:** Proposed
**Date:** 2026-11-20

## Context

Clients want to be told when something happens: a note changed, an export finished, a
job moved on. Today they poll. The events already exist: aggregates raise them
(ADR-012), they reach the in-process bus, and with the outbox they reach it once
committed (ADR-016). What is missing is a way out to a client, and the runtime is built
around requests that end:

- the graceful drain waits for in-flight requests, and a stream never finishes;
- authorization is decided once per request, and a stream outlives the decision;
- the in-process bus delivers in one replica, and a client is connected to one replica,
  usually not the one that handled the write;
- consuming events is inbound dispatch (ADR-006), whose handlers so far call use cases,
  not connections.

## Proposal

### SSE first, WebSocket where the client talks back

**Server-Sent Events** (`text/event-stream`, axum's `Sse`) for server-to-client streams.
SSE is plain HTTP, passes proxies and HTTP/2, and browsers reconnect by themselves with
`Last-Event-ID`. It needs nothing new from the gate, because opening a stream is a `GET`
on a route template, authorized like any other.

**WebSocket** (axum's `ws`) where the client sends as well: collaborative editing,
interactive sessions. The upgrade is a `GET` and is authorized at the handshake. Messages
after it are not requests, so authorizing each one is the use case's job, with the
`Context` of the handshake.

gRPC server streaming (ADR-021) shares the same source.

### The source is inbound dispatch

A stream subscribes to events through the inbound dispatch ADR-006 built: a stream is a
subscription (`JennyServer::handle`, a `BusHandler`) whose handler writes to a
connection. With the in-process bus a client sees only the events of the replica it is
connected to. With NATS subjects every replica sees every event. Proposal: streams work
on the in-process bus for one replica and development, and multi-replica streaming
requires NATS. Both are documented where streams are configured, because a stream that
silently misses half the events is the failure everyone hits first.

### Which events, to whom

Events are internal contracts, so a stream exposes an explicit list of event kinds, not
everything on the bus. Each event carries the resource it is about, and a stream opened
for `/notes/{id}/events` delivers that note's events only. Evaluating Cedar for every
event is possible, at a cost per event and per subscriber. Proposal: authorize the
subscription, filter by resource, and leave per-event checks to streams that need them.

### Resuming

`Last-Event-ID` can only resume from a log that can be replayed: a JetStream stream, or
the outbox table (ADR-016), keyed by the event id that already travels as the `event-id`
header (ADR-006). Without one, a reconnecting client gets new events only, and the stream
says so with a marker event, so the client knows to re-read.

### The runtime around long-lived responses

- On shutdown, streams end when the drain starts. SSE ends the response, and the client
  reconnects to another replica. WebSocket sends close code 1001 (going away). The drain
  then does not wait on them.
- Heartbeats (SSE comment lines, WebSocket pings) keep idle-timeout proxies from cutting
  the connection.
- Limits on open streams per instance and per principal. A slow client is disconnected,
  never buffered without bound.
- Request timeouts exempt streams, and ADR-026 counts open streams, messages and drops.

### A live resource, from pieces already built

A `change(…)` can already publish `<kind>.updated`, carrying the id, the new version and
what changed as an RFC 6902 patch (`Changes`, ADR-012). A client that read a resource at
`ETag: "3"` could subscribe to its changes and receive the patches from version 3 on,
each tagged with the version it produces. That makes a live view without a new wire
format.

## Open questions

- The WebSocket envelope: with ADR-021 each frame is a protobuf message, binary or as
  proto3 JSON. Is a subprotocol needed beyond that, to negotiate which?
- Is the live resource wanted, and through SSE only or also over WebSocket and gRPC?

## Consequences

- Streaming depends on messaging inbound dispatch, and multi-replica streaming on NATS.
- Ingress and load-balancer timeouts become deployment settings that matter (ADR-027).
