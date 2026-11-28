//! Liveness, readiness and status endpoints: three endpoint groups
//! (`server.endpoints`), each served, placed and logged as configured.
//!
//! | Group | Default path | Question it answers | Who asks |
//! |---|---|---|---|
//! | `probes` | `GET <framework_path>/health/live` | is the process alive? | the orchestrator's restart policy |
//! | `probes` | `GET <framework_path>/health/ready` | should it receive traffic? | the load balancer |
//! | `status` | `GET <framework_path>/health/status` | what is running in here? | a human |
//! | `public_status` | `GET <framework_path>/health/public` | is it working? | the public (off by default) |
//!
//! The distinction between live and ready is the one that matters operationally:
//! a service whose database is down is *live* (restarting it will not help) but not
//! *ready* (do not send it traffic). Conflating them turns a dependency outage into a
//! restart loop.
//!
//! `status` names every check and reports its error, which describes the
//! infrastructure behind the service. `public_status` says how the service is and
//! nothing about why: `{"status": "up"}` when ready, `"degraded"` (still 200) when
//! started but a check fails, `"down"` (503) before it has started and once it is
//! stopping.
//!
//! ### What checks itself, and what you register
//!
//! The runtime auto-registers checks for the things it *owns* — the OTLP endpoint it
//! was configured with, the in-process event bus it started, and with the
//! `persistence` feature the Postgres pool it built from `persistence.postgres`. A pool
//! a service builds itself is a line in the composition root:
//!
//! ```ignore
//! server.register_health_check(DatabaseCheck::new("reporting-db", pool.clone()))
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
    stopping: Arc<AtomicBool>,
    started_at: Instant,
}

impl HealthRegistry {
    pub(crate) fn new(checks: Vec<Arc<dyn HealthCheck>>) -> Self {
        Self {
            checks: checks.into(),
            started: Arc::new(AtomicBool::new(false)),
            stopping: Arc::new(AtomicBool::new(false)),
            started_at: Instant::now(),
        }
    }

    pub(crate) fn mark_started(&self) {
        self.started.store(true, Ordering::Release);
    }

    pub fn started(&self) -> bool {
        self.started.load(Ordering::Acquire)
    }

    /// From the moment shutdown begins, readiness fails, so the load balancer stops
    /// sending traffic while in-flight requests finish.
    pub(crate) fn mark_stopping(&self) {
        self.stopping.store(true, Ordering::Release);
    }

    pub fn stopping(&self) -> bool {
        self.stopping.load(Ordering::Acquire)
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
    Stopping,
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

/// Something `/health/status` asks for a report each time, such as a JetStream
/// consumer's pending count.
pub(crate) type StatusProbe =
    Arc<dyn Fn() -> futures::future::BoxFuture<'static, serde_json::Value> + Send + Sync>;

/// What `/health/status` reports about the running service.
#[derive(Clone, Serialize)]
pub(crate) struct StatusInfo {
    pub service: String,
    pub service_version: String,
    pub framework_version: &'static str,
    pub adapters: Vec<AdapterSummary>,
    #[serde(skip)]
    pub consumers: Vec<StatusProbe>,
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
    /// JetStream consumers: name, stream, filter subjects, pending count.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    consumers: Vec<serde_json::Value>,
    checks: Vec<CheckReport>,
}

#[derive(Clone)]
pub(crate) struct HealthEndpointState(pub HealthRegistry, pub StatusInfo);

/// `probes`: `<prefix>/live` and `<prefix>/ready`.
pub(crate) fn probes_router(registry: HealthRegistry, prefix: &str) -> Router {
    Router::new()
        .route(&format!("{prefix}/live"), get(live))
        .route(&format!("{prefix}/ready"), get(ready))
        .with_state(registry)
}

/// `status`, at `path`.
pub(crate) fn status_router(registry: HealthRegistry, status: StatusInfo, path: &str) -> Router {
    Router::new()
        .route(path, get(status_report))
        .with_state(HealthEndpointState(registry, status))
}

/// `public_status`, at `path`.
pub(crate) fn public_status_router(registry: HealthRegistry, path: &str) -> Router {
    Router::new()
        .route(path, get(public_status))
        .with_state(registry)
}

async fn live() -> impl IntoResponse {
    Json(serde_json::json!({ "status": "up" }))
}

async fn ready(State(registry): State<HealthRegistry>) -> impl IntoResponse {
    let checks = registry.run_checks().await;
    let ready = registry.started() && !registry.stopping() && checks.iter().all(CheckReport::is_up);
    let report = HealthReport {
        status: if registry.stopping() {
            HealthState::Stopping
        } else if ready {
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

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
enum PublicState {
    Up,
    Degraded,
    Down,
}

/// How the service is, and nothing about why: no check names, errors or versions.
#[derive(Serialize)]
struct PublicReport {
    status: PublicState,
}

async fn public_status(State(registry): State<HealthRegistry>) -> impl IntoResponse {
    let status = if !registry.started() || registry.stopping() {
        PublicState::Down
    } else if registry.run_checks().await.iter().all(CheckReport::is_up) {
        PublicState::Up
    } else {
        PublicState::Degraded
    };
    let code = if status == PublicState::Down {
        StatusCode::SERVICE_UNAVAILABLE
    } else {
        StatusCode::OK
    };
    (code, Json(PublicReport { status }))
}

async fn status_report(
    State(HealthEndpointState(registry, status)): State<HealthEndpointState>,
) -> impl IntoResponse {
    let checks = registry.run_checks().await;
    let consumers = futures::future::join_all(status.consumers.iter().map(|probe| probe())).await;
    Json(StatusReport {
        service: status.service.clone(),
        service_version: status.service_version.clone(),
        framework_version: status.framework_version,
        uptime_seconds: registry.uptime().as_secs(),
        adapters: status.adapters.clone(),
        consumers,
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
            consumers: Vec::new(),
        }
    }

    /// Every health group, at its default path under an empty framework prefix.
    fn health_router(registry: HealthRegistry, status: StatusInfo) -> Router {
        probes_router(registry.clone(), "/health")
            .merge(status_router(registry.clone(), status, "/health/status"))
            .merge(public_status_router(registry, "/health/public"))
    }

    #[tokio::test]
    async fn public_status_says_how_the_service_is_and_nothing_else() {
        let healthy = Arc::new(AtomicBool::new(true));
        struct Toggle(Arc<AtomicBool>);
        #[async_trait]
        impl HealthCheck for Toggle {
            fn name(&self) -> &str {
                "secret-database-name"
            }
            async fn check(&self) -> Result<(), String> {
                if self.0.load(Ordering::Acquire) {
                    Ok(())
                } else {
                    Err("connection refused by 10.0.0.7".into())
                }
            }
        }
        let registry = HealthRegistry::new(vec![Arc::new(Toggle(healthy.clone()))]);
        let router = health_router(registry.clone(), status_info());

        let (status, body) = call(router.clone(), "/health/public").await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "not started yet");
        assert_eq!(body, serde_json::json!({"status": "down"}));

        registry.mark_started();
        let (status, body) = call(router.clone(), "/health/public").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, serde_json::json!({"status": "up"}));

        healthy.store(false, Ordering::Release);
        let (status, body) = call(router.clone(), "/health/public").await;
        assert_eq!(status, StatusCode::OK, "degraded is still serving");
        assert_eq!(body, serde_json::json!({"status": "degraded"}));
        // The readiness probe, by contrast, takes it out of rotation.
        assert_eq!(
            call(router.clone(), "/health/ready").await.0,
            StatusCode::SERVICE_UNAVAILABLE
        );

        healthy.store(true, Ordering::Release);
        registry.mark_stopping();
        let (status, body) = call(router, "/health/public").await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "stopping");
        assert_eq!(body, serde_json::json!({"status": "down"}));
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
    async fn a_stopping_server_is_not_ready_but_still_live() {
        let registry = HealthRegistry::new(vec![]);
        registry.mark_started();
        let router = health_router(registry.clone(), status_info());
        assert_eq!(
            call(router.clone(), "/health/ready").await.0,
            StatusCode::OK
        );

        registry.mark_stopping();
        let (status, body) = call(router.clone(), "/health/ready").await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body["status"], "stopping");
        assert_eq!(call(router, "/health/live").await.0, StatusCode::OK);
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
