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
//! | [`jenny::domain`](domain) | the hexagon | [`DomainError`], [`Validate`], [`Principal`], [`Urn`], [`DomainEvent`] |
//! | [`jenny::port`](port) | the boundary | [`UseCase`], [`Context`], [`EventPublisher`], [`Authenticator`], [`Authorizer`] |
//! | `jenny::rest` | driving adapter | `RestAdapter`, `Routes`, `Problem` — axum/tower |
//! | `jenny::messaging` | both sides | `EventBus` (driven), `MessagingAdapter` (driving) |
//! | `jenny::persistence` | driven adapter | Postgres/sqlx query and mapping helpers |
//! | `jenny::security` | driven adapter | JWT/API-key authentication, RBAC |
//! | [`jenny::task`](task) | driving adapter | the clock: [`Task`](task::Task), [`ScheduledTask`](task::ScheduledTask) |
//! | [`jenny::telemetry`](telemetry) | driven adapter | logs, traces, metrics |
//! | `jenny::server` | composition root | the `JennyServer` runtime |
//!
//! The most-used items are also re-exported flat, so ordinary application code reads
//! `use jenny::{Context, DomainError, UseCase};`.
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
//!                     repository · event publisher · storage
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
//! | `persistence` | Postgres/sqlx helpers |
//! | `security` | authentication and RBAC |
//! | `openapi` | OpenAPI schemas for the REST wire types |
//! | `telemetry-otlp` / `telemetry-prometheus` / `telemetry-loki` | telemetry backends |
//! | `full` | every adapter and the runtime |

/// One set of field constraints on an inbound DTO; a `Validate` impl and — with the
/// `openapi` feature — a `utoipa::ToSchema` out of it.
///
/// Deliberately for adapter DTOs only. See the derive's own documentation for why the
/// domain keeps smart constructors instead.
pub use jenny_macros::Dto;

#[doc(inline)]
pub use jenny_core::service_version;

// The rings, as modules.
pub use jenny_core::domain;
pub use jenny_core::port;
pub use jenny_core::task;
pub use jenny_core::telemetry;

// The everyday vocabulary, flattened.
#[doc(no_inline)]
pub use jenny_core::domain::{
    AuthenticationError, AuthorizationError, Credentials, DomainError, DomainEvent, DomainResult,
    Label, Principal, PrincipalBuilder, Urn, Validate, ValidationErrors, ValidationIssue,
};
#[doc(no_inline)]
pub use jenny_core::port::{
    Authenticator, Authorizer, Context, EventEnvelope, EventPublisher, PublishError, UseCase,
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
