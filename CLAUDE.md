# jenny

A pragmatic hexagonal webservice framework for Rust: a cargo workspace of ten crates
behind the `jenny` facade, plus the `cargo jenny` scaffolding CLI. Pre-1.0: breaking
changes are fine, and removing a low-value feature beats adapting it.

## Layout

| Crate | Ring | Holds |
|---|---|---|
| `crates/jenny` | facade | feature-gated re-exports; the only crate services depend on |
| `crates/jenny-core` | domain + ports | `Aggregate`, `DomainError`, `Changes`, `Criteria`, `Context`, `Store`/`StoreExt`, `Blobs`, `Events`, outbound ports, `Memory` + `Queryable`, `MemoryBlobs`, tasks |
| `crates/jenny-rest` | driving adapter | axum/tower: `RestAdapter` (plain or JSON Patch mode), `Routes`, `Problem`, `Ctx`, `Listing`, `Versioned`/`IfMatch`, auth/CORS layers |
| `crates/jenny-messaging` | both | `EventBus` (driven, on topmesys), `NatsGateway`, `Nats` + `NatsKv` key-value store, `MessagingAdapter` (declaration only) |
| `crates/jenny-persistence` | driven adapter | sqlx/Postgres: `Postgres` pool + `PostgresConfig`, `PgDocuments` and `PgTable` stores, the outbox + `OutboxRelay`, `SqlCriteria` + `Columns` allowlist, `QueryFormat`, `DatabaseCheck`; Apache AGE `Graph` + `Cypher` (feature `graph`) |
| `crates/jenny-objects` | driven adapter | `object_store`: `ObjectBlobs` (the `Blobs` port over S3 and compatibles, files, memory), `ObjectsConfig`, `ObjectsCheck` |
| `crates/jenny-security` | driven adapter | JWT/JWKS + hashed API keys (`AuthenticationConfig`), Cedar policies (`CedarAuthorizer`) |
| `crates/jenny-server` | composition root | `JennyServer`: config, telemetry, health, metrics, OpenAPI, request ids, the Postgres pool (`persistence`), the NATS connection (`messaging`), the object store (`objects`), hosting, graceful shutdown |
| `crates/jenny-macros` | — | `#[derive(Dto)]`, `#[derive(QueryFormat)]`, `#[derive(Aggregate)]`, `#[derive(DomainEvent)]`, `#[use_cases]` |
| `crates/cargo-jenny` | — | `cargo jenny new` / `add resource`; text templates in `templates/` |

`examples/basic` is the reference service and mirrors the layout a generated project
gets. `docs/adr/` records every structural decision: read the ADR for a layer before
changing it, and write a new one (see the `write-adr` skill) for a new decision.

## Rules that are not negotiable

- **Dependencies point inward.** `jenny_core::domain` and `jenny_core::port` import no
  web, database or broker crate. No adapter crate depends on another adapter crate; only
  `jenny-server` depends on more than the core (ADR-001, ADR-002).
- **Aggregates are stored through `Store<A>`, and every save is versioned** (ADR-012).
  Queries in the domain's own language go on a port that extends it, never on `Store`
  itself. Files go through `Blobs` (ADR-018), graphs through a port of the domain's own
  (ADR-019).
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
  Cypher gets the same treatment: queries are `&'static str`, values go through `bind`,
  and labels through `identifier` from the graph's allowlist (ADR-019).
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
just test-nats          # key-value tests + the example against a disposable NATS (docker)
just test-objects       # object-storage tests + the example against a disposable S3 (RustFS, docker)
just test-age           # graph tests + the example against Postgres with Apache AGE, preloaded and not (docker)
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
- **Backend tests read `JENNY_TEST_POSTGRES_URL` / `JENNY_TEST_NATS_URL` /
  `JENNY_TEST_S3_URL` / `JENNY_TEST_AGE_URL`** (S3 with the standard `AWS_*` variables):
  unset, they print "skipping" and pass; unset under `CI`, they fail. So `just test-all`
  passing says nothing about them — run `just test-postgres` / `just test-nats` /
  `just test-objects` / `just test-age` for any change in a store or its runtime wiring.
  Each test uses a table, bucket, key prefix or graph of its own and removes it.
- **AGE needs `ag_catalog` on the search path** for edges and even `create_graph`
  ("operator does not exist: graphid = graphid"). `Graph` prepends it per transaction
  with `set_config(…, true)`, so `fetch_all_in` takes a `Transaction`: outside one the
  setting silently does nothing. Result columns report the type as bare `agtype`, so
  `Agtype::compatible` accepts both names.
- **S3 conditional writes are off until a service turns them on.** `object_store` 0.14
  sends `If-None-Match`/`If-Match` by default, and a server that ignores them overwrites
  silently, so `ObjectBlobs::from_url` sets `conditional_put: disabled` unless
  `objects.options.conditional_put` or `AWS_CONDITIONAL_PUT` names a value;
  `put_if_absent`/`put_if_match` are then refused. `etag` turns them on. `MemoryBlobs`
  does them unasked. `put_if_match` on a missing key is `Stale` in every implementation,
  as S3's 404 arrives as a failed precondition.
  Readiness lists the bucket rather than `HEAD`ing a key, because S3 answers both a
  missing key and a missing bucket with 404.
- **Listing rules live in two places.** `Memory` and `NatsKv` evaluate criteria through
  `jenny_core::memory::Queryable`; the Postgres stores translate them in
  `jenny-persistence/src/criteria.rs`. A listing change needs both, and the known
  differences (text equality, ADR-015) are documented, not accidental.
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

Planned features are ADR stubs with status `Proposed` (ADR-020 to ADR-032), indexed in
the README's Roadmap with the order agreed. Gaps in what is built:

Messaging inbound dispatch (`MessagingAdapter` is declaration only), HTTP metrics, W3C
trace-context propagation (event headers can now carry it), JSON Merge Patch and PATCH in
the OpenAPI document (ADR-014), events raised with a `remove` and `LISTEN/NOTIFY` for relays (ADR-016), a `migrations/`
directory in the scaffold (ADR-015), MongoDB (deferred, ADR-013), graph writes committed with the outbox (ADR-019), Azure/GCS, streaming and multipart blobs, signed URLs with conditions, conditional blob writes jenny provides itself whatever the backend (ADR-018), watching a NATS bucket and per-key time-to-live (ADR-017), `cargo jenny new --manifest`,
reloading Cedar policies without a restart, cursor pagination.
