# ADR-021: Protobuf Contracts, over gRPC, Binary HTTP and JSON

**Status:** Proposed
**Date:** 2026-11-20

## Context

REST with JSON is the only driving adapter, and its DTOs are Rust types: serde gives
their JSON, and `#[derive(Dto)]` their validation and, with the `openapi` feature of
`jenny-macros`, their OpenAPI schema (ADR-005). The core was built for more than one
adapter. `Criteria` is adapter-neutral (ADR-010), and `Context` carries the request id
without knowing about HTTP (ADR-003). `DomainError::Stale` was given a gRPC meaning,
`FAILED_PRECONDITION`, when it was introduced (ADR-014).

The goal is to use protobuf's strict contract to define the DTOs, and let each driving
adapter speak whichever protocol it wants:

- gRPC;
- HTTP with binary protobuf bodies;
- HTTP with JSON that follows the protobuf schema.

jenny is Rust-first, and a service's implementation interface should be the same
whichever of these it enables.

## Proposal

### `#[derive(Dto)]` stays, and the features decide what it emits

The derive already changes with a feature: under `openapi` it emits `utoipa` impls,
decided when `jenny-macros` compiles (ADR-005). `protobuf` is one more such feature.

| `jenny-macros` features | `#[derive(Dto)]` emits |
|---|---|
| none | validation |
| `openapi` | validation, the OpenAPI schema |
| `protobuf` | validation, `prost::Message`, a message descriptor |
| both | all of it |

The DTO is the Rust type, and the `.proto` is derived from it. A service's handlers and
use cases do not change when it turns `protobuf` on.

Writing the `.proto` first and generating Rust from it was considered: the usual way,
and the one a multi-language organisation that owns its contracts in `.proto` files
wants. It puts a code generator and `protoc` into every build, and makes the DTOs
generated code instead of the service's own types. A Rust-first framework can do better:
the types exist already, so only the contract has to be derived.

### No protobuf compiler in the build

A `build.rs` runs before the crate it builds is compiled, so it cannot collect what that
crate's derives emit. Nothing needs it to. The derive implements `prost::Message`
directly, so the Rust side never needs generated code, and the encoding needs no
`protoc`. Whether that goes through prost's public `encoding` module or by expanding to
prost's own derive is for a spike to settle. The first ties jenny to a module prost
documents as meant for generated code.

The `.proto` file is an export, assembled from the descriptors the derives register, as
the OpenAPI document is assembled from `utoipa` schemas today. A test, or
`cargo jenny proto`, writes it, and the same descriptors answer gRPC reflection.

### The contract is still a contract

Field numbers are protobuf's compatibility promise, and a derive must not hide them:

- Under `protobuf`, every field needs `#[dto(tag = n)]`, and a missing tag is a compile
  error. Numbering by declaration order would make reordering two fields a silent
  break.
- A removed field's number is kept with `#[dto(reserved(4))]`, so it is never reused.
- The exported `.proto` is committed. A test compares it with what the derives produce,
  and fails on a breaking change (a changed number or type, a reused tag), the way
  `buf breaking` does. A deliberate break updates the committed file in the same commit.

The committed `.proto` is what other languages generate their clients from. It is
derived, but it is reviewed and guarded like a hand-written one.

### JSON is serde's, made to match the schema

The JSON path stays serde. ADR-014's JSON Patch machinery (`jenny_rest::Json`,
`serde_ignored`, the `PUT` body as the allowlist) therefore works unchanged. To make that
JSON the schema's JSON:

- The derive writes serde's field names into the `.proto` as `json_name`, so a proto3
  JSON parser in another language reads exactly what serde writes.
- Rust types map onto protobuf types the canonical way: `Option<T>` is proto3
  `optional`, `Vec<T>` `repeated`, maps `map`, a nested `Dto` a message, `Uuid` a
  string, `DateTime<Utc>` a `google.protobuf.Timestamp`. Unit enums become protobuf
  enums with a `…_UNSPECIFIED = 0` first value, which decodes as an error. Enums with
  data become `oneof`s.
- A serde attribute the schema cannot express (`flatten`, untagged enums) is a compile
  error under `protobuf`, not ignored as it is today. A mismatch here would break the
  contract silently.

The derive's validation constraints (`min_len`, `max`) could also be written into the
`.proto` as protovalidate annotations, so clients in other languages check the same
rules. That is optional and later.

### One extractor, every encoding

`jenny-rest` gets a body extractor and responder that negotiates by `Content-Type` and
`Accept`: JSON always, and `application/protobuf` when the feature is on. Any other body
is 415, and an `Accept` that matches neither is 406. A handler written against it today
serves protobuf tomorrow without a change. `Dto` then applies to response types too,
not only inbound ones, since a response must encode as well. Validation is not run on the
way out.

### gRPC services through a builder

`jenny-grpc` is a crate in the driving-adapter ring, on `tonic`. It uses tonic's server
side with a prost codec and no generated code. Services are declared with a builder, as
REST resources are with `Routes`:

```rust,ignore
GrpcRoutes::service("notes.v1.Notes")
    .unary("Get", get_note)
    .unary("Update", update_note)
    .server_streaming("Watch", watch_note)
```

A macro over a trait was considered and rejected: generating an adapter is what ADR-001
keeps macros from doing. The builder registers each method's descriptor, so the exported
`.proto` contains the services as well as the messages.

| Concern | gRPC |
|---|---|
| request id | `x-request-id` metadata, minted when absent |
| authentication | `authorization` metadata, through the same `Authenticator` port |
| authorization | the Cedar gate asks about `Method::"/notes.v1.Notes/Get"` inside the service's resource and adapter URNs, as ADR-011 does for routes |
| failures | `DomainError` → `tonic::Status`, decided once as `Problem` is for HTTP: `NotFound` → `NOT_FOUND`, `Invalid` → `INVALID_ARGUMENT`, `Conflict` → `ALREADY_EXISTS` or `ABORTED`, `Stale` → `FAILED_PRECONDITION`, `Internal` → `INTERNAL` with the detail logged, never sent |
| health | `grpc.health.v1` answered from the same `HealthCheck`s, an endpoint group of ADR-020 |
| reflection | from the registered descriptors, an endpoint group of ADR-020 |

An expected version travels as a field (`expected_version`), not as metadata. gRPC has
no conditional-request semantics of its own, and a field shows up in the contract.
Listings take REST's filter syntax (ADR-010) in a string field, parsed by the same code
into the same `Criteria`.

REST and gRPC share a listener by default, routed by `content-type: application/grpc`.
That takes HTTP/2: h2c inside a cluster, or ALPN once the server terminates TLS
(ADR-023). `server.grpc.address` gives gRPC a port of its own, for load balancers and
ingresses that need one.

### Where proto-first still applies

- **Calling someone else's gRPC API** is an outbound adapter, generated from their
  `.proto` with `prost-build` and `tonic-build`, as anywhere else.
- **Implementing a contract someone else owns** in a `.proto` does not fit Rust-first.
  Such a service uses tonic's generated server directly, inside an adapter of its own,
  and still calls its use cases.

## Open questions

- Does the derive generate prost's encoding itself, or expand to prost's derive? (The
  spike.)
- Should event payloads (ADR-006) be protobuf too, from the same derive, so consumers in
  other languages share the contract?
- Package names and versions (`notes.v1`): one per service, set in configuration, or one
  per resource?

## Consequences

- One derive, one set of types, one handler signature, whichever protocols are on.
- The DTOs stay plain Rust, so a Rust client, or a Dioxus frontend (ADR-028), uses the
  same types through `jenny-client` without generated code.
- Builds need no protobuf compiler.
- Field numbers become explicit wherever `protobuf` is on, and the committed `.proto`
  adds a contract check to the test suite.
- `#[derive(Dto)]` grows from validation and OpenAPI into the definition of a wire
  contract, so its mapping rules need the same care the Cedar gate gets.
