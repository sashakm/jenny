//! The composition root.

use std::future::IntoFuture;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

#[cfg(feature = "messaging")]
use crate::config::DEFAULT_EVENT_NAMESPACE;
use axum::Router;
use axum::extract::DefaultBodyLimit;
use clap::Parser;
use jenny_core::task::{ScheduledTask, TaskScheduler};
#[cfg(feature = "messaging")]
use jenny_messaging::MessagingAdapter;
use jenny_rest::{RequestId, RequestIdLayer, RestAdapter};
use tokio::net::TcpListener;
use tokio::sync::watch;
use tower_http::trace::TraceLayer;
use tracing::{info, warn};

use crate::{
    config::{ConfigSource, ServerArgs, ServerConfig},
    context::ServerContext,
    health::{AdapterSummary, HealthCheck, HealthRegistry, StatusInfo, health_router},
    telemetry::TelemetryHandle,
};

/// The one place that knows every layer.
///
/// `main` is the composition root of a jenny service: it reads configuration,
/// constructs the concrete driven adapters, injects them into use cases, hands those
/// to the driving adapters, and registers the result here. There is no container and
/// no runtime injection — the dependency graph is the code you can read in `main`, and
/// if something is not wired there, it does not exist.
///
/// ```no_run
/// use jenny_server::JennyServer;
///
/// #[tokio::main]
/// async fn main() -> anyhow::Result<()> {
///     let mut server = JennyServer::from_args()?;   // config loaded, telemetry up
///
///     // let config: MyConfig = server.context().app_section("my-notes")?;
///     // let store = Arc::new(Memory::<Note>::new());              // driven adapter
///     // let notes = Notes::new(store, Events::new(Arc::new(EventLog))); // application
///     // let adapter = RestAdapter::new("my-notes")?              // driving adapter
///     //     .mount(rest::notes::routes(notes))?;
///     // server = server.register(adapter)?;
///
///     server.run().await
/// }
/// ```
pub struct JennyServer {
    config: ServerConfig,
    context: ServerContext,
    telemetry: Option<TelemetryHandle>,
    rest_adapters: Vec<RestAdapter>,
    #[cfg(feature = "messaging")]
    messaging_adapters: Vec<MessagingAdapter>,
    scheduler: TaskScheduler,
    checks: Vec<Arc<dyn HealthCheck>>,
    version: Option<String>,
    #[cfg(feature = "messaging")]
    events: Option<Arc<jenny_messaging::EventBus>>,
    #[cfg(feature = "messaging")]
    broker: Option<topmesys::EventBroker<topmesys::type_states::Running>>,
    #[cfg(feature = "persistence")]
    postgres: Option<jenny_persistence::Postgres>,
}

/// Leading slash on, trailing slash off. `"/"` or `""` mean "no prefix", which axum
/// expresses by not nesting rather than by nesting under `"/"`.
fn normalize_prefix(path: &str) -> String {
    let trimmed = path.trim_end_matches('/');
    if trimmed.is_empty() {
        String::new()
    } else if trimmed.starts_with('/') {
        trimmed.to_string()
    } else {
        format!("/{trimmed}")
    }
}

/// Combine every adapter's OpenAPI document into one.
///
/// Adapters are separate bounded contexts but they share a host and a port, so clients
/// want one document describing everything reachable at that address.
#[cfg(feature = "openapi")]
fn merged_openapi(adapters: &[RestAdapter]) -> Option<utoipa::openapi::OpenApi> {
    let mut documents = adapters.iter().filter_map(|a| a.openapi().cloned());
    let mut merged = documents.next()?;
    for document in documents {
        merged.merge(document);
    }
    Some(merged)
}

/// Asks a running server to shut down gracefully. Cheap to clone.
#[derive(Clone, Debug)]
pub struct ServerHandle(watch::Sender<bool>);

impl ServerHandle {
    /// Begin the shutdown sequence: report not ready, keep serving for
    /// `server.shutdown_delay_seconds`, then stop accepting connections and let
    /// in-flight requests finish within `server.shutdown_timeout_seconds`.
    pub fn shutdown(&self) {
        let _ = self.0.send(true);
    }
}

/// Resolves on `SIGINT` or, on Unix, `SIGTERM`, which is what `docker stop` and
/// Kubernetes send. A signal handler that cannot be installed never resolves rather
/// than shutting the server down at once.
async fn shutdown_signal() {
    let interrupt = async {
        if tokio::signal::ctrl_c().await.is_err() {
            warn!("cannot listen for SIGINT");
            std::future::pending::<()>().await;
        }
    };
    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut terminate) => {
                terminate.recv().await;
            }
            Err(e) => {
                warn!("cannot listen for SIGTERM: {e}");
                std::future::pending::<()>().await;
            }
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        () = interrupt => info!("SIGINT received"),
        () = terminate => info!("SIGTERM received"),
    }
}

/// A bound-but-not-yet-serving server.
///
/// [`JennyServer::bind`] stops one step short of serving so tests and embedders can
/// take the socket's real address before any traffic flows — which is what makes
/// binding to port `0` practical instead of hard-coding a port and hoping it is free.
pub struct ServerParts {
    listener: TcpListener,
    router: Router,
    shutdown: watch::Sender<bool>,
    shutdown_delay: Duration,
    shutdown_timeout: Duration,
    /// Dropping this flushes telemetry exporters.
    pub telemetry: Option<TelemetryHandle>,
    /// The background scheduler, if any tasks were scheduled. It stops starting runs
    /// when shutdown begins and finishes once the runs in progress have.
    pub scheduler: Option<tokio::task::JoinHandle<()>>,
    /// The health registry of this server.
    pub health: HealthRegistry,
    /// The running event broker, stopped on shutdown so in-flight listeners finish.
    #[cfg(feature = "messaging")]
    broker: Option<topmesys::EventBroker<topmesys::type_states::Running>>,
    /// The Postgres pool, closed once requests and listeners are done with it.
    #[cfg(feature = "persistence")]
    postgres: Option<jenny_persistence::Postgres>,
}

impl ServerParts {
    /// The address actually bound — the resolved one, when the configured port was `0`.
    pub fn local_addr(&self) -> anyhow::Result<SocketAddr> {
        Ok(self.listener.local_addr()?)
    }

    /// A handle that can ask this server to stop.
    pub fn handle(&self) -> ServerHandle {
        ServerHandle(self.shutdown.clone())
    }

    /// Serve until [`ServerHandle::shutdown`] is called, then drain, flush and stop.
    pub async fn serve(self) -> anyhow::Result<()> {
        let Self {
            listener,
            router,
            shutdown,
            shutdown_delay,
            shutdown_timeout,
            telemetry,
            scheduler,
            health,
            #[cfg(feature = "messaging")]
            broker,
            #[cfg(feature = "persistence")]
            postgres,
        } = self;

        let mut requested = shutdown.subscribe();
        let close_listener = async move {
            let _ = requested.wait_for(|stopping| *stopping).await;
            health.mark_stopping();
            info!(
                delay_seconds = shutdown_delay.as_secs(),
                "shutting down: reporting not ready"
            );
            tokio::time::sleep(shutdown_delay).await;
            info!("closing the listener and draining in-flight requests");
        };
        let mut requested = shutdown.subscribe();
        let deadline = async move {
            let _ = requested.wait_for(|stopping| *stopping).await;
            tokio::time::sleep(shutdown_delay + shutdown_timeout).await;
        };

        let serving = axum::serve(listener, router)
            .with_graceful_shutdown(close_listener)
            .into_future();
        let abort_scheduler = scheduler.as_ref().map(|handle| handle.abort_handle());
        // In-flight requests and scheduled runs in progress share one deadline.
        let drain = async move {
            let result = serving.await;
            if let Some(handle) = scheduler {
                let _ = handle.await;
            }
            result
        };
        let result = tokio::select! {
            result = drain => result,
            () = deadline => {
                warn!(
                    timeout_seconds = shutdown_timeout.as_secs(),
                    "in-flight work did not finish in time; stopping without it"
                );
                if let Some(abort) = abort_scheduler {
                    abort.abort();
                }
                Ok(())
            }
        };
        // Stopping the broker waits for listeners that are still handling an event, so
        // a graceful shutdown does not drop side effects that were already in flight.
        #[cfg(feature = "messaging")]
        if let Some(broker) = broker {
            broker.stop().await;
        }
        // Last, because listeners stopping above may still have been writing.
        #[cfg(feature = "persistence")]
        if let Some(postgres) = postgres {
            postgres.close().await;
        }
        drop(telemetry);
        Ok(result?)
    }
}

impl JennyServer {
    /// Load configuration from the process arguments and bring telemetry up.
    pub fn from_args() -> anyhow::Result<Self> {
        Self::from_parsed_args(ServerArgs::parse())
    }

    /// [`from_args`](Self::from_args) with arguments you built yourself.
    pub fn from_parsed_args(args: ServerArgs) -> anyhow::Result<Self> {
        let source = ConfigSource::load(&args)?;
        let config = source.server_config(&args)?;
        let telemetry = config.telemetry.clone().init()?;
        let mut server = Self::from_config(config, source);
        server.telemetry = Some(telemetry);
        Ok(server)
    }

    /// Build from an already-loaded configuration, **without** touching telemetry.
    /// Use [`from_args`](Self::from_args) for the full bootstrap.
    pub fn from_config(config: ServerConfig, source: ConfigSource) -> Self {
        Self {
            config,
            context: ServerContext::new(source),
            telemetry: None,
            rest_adapters: Vec::new(),
            #[cfg(feature = "messaging")]
            messaging_adapters: Vec::new(),
            scheduler: TaskScheduler::default(),
            checks: Vec::new(),
            version: None,
            #[cfg(feature = "messaging")]
            events: None,
            #[cfg(feature = "messaging")]
            broker: None,
            #[cfg(feature = "persistence")]
            postgres: None,
        }
    }

    /// Record the service's own version, reported by `/health/status`.
    ///
    /// Pass [`service_version!`](jenny_core::service_version), which expands to your
    /// crate's version at the call site:
    ///
    /// ```no_run
    /// # use jenny_server::JennyServer;
    /// # fn go(server: JennyServer) -> JennyServer {
    /// server.with_version(jenny_core::service_version!())
    /// # }
    /// ```
    ///
    /// Falls back to `telemetry.service_version` from configuration, then to
    /// `"unknown"`.
    pub fn with_version(mut self, version: impl Into<String>) -> Self {
        self.version = Some(version.into());
        self
    }

    pub fn config(&self) -> &ServerConfig {
        &self.config
    }

    /// The in-process event bus, behind the
    /// [`EventPublisher`](jenny_core::port::outbound::EventPublisher) port.
    ///
    /// The broker starts on the first call and stops when the server does. Available
    /// during wiring, which is the point — use cases need the publisher before anything
    /// is served:
    ///
    /// ```no_run
    /// # use std::sync::Arc;
    /// # use jenny_server::JennyServer;
    /// # struct Notes; impl Notes { fn new(_: Arc<dyn jenny_core::port::outbound::EventPublisher>) -> Self { Notes } }
    /// # fn wire(mut server: JennyServer) -> anyhow::Result<()> {
    /// let notes = Arc::new(Notes::new(server.events()?));
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// Fails only when there is no tokio runtime to run the broker's event loop on.
    #[cfg(feature = "messaging")]
    pub fn events(
        &mut self,
    ) -> anyhow::Result<Arc<dyn jenny_core::port::outbound::EventPublisher>> {
        self.ensure_broker()?;
        Ok(self.events.clone().expect("broker was just started"))
    }

    /// Subscribe a listener to the in-process bus.
    ///
    /// Each listener is its own small outbound effect, and one published event reaches
    /// every listener whose topic pattern matches. Adding one is additive: nothing that
    /// publishes needs to know it exists.
    ///
    /// ```no_run
    /// # use jenny_server::JennyServer;
    /// # fn wire(mut server: JennyServer) -> anyhow::Result<()> {
    /// # #[derive(Debug)] struct AuditLog;
    /// # #[async_trait::async_trait]
    /// # impl topmesys::EventConsumer for AuditLog {
    /// #     type Topic = ();
    /// #     fn subscriptions(&self) -> topmesys::Subscriptions<()> { "my-notes.note.*".into() }
    /// #     async fn handle_event(&self, _: &(), _: &topmesys::Delivery) -> Result<(), topmesys::HandlerError> { Ok(()) }
    /// # }
    /// server.subscribe(AuditLog)?;
    /// # Ok(())
    /// # }
    /// ```
    #[cfg(feature = "messaging")]
    pub fn subscribe(
        &mut self,
        listener: impl topmesys::EventConsumer,
    ) -> anyhow::Result<&mut Self> {
        self.ensure_broker()?;
        let consumer = std::any::type_name_of_val(&listener);
        self.broker
            .as_ref()
            .expect("broker was just started")
            .add_topic_consumer(listener)?;
        info!(consumer, "subscribed event listener");
        Ok(self)
    }

    /// Start the broker if it is not running yet.
    #[cfg(feature = "messaging")]
    fn ensure_broker(&mut self) -> anyhow::Result<()> {
        if self.broker.is_some() {
            return Ok(());
        }
        // An empty namespace would produce topics like `.note.created`, which the
        // broker rejects as having an empty segment — so fall back rather than build
        // a bus whose every publish fails.
        let namespace = self
            .config
            .messaging
            .namespace
            .clone()
            .filter(|n| !n.trim().is_empty())
            .or_else(|| {
                Some(self.config.telemetry.service_name.clone()).filter(|n| !n.trim().is_empty())
            })
            .unwrap_or_else(|| DEFAULT_EVENT_NAMESPACE.to_string());
        let broker = topmesys::EventBroker::new(self.config.messaging.buffer_size).run()?;
        info!(
            namespace,
            buffer = self.config.messaging.buffer_size,
            "event broker started"
        );
        self.events = Some(Arc::new(jenny_messaging::EventBus::new(
            broker.get_sender(),
            namespace,
        )));
        self.broker = Some(broker);
        Ok(())
    }

    /// The Postgres pool configured under `persistence.postgres`, or `None` when no
    /// `url` is set.
    ///
    /// The pool connects lazily, so this is available during wiring without a database
    /// being up yet. The runtime then does the rest before it binds its socket: it runs
    /// the migrations given to [`Postgres::migrate`](jenny_persistence::Postgres::migrate),
    /// creates the tables of every document store handed out, and registers a
    /// `postgres` readiness check. A database that is down or a migration that fails
    /// stops the service at startup, not on its first request.
    ///
    /// `None` is what makes "it only needs configuration" work: the same build keeps its
    /// data in memory until a URL is configured.
    ///
    /// ```no_run
    /// # use std::sync::Arc;
    /// # use jenny_core::domain::{Aggregate, ValueKind};
    /// # use jenny_core::memory::Memory;
    /// # use jenny_core::port::Store;
    /// # use jenny_server::JennyServer;
    /// # #[derive(serde::Serialize, serde::Deserialize)] struct Note { id: u32 }
    /// # impl Aggregate for Note { type Id = u32; const KIND: &'static str = "note"; fn id(&self) -> u32 { self.id } }
    /// # fn wire(mut server: JennyServer) -> anyhow::Result<()> {
    /// const QUERYABLE: [(&str, ValueKind); 1] = [("title", ValueKind::Text)];
    ///
    /// let notes: Arc<dyn Store<Note>> = match server.postgres()? {
    ///     Some(postgres) => Arc::new(postgres.documents::<Note>().queryable(QUERYABLE)),
    ///     None => Arc::new(Memory::<Note>::new().queryable(QUERYABLE)),
    /// };
    /// # Ok(())
    /// # }
    /// ```
    #[cfg(feature = "persistence")]
    pub fn postgres(&mut self) -> anyhow::Result<Option<jenny_persistence::Postgres>> {
        if self.postgres.is_none() && self.config.persistence.postgres.url.is_some() {
            let postgres =
                jenny_persistence::Postgres::connect_lazy(&self.config.persistence.postgres)?;
            info!(
                max_connections = self.config.persistence.postgres.max_connections,
                "postgres pool configured"
            );
            self.postgres = Some(postgres);
        }
        Ok(self.postgres.clone())
    }

    /// Configuration and shared resources for wiring adapters up.
    pub fn context(&self) -> &ServerContext {
        &self.context
    }

    pub fn context_mut(&mut self) -> &mut ServerContext {
        &mut self.context
    }

    /// Mount a REST driving adapter. Fails when another adapter already occupies the
    /// same base path, because the second one's routes would silently never match.
    pub fn register(mut self, adapter: RestAdapter) -> anyhow::Result<Self> {
        if let Some(existing) = self
            .rest_adapters
            .iter()
            .find(|a| a.path() == adapter.path())
        {
            anyhow::bail!(
                "cannot mount `{}` at `{}`: `{}` is already mounted there",
                adapter.name(),
                adapter.path(),
                existing.name()
            );
        }
        self.rest_adapters.push(adapter);
        Ok(self)
    }

    /// Register a messaging driving adapter.
    ///
    /// **Declaration only for now**: the subjects are recorded and logged at startup,
    /// but no dispatch path runs yet.
    #[cfg(feature = "messaging")]
    pub fn register_messaging(mut self, adapter: MessagingAdapter) -> Self {
        self.messaging_adapters.push(adapter);
        self
    }

    /// Add a readiness signal, reported by `/health/ready` and `/health/status`.
    pub fn register_health_check(mut self, check: impl HealthCheck) -> Self {
        self.checks.push(Arc::new(check));
        self
    }

    /// Run a task on a cron schedule alongside the server.
    pub fn schedule(mut self, task: ScheduledTask) -> Self {
        self.scheduler = self.scheduler.with_task(task);
        self
    }

    /// Assemble the router and bind the socket, without serving yet.
    pub async fn bind(self) -> anyhow::Result<ServerParts> {
        let Self {
            config,
            context: _,
            telemetry,
            mut rest_adapters,
            #[cfg(feature = "messaging")]
            messaging_adapters,
            scheduler,
            #[allow(unused_mut)]
            mut checks,
            version,
            #[cfg(feature = "messaging")]
            events,
            #[cfg(feature = "messaging")]
            broker,
            #[cfg(feature = "persistence")]
            postgres,
        } = self;

        // Schema first: nothing should be served against tables that do not exist yet.
        #[cfg(feature = "persistence")]
        if let Some(postgres) = postgres.as_ref() {
            postgres
                .prepare()
                .await
                .map_err(|e| e.context("preparing the Postgres schema"))?;
            info!("postgres schema prepared");
            checks.push(Arc::new(postgres.health_check()));
        }

        #[cfg(feature = "messaging")]
        for adapter in &messaging_adapters {
            for binding in adapter.subjects() {
                info!(
                    adapter = adapter.name(),
                    subject = binding.subject,
                    "declared messaging subject"
                );
            }
            warn!(
                adapter = adapter.name(),
                "messaging dispatch is not implemented yet; subjects are declared only"
            );
        }

        // Adapters that did not choose a path in code take the configured one.
        for adapter in &mut rest_adapters {
            adapter.apply_default_path(&config.server.api_path);
        }

        // `register` rejects collisions between the paths known at that point, but
        // applying the configured default can create a new one — an adapter explicitly
        // at `/x` and `server.api_path: /x`. Checking again here is what makes that
        // a startup failure rather than routes that silently never match.
        for (index, adapter) in rest_adapters.iter().enumerate() {
            if let Some(other) = rest_adapters[..index]
                .iter()
                .find(|earlier| earlier.path() == adapter.path())
            {
                anyhow::bail!(
                    "`{}` and `{}` both resolve to the mount path `{}`; \
                     set `server.api_path` or call `.at(…)` on one of them",
                    other.name(),
                    adapter.name(),
                    adapter.path()
                );
            }
        }

        // Checks for what the runtime itself owns. Anything it does not own — a
        // database pool above all — is registered by the composition root.
        #[cfg(feature = "telemetry-otlp")]
        if let Some(endpoint) = config.telemetry.otlp_endpoint.as_deref()
            && let Some(check) = crate::health::Reachable::from_url("otlp", "telemetry", endpoint)
        {
            info!(target = check.authority(), "watching otlp endpoint");
            checks.push(Arc::new(check));
        }
        #[cfg(feature = "messaging")]
        if let Some(bus) = events.as_ref() {
            checks.push(Arc::new(jenny_messaging::EventBusCheck::new(
                "event-bus",
                bus.as_ref(),
            )));
        }

        let health = HealthRegistry::new(checks);
        let status = StatusInfo {
            service: config.telemetry.service_name.clone(),
            service_version: version
                .or_else(|| config.telemetry.service_version.clone())
                .unwrap_or_else(|| "unknown".to_string()),
            framework_version: crate::health::FRAMEWORK_VERSION,
            adapters: {
                #[allow(unused_mut)]
                let mut adapters: Vec<AdapterSummary> = rest_adapters
                    .iter()
                    .map(|adapter| AdapterSummary {
                        name: adapter.name().to_string(),
                        kind: "rest",
                        path: adapter.path().to_string(),
                        resources: adapter
                            .resources()
                            .iter()
                            .map(|r| r.urn().to_string())
                            .collect(),
                    })
                    .collect();
                #[cfg(feature = "messaging")]
                adapters.extend(messaging_adapters.iter().map(|adapter| AdapterSummary {
                    name: adapter.name().to_string(),
                    kind: "messaging",
                    path: adapter.subject_prefix().to_string(),
                    resources: adapter.subjects().into_iter().map(|b| b.subject).collect(),
                }));
                adapters
            },
        };

        // The framework's own endpoints, gathered under one prefix so they can never
        // collide with a route the service wanted.
        let mut framework = Router::new();
        if config.server.health {
            framework = framework.merge(health_router(health.clone(), status));
        }

        #[cfg(feature = "telemetry-prometheus")]
        if config.server.metrics.enabled
            && let Some(registry) = telemetry
                .as_ref()
                .and_then(|handle| handle.prometheus_registry().cloned())
        {
            let mut metrics = crate::metrics::metrics_router(registry);
            if let Some(auth) = config.server.metrics.auth.as_ref() {
                info!("metrics endpoint requires basic authentication");
                metrics = metrics.layer(
                    jenny_rest::security::BasicAuth::new(&auth.username, &auth.password)
                        .with_realm("metrics"),
                );
            }
            framework = framework.merge(metrics);
        }

        #[cfg(feature = "openapi")]
        if let Some(document) = merged_openapi(&rest_adapters) {
            framework = framework.merge(crate::openapi::openapi_router(document));
        }

        let framework_path = normalize_prefix(&config.server.framework_path);
        let mut router = if framework_path.is_empty() {
            framework
        } else {
            Router::new().nest(&framework_path, framework)
        };

        for adapter in rest_adapters {
            router = router.merge(adapter.into_router());
        }

        // Everything below applies to every route, including the fallbacks, so that a
        // 404 looks like every other failure rather than like a hole in the API.
        let router = router
            .fallback(jenny_rest::not_found)
            .method_not_allowed_fallback(jenny_rest::method_not_allowed)
            .layer(DefaultBodyLimit::max(config.server.body_limit))
            .layer(TraceLayer::new_for_http().make_span_with(request_span))
            // Outermost, so the trace span, every layer and every problem document see
            // the same id.
            .layer(RequestIdLayer);

        let listener = TcpListener::bind(&config.server.address).await?;
        let (shutdown, _) = watch::channel(false);

        let scheduler_handle = (!scheduler.is_empty()).then(|| {
            let mut requested = shutdown.subscribe();
            tokio::spawn(scheduler.run_until(async move {
                let _ = requested.wait_for(|stopping| *stopping).await;
            }))
        });

        info!(address = %listener.local_addr()?, "jenny server bound");
        health.mark_started();

        Ok(ServerParts {
            listener,
            router,
            shutdown,
            shutdown_delay: Duration::from_secs(config.server.shutdown_delay_seconds),
            shutdown_timeout: Duration::from_secs(config.server.shutdown_timeout_seconds),
            telemetry,
            scheduler: scheduler_handle,
            health,
            #[cfg(feature = "messaging")]
            broker,
            #[cfg(feature = "persistence")]
            postgres,
        })
    }

    /// Bind and serve until `SIGINT` or `SIGTERM`, then shut down gracefully.
    pub async fn run(self) -> anyhow::Result<()> {
        let parts = self.bind().await?;
        let handle = parts.handle();
        tokio::spawn(async move {
            shutdown_signal().await;
            handle.shutdown();
        });
        parts.serve().await
    }
}

/// The span every request is traced in, carrying the id [`RequestIdLayer`] assigned.
fn request_span(request: &axum::extract::Request) -> tracing::Span {
    let request_id = request
        .extensions()
        .get::<RequestId>()
        .map(ToString::to_string)
        .unwrap_or_default();
    tracing::debug_span!(
        "request",
        method = %request.method(),
        uri = %request.uri(),
        request_id,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::{Body, to_bytes};
    use http::StatusCode;
    use jenny_rest::Routes;
    use tower::ServiceExt;

    fn server_with(address: &str) -> JennyServer {
        let args = ServerArgs {
            address: Some(address.to_string()),
            env_prefix: Some("JENNY_TEST_SERVER".into()),
            ..Default::default()
        };
        let source = ConfigSource::load(&args).unwrap();
        let config = source.server_config(&args).unwrap();
        JennyServer::from_config(config, source)
    }

    fn notes() -> RestAdapter {
        RestAdapter::new("my-notes")
            .unwrap()
            .mount(Routes::new("notes").get("/notes", || async { "[]" }))
            .unwrap()
    }

    async fn call(router: Router, uri: &str) -> (StatusCode, serde_json::Value) {
        let response = router
            .oneshot(http::Request::get(uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let body = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
        (status, body)
    }

    #[test]
    fn two_adapters_cannot_claim_the_same_mount_path() {
        let server = server_with("127.0.0.1:0")
            .register(RestAdapter::new("app-one").unwrap())
            .unwrap();
        let Err(err) = server.register(RestAdapter::new("app-two").unwrap()) else {
            panic!("mounting a second adapter at the default path should be rejected");
        };
        assert!(err.to_string().contains("already mounted"));
        assert!(err.to_string().contains("app-one"));
    }

    #[test]
    fn different_mount_paths_coexist() {
        let server = server_with("127.0.0.1:0")
            .register(RestAdapter::new("app-one").unwrap())
            .unwrap();
        assert!(
            server
                .register(RestAdapter::new("app-two").unwrap().at("/internal"))
                .is_ok()
        );
    }

    #[tokio::test]
    async fn a_collision_created_by_configuration_is_caught_at_startup() {
        // `register` cannot see this one: at that point the defaulted adapter still
        // reads `/api`, and only the configured default makes the two collide.
        let mut server = server_with("127.0.0.1:0");
        server.config.server.api_path = "/internal".to_string();

        let server = server
            .register(RestAdapter::new("explicit").unwrap().at("/internal"))
            .unwrap()
            .register(RestAdapter::new("defaulted").unwrap())
            .unwrap();

        let Err(err) = server.bind().await else {
            panic!("a configured mount-path collision should fail at startup");
        };
        assert!(err.to_string().contains("both resolve to"), "{err}");
        assert!(err.to_string().contains("/internal"), "{err}");
    }

    #[tokio::test]
    async fn binding_to_port_zero_reports_the_real_address() {
        let parts = server_with("127.0.0.1:0").bind().await.unwrap();
        let addr = parts.local_addr().unwrap();
        assert_ne!(addr.port(), 0);
        assert!(parts.health.started());
    }

    #[tokio::test]
    async fn a_shutdown_reports_not_ready_while_it_still_serves() {
        let mut server = server_with("127.0.0.1:0");
        server.config.server.shutdown_delay_seconds = 1;
        let parts = server.bind().await.unwrap();
        let addr = parts.local_addr().unwrap();
        let handle = parts.handle();
        let serving = tokio::spawn(parts.serve());

        let before = raw_get(&format!("http://{addr}/j/health/ready")).await;
        assert!(before.starts_with("HTTP/1.0 200"), "{before}");
        handle.shutdown();
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        let during_delay = raw_get(&format!("http://{addr}/j/health/ready")).await;
        assert!(during_delay.starts_with("HTTP/1.0 503"), "{during_delay}");
        assert!(during_delay.contains("\"stopping\""), "{during_delay}");

        serving.await.unwrap().unwrap();
        assert!(
            tokio::net::TcpStream::connect(addr).await.is_err(),
            "after the delay the listener is closed"
        );
    }

    #[tokio::test]
    async fn the_drain_is_bounded_by_the_shutdown_timeout() {
        let mut server = server_with("127.0.0.1:0");
        server.config.server.shutdown_timeout_seconds = 1;
        let server = server
            .register(
                RestAdapter::new("slow")
                    .unwrap()
                    .mount(Routes::new("slow").get("/slow", || async {
                        tokio::time::sleep(std::time::Duration::from_secs(60)).await;
                        "done"
                    }))
                    .unwrap(),
            )
            .unwrap();
        let parts = server.bind().await.unwrap();
        let addr = parts.local_addr().unwrap();
        let handle = parts.handle();
        let serving = tokio::spawn(parts.serve());

        let stuck = tokio::spawn(async move { raw_get(&format!("http://{addr}/api/slow")).await });
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        handle.shutdown();

        let started = std::time::Instant::now();
        serving.await.unwrap().unwrap();
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
        stuck.abort();
    }

    #[tokio::test]
    async fn every_response_carries_the_request_id_and_problems_name_it() {
        let parts = server_with("127.0.0.1:0").bind().await.unwrap();
        let response = parts
            .router
            .clone()
            .oneshot(http::Request::get("/nowhere").body(Body::empty()).unwrap())
            .await
            .unwrap();
        let id = response.headers()[jenny_rest::REQUEST_ID_HEADER]
            .to_str()
            .unwrap()
            .to_string();
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let problem: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(problem["instance"], format!("urn:uuid:{id}"));
    }

    #[tokio::test]
    async fn a_bound_server_serves_and_then_stops_on_request() {
        let parts = server_with("127.0.0.1:0").bind().await.unwrap();
        let addr = parts.local_addr().unwrap();
        let handle = parts.handle();
        let serving = tokio::spawn(parts.serve());

        let body = raw_get(&format!("http://{addr}/j/health/live")).await;
        assert!(body.contains("\"up\""), "got {body}");

        handle.shutdown();
        serving.await.unwrap().unwrap();
    }

    /// Minimal GET so the runtime crate needs no HTTP client dependency.
    async fn raw_get(url: &str) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let url = url.strip_prefix("http://").unwrap();
        let (authority, path) = url.split_once('/').unwrap();
        let mut stream = tokio::net::TcpStream::connect(authority).await.unwrap();
        stream
            .write_all(format!("GET /{path} HTTP/1.0\r\nHost: {authority}\r\n\r\n").as_bytes())
            .await
            .unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).await.unwrap();
        response
    }

    #[tokio::test]
    async fn framework_endpoints_live_under_the_framework_path() {
        let parts = server_with("127.0.0.1:0")
            .register(notes())
            .unwrap()
            .bind()
            .await
            .unwrap();
        let router = parts.router.clone();

        let (status, body) = call(router.clone(), "/j/health/live").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, serde_json::json!({"status": "up"}));

        // ...and not at the root, where a service route might want to live.
        let (status, _) = call(router.clone(), "/health/live").await;
        assert_eq!(status, StatusCode::NOT_FOUND);

        // The API is where it always was.
        let (status, _) = call(router, "/api/notes").await;
        assert_eq!(status, StatusCode::OK);
    }

    #[tokio::test]
    async fn the_framework_path_is_configurable() {
        let mut server = server_with("127.0.0.1:0");
        server.config.server.framework_path = "/_internal".to_string();
        let parts = server.bind().await.unwrap();

        let (status, _) = call(parts.router.clone(), "/_internal/health/live").await;
        assert_eq!(status, StatusCode::OK);
        let (status, _) = call(parts.router.clone(), "/j/health/live").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn the_api_path_is_configurable_but_an_explicit_one_wins() {
        let mut server = server_with("127.0.0.1:0");
        server.config.server.api_path = "/service".to_string();
        let parts = server
            .register(notes())
            .unwrap()
            .register(
                RestAdapter::new("pinned")
                    .unwrap()
                    .at("/v2")
                    .mount(Routes::new("ping").get("/ping", || async { "pong" }))
                    .unwrap(),
            )
            .unwrap()
            .bind()
            .await
            .unwrap();

        let (status, _) = call(parts.router.clone(), "/service/notes").await;
        assert_eq!(status, StatusCode::OK, "the default moved to /service");

        let (status, _) = call(parts.router.clone(), "/api/notes").await;
        assert_eq!(status, StatusCode::NOT_FOUND);

        let (status, _) = call(parts.router.clone(), "/v2/ping").await;
        assert_eq!(status, StatusCode::OK, "an explicit `.at` is not relocated");
    }

    #[tokio::test]
    async fn status_reports_both_versions() {
        let parts = server_with("127.0.0.1:0")
            .with_version("9.9.9")
            .register(notes())
            .unwrap()
            .bind()
            .await
            .unwrap();

        let (status, body) = call(parts.router.clone(), "/j/health/status").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["service_version"], "9.9.9");
        assert_eq!(body["framework_version"], crate::health::FRAMEWORK_VERSION);
    }

    #[tokio::test]
    async fn an_unset_service_version_is_reported_as_unknown() {
        let parts = server_with("127.0.0.1:0").bind().await.unwrap();
        let (_, body) = call(parts.router.clone(), "/j/health/status").await;
        assert_eq!(body["service_version"], "unknown");
    }

    #[tokio::test]
    async fn health_endpoints_can_be_switched_off() {
        let mut server = server_with("127.0.0.1:0");
        server.config.server.health = false;
        let parts = server.bind().await.unwrap();

        let (status, _) = call(parts.router.clone(), "/j/health/live").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn unmatched_routes_answer_with_a_problem_document() {
        let parts = server_with("127.0.0.1:0")
            .register(notes())
            .unwrap()
            .bind()
            .await
            .unwrap();

        let response = parts
            .router
            .clone()
            .oneshot(http::Request::get("/nope").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert_eq!(
            response.headers().get(http::header::CONTENT_TYPE).unwrap(),
            jenny_rest::PROBLEM_CONTENT_TYPE
        );
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["type"], "urn:jenny:problem:not-found");
    }

    #[tokio::test]
    async fn a_wrong_method_answers_with_a_problem_document() {
        let parts = server_with("127.0.0.1:0")
            .register(notes())
            .unwrap()
            .bind()
            .await
            .unwrap();

        let response = parts
            .router
            .clone()
            .oneshot(
                http::Request::post("/api/notes")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["type"], "urn:jenny:problem:method-not-allowed");
    }

    #[tokio::test]
    async fn a_body_over_the_limit_is_rejected_when_a_handler_reads_it() {
        let mut server = server_with("127.0.0.1:0");
        server.config.server.body_limit = 16;
        let parts = server
            .register(
                RestAdapter::new("echo")
                    .unwrap()
                    .mount(Routes::new("echo").post("/echo", |body: String| async move { body }))
                    .unwrap(),
            )
            .unwrap()
            .bind()
            .await
            .unwrap();

        let router = parts.router.clone();

        let within = router
            .clone()
            .oneshot(
                http::Request::post("/api/echo")
                    .body(Body::from("small"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(within.status(), StatusCode::OK);

        let over = router
            .oneshot(
                http::Request::post("/api/echo")
                    .body(Body::from("x".repeat(128)))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(over.status(), StatusCode::PAYLOAD_TOO_LARGE);
    }

    #[cfg(feature = "messaging")]
    #[tokio::test]
    async fn status_lists_rest_and_messaging_adapters() {
        use jenny_messaging::{MessagingAdapter, SubjectBinding};

        let parts = server_with("127.0.0.1:0")
            .register(notes())
            .unwrap()
            .register_messaging(
                MessagingAdapter::new("my-notes-events", "notes")
                    .unwrap()
                    .subscribe(SubjectBinding::new("archived")),
            )
            .bind()
            .await
            .unwrap();

        let (_, body) = call(parts.router.clone(), "/j/health/status").await;
        assert_eq!(body["adapters"][0]["kind"], "rest");
        assert_eq!(body["adapters"][0]["resources"][0], "urn:my-notes:notes");
        assert_eq!(body["adapters"][1]["kind"], "messaging");
        assert_eq!(body["adapters"][1]["resources"][0], "notes.archived");
    }

    #[cfg(feature = "messaging")]
    #[tokio::test]
    async fn the_event_bus_is_available_during_wiring_and_checked_for_health() {
        use jenny_core::port::outbound::EventEnvelope;

        let mut server = server_with("127.0.0.1:0");
        let events = server.events().unwrap();
        events
            .publish(EventEnvelope::new("note.created", b"{}".to_vec()))
            .await
            .unwrap();

        let parts = server.bind().await.unwrap();
        let (status, body) = call(parts.router.clone(), "/j/health/ready").await;
        assert_eq!(status, StatusCode::OK);

        let bus = body["checks"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["name"] == "event-bus")
            .expect("the runtime should register a check for the bus it started");
        assert_eq!(bus["kind"], "eventbus");
        assert_eq!(bus["status"], "up");
    }

    #[cfg(feature = "messaging")]
    #[tokio::test]
    async fn a_subscribed_listener_receives_what_the_port_publishes() {
        use jenny_core::port::outbound::EventEnvelope;
        use std::sync::Mutex;
        use topmesys::{Delivery, EventConsumer, HandlerError, Subscriptions};

        #[derive(Debug)]
        struct Recorder(Arc<Mutex<Vec<String>>>);

        #[async_trait::async_trait]
        impl EventConsumer for Recorder {
            type Topic = ();

            fn subscriptions(&self) -> Subscriptions<()> {
                "test-service.note.*".into()
            }

            async fn handle_event(&self, _: &(), delivery: &Delivery) -> Result<(), HandlerError> {
                self.0
                    .lock()
                    .unwrap()
                    .push(delivery.message().topic().text().to_string());
                Ok(())
            }
        }

        let mut server = server_with("127.0.0.1:0");
        server.config.telemetry.service_name = "test-service".to_string();

        let seen = Arc::new(Mutex::new(Vec::new()));
        server.subscribe(Recorder(seen.clone())).unwrap();

        let events = server.events().unwrap();
        events
            .publish(EventEnvelope::new("note.created", b"{}".to_vec()))
            .await
            .unwrap();

        // Give the broker's event loop a turn.
        for _ in 0..50 {
            if !seen.lock().unwrap().is_empty() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }

        assert_eq!(
            seen.lock().unwrap().as_slice(),
            ["test-service.note.created"]
        );
    }

    #[cfg(feature = "telemetry-prometheus")]
    #[tokio::test]
    async fn the_metrics_endpoint_can_require_basic_auth() {
        use crate::config::BasicAuthConfig;

        let mut server = server_with("127.0.0.1:0");
        server.config.server.metrics.auth = Some(BasicAuthConfig {
            username: "prometheus".into(),
            password: "s3cret".into(),
        });
        // A registry is normally supplied by telemetry init; build the router directly
        // so the test does not need a live exporter.
        let metrics = crate::metrics::metrics_router(prometheus::Registry::new()).layer(
            jenny_rest::security::BasicAuth::new(
                &server.config.server.metrics.auth.as_ref().unwrap().username,
                &server.config.server.metrics.auth.as_ref().unwrap().password,
            )
            .with_realm("metrics"),
        );

        let unauthorized = metrics
            .clone()
            .oneshot(http::Request::get("/metrics").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(unauthorized.status(), StatusCode::UNAUTHORIZED);

        use base64::Engine as _;
        let credential = base64::engine::general_purpose::STANDARD.encode("prometheus:s3cret");
        let authorized = metrics
            .oneshot(
                http::Request::get("/metrics")
                    .header(http::header::AUTHORIZATION, format!("Basic {credential}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(authorized.status(), StatusCode::OK);
    }

    #[cfg(feature = "persistence")]
    #[tokio::test]
    async fn without_a_postgres_url_there_is_no_pool_and_no_check() {
        let mut server = server_with("127.0.0.1:0");
        assert!(server.postgres().unwrap().is_none());

        let parts = server.register(notes()).unwrap().bind().await.unwrap();
        let (_, ready) = call(parts.router.clone(), "/j/health/ready").await;
        assert!(
            ready["checks"]
                .as_array()
                .unwrap()
                .iter()
                .all(|c| c["name"] != "postgres")
        );
    }

    /// Needs `JENNY_TEST_POSTGRES_URL`, like jenny-persistence's own database tests.
    #[cfg(feature = "persistence")]
    #[tokio::test]
    async fn a_configured_pool_is_prepared_before_serving_and_reported_ready() {
        use jenny_core::port::{Store, StoreExt};

        let url = match std::env::var("JENNY_TEST_POSTGRES_URL") {
            Ok(url) => url,
            Err(_) if std::env::var_os("CI").is_some() => {
                panic!("JENNY_TEST_POSTGRES_URL must be set in CI")
            }
            Err(_) => return eprintln!("skipping: JENNY_TEST_POSTGRES_URL is not set"),
        };

        #[derive(serde::Serialize, serde::Deserialize)]
        struct Note {
            id: u32,
        }
        impl jenny_core::domain::Aggregate for Note {
            type Id = u32;
            const KIND: &'static str = "note";
            fn id(&self) -> u32 {
                self.id
            }
        }

        let mut server = server_with("127.0.0.1:0");
        server.config.persistence.postgres.url = Some(url);
        let postgres = server.postgres().unwrap().expect("a URL is configured");
        let table = format!("server_test_{}", uuid::Uuid::new_v4().simple());
        let store = postgres.documents_in::<Note>(&table);

        // The table does not exist until `bind` prepares the schema.
        let parts = server.register(notes()).unwrap().bind().await.unwrap();
        store.insert(Note { id: 1 }).await.unwrap();
        assert_eq!(store.load(&1).await.unwrap().id, 1);

        let (status, ready) = call(parts.router.clone(), "/j/health/ready").await;
        assert_eq!(status, StatusCode::OK);
        let check = ready["checks"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["name"] == "postgres")
            .expect("the runtime registers the pool's readiness");
        assert_eq!(check["status"], "up");

        jenny_persistence::sqlx::query(jenny_persistence::sqlx::AssertSqlSafe(format!(
            "DROP TABLE \"{table}\""
        )))
        .execute(postgres.pool())
        .await
        .unwrap();
    }

    #[cfg(feature = "openapi")]
    #[tokio::test]
    async fn the_openapi_document_is_served_under_the_framework_path() {
        use utoipa::OpenApi as _;

        #[derive(serde::Deserialize, utoipa::ToSchema)]
        #[allow(dead_code)]
        struct NoteRequest {
            title: String,
        }

        #[derive(utoipa::OpenApi)]
        #[openapi(components(schemas(NoteRequest)))]
        struct NotesApi;

        let parts = server_with("127.0.0.1:0")
            .register(
                RestAdapter::new("my-notes")
                    .unwrap()
                    .with_openapi(NotesApi::openapi())
                    .mount(Routes::new("notes").get("/notes", || async { "[]" }))
                    .unwrap(),
            )
            .unwrap()
            .bind()
            .await
            .unwrap();

        let (status, body) = call(parts.router.clone(), "/j/openapi.json").await;
        assert_eq!(status, StatusCode::OK);
        assert!(body["components"]["schemas"]["NoteRequest"].is_object());
    }
}
