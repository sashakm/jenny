# jenny

A pragmatic hexagonal webservice framework for Rust: a cargo workspace of nine crates
behind the `jenny` facade, plus the `cargo jenny` scaffolding CLI. Pre-1.0: breaking
changes are fine, and removing a low-value feature beats adapting it.

## Layout

| Crate | Ring | Holds |
|---|---|---|
| `crates/jenny` | facade | feature-gated re-exports; the only crate services depend on |
| `crates/jenny-core` | domain + ports | `Aggregate`, `DomainError`, `Changes`, `Criteria`, `Context`, `Store`/`StoreExt`, `Events`, outbound ports, `Memory`, tasks |
| `crates/jenny-rest` | driving adapter | axum/tower: `RestAdapter` (plain or JSON Patch mode), `Routes`, `Problem`, `Ctx`, `Listing`, `Versioned`/`IfMatch`, auth/CORS layers |
| `crates/jenny-messaging` | both | `EventBus` (driven, on topmesys), `NatsGateway`, `MessagingAdapter` (declaration only) |
| `crates/jenny-persistence` | driven adapter | sqlx/Postgres: `Postgres` pool + `PostgresConfig`, `PgDocuments` and `PgTable` stores, `SqlCriteria` + `Columns` allowlist, `QueryFormat`, `DatabaseCheck` |
| `crates/jenny-security` | driven adapter | JWT/JWKS + hashed API keys (`AuthenticationConfig`), Cedar policies (`CedarAuthorizer`) |
| `crates/jenny-server` | composition root | `JennyServer`: config, telemetry, health, metrics, OpenAPI, request ids, the Postgres pool (`persistence`), hosting, graceful shutdown |
| `crates/jenny-macros` | — | `#[derive(Dto)]`, `#[derive(QueryFormat)]`, `#[derive(Aggregate)]`, `#[derive(DomainEvent)]`, `#[use_cases]` |
| `crates/cargo-jenny` | — | `cargo jenny new` / `add resource`; text templates in `templates/` |

`examples/basic` is the reference service and mirrors the layout a generated project
gets. `docs/adr/` records every structural decision: read the ADR for a layer before
changing it, and write a new one (see the `write-adr` skill) for a new decision.

## Rules that are not negotiable

- **Dependencies point inward.** `jenny_core::domain` and `jenny_core::port` import no
  web, database or broker crate. No adapter crate depends on another adapter crate; only
  `jenny-server` depends on more than the core (ADR-001, ADR-002).
- **Storage goes through `Store<A>`, and every save is versioned** (ADR-012). Queries in
  the domain's own language go on a port that extends it, never on `Store` itself.
- **Events that report a write go with the write**: `change(…).raising(&events, cx)` or
  `add(aggregate, raised).raising(…)`, never `change(…).await` then `events.publish(…)`.
  Only then can a Postgres store with an outbox commit them together (ADR-016). No
  error-code catalogue, no DI container: both were considered and rejected.
- **Serde is the only foreign derive in `domain/`.** Aggregates and events derive it;
  value objects deserialise through their constructors (`#[serde(try_from = …)]`), so a
  stored value that breaks an invariant fails to load. No `FromRow`, no `ToSchema` there.
- **Macros wrap or derive; they never generate ports, adapters or wiring** (ADR-001).
  `#[use_cases]` only wraps method bodies; keep it that way.
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
just test-postgres      # database tests + the example against a disposable Postgres 18 (docker)
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
- **`#[use_cases]` wraps only `pub async fn`s** and finds the request id through a
  parameter typed `&Context`. A wrapped method must return `Result<_, E: UseCaseError>`.
  Metrics reach OTel through `observe_use_cases`, a process-wide hook installed by
  jenny-server's telemetry *after* the meter provider: an instrument made from the no-op
  provider stays a no-op.
- **`Memory<A>` stores JSON documents, not aggregates**, so a value object that cannot
  round-trip fails in unit tests. Nothing is queryable until `.queryable([...])`.
- **A derived `PATCH` calls the GET/PUT `MethodRouter`s, never the `Router`.** Routing a
  cloned request again appends to `MatchedPath` and duplicates the private `UrlParams`.
  `Routes` keeps layered copies of each GET and PUT for this; a new way of adding or
  layering routes must keep them in step (ADR-014).
- **Database tests read `JENNY_TEST_POSTGRES_URL`**: unset, they print "skipping" and
  pass; unset under `CI`, they fail. So `just test-all` passing says nothing about
  Postgres — run `just test-postgres` for any change in `jenny-persistence` or in the
  runtime's `persistence` wiring. Each test uses a table of its own and drops it.
- **`/tmp` has a per-user quota** that a few scaffolded target dirs fill. Run the slow
  gates with `TMPDIR=$PWD/target/tmp` when they fail with "Disk quota exceeded".
- **The outbox delivers at least once**, in order per relay, a moment after the request.
  The relay starts in `bind`, stops after the HTTP drain and before the broker, and the
  pool closes last; keep that order. Outbox tests each use their own schema
  (`search_path`), because every outbox table is `jenny_outbox`.
- **JSON Patch needs two conventions from a resource**: its PUT accepts the GET
  representation (no `deny_unknown_fields`), and reads it through `jenny_rest::Json`,
  which refuses ignored fields a patch changed. `accept_json_patch()` must precede
  `mount()`.

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

Messaging inbound dispatch (`MessagingAdapter` is declaration only), HTTP metrics, W3C
trace-context propagation (event headers can now carry it), JSON Merge Patch and PATCH in
the OpenAPI document (ADR-014), events raised with a `remove` (ADR-016), the MongoDB, NATS KV, object-storage and AGE adapters (ADR-013), `cargo jenny new --manifest`,
reloading Cedar policies without a restart, cursor pagination.
