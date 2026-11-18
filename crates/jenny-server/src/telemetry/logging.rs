//! Tracing subscriber layer construction.
//!
//! Builds the composable [`tracing_subscriber`] layer stack from a [`TelemetryConfig`].
//! All log output — regardless of backend — flows through the `tracing` instrumentation API.

use anyhow::Context;
#[cfg(any(feature = "telemetry-otlp", feature = "telemetry-prometheus"))]
use opentelemetry::KeyValue;
#[cfg(any(feature = "telemetry-otlp", feature = "telemetry-prometheus"))]
use opentelemetry_sdk::Resource;
use tracing::Level;
#[cfg(feature = "telemetry-otlp")]
use tracing_opentelemetry::OpenTelemetryLayer;
use tracing_subscriber::{
    EnvFilter,
    fmt::{self, format::FmtSpan},
    layer::SubscriberExt,
    util::SubscriberInitExt,
};

use super::{LogFormat, TelemetryConfig, TelemetryHandle};

/// Initialise the global `tracing` subscriber from the given config.
///
/// Called by [`TelemetryConfig::init`]. Composes layers for stdout, OTLP traces, OTLP log
/// records, and (optionally) direct Loki push, then installs the subscriber globally.
pub(super) fn init_logging(config: TelemetryConfig) -> anyhow::Result<TelemetryHandle> {
    let filter = build_env_filter(&config);
    // Needed by any backend that labels its output with the service, which is all of
    // them — binding it under one feature only meant the others did not compile.
    #[cfg(any(
        feature = "telemetry-otlp",
        feature = "telemetry-loki",
        feature = "telemetry-prometheus"
    ))]
    let service_name = config.service_name.clone();

    // --- Stdout layer (always enabled) ---
    // Box the layer so it can be composed with other optional layers without
    // fighting the type system: Vec<Box<dyn Layer>> implements Layer itself.
    let fmt_layer: Box<dyn tracing_subscriber::Layer<_> + Send + Sync> = match config.log_format {
        LogFormat::Pretty => Box::new(
            fmt::layer()
                .with_target(true)
                .with_file(true)
                .with_line_number(true)
                .with_span_events(FmtSpan::NEW | FmtSpan::CLOSE),
        ),
        LogFormat::Json => Box::new(
            fmt::layer()
                .json()
                .with_target(true)
                .with_file(true)
                .with_line_number(true)
                .with_span_events(FmtSpan::NEW | FmtSpan::CLOSE),
        ),
    };

    // Collect optional layers into a Vec so they can be applied uniformly.
    //
    // Gated on *either* backend, not both: requiring both meant that a service with
    // only `telemetry-otlp` enabled built its OTLP layers and then dropped them on the
    // floor, exporting nothing.
    #[cfg(any(feature = "telemetry-otlp", feature = "telemetry-loki"))]
    #[allow(unused_mut)]
    let mut extra_layers: Vec<Box<dyn tracing_subscriber::Layer<_> + Send + Sync>> = Vec::new();

    // --- OTel tracer layer (OTLP feature) ---
    // Bridges tracing spans to OTel trace records and exports via OTLP.
    // When no endpoint is configured, a no-op tracer is used (spans are not exported).
    #[cfg(feature = "telemetry-otlp")]
    let tracer_provider = if let Some(ref endpoint) = config.otlp_endpoint {
        let provider = build_otlp_tracer_provider(&service_name, endpoint)
            .context("failed to build OTLP tracer provider")?;
        let tracer = opentelemetry::trace::TracerProvider::tracer(&provider, service_name.clone());
        extra_layers.push(Box::new(OpenTelemetryLayer::new(tracer)));
        Some(provider)
    } else {
        None
    };

    #[cfg(not(feature = "telemetry-otlp"))]
    let tracer_provider: Option<opentelemetry_sdk::trace::SdkTracerProvider> = None;

    // --- OTel log bridge layer (OTLP feature) ---
    // Bridges tracing events to OTel log records and exports via OTLP.
    // This is the path that feeds logs into Logstash or Loki via an OTel Collector.
    #[cfg(feature = "telemetry-otlp")]
    let logger_provider = if let Some(ref endpoint) = config.otlp_endpoint {
        let provider = build_otlp_logger_provider(&service_name, endpoint)
            .context("failed to build OTLP logger provider")?;
        let log_layer =
            opentelemetry_appender_tracing::layer::OpenTelemetryTracingBridge::new(&provider);
        extra_layers.push(Box::new(log_layer));
        Some(provider)
    } else {
        None
    };

    #[cfg(not(feature = "telemetry-otlp"))]
    let logger_provider: Option<opentelemetry_sdk::logs::SdkLoggerProvider> = None;

    // --- Prometheus meter provider (telemetry-prometheus feature) ---
    #[cfg(feature = "telemetry-prometheus")]
    let prometheus_registry = {
        let (registry, provider) = build_prometheus_meter_provider(&service_name)
            .context("failed to build Prometheus meter provider")?;
        opentelemetry::global::set_meter_provider(provider);
        Some(registry)
    };

    #[cfg(not(feature = "telemetry-prometheus"))]
    // OTLP metric export when prometheus is not enabled
    {
        #[cfg(feature = "telemetry-otlp")]
        if let Some(ref endpoint) = config.otlp_endpoint {
            let provider = build_otlp_meter_provider(&service_name, endpoint)
                .context("failed to build OTLP meter provider")?;
            opentelemetry::global::set_meter_provider(provider);
        }
    }

    // --- Direct Loki layer (telemetry-loki feature) ---
    #[cfg(feature = "telemetry-loki")]
    let loki_task = if let Some(ref loki_url) = config.loki_url {
        let url = url::Url::parse(loki_url).context("invalid Loki URL")?;
        let (loki_layer, loki_bg_task) = tracing_loki::builder()
            .label("service_name", &service_name)
            .context("invalid Loki label")?
            .extra_field("pid", format!("{}", std::process::id()))
            .context("invalid Loki extra field")?
            .build_url(url)
            .context("failed to build Loki layer")?;
        extra_layers.push(Box::new(loki_layer));
        Some(tokio::spawn(loki_bg_task))
    } else {
        None
    };

    #[cfg(not(feature = "telemetry-loki"))]
    let loki_task: Option<tokio::task::JoinHandle<()>> = None;

    // --- Compose and install the global subscriber ---
    #[cfg(any(feature = "telemetry-otlp", feature = "telemetry-loki"))]
    let global_registry = tracing_subscriber::registry()
        .with(filter)
        .with(fmt_layer)
        .with(extra_layers);
    #[cfg(not(any(feature = "telemetry-otlp", feature = "telemetry-loki")))]
    let global_registry = tracing_subscriber::registry().with(filter).with(fmt_layer);

    global_registry
        .try_init()
        .context("failed to install global tracing subscriber")?;

    Ok(TelemetryHandle {
        tracer_provider,
        logger_provider,
        #[cfg(feature = "telemetry-prometheus")]
        prometheus_registry,
        loki_task,
    })
}

fn build_env_filter(config: &TelemetryConfig) -> EnvFilter {
    // RUST_LOG takes precedence; fall back to debug/info based on config.
    EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        let level = if config.debug {
            Level::DEBUG
        } else {
            Level::INFO
        };
        EnvFilter::new(level.as_str())
    })
}

/// OTel SDK resource carrying service identity attributes.
///
/// Shared by every exporter, so it is gated on any of them rather than on OTLP alone.
#[cfg(any(feature = "telemetry-otlp", feature = "telemetry-prometheus"))]
fn build_resource(service_name: &str) -> Resource {
    Resource::builder()
        .with_attribute(KeyValue::new("service.name", service_name.to_string()))
        .build()
}

/// Build an OTLP span (trace) exporter and tracer provider.
#[cfg(feature = "telemetry-otlp")]
fn build_otlp_tracer_provider(
    service_name: &str,
    endpoint: &str,
) -> anyhow::Result<opentelemetry_sdk::trace::SdkTracerProvider> {
    use opentelemetry_otlp::WithExportConfig;

    let exporter = opentelemetry_otlp::SpanExporter::builder()
        .with_tonic()
        .with_endpoint(endpoint)
        .build()
        .context("failed to build OTLP span exporter")?;

    let provider = opentelemetry_sdk::trace::SdkTracerProvider::builder()
        .with_resource(build_resource(service_name))
        .with_batch_exporter(exporter)
        .build();

    opentelemetry::global::set_tracer_provider(provider.clone());
    Ok(provider)
}

/// Build an OTLP log record exporter and logger provider.
#[cfg(feature = "telemetry-otlp")]
fn build_otlp_logger_provider(
    service_name: &str,
    endpoint: &str,
) -> anyhow::Result<opentelemetry_sdk::logs::SdkLoggerProvider> {
    use opentelemetry_otlp::WithExportConfig;

    let exporter = opentelemetry_otlp::LogExporter::builder()
        .with_tonic()
        .with_endpoint(endpoint)
        .build()
        .context("failed to build OTLP log exporter")?;

    let provider = opentelemetry_sdk::logs::SdkLoggerProvider::builder()
        .with_resource(build_resource(service_name))
        .with_batch_exporter(exporter)
        .build();

    Ok(provider)
}

/// Build an OTLP metrics exporter and meter provider (used when prometheus is not enabled).
#[cfg(all(feature = "telemetry-otlp", not(feature = "telemetry-prometheus")))]
fn build_otlp_meter_provider(
    service_name: &str,
    endpoint: &str,
) -> anyhow::Result<opentelemetry_sdk::metrics::SdkMeterProvider> {
    use opentelemetry_otlp::WithExportConfig;

    let exporter = opentelemetry_otlp::MetricExporter::builder()
        .with_tonic()
        .with_endpoint(endpoint)
        .build()
        .context("failed to build OTLP metric exporter")?;

    let reader = opentelemetry_sdk::metrics::PeriodicReader::builder(exporter).build();

    let provider = opentelemetry_sdk::metrics::SdkMeterProvider::builder()
        .with_resource(build_resource(service_name))
        .with_reader(reader)
        .build();

    Ok(provider)
}

/// Build a Prometheus metrics exporter and meter provider.
/// Returns the prometheus `Registry` so the caller can expose it on a `/metrics` endpoint.
#[cfg(feature = "telemetry-prometheus")]
fn build_prometheus_meter_provider(
    service_name: &str,
) -> anyhow::Result<(
    prometheus::Registry,
    opentelemetry_sdk::metrics::SdkMeterProvider,
)> {
    let registry = prometheus::Registry::new();
    let exporter = opentelemetry_prometheus::exporter()
        .with_registry(registry.clone())
        .build()
        .context("failed to build Prometheus exporter")?;

    let provider = opentelemetry_sdk::metrics::SdkMeterProvider::builder()
        .with_resource(build_resource(service_name))
        .with_reader(exporter)
        .build();

    Ok((registry, provider))
}
