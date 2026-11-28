//! Prometheus scrape endpoint.

use axum::{
    Extension, Router, extract::State, http::StatusCode, response::IntoResponse, routing::get,
};
use jenny_rest::RequestId;

/// `text/plain; version=0.0.4; charset=utf-8`
const PROMETHEUS_CONTENT_TYPE: &str = "text/plain; version=0.0.4; charset=utf-8";

/// Serves the registry in Prometheus text exposition format.
///
/// [`JennyServer`](crate::JennyServer) serves this as the `metrics` endpoint group
/// (`<framework_path>/metrics` by default, `server.endpoints.metrics`) whenever the
/// `telemetry-prometheus` feature is on. It is public so a hand-rolled axum app can
/// mount it too.
///
/// Scrapes produce no request span by default, so a failure to encode is logged with
/// the request id itself.
///
/// Metrics are an *outbound* concern — the application emits through the telemetry
/// port — but the scrape endpoint is inbound by direction, which is why it belongs to
/// the runtime rather than to any application adapter.
pub async fn metrics_handler(
    State(registry): State<prometheus::Registry>,
    request_id: Option<Extension<RequestId>>,
) -> impl IntoResponse {
    use prometheus::Encoder;

    let encoder = prometheus::TextEncoder::new();
    let mut buffer = Vec::new();
    if let Err(e) = encoder.encode(&registry.gather(), &mut buffer) {
        let request_id = request_id.map(|Extension(id)| id.to_string());
        tracing::error!(request_id, "failed to encode prometheus metrics: {e}");
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }
    (
        [(axum::http::header::CONTENT_TYPE, PROMETHEUS_CONTENT_TYPE)],
        buffer,
    )
        .into_response()
}

pub(crate) fn metrics_router(registry: prometheus::Registry, path: &str) -> Router {
    Router::new()
        .route(path, get(metrics_handler))
        .with_state(registry)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::{Body, to_bytes};
    use prometheus::{Counter, Registry};
    use tower::ServiceExt;

    #[tokio::test]
    async fn registered_metrics_are_exposed_in_text_format() {
        let registry = Registry::new();
        let counter = Counter::new("jenny_test_total", "a test counter").unwrap();
        registry.register(Box::new(counter.clone())).unwrap();
        counter.inc();

        let response = metrics_router(registry, "/metrics")
            .oneshot(http::Request::get("/metrics").body(Body::empty()).unwrap())
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response
                .headers()
                .get(axum::http::header::CONTENT_TYPE)
                .unwrap(),
            PROMETHEUS_CONTENT_TYPE
        );

        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let body = String::from_utf8(bytes.to_vec()).unwrap();
        assert!(body.contains("jenny_test_total 1"), "got: {body}");
    }
}
