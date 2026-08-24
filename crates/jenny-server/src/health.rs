//! Liveness, readiness and status endpoints.
//!
//! | Path | Question it answers | Who asks |
//! |---|---|---|
//! | `GET <framework_path>/health/live` | is the process alive? | the orchestrator's restart policy |
//! | `GET <framework_path>/health/ready` | should it receive traffic? | the load balancer |
//! | `GET <framework_path>/health/status` | what is running in here? | a human |
//!
//! The distinction between live and ready is the one that matters operationally:
//! a service whose database is down is *live* (restarting it will not help) but not
//! *ready* (do not send it traffic). Conflating them turns a dependency outage into a
//! restart loop.
//!
//! ### What checks itself, and what you register
//!
//! The runtime auto-registers checks for the things it *owns* — the OTLP endpoint it
//! was configured with, the in-process event bus it started. It cannot auto-register
//! your database pool: reaching it would need either a dependency-injection container
//! or a `sqlx` dependency in the runtime, and the composition-root ADR rules out the
//! first. So that one is a line in `main`:
//!
//! ```ignore
//! server.register_health_check(DatabaseCheck::new("notes-db", pool.clone()))
//! ```

use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use async_trait::async_trait;
use axum::{Json, Router, extract::State, http::StatusCode, response::IntoResponse, routing::get};
use serde::Serialize;

pub use jenny_core::port::outbound::HealthCheck;

/// How long any single check may take before it counts as failed. A readiness probe
/// that hangs is worse than one that fails.
const CHECK_TIMEOUT: Duration = Duration::from_secs(2);

/// The version of the framework this service was built against.
pub const FRAMEWORK_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Readiness by opening a TCP connection to a host and port.
///
/// The generic answer to "is that remote thing reachable" when there is no protocol
/// client to ask — the OTLP collector being the case the runtime uses it for. A
/// successful connect proves the address resolves and something is listening. It does
/// not prove that something speaks the protocol you want, and this check does not
/// claim otherwise.
pub struct Reachable {
    name: String,
    kind: String,
    authority: String,
}

impl Reachable {
    /// Derive the target from a URL, falling back to the scheme's usual port.
    ///
    /// `None` when the URL carries no host, since there is then nothing to check.
    pub fn from_url(name: impl Into<String>, kind: impl Into<String>, url: &str) -> Option<Self> {
        let rest = url.split_once("://").map(|(_, rest)| rest).unwrap_or(url);
        let authority = rest
            .split(['/', '?'])
            .next()
            .filter(|a| !a.is_empty())?
            .to_string();

        let authority = if authority.contains(':') {
            authority
        } else if url.starts_with("https") {
            format!("{authority}:443")
        } else {
            format!("{authority}:80")
        };

        Some(Self {
            name: name.into(),
            kind: kind.into(),
            authority,
        })
    }

    pub fn new(
        name: impl Into<String>,
        kind: impl Into<String>,
        authority: impl Into<String>,
    ) -> Self {
        Self {
            name: name.into(),
            kind: kind.into(),
            authority: authority.into(),
        }
    }

    pub fn authority(&self) -> &str {
        &self.authority
    }
}

#[async_trait]
impl HealthCheck for Reachable {
    fn name(&self) -> &str {
        &self.name
    }

    fn kind(&self) -> &str {
        &self.kind
    }

    async fn check(&self) -> Result<(), String> {
        match tokio::net::TcpStream::connect(&self.authority).await {
            Ok(_) => Ok(()),
            Err(e) => Err(format!("cannot reach {}: {e}", self.authority)),
        }
    }
}

/// The registered checks plus the server's own lifecycle state.
#[derive(Clone)]
pub struct HealthRegistry {
    checks: Arc<[Arc<dyn HealthCheck>]>,
    started: Arc<AtomicBool>,
    started_at: Instant,
}

impl HealthRegistry {
    pub(crate) fn new(checks: Vec<Arc<dyn HealthCheck>>) -> Self {
        Self {
            checks: checks.into(),
            started: Arc::new(AtomicBool::new(false)),
            started_at: Instant::now(),
        }
    }

    pub(crate) fn mark_started(&self) {
        self.started.store(true, Ordering::Release);
    }

    pub fn started(&self) -> bool {
        self.started.load(Ordering::Acquire)
    }

    pub fn uptime(&self) -> Duration {
        self.started_at.elapsed()
    }

    /// Run every check, each under its own timeout, and collect the reports.
    ///
    /// Checks run concurrently: they are independent I/O, and six two-second timeouts
    /// in series would make a readiness probe take twelve seconds to fail.
    pub async fn run_checks(&self) -> Vec<CheckReport> {
        let running = self.checks.iter().map(|check| async move {
            match tokio::time::timeout(CHECK_TIMEOUT, check.check()).await {
                Ok(Ok(())) => CheckReport::up(check.name(), check.kind()),
                Ok(Err(detail)) => CheckReport::down(check.name(), check.kind(), detail),
                Err(_) => CheckReport::down(
                    check.name(),
                    check.kind(),
                    format!("check timed out after {}s", CHECK_TIMEOUT.as_secs()),
                ),
            }
        });
        futures::future::join_all(running).await
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum HealthState {
    Up,
    Down,
}

/// The outcome of one [`HealthCheck`].
#[derive(Debug, Serialize)]
pub struct CheckReport {
    name: String,
    kind: String,
    status: HealthState,
    #[serde(skip_serializing_if = "Option::is_none")]
    detail: Option<String>,
}

impl CheckReport {
    fn up(name: &str, kind: &str) -> Self {
        Self {
            name: name.to_string(),
            kind: kind.to_string(),
            status: HealthState::Up,
            detail: None,
        }
    }

    fn down(name: &str, kind: &str, detail: impl Into<String>) -> Self {
        Self {
            name: name.to_string(),
            kind: kind.to_string(),
            status: HealthState::Down,
            detail: Some(detail.into()),
        }
    }

    fn is_up(&self) -> bool {
        self.status == HealthState::Up
    }
}

#[derive(Serialize)]
struct HealthReport {
    status: HealthState,
    checks: Vec<CheckReport>,
}

/// What `/health/status` reports about the running service.
#[derive(Clone, Serialize)]
pub(crate) struct StatusInfo {
    pub service: String,
    pub service_version: String,
    pub framework_version: &'static str,
    pub adapters: Vec<AdapterSummary>,
}

#[derive(Clone, Serialize)]
pub(crate) struct AdapterSummary {
    pub name: String,
    pub kind: &'static str,
    pub path: String,
    pub resources: Vec<String>,
}

#[derive(Serialize)]
struct StatusReport {
    service: String,
    service_version: String,
    framework_version: &'static str,
    uptime_seconds: u64,
    adapters: Vec<AdapterSummary>,
    checks: Vec<CheckReport>,
}

#[derive(Clone)]
pub(crate) struct HealthEndpointState(pub HealthRegistry, pub StatusInfo);

pub(crate) fn health_router(registry: HealthRegistry, status: StatusInfo) -> Router {
    Router::new()
        .route("/health/live", get(live))
        .route("/health/ready", get(ready))
        .route("/health/status", get(status_report))
        .with_state(HealthEndpointState(registry, status))
}

async fn live() -> impl IntoResponse {
    Json(serde_json::json!({ "status": "up" }))
}

async fn ready(
    State(HealthEndpointState(registry, _)): State<HealthEndpointState>,
) -> impl IntoResponse {
    let checks = registry.run_checks().await;
    let ready = registry.started() && checks.iter().all(CheckReport::is_up);
    let report = HealthReport {
        status: if ready {
            HealthState::Up
        } else {
            HealthState::Down
        },
        checks,
    };
    let code = if ready {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    (code, Json(report))
}

async fn status_report(
    State(HealthEndpointState(registry, status)): State<HealthEndpointState>,
) -> impl IntoResponse {
    let checks = registry.run_checks().await;
    Json(StatusReport {
        service: status.service.clone(),
        service_version: status.service_version.clone(),
        framework_version: status.framework_version,
        uptime_seconds: registry.uptime().as_secs(),
        adapters: status.adapters.clone(),
        checks,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::{Body, to_bytes};
    use tower::ServiceExt;

    struct Static {
        name: &'static str,
        healthy: bool,
    }

    #[async_trait]
    impl HealthCheck for Static {
        fn name(&self) -> &str {
            self.name
        }
        fn kind(&self) -> &str {
            "test"
        }
        async fn check(&self) -> Result<(), String> {
            if self.healthy {
                Ok(())
            } else {
                Err("broken".to_string())
            }
        }
    }

    struct Hanging;

    #[async_trait]
    impl HealthCheck for Hanging {
        fn name(&self) -> &str {
            "hanging"
        }
        async fn check(&self) -> Result<(), String> {
            std::future::pending::<()>().await;
            Ok(())
        }
    }

    fn status_info() -> StatusInfo {
        StatusInfo {
            service: "test-service".to_string(),
            service_version: "1.2.3".to_string(),
            framework_version: FRAMEWORK_VERSION,
            adapters: vec![AdapterSummary {
                name: "my-notes".to_string(),
                kind: "rest",
                path: "/api".to_string(),
                resources: vec!["urn:my-notes:notes".to_string()],
            }],
        }
    }

    async fn call(router: Router, uri: &str) -> (StatusCode, serde_json::Value) {
        let response = router
            .oneshot(http::Request::get(uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        (status, serde_json::from_slice(&bytes).unwrap())
    }

    #[tokio::test]
    async fn live_is_up_regardless_of_checks() {
        let registry = HealthRegistry::new(vec![Arc::new(Static {
            name: "broken",
            healthy: false,
        })]);
        let (status, body) = call(health_router(registry, status_info()), "/health/live").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, serde_json::json!({"status": "up"}));
    }

    #[tokio::test]
    async fn ready_needs_both_a_started_server_and_passing_checks() {
        let registry = HealthRegistry::new(vec![Arc::new(Static {
            name: "always-up",
            healthy: true,
        })]);
        let router = health_router(registry.clone(), status_info());

        let (status, _) = call(router.clone(), "/health/ready").await;
        assert_eq!(
            status,
            StatusCode::SERVICE_UNAVAILABLE,
            "not started yet, so not ready"
        );

        registry.mark_started();
        let (status, body) = call(router, "/health/ready").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["status"], "up");
        assert_eq!(body["checks"][0]["name"], "always-up");
        assert_eq!(body["checks"][0]["kind"], "test");
    }

    #[tokio::test]
    async fn one_failing_check_makes_the_service_unready() {
        let registry = HealthRegistry::new(vec![
            Arc::new(Static {
                name: "fine",
                healthy: true,
            }),
            Arc::new(Static {
                name: "broken",
                healthy: false,
            }),
        ]);
        registry.mark_started();
        let (status, body) = call(health_router(registry, status_info()), "/health/ready").await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body["status"], "down");
        assert_eq!(body["checks"][1]["detail"], "broken");
    }

    #[tokio::test(start_paused = true)]
    async fn a_check_that_hangs_fails_instead_of_blocking_the_probe() {
        let registry = HealthRegistry::new(vec![Arc::new(Hanging)]);
        registry.mark_started();
        let (status, body) = call(health_router(registry, status_info()), "/health/ready").await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert!(
            body["checks"][0]["detail"]
                .as_str()
                .unwrap()
                .contains("timed out")
        );
        assert_eq!(
            body["checks"][0]["kind"], "dependency",
            "the trait's default kind"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn checks_run_concurrently_rather_than_in_series() {
        let registry = HealthRegistry::new(vec![
            Arc::new(Hanging),
            Arc::new(Hanging),
            Arc::new(Hanging),
        ]);
        registry.mark_started();

        let started = Instant::now();
        let (status, _) = call(health_router(registry, status_info()), "/health/ready").await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);

        // Three 2s timeouts in series would be 6s of simulated time.
        assert!(
            started.elapsed() < Duration::from_secs(4),
            "checks appear to run in series: took {:?}",
            started.elapsed()
        );
    }

    #[tokio::test]
    async fn status_reports_versions_and_what_is_mounted() {
        let registry = HealthRegistry::new(vec![]);
        registry.mark_started();
        let (status, body) = call(health_router(registry, status_info()), "/health/status").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["service"], "test-service");
        assert_eq!(body["service_version"], "1.2.3");
        assert_eq!(body["framework_version"], FRAMEWORK_VERSION);
        assert!(!FRAMEWORK_VERSION.is_empty());
        assert_eq!(body["adapters"][0]["name"], "my-notes");
        assert_eq!(body["adapters"][0]["kind"], "rest");
        assert_eq!(body["adapters"][0]["resources"][0], "urn:my-notes:notes");
    }

    #[tokio::test]
    async fn with_no_checks_registered_ready_just_means_started() {
        let registry = HealthRegistry::new(vec![]);
        registry.mark_started();
        let (status, body) = call(health_router(registry, status_info()), "/health/ready").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["checks"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn reachability_targets_are_derived_from_urls() {
        for (url, expected) in [
            ("http://otel-collector:4317", "otel-collector:4317"),
            ("https://collector.example.com", "collector.example.com:443"),
            ("http://collector.example.com", "collector.example.com:80"),
            ("http://localhost:4317/v1/traces", "localhost:4317"),
            ("otel-collector:4317", "otel-collector:4317"),
        ] {
            let check = Reachable::from_url("otlp", "telemetry", url)
                .unwrap_or_else(|| panic!("could not derive a target from {url}"));
            assert_eq!(check.authority(), expected, "for {url}");
        }

        assert!(Reachable::from_url("otlp", "telemetry", "").is_none());
        assert!(Reachable::from_url("otlp", "telemetry", "http://").is_none());
    }

    #[tokio::test]
    async fn an_unreachable_target_reports_the_address_it_tried() {
        // Port 1 on loopback: reserved and not listening.
        let check = Reachable::new("nothing", "test", "127.0.0.1:1");
        let err = check.check().await.unwrap_err();
        assert!(err.contains("127.0.0.1:1"), "{err}");
    }

    #[tokio::test]
    async fn a_reachable_target_passes() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let check = Reachable::new("local", "test", addr.to_string());
        assert!(check.check().await.is_ok());
    }
}
