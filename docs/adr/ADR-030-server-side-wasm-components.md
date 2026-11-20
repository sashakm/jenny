# ADR-030: Server-Side WebAssembly Components

**Status:** Proposed — after the day-2 features, which it depends on
**Date:** 2026-11-20

## Context

WebAssembly in jenny has two halves. ADR-028 covers code compiled to WebAssembly and
run in the browser. This record covers WebAssembly run inside the service: sandboxed
components that extend a service without recompiling or redeploying it. They may be
written by its implementors, by its operators, or by users whom ADR-025 delegated to.

What a component could do:

- validate a message beyond what types and value objects say: a tenant's own rules for
  an order;
- react to domain events: a team's notification, a transformation, a webhook;
- resolve attributes (ADR-024) from a source the service does not know about.

Cedar policies (ADR-025) say who may do what. Components say what happens, and what is
valid. The two complement each other, and a component is subject to policies like any
other principal.

## Proposal

### wasmtime and the component model

Components implement WIT interfaces that jenny defines. The runtime is `wasmtime`,
embedded in the process, on WASI 0.2, with 0.3's async once it is stable. The component
model is language-neutral, so a component can be written in Rust, Go, JavaScript or
Python, as far as each toolchain produces components. It lives in a crate of its own,
`jenny-wasm`, behind a feature, because wasmtime is a large dependency.

Running components out of process, on a host like wasmCloud or Spin, was considered. Its
isolation is stronger. It also needs another system operated beside the service, for a
feature that should come with a setting, as storage does.

### Extension points, smallest first

1. **Validators**: a pure function from a message to accept, or reject with reasons. No
   host functions at all, so the lowest risk. Built first.
2. **Event handlers**: given an event, return commands, or call granted host functions.
3. **Attribute resolvers** for ADR-024.

### Capabilities, never ambient authority

A component starts with nothing: no filesystem, no network, no clock beyond what WASI
gives every component. Host functions are capabilities, granted per component in its
manifest: read a given store, publish given event kinds, fetch from allowlisted hosts.

Each host call is authorized by Cedar with the component as principal
(`Component::"<id>"`), within the scopes of whoever installed it. A component can never
do more than its installer could.

### Limits

Fuel or epoch interruption bounds CPU time, and memory is capped per instance, as is the
number of instances. A component that traps or runs out fails its invocation. It never
takes the service down, and ADR-026 counts traps, timeouts and durations per component.

### Provenance

A component is signed. Its signature is checked against ADR-023's algorithm policy, by a
key the operator trusts or the installing principal's. The component is kept in `Blobs`
(ADR-018) under its digest, and loading checks both. Installing one is a use case,
authorized and recorded by an event. Compiled modules are cached per architecture, so
replicas do not each recompile.

## Open questions

- Which extension point does the first user need? The proposal starts with validators
  because they need no host functions.
- Can components keep state of their own, through a key-value host function, or only
  through the service's ports?
- Are components reloaded without a restart, like policies (ADR-025)?
- Is a component's WIT world versioned with jenny, or separately, so components outlive
  a framework release?

## Consequences

- Every host function is attack surface, so the WIT worlds stay small and are reviewed
  like the policies.
- A service with `wasm` carries wasmtime's size and compile time.
- Delegation gains logic, not just permissions, which makes ADR-025's bounds matter more.
