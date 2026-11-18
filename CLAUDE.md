# jenny

A pragmatic hexagonal webservice framework for Rust: a cargo workspace of nine crates
behind the `jenny` facade, plus the `cargo jenny` scaffolding CLI. Pre-1.0: breaking
changes are fine, and removing a low-value feature beats adapting it.

## Layout

| Crate | Ring | Holds |
|---|---|---|
| `crates/jenny` | facade | feature-gated re-exports; the only crate services depend on |
| `crates/jenny-core` | domain + ports | `DomainError`, `Validate`, `Principal`, `Urn`, `Criteria`, `UseCase`, `Context`, outbound ports, tasks |
| `crates/jenny-rest` | driving adapter | axum/tower: `RestAdapter`, `Routes`, `Problem`, `Ctx`, `Listing`, auth/CORS layers |
| `crates/jenny-messaging` | both | `EventBus` (driven, on topmesys), `NatsGateway`, `MessagingAdapter` (declaration only) |
| `crates/jenny-persistence` | driven adapter | sqlx/Postgres: `SqlCriteria` + `Columns` allowlist, `QueryFormat`, `DatabaseCheck` |
| `crates/jenny-security` | driven adapter | JWT/JWKS + hashed API keys (`AuthenticationConfig`), Cedar policies (`CedarAuthorizer`) |
| `crates/jenny-server` | composition root | `JennyServer`: config, telemetry, health, metrics, OpenAPI, request ids, hosting, graceful shutdown |
| `crates/jenny-macros` | — | `#[derive(Dto)]`, `#[derive(QueryFormat)]` |
| `crates/cargo-jenny` | — | `cargo jenny new` / `add resource`; text templates in `templates/` |

`examples/basic` is the reference service and mirrors the layout a generated project
gets. `docs/adr/` records every structural decision: read the ADR for a layer before
changing it, and write a new one (see the `write-adr` skill) for a new decision.

## Rules that are not negotiable

- **Dependencies point inward.** `jenny_core::domain` and `jenny_core::port` import no
  web, database or broker crate. No adapter crate depends on another adapter crate; only
  `jenny-server` depends on more than the core (ADR-001, ADR-002).
- **No generic `Repository<T, Id>`**, no error-code catalogue, no DI container. Each was
  considered and rejected; see the ADRs before reintroducing one.
- **Filter paths are attacker-controlled.** Nothing turns a `Criteria` into SQL without a
  `Columns` allowlist; identifiers come from it, values are always bound parameters.
- **Authorization never pattern-matches request paths.** The HTTP gate asks about the
  matched route template inside its resource and adapter URNs, and policies are Cedar
  (ADR-011). Anything that cannot be evaluated denies.
- **One version for every crate** (`[workspace.package] version`). Never bump a single
  crate's version, and never edit `CHANGELOG.md` by hand: release-plz writes both.

## Commands

```sh
just check              # fmt, clippy -D warnings, rustdoc -D warnings, every feature combination
just test-all           # every test, all features
just test               # what CI runs: doc tests + all tests under llvm-cov
just test-cli           # scaffold a default project; it must build, test and lint clean
just test-cli-features  # the same once per optional feature (slow; shared target dir)
just msrv               # cargo +1.94 check, the declared rust-version
just package            # package + verify every crate as crates.io will see it
```

Which ones a change needs is in the `verify` skill. Run `cargo fmt --all` before
`just check`; a hook already formats `.rs` files as they are edited.

## Things that cost time to rediscover

- **`--all-features` hides feature-combination bugs.** A `#[cfg]` that only holds with two
  features on compiles under `--all-features` and breaks for anyone enabling one. `just
  features` builds each set on its own; add a new feature to its lists.
- **Templates are only checked by generating from them.** `templates/**/*.tmpl` are plain
  text to cargo. A template change needs `just test-cli`, and `just test-cli-features`
  when it sits inside an `{{#if feature}}` block.
- **Template dependency majors must match the workspace.** A generated project on a
  different major of axum/utoipa/sqlx links a second copy and jenny's trait impls stop
  matching. `template_dependencies_track_the_workspace_majors` enforces it, so bump both
  files together.
- **`#[derive(Dto)]` emits `::utoipa::…`** under `openapi`, so the using crate needs a
  direct `utoipa` dependency on jenny's major. It reads serde's `rename`, `rename_all`,
  `default` and `skip` for property names and `required`; other serde options are ignored.
- **`Router::nest` rewrites the URI.** A layer inside a nested adapter sees `/orders`, not
  `/api/orders`; the authorization layer reads `OriginalUri` (there is a regression test).
- **axum layers only wrap routes declared before them.** `Routes::layer` and
  `RestAdapter::layer` apply to what was mounted so far. Mount first, then layer.
- **`service_version!()` must stay a macro**: `env!` expands where it is written.
- **Generated projects are lib + bin**, because `#[path = "../src/main.rs"]` would re-root
  every `crate::` path in the included file.

## topmesys

`../topmesys` is the sibling repo providing the in-process broker; jenny uses the
published crate. To develop both at once, add `[patch.crates-io] topmesys = { path =
"../topmesys" }` to the root `Cargo.toml` temporarily. Never commit it: CI has no sibling
checkout, and generated projects (`just test-cli*`) depend on jenny by path and do not
see the patch, so they still resolve the published topmesys. A topmesys release must
happen before jenny can depend on it.

## Releasing and commits

Conventional commits, one change per commit, and a breaking change needs `!` in the
header or a `BREAKING CHANGE:` footer: release-plz reads only the first line. Pushing to
`main` updates the release PR; merging it publishes. Never run `cargo publish`,
`cargo yank` or `cargo owner` (the project settings deny them); use `just package` to
check publishability. Details: `RELEASING.md`.

## Known gaps (not yet built, recorded in ADRs)

Messaging inbound dispatch (`MessagingAdapter` is declaration only), framework use-case
and HTTP metrics, W3C trace-context propagation (event headers can now carry it),
`cargo jenny new --manifest`, reloading Cedar policies without a restart, cursor
pagination.
