//! jenny — a pragmatic hexagonal webservice framework for Rust.
//!
//! The dependency rule is the whole design: **dependencies point inward**. The domain
//! depends on nothing external, infrastructure depends on the domain, never the
//! reverse. jenny's job is to make the inward-pointing version the easy one to write.
//!
//! ### Where things live
//!
//! The facade mirrors the rings, so an import says which ring it belongs to:
//!
//! | Path | Ring | Contents |
//! |---|---|---|
//! | [`jenny::domain`](domain) | the hexagon | [`Aggregate`], [`DomainError`], [`DomainEvent`], [`Changes`], [`Validate`], [`Principal`], [`Urn`] |
//! | [`jenny::port`](port) | the boundary | [`Store`], [`Events`], [`Context`], [`EventPublisher`], [`Authenticator`], [`Authorizer`] |
//! | `jenny::rest` | driving adapter | `RestAdapter` (plain or JSON Patch mode), `Routes`, `Problem`, `Versioned`/`IfMatch` — axum/tower |
//! | `jenny::messaging` | both sides | `EventBus` (driven), `MessagingAdapter` (driving) |
//! | `jenny::persistence` | driven adapter | Postgres/sqlx: `Postgres`, `PgDocuments`, `PgTable`, `OutboxRelay`, criteria to SQL, `QueryFormat`, `DatabaseCheck` |
//! | `jenny::security` | driven adapter | JWT/API-key authentication, Cedar authorization |
//! | [`jenny::task`](task) | driving adapter | the clock: [`Task`](task::Task), [`ScheduledTask`](task::ScheduledTask) |
//! | [`jenny::memory`](memory) | driven adapter | [`Memory`](memory::Memory), an in-memory [`Store`] |
//! | [`jenny::testing`](testing) | tests | [`RecordedEvents`](testing::RecordedEvents) |
//! | `jenny::server` | composition root | the `JennyServer` runtime, and `jenny::server::telemetry` (logs, traces, metrics) |
//!
//! The most-used items are also re-exported flat, so ordinary application code reads
//! `use jenny::{Context, DomainResult, Events, Store, StoreExt};`.
//!
//! ### An application layer in one screen
//!
//! ```ignore
//! #[derive(Clone)]
//! pub struct Notes {
//!     notes: Arc<dyn Store<Note>>,
//!     events: Events,
//! }
//!
//! #[jenny::use_cases]                     // a span and a measurement per method
//! impl Notes {
//!     pub async fn archive(&self, id: NoteId, cx: &Context) -> DomainResult<()> {
//!         self.notes
//!             .change(&id, Note::archive)       // load, change, save with a version check
//!             .raising(&self.events, cx)        // and publish what it raised, with the write
//!             .await?;
//!         Ok(())
//!     }
//! }
//! ```
//!
//! ### A service in one picture
//!
//! ```text
//!   HTTP ─┐
//!   gRPC ─┼─▶ adapter/inbound ──▶ use case ──▶ domain
//!   bus  ─┤     (DTO -> command)      │          (entities, rules, events)
//!   cron ─┘                           │
//!                                     ▼
//!                              outbound ports
//!                        store · event publisher · storage
//!                                     │
//!                            adapter/outbound
//!                     postgres · eventbus · object store
//! ```
//!
//! ### Feature flags
//!
//! | Feature | Enables |
//! |---|---|
//! | `rest` | the axum driving adapter |
//! | `server` | the runtime and composition root (implies `rest`) |
//! | `messaging` | the event bus and messaging adapter |
//! | `persistence` | Postgres: document and table stores, the outbox, criteria to SQL; with `server`, a pool from `persistence.postgres` |
//! | `security` | authentication and Cedar authorization |
//! | `openapi` | OpenAPI schemas for the REST wire types |
//! | `swagger-ui` | a browsable Swagger UI, assets compiled in (implies `openapi`) |
//! | `telemetry-otlp` / `telemetry-prometheus` / `telemetry-loki` | telemetry backends (imply `server`) |
//! | `full` | every adapter and the runtime |

/// One set of field constraints on an inbound DTO; a `Validate` impl and — with the
/// `openapi` feature — a `utoipa::ToSchema` out of it.
///
/// Deliberately for adapter DTOs only. See the derive's own documentation for why the
/// domain keeps smart constructors instead.
pub use jenny_macros::Dto;

/// `impl Aggregate` from the identity field. See the [`Aggregate`] trait.
pub use jenny_macros::Aggregate;

/// `impl DomainEvent` with a name derived from the type. See the [`DomainEvent`]
/// trait.
pub use jenny_macros::DomainEvent;

/// A span and a `jenny.usecase.duration` measurement for every `pub async fn` in an
/// `impl` block. See [`jenny::port::use_case`](port::use_case).
pub use jenny_macros::use_cases;

#[doc(inline)]
pub use jenny_core::service_version;

// The rings, as modules.
pub use jenny_core::domain;
pub use jenny_core::memory;
pub use jenny_core::port;
pub use jenny_core::task;
pub use jenny_core::testing;

// The everyday vocabulary, flattened.
#[doc(no_inline)]
pub use jenny_core::domain::{
    Aggregate, AuthenticationError, AuthorizationError, Changes, Credentials, Criteria,
    DomainError, DomainEvent, DomainResult, Label, Principal, PrincipalBuilder, Urn, Validate,
    ValidationErrors, ValidationIssue,
};
#[doc(no_inline)]
pub use jenny_core::port::{
    AccessRequest, Authenticator, Authorizer, Changed, Context, EventEnvelope, EventPublisher,
    Events, Loaded, PublishError, Resource, Store, StoreExt, Version,
};

// Adapters and the runtime, behind their features.
#[cfg(feature = "messaging")]
pub use jenny_messaging as messaging;
#[cfg(feature = "persistence")]
pub use jenny_persistence as persistence;
#[cfg(feature = "rest")]
pub use jenny_rest as rest;
#[cfg(feature = "security")]
pub use jenny_security as security;
#[cfg(feature = "server")]
pub use jenny_server as server;
