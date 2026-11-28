//! The runtime a jenny service boots through — and the only place that knows every
//! layer.
//!
//! [`JennyServer`] is the composition root. It is deliberately the one crate allowed
//! to depend on everything: it reads configuration, brings telemetry up, constructs
//! concrete driven adapters, hands them to use cases, hands those to driving adapters,
//! and hosts the lot on a single axum server. Every other crate points inward; this
//! one closes the circle.
//!
//! ### What the runtime brings
//!
//! - **Configuration** — YAML, then environment, then CLI ([`ServerConfig`], [`ConfigSource`]).
//! - **Telemetry** — logs, traces and metrics initialised before anything else runs
//!   ([`telemetry`]). It lives here rather than in `jenny-core`, because only the
//!   composition root starts exporters, and the domain should not compile OpenTelemetry.
//! - **Health** — `<framework_path>/health/live`, `…/ready`, `…/status`
//!   ([`HealthCheck`]); `framework_path` defaults to `/j`.
//! - **Metrics** — `<framework_path>/metrics`, with the `telemetry-prometheus` feature.
//! - **Scheduling** — [`ScheduledTask`](jenny_core::task::ScheduledTask)s driven in the background.
//! - **Postgres** — with the `persistence` feature, a pool from `persistence.postgres`
//!   ([`JennyServer::postgres`]), migrated, its document tables (and, with jenny's
//!   `graph` feature, its AGE graphs) created and its readiness checked before the
//!   socket is bound; with `outbox: true`, the relay that
//!   forwards committed events to the bus, until shutdown.
//! - **NATS** — with `messaging`, one connection from `messaging.nats`
//!   ([`JennyServer::nats`]): plain, TLS or mTLS, with `.creds`, an NKey, a user and
//!   password or a token, adjusted by [`JennyServer::nats_options`]. Whenever a URL is
//!   configured it connects before the socket is bound, opens the key-value buckets and
//!   checks its readiness; a rotated certificate or `.creds` file reconnects it.
//! - **An object store** — with `objects`, blobs from `objects.url`
//!   ([`JennyServer::objects`]), its bucket's readiness checked.
//! - **Hosting** — every registered adapter merged into one router, with graceful shutdown.
//!
//! ### No injection container
//!
//! [`ServerContext`] hands applications their configuration section and any shared
//! resources during wiring, and then gets out of the way. After `main` returns, use
//! cases own their `Arc`s outright — there is no registry to look things up in at
//! runtime, because a dependency graph you can read top-to-bottom in one function is
//! worth more than one you have to trace through a container.
//!
//! ```no_run
//! use jenny_server::JennyServer;
//!
//! #[tokio::main]
//! async fn main() -> anyhow::Result<()> {
//!     JennyServer::from_args(jenny_core::service!())?.run().await
//! }
//! ```

mod config;
mod context;
mod health;
#[cfg(feature = "telemetry-prometheus")]
mod metrics;
#[cfg(feature = "openapi")]
mod openapi;
mod server;
pub mod telemetry;

#[cfg(feature = "persistence")]
#[doc(inline)]
pub use config::PersistenceConfig;
pub use config::{
    BasicAuthConfig, ConfigSource, DEFAULT_BODY_LIMIT, DEFAULT_CONFIG_PATH, DEFAULT_ENV_PREFIX,
    DEFAULT_EVENT_BUFFER, DEFAULT_FRAMEWORK_PATH, DEFAULT_SHUTDOWN_TIMEOUT_SECONDS, HttpConfig,
    MessagingConfig, MetricsConfig, ServerArgs, ServerConfig, SystemConfig,
};
pub use context::ServerContext;
pub use health::{CheckReport, FRAMEWORK_VERSION, HealthCheck, HealthRegistry, Reachable};
#[cfg(feature = "telemetry-prometheus")]
pub use metrics::metrics_handler;
pub use server::{JennyServer, ServerHandle, ServerParts};
