# ADR-009: The `cargo jenny` Scaffolder

**Status:** Accepted — implemented
**Date:** 2026-08-24

## Context

With the layering (ADR-001), the port shapes (ADR-003, ADR-004) and the runtime (ADR-007)
settled, every jenny service has the same skeleton: a Cargo project depending on the
facade, a `main.rs` that boots `JennyServer`, a `config.yaml`, and a
`domain`/`application`/`adapter` tree per bounded context.

Writing that by hand is repetitive, and — more to the point — the repetitive parts are
exactly where the layering gets bent. Someone in a hurry puts the DTO next to the entity,
or the repository trait next to its sqlx implementation, and the structure that ADR-001
relies on review to uphold quietly stops holding. Scaffolding that produces the right
shape by default is a cheaper enforcement mechanism than catching it later.

## Decision

A cargo plugin, `cargo jenny`, in `crates/cargo-jenny`.

### `cargo jenny new <name>`

Generates a project that compiles and passes `cargo test` immediately: the full ring
structure, one starter resource, `Cargo.toml`, `config.yaml`, a `justfile`, a
`Dockerfile`, a `compose.yaml`, `.gitignore`, a `README.md`, and an end-to-end test that
boots the real composition root.

`--features` drives what appears. No `persistence` means no Postgres service in the
compose file; no `messaging` means the event publisher writes to the log instead of onto
a bus. Unknown feature names are an error rather than being ignored — silently dropping
one produces a project missing the very thing it was generated for.

Generated projects are **library plus thin binary**. The service lives in `lib.rs` and
`main.rs` calls `build`, so the integration test can drive the actual composition root.
The `#[path = "../src/main.rs"]` alternative does not work: re-including the binary as a
module re-roots every `crate::` path in it.

Generated `Cargo.toml` carries an empty `[workspace]` table, so a project created inside
another workspace is standalone rather than being adopted by it.

### `cargo jenny add resource <name>`

Stubs one vertical slice across every ring — aggregate with a value object and a smart
constructor, domain event, repository port, use cases with listing support, REST DTOs and
routes, an in-memory repository with row mapping, and tests at each level — then inserts
the `pub mod` declarations into the six module files that need them, sorted and
idempotently.

It **prints** the `main.rs` wiring rather than editing it. The composition root is the one
file whose readability matters most (ADR-007), and a generator is bad at preserving it.

One name in, every spelling out: `note` becomes the module `note`, the type `Note`, the
path segment `notes`. Four flags would be four chances to make them disagree.

### A template renderer, not a template engine

Two constructs: `{{name}}` and `{{#if flag}}…{{else}}…{{/if}}`. A real engine would bring
an expression language, its own error surface and a dependency, to render files that ship
inside the binary.

What it does have is strictness. An unknown variable, an unbalanced block, or an ambiguous
`{{{` is an error, so a broken template fails in the generator rather than emitting broken
Rust that fails later and further away.

### The CLI depends on none of the framework crates

It emits text. Coupling the generator to the thing it generates would mean a breaking
change in the framework breaks the tool meant to help you adopt it — and would make the
CLI's build time proportional to the framework's.

### The CLI is released with the framework

`cargo-jenny` ships in lockstep with the library crates (ADR-002), so a generated project
depends on `version = "<the CLI's own version>"`, the release its templates were written
against. `--jenny-path <checkout>` emits a path dependency instead, for working on jenny
itself.

### The scaffolding is tested by using it

`just test-cli` generates a project, runs its tests, adds a second resource, runs them
again, and runs clippy with `-D warnings` over the result. `just test-cli-features` does
the same once per optional feature. Templates are exactly the kind of thing that rots
silently, and the only honest check is whether what comes out actually builds: a
scaffolded `openapi` project once lacked the `utoipa` dependency its own DTOs needed,
because only the default features were being generated.

The template's third-party dependencies must stay on the workspace's majors, or a
generated project links a second copy of the crate and jenny's trait impls stop matching
its types. A unit test compares the two manifests.

## Consequences

- New services start in the right shape, which turns ADR-001 from a review rule into a
  default.
- The templates are a second consumer of the public API, so a breaking change shows up as
  broken scaffolding — useful pressure toward keeping that API stable.
- Templates duplicate the ideas in `examples/basic` rather than being generated from it.
  Deriving one from the other was considered and rejected: the example optimises for
  reading, the templates for being a starting point, and the `test-cli` target catches
  the drift that matters.
- Pluralisation is naive (`+s`, with the usual `-y`/`-s`/`-x`/`-ch`/`-sh` cases). A
  resource whose plural it gets wrong can simply be named what you want.

## Not built yet

`cargo jenny new --manifest project.yaml`, describing contexts, resources and features
declaratively so scaffolding is reproducible and reviewable. Worth having once there is
evidence people are generating more than one or two resources at a time; a second input
format to design and maintain is not free.
