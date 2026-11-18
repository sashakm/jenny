//! `jenny.usecase.duration`: what `#[jenny::use_cases]` measures, recorded as an
//! OpenTelemetry histogram.

use std::time::Duration;

use jenny_core::port::UseCaseObserver;
use opentelemetry::KeyValue;
use opentelemetry::metrics::Histogram;

/// Bucket boundaries in seconds — the ones OpenTelemetry's HTTP conventions recommend,
/// since a use case is usually most of a request. The SDK default is scaled for
/// milliseconds and would put almost every call in its first bucket.
const BOUNDARIES: [f64; 14] = [
    0.005, 0.01, 0.025, 0.05, 0.075, 0.1, 0.25, 0.5, 0.75, 1.0, 2.5, 5.0, 7.5, 10.0,
];

/// Records one histogram point per finished use case, labelled `use_case` and
/// `outcome`. Counts and error rates fall out of the histogram's count, so there is no
/// separate counter to keep consistent with it.
pub(crate) struct UseCaseMetrics {
    duration: Histogram<f64>,
}

impl UseCaseMetrics {
    /// Build against the global meter provider, which must already be installed:
    /// an instrument created from the no-op provider stays a no-op.
    pub(crate) fn from_global() -> Self {
        Self::new(&opentelemetry::global::meter("jenny"))
    }

    fn new(meter: &opentelemetry::metrics::Meter) -> Self {
        let duration = meter
            .f64_histogram("jenny.usecase.duration")
            .with_unit("s")
            .with_description("How long a use case took, by outcome")
            .with_boundaries(BOUNDARIES.to_vec())
            .build();
        Self { duration }
    }
}

impl UseCaseObserver for UseCaseMetrics {
    fn finished(&self, use_case: &'static str, outcome: &'static str, elapsed: Duration) {
        self.duration.record(
            elapsed.as_secs_f64(),
            &[
                KeyValue::new("use_case", use_case),
                KeyValue::new("outcome", outcome),
            ],
        );
    }
}

#[cfg(all(test, feature = "telemetry-prometheus"))]
mod tests {
    use super::*;
    use opentelemetry::metrics::MeterProvider as _;

    #[test]
    fn a_finished_use_case_lands_in_the_prometheus_registry() {
        let registry = prometheus::Registry::new();
        let exporter = opentelemetry_prometheus::exporter()
            .with_registry(registry.clone())
            .build()
            .unwrap();
        let provider = opentelemetry_sdk::metrics::SdkMeterProvider::builder()
            .with_reader(exporter)
            .build();

        let metrics = UseCaseMetrics::new(&provider.meter("jenny"));
        metrics.finished("notes.archive", "ok", Duration::from_millis(30));
        metrics.finished("notes.archive", "conflict", Duration::from_millis(2));

        let mut text = Vec::new();
        prometheus::Encoder::encode(
            &prometheus::TextEncoder::new(),
            &registry.gather(),
            &mut text,
        )
        .unwrap();
        let text = String::from_utf8(text).unwrap();
        assert!(
            text.contains("jenny_usecase_duration_seconds_count"),
            "{text}"
        );
        assert!(text.contains(r#"use_case="notes.archive""#), "{text}");
        assert!(text.contains(r#"outcome="conflict""#), "{text}");
    }
}
