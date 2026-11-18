---
description: Review the current diff (or a given ref range) against jenny's hexagonal layering rules
argument-hint: "[git ref range, default: uncommitted changes]"
allowed-tools: Bash(git diff:*), Bash(git log:*), Bash(git status:*), Bash(cargo tree:*), Read, Grep, Glob
---

Review the changes in `$ARGUMENTS` (if empty: `git diff HEAD` plus untracked files from
`git status --short`) against jenny's dependency rule. Read ADR-001 and ADR-002 in
`docs/adr/` first if they are not already in context.

Check each of these, citing `file:line` for every finding:

1. **The core stays infrastructure-free.** Nothing under `crates/jenny-core/src/domain/`
   or `crates/jenny-core/src/port/` imports axum, tower, http, sqlx, async-nats,
   topmesys, reqwest, utoipa or any other transport, storage or broker crate. New entries
   in `crates/jenny-core/Cargo.toml` `[dependencies]` need a reason.
2. **Adapters do not depend on adapters.** `jenny-rest`, `jenny-messaging`,
   `jenny-persistence` and `jenny-security` may depend on `jenny-core` (and
   `jenny-macros`) only. A dev-dependency between them is acceptable for tests. Confirm
   with `cargo tree -e normal -p <crate> --depth 1`.
3. **Only the composition root wires.** `jenny-server` may depend on more than the core;
   nothing else constructs another crate's adapters.
4. **Ports are owned inside.** A new trait an adapter implements belongs in
   `jenny_core::port::outbound` when it is application-independent, and in the
   service's `domain/port/` otherwise. A port shaped like one adapter's API (taking a
   `sqlx::Pool`, returning an `axum::Response`) is a leak.
5. **Errors cross the boundary as `DomainError`.** Adapters map their own failures into
   it; protocol status mapping (`Problem`, gRPC codes) happens in the inbound adapter.
   Server-side failures must map to `Internal`, never to a 4xx.
6. **The facade mirrors the rings.** New public items are re-exported from `crates/jenny`
   under the module of their ring, behind the right feature.
7. **Examples and templates follow the same rules.** `examples/basic/` and
   `crates/cargo-jenny/templates/` are what users copy.

Report findings ordered by severity: violations of 1–3 first. For each one, give the
concrete fix. If there are none, say so in one line. Do not edit files; this is a
review.
