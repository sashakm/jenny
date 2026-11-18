//! Observability: OpenTelemetry-compliant logging, distributed tracing, and metrics.
//!
//! All instrumentation within the framework uses the [`tracing`] crate as the unified API.
//! This module configures and wires up the backends at application startup.
//!
//! # Supported backends
//!
//! | Signal | Backend | Feature flag |
//! |---|---|---|
//! | Logs | Stdout (pretty / JSON) | always |
//! | Logs + Traces | OTLP → OTel Collector → Logstash / Loki | `telemetry-otlp` |
//! | Logs | Direct Loki push | `telemetry-loki` |
//! | Metrics (pull) | Prometheus scrape `/metrics` | `telemetry-prometheus` |
//! | Metrics (push) | OTLP → OTel Collector → Grafana | `telemetry-otlp` |
//!
//! # Usage
//!
//! Call [`TelemetryConfig::init`] once at application startup, before any `tracing`
//! instrumentation fires. Hold the returned [`TelemetryHandle`] for the lifetime of the
//! process — dropping it triggers a graceful flush of all OTel exporters.
//!
//! ```ignore
//! use jenny_server::telemetry::{TelemetryConfig, LogFormat};
//!
//! #[tokio::main]
//! async fn main() -> anyhow::Result<()> {
//!     let _telemetry = TelemetryConfig::new("my-service")
//!         .with_log_format(LogFormat::Json)
//!         .init()?;
//!
//!     tracing::info!("service started");
//!     Ok(())
//! }
//! ```
//!
//! # Configuration via file
//!
//! When an application is hosted by the `jenny-server` runtime, the `telemetry` section
//! of its config file maps directly to [`TelemetryConfig`] and the runtime initialises
//! the stack during bootstrap:
//!
//! ```yaml
//! telemetry:
//!   service_name: my-service
//!   log_format: json
//!   debug: false
//!   otlp_endpoint: "http://otel-collector:4317"   # requires telemetry-otlp feature
//!   loki_url: "http://loki:3100"                  # requires telemetry-loki feature
//!   prometheus_port: 9090                         # requires telemetry-prometheus feature
//! ```

mod logging;
#[cfg(any(feature = "telemetry-otlp", feature = "telemetry-prometheus"))]
mod use_cases;

use serde::Deserialize;

/// Stdout log format.
///
/// Selects the format emitted by the stdout tracing subscriber layer.
#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum LogFormat {
    /// Human-readable, coloured output. Recommended for local development.
    #[default]
    Pretty,
    /// Machine-readable JSON (one object per event). Recommended for production.
    Json,
}

/// Top-level telemetry configuration.
///
/// Can be deserialised from a `[telemetry]` section in the application config file, or built
/// programmatically using the provided builder methods.
///
/// See the [module documentation](self) for examples.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct TelemetryConfig {
    /// Name identifying this service in traces, metrics labels, and structured log fields.
    /// Becomes the `service.name` OTel resource attribute.
    #[serde(default)]
    pub service_name: String,

    /// Version of the service, reported by `/health/status` and as the
    /// `service.version` OTel resource attribute.
    ///
    /// Usually your crate's version. The framework cannot read it for you — `env!`
    /// expands where it is written, which for framework code is the framework's own
    /// manifest — so pass [`service_version!`](jenny_core::service_version) from your
    /// binary, or set it here.
    #[serde(default)]
    pub service_version: Option<String>,

    /// Stdout log format. Defaults to [`LogFormat::Pretty`].
    #[serde(default)]
    pub log_format: LogFormat,

    /// Emit `DEBUG`-level events and spans. Defaults to `false`.
    ///
    /// **WARNING**: may expose sensitive values in log output.
    #[serde(default)]
    pub debug: bool,

    /// OTLP collector gRPC endpoint (e.g. `http://localhost:4317`).
    ///
    /// When set, enables:
    /// - Distributed trace export (via `tracing-opentelemetry` + OTLP)
    /// - Structured log record export (via `opentelemetry-appender-tracing` + OTLP)
    /// - Metrics push export (when `telemetry-prometheus` is **not** enabled)
    ///
    /// The OTel Collector at this endpoint is responsible for routing signals to downstream
    /// systems such as Logstash, Loki (for logs), Jaeger/Tempo (for traces), and Mimir/Thanos
    /// (for metrics).
    ///
    /// Requires the `telemetry-otlp` feature.
    pub otlp_endpoint: Option<String>,

    /// Loki push API URL (e.g. `http://localhost:3100`).
    ///
    /// When set, enables direct log shipping to Loki without an OTel Collector intermediary.
    /// Can be used alongside or instead of [`otlp_endpoint`](Self::otlp_endpoint).
    ///
    /// Requires the `telemetry-loki` feature.
    pub loki_url: Option<String>,

    /// Port to expose a Prometheus `/metrics` scrape endpoint.
    ///
    /// When set, a Prometheus-compatible metrics exporter is configured and the registry is
    /// made available via [`TelemetryHandle::prometheus_registry`]. The `jenny-server`
    /// runtime mounts `/metrics` from it automatically; a hand-rolled axum app can mount
    /// `jenny_server::metrics_handler` itself.
    ///
    /// Requires the `telemetry-prometheus` feature.
    pub prometheus_port: Option<u16>,
}

impl TelemetryConfig {
    pub fn new(service_name: impl Into<String>) -> Self {
        Self {
            service_name: service_name.into(),
            ..Default::default()
        }
    }

    /// Set the service version reported in status and telemetry.
    pub fn with_service_version(mut self, version: impl Into<String>) -> Self {
        self.service_version = Some(version.into());
        self
    }

    /// Set the stdout log format.
    pub fn with_log_format(mut self, format: LogFormat) -> Self {
        self.log_format = format;
        self
    }

    /// Enable or disable `DEBUG`-level output.
    pub fn with_debug(mut self, debug: bool) -> Self {
        self.debug = debug;
        self
    }

    /// Set the OTLP collector endpoint for trace, log, and metrics export.
    ///
    /// Has no effect unless the `telemetry-otlp` feature is enabled.
    pub fn with_otlp_endpoint(mut self, endpoint: impl Into<String>) -> Self {
        self.otlp_endpoint = Some(endpoint.into());
        self
    }

    /// Set the Loki push URL for direct log shipping.
    ///
    /// Has no effect unless the `telemetry-loki` feature is enabled.
    pub fn with_loki_url(mut self, url: impl Into<String>) -> Self {
        self.loki_url = Some(url.into());
        self
    }

    /// Set the port on which to expose a Prometheus `/metrics` scrape endpoint.
    ///
    /// Has no effect unless the `telemetry-prometheus` feature is enabled.
    pub fn with_prometheus_port(mut self, port: u16) -> Self {
        self.prometheus_port = Some(port);
        self
    }

    /// Initialise the global telemetry stack from this configuration.
    ///
    /// Sets up the `tracing` subscriber with the configured backends and installs global
    /// OpenTelemetry providers. Should be called **once** at application startup before any
    /// `tracing` instrumentation fires.
    ///
    /// Returns a [`TelemetryHandle`] that must be held for the lifetime of the process.
    /// Dropping it flushes pending OTel exports.
    ///
    /// # Errors
    ///
    /// Returns an error if any enabled exporter fails to initialise (e.g. unreachable OTLP
    /// endpoint at startup, malformed Loki URL, or a global subscriber already installed).
    pub fn init(self) -> anyhow::Result<TelemetryHandle> {
        logging::init_logging(self)
    }
}

/// Holds live handles to the telemetry stack. Must be kept alive for the lifetime of the process.
///
/// Dropping this value triggers a graceful shutdown of all OTel exporters, flushing any pending
/// spans, log records, and metric data points before they are discarded.
pub struct TelemetryHandle {
    pub(crate) tracer_provider: Option<opentelemetry_sdk::trace::SdkTracerProvider>,
    pub(crate) logger_provider: Option<opentelemetry_sdk::logs::SdkLoggerProvider>,
    #[cfg(feature = "telemetry-prometheus")]
    pub(crate) prometheus_registry: Option<prometheus::Registry>,
    pub(crate) loki_task: Option<tokio::task::JoinHandle<()>>,
}

impl TelemetryHandle {
    /// Returns the Prometheus metric registry, if `prometheus_port` was configured.
    ///
    /// The `jenny-server` runtime mounts a `/metrics` handler automatically; use this to
    /// mount `jenny_server::metrics_handler` in a hand-rolled axum app.
    #[cfg(feature = "telemetry-prometheus")]
    pub fn prometheus_registry(&self) -> Option<&prometheus::Registry> {
        self.prometheus_registry.as_ref()
    }

    /// Flush and shut down all OTel exporters. Equivalent to dropping the handle.
    pub fn shutdown(self) {
        drop(self);
    }
}

impl Drop for TelemetryHandle {
    fn drop(&mut self) {
        if let Some(provider) = self.tracer_provider.take()
            && let Err(e) = provider.shutdown()
        {
            eprintln!("warning: OTLP tracer provider shutdown error: {e}");
        }
        if let Some(provider) = self.logger_provider.take()
            && let Err(e) = provider.shutdown()
        {
            eprintln!("warning: OTLP logger provider shutdown error: {e}");
        }
        if let Some(task) = self.loki_task.take() {
            task.abort();
        }
        // Meter provider shutdown is handled globally; the tracer and logger providers
        // are already shut down above via their stored handles.
    }
}
