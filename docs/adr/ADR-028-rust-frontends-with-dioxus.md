# ADR-028: Rust Frontends with Dioxus

**Status:** Proposed — after the day-2 features, which it depends on
**Date:** 2026-11-20

## Context

Implementors should be able to write client applications in Rust next to their
backend, compiled to WebAssembly and served by jenny. This is the principle behind
Dioxus fullstack, Next.js and SvelteKit, approached from the other side. Those start
from the frontend and grow a backend; jenny is a deep backend platform that offers an
option of targeted frontends.

WebAssembly that runs inside the server, as sandboxed components, is the other half of
the idea and has a record of its own (ADR-030).

It depends on most of the other day-2 work. Browser authentication needs ADR-023 and
ADR-024. Live views need ADR-022. Anything that adapts to its user needs ADR-025's
scopes.

## Proposal

### The frontend is a client of the API, not a second backend

Dioxus fullstack's server functions are axum handlers that Dioxus registers. Mounted as
they come, they would form a second driving adapter outside the REST adapter: no Cedar
gate, no `Ctx`, no request id, no problem documents. Proposal: the frontend talks to
jenny's own adapters (REST, gRPC-Web, SSE and WebSocket) through a typed client. jenny
serves the built application as an adapter of its own. If server functions are
supported at all, they run as a jenny adapter, behind the same authentication, gate and
`Ctx`.

### Shared types, compiled twice

The DTOs, and the value objects whose rules a form should check before sending, must
compile to `wasm32-unknown-unknown`. `jenny-core` depends on `tokio`, `tokio-util` and
`cron`, so it needs a feature set without a runtime, and an audit of what else does not
build for wasm.

Proposal: a `jenny-client` crate. It provides:

- a typed REST client over a resource's DTOs;
- problem documents decoded into errors;
- `ETag` and `If-Match` handled for the caller;
- a JSON Patch builder;
- an SSE client.

It runs natively and in the browser. It is worth having before Dioxus is: for CLIs,
for other Rust services, and for tests. So it comes first.

### Serving

- A UI adapter mounts the `dx build` output at a path. Hashed assets get immutable cache
  headers, `index.html` none. A content security policy is set. Unknown paths fall back
  to `index.html` for client-side routing, but never under `/api` or `/j`.
- Authentication in the browser follows the backend-for-frontend pattern. jenny runs the
  OIDC authorization-code flow with PKCE and holds the session server-side, behind an
  `HttpOnly`, `SameSite` cookie. Tokens never sit where page scripts can read them, and
  the cookie then needs CSRF protection. This is a security design of its own.
- `cargo jenny new --features ui` adds a `frontend/` workspace member and a `just`
  recipe that runs `dx serve` with the API proxied to the backend.

### Not locked to one framework

Dioxus is pre-1.0, and its APIs change between minor releases. Keeping the integration
at the adapter boundary (built assets, plus a typed client) means a Dioxus release
breaks nothing in jenny, and Leptos or Yew users get the same client. Dioxus is the
framework the scaffold sets up; the rest is framework-neutral.

## Open questions

- Is server-side rendering with hydration needed, or is a single-page application
  enough? Rendering on the server runs Dioxus inside jenny, which brings the
  server-function question back.
- Backend and frontend in one workspace, or separate projects sharing `jenny-client`?

## Consequences

- `jenny-core` must be buildable for wasm, which constrains its future dependencies.
- jenny takes on browser security: sessions, CSRF, CSP.
