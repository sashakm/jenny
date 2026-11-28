//! The composition root.

use std::future::IntoFuture;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

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
    config::{ConfigSource, Listener, ServerArgs, ServerConfig},
    context::ServerContext,
    endpoints::{AdapterRoutes, Available, Group},
    health::{AdapterSummary, HealthCheck, HealthRegistry, StatusInfo},
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
///     let mut server = JennyServer::from_args(jenny_core::service!())?; // config, telemetry
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
    service: jenny_core::Service,
    config: ServerConfig,
    context: ServerContext,
    telemetry: Option<TelemetryHandle>,
    rest_adapters: Vec<RestAdapter>,
    #[cfg(feature = "messaging")]
    messaging_adapters: Vec<MessagingAdapter>,
    scheduler: TaskScheduler,
    checks: Vec<Arc<dyn HealthCheck>>,
    authorizer: Option<Arc<dyn jenny_core::port::outbound::Authorizer>>,
    /// Whether `handle` has built a dispatcher, which took the authorizer as it was.
    handlers_registered: bool,
    #[cfg(feature = "messaging")]
    events: Option<Arc<jenny_messaging::EventBus>>,
    #[cfg(feature = "messaging")]
    broker: Option<topmesys::EventBroker<topmesys::type_states::Running>>,
    #[cfg(feature = "persistence")]
    postgres: Option<jenny_persistence::Postgres>,
    #[cfg(feature = "messaging")]
    nats: Option<jenny_messaging::kv::Nats>,
    #[cfg(feature = "messaging")]
    nats_options: Option<jenny_messaging::kv::OptionsHook>,
    #[cfg(feature = "objects")]
    objects: Option<Arc<jenny_objects::ObjectBlobs>>,
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
    /// The management listener and what it serves, with `server.management`.
    management: Option<(TcpListener, Router)>,
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
    /// The NATS connection, closed once the broker's listeners are done with it.
    #[cfg(feature = "messaging")]
    nats: Option<jenny_messaging::kv::Nats>,
    /// The JetStream consumers, which stop fetching as soon as shutdown begins.
    #[cfg(feature = "messaging")]
    consumers: Vec<tokio::task::JoinHandle<()>>,
    /// The Postgres pool, closed once requests and listeners are done with it.
    #[cfg(feature = "persistence")]
    postgres: Option<jenny_persistence::Postgres>,
    /// The outbox relay and the switch that stops it after one last pass.
    #[cfg(feature = "persistence")]
    relay: Option<(watch::Sender<bool>, tokio::task::JoinHandle<()>)>,
}

impl ServerParts {
    /// The address actually bound — the resolved one, when the configured port was `0`.
    pub fn local_addr(&self) -> anyhow::Result<SocketAddr> {
        Ok(self.listener.local_addr()?)
    }

    /// The management listener's address, resolved like [`local_addr`](Self::local_addr),
    /// or `None` without `server.management`.
    pub fn management_addr(&self) -> anyhow::Result<Option<SocketAddr>> {
        self.management
            .as_ref()
            .map(|(listener, _)| listener.local_addr())
            .transpose()
            .map_err(Into::into)
    }

    /// A handle that can ask this server to stop.
    pub fn handle(&self) -> ServerHandle {
        ServerHandle(self.shutdown.clone())
    }

    /// Serve until [`ServerHandle::shutdown`] is called, then drain, flush and stop.
    ///
    /// Readiness turns false first, the API keeps serving for `shutdown_delay`, then
    /// closes and drains; the outbox relay, the broker, the NATS connection and the
    /// Postgres pool stop in that order; the management listener closes last, so a
    /// load balancer sees "not ready" rather than a refused connection for all of it.
    /// `shutdown_delay + shutdown_timeout` bounds the whole sequence's waiting on
    /// requests, management listener included.
    pub async fn serve(self) -> anyhow::Result<()> {
        let Self {
            listener,
            router,
            management,
            shutdown,
            shutdown_delay,
            shutdown_timeout,
            telemetry,
            scheduler,
            health,
            #[cfg(feature = "messaging")]
            broker,
            #[cfg(feature = "messaging")]
            nats,
            #[cfg(feature = "messaging")]
            consumers,
            #[cfg(feature = "persistence")]
            postgres,
            #[cfg(feature = "persistence")]
            relay,
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
        let deadline = futures::FutureExt::shared(Box::pin(async move {
            let _ = requested.wait_for(|stopping| *stopping).await;
            tokio::time::sleep(shutdown_delay + shutdown_timeout).await;
        }));

        // Serving beside the API from the start, and closed only at the very end.
        let management = management.map(|(listener, router)| {
            let (close, mut closing) = watch::channel(false);
            let serving = tokio::spawn(
                axum::serve(listener, router)
                    .with_graceful_shutdown(async move {
                        let _ = closing.wait_for(|close| *close).await;
                    })
                    .into_future(),
            );
            (close, serving)
        });

        let serving = axum::serve(listener, router)
            .with_graceful_shutdown(close_listener)
            .into_future();
        let abort_scheduler = scheduler.as_ref().map(|handle| handle.abort_handle());
        #[cfg(feature = "messaging")]
        let abort_consumers: Vec<_> = consumers.iter().map(|c| c.abort_handle()).collect();
        // The consumers stopped fetching when shutdown began; the messages they hold are
        // finished and settled while the HTTP server drains beside them.
        #[cfg(feature = "messaging")]
        let consumed = futures::future::join_all(consumers);
        #[cfg(not(feature = "messaging"))]
        let consumed = std::future::ready(());
        // In-flight requests, messages and scheduled runs in progress share one deadline.
        let drain = async move {
            let (result, _) = tokio::join!(serving, consumed);
            if let Some(handle) = scheduler {
                let _ = handle.await;
            }
            result
        };
        let result = tokio::select! {
            result = drain => result,
            () = deadline.clone() => {
                warn!(
                    timeout_seconds = shutdown_timeout.as_secs(),
                    "in-flight work did not finish in time; stopping without it"
                );
                if let Some(abort) = abort_scheduler {
                    abort.abort();
                }
                #[cfg(feature = "messaging")]
                for abort in abort_consumers {
                    abort.abort();
                }
                Ok(())
            }
        };
        // The relay forwards what the last requests committed, into the bus, so it stops
        // after them and before the broker.
        #[cfg(feature = "persistence")]
        if let Some((stop, handle)) = relay {
            let _ = stop.send(true);
            let _ = handle.await;
        }
        // Stopping the broker waits for listeners that are still handling an event, so
        // a graceful shutdown does not drop side effects that were already in flight.
        #[cfg(feature = "messaging")]
        if let Some(broker) = broker {
            broker.stop().await;
        }
        // After the broker, whose listeners (a gateway among them) may still have been
        // publishing: stops the credential watcher and drains the connection.
        #[cfg(feature = "messaging")]
        if let Some(nats) = nats {
            nats.close().await;
        }
        // Last, because listeners stopping above may still have been writing.
        #[cfg(feature = "persistence")]
        if let Some(postgres) = postgres {
            postgres.close().await;
        }
        // Last of all, so readiness answered `stopping` until here.
        let mut management_result = Ok(());
        if let Some((close, mut serving)) = management {
            info!("closing the management listener");
            let _ = close.send(true);
            tokio::select! {
                biased;
                joined = &mut serving => {
                    management_result = joined
                        .map_err(anyhow::Error::from)
                        .and_then(|served| served.map_err(Into::into));
                }
                () = deadline => {
                    warn!("the management listener did not drain in time; stopping without it");
                    serving.abort();
                }
            }
        }
        drop(telemetry);
        result?;
        management_result
    }
}

impl JennyServer {
    /// Load configuration from the process arguments and bring telemetry up.
    ///
    /// `service` is who the service is, from [`jenny_core::service!`], which reads your
    /// package's name and version where you write it:
    ///
    /// ```no_run
    /// # use jenny_server::JennyServer;
    /// # fn go() -> anyhow::Result<()> {
    /// let server = JennyServer::from_args(jenny_core::service!())?;
    /// # Ok(()) }
    /// ```
    ///
    /// The top-level `service_name` and `service_version` keys override it, and
    /// `telemetry`, `messaging.namespace` and `/health/status` default to the result.
    pub fn from_args(service: jenny_core::Service) -> anyhow::Result<Self> {
        Self::from_parsed_args(ServerArgs::parse(), service)
    }

    /// [`from_args`](Self::from_args) with arguments you built yourself.
    pub fn from_parsed_args(
        args: ServerArgs,
        service: jenny_core::Service,
    ) -> anyhow::Result<Self> {
        let source = ConfigSource::load(&args)?;
        let config = source.server_config(&args)?;
        let mut server = Self::from_config(config, source, service)?;
        server.telemetry = Some(server.config.telemetry.clone().init()?);
        Ok(server)
    }

    /// Build from an already-loaded configuration, **without** touching telemetry.
    /// Use [`from_args`](Self::from_args) for the full bootstrap.
    ///
    /// Fails when the service name is empty once configuration has had its say.
    pub fn from_config(
        mut config: ServerConfig,
        source: ConfigSource,
        service: jenny_core::Service,
    ) -> anyhow::Result<Self> {
        let service = config.identify(service)?;
        Ok(Self {
            service,
            config,
            context: ServerContext::new(source),
            telemetry: None,
            rest_adapters: Vec::new(),
            #[cfg(feature = "messaging")]
            messaging_adapters: Vec::new(),
            scheduler: TaskScheduler::default(),
            checks: Vec::new(),
            authorizer: None,
            handlers_registered: false,
            #[cfg(feature = "messaging")]
            events: None,
            #[cfg(feature = "messaging")]
            broker: None,
            #[cfg(feature = "persistence")]
            postgres: None,
            #[cfg(feature = "messaging")]
            nats: None,
            #[cfg(feature = "messaging")]
            nats_options: None,
            #[cfg(feature = "objects")]
            objects: None,
        })
    }

    /// Who the service is, once configuration has overridden what `service!()` read.
    pub fn service(&self) -> &jenny_core::Service {
        &self.service
    }

    /// The service acting on its own as `component`: `System[<service_name>/<component>]`,
    /// with the roles under `system.roles`.
    pub fn system_principal(&self, component: &str) -> jenny_core::domain::Principal {
        jenny_core::domain::Principal::system(&self.service.name, component)
            .with_roles(self.config.system.roles.iter().cloned())
    }

    /// The policy engine attached to the [`Context`](jenny_core::port::Context) of every
    /// scheduled run and every inbound event, so a use case's `cx.authorize(…)` gets an
    /// answer there as it does in a request. REST adapters take theirs from their own
    /// `Authorization` layer.
    ///
    /// ```no_run
    /// # use std::sync::Arc;
    /// # use jenny_server::JennyServer;
    /// # fn wire(mut server: JennyServer, cedar: Arc<dyn jenny_core::port::outbound::Authorizer>) -> anyhow::Result<()> {
    /// server.with_authorizer(cedar)?;
    /// # Ok(()) }
    /// ```
    ///
    /// Fails when an event handler is already registered: its contexts would be built
    /// without it.
    pub fn with_authorizer(
        &mut self,
        authorizer: Arc<dyn jenny_core::port::outbound::Authorizer>,
    ) -> anyhow::Result<&mut Self> {
        if self.handlers_registered {
            anyhow::bail!(
                "with_authorizer() must come before handle(): an event handler was already \
                 registered without it"
            );
        }
        self.authorizer = Some(authorizer);
        Ok(self)
    }

    /// The authorizer given to [`with_authorizer`](Self::with_authorizer), if any.
    pub fn authorizer(&self) -> Option<&Arc<dyn jenny_core::port::outbound::Authorizer>> {
        self.authorizer.as_ref()
    }

    pub fn config(&self) -> &ServerConfig {
        &self.config
    }

    /// What use cases publish events through: [`Events`](jenny_core::port::Events) over
    /// the in-process bus, with the enrichers `messaging.events.headers` turns on
    /// (`caller`, `source`; both off by default).
    ///
    /// The broker starts on the first call and stops when the server does. Available
    /// during wiring, which is the point — use cases need it before anything is served.
    /// Add enrichers of your own after the configured ones:
    ///
    /// ```no_run
    /// # use jenny_core::port::{Context, Headers};
    /// # use jenny_server::JennyServer;
    /// # struct Notes; impl Notes { fn new(_: jenny_core::port::Events) -> Self { Notes } }
    /// # fn wire(mut server: JennyServer) -> anyhow::Result<()> {
    /// let events = server.events()?.enriched_with(|cx: &Context, headers: &mut Headers| {
    ///     headers.insert("region", "eu-1");
    /// });
    /// let notes = Notes::new(events);
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// Each call returns a new `Events` with the configured enrichers only. Use the same
    /// one for every use case whose events should carry your own.
    ///
    /// Fails only when there is no tokio runtime to run the broker's event loop on.
    #[cfg(feature = "messaging")]
    pub fn events(&mut self) -> anyhow::Result<jenny_core::port::Events> {
        let mut events = jenny_core::port::Events::new(self.publisher()?);
        let headers = &self.config.messaging.events.headers;
        if headers.caller {
            events = events.enriched_with(jenny_core::port::Caller);
        }
        if headers.source {
            events = events.enriched_with(jenny_core::port::Source::new(
                &self.service.name,
                &self.service.version,
            ));
        }
        Ok(events)
    }

    /// The bus itself, unenriched: what the outbox relay forwards to, because what it
    /// replays was enriched when it was raised.
    #[cfg(feature = "messaging")]
    fn publisher(&mut self) -> anyhow::Result<Arc<dyn jenny_core::port::outbound::EventPublisher>> {
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

    /// Handle events of the in-process bus whose topic matches `pattern`
    /// (`"my-notes.note.*"`) with a typed handler: a
    /// [`Handler`](jenny_messaging::inbound::Handler), or a closure taking the message and
    /// its [`Context`](jenny_core::port::Context) by value.
    ///
    /// The payload is decoded as JSON into the handler's type. The handler runs as
    /// `System[<service_name>/events]`, or as the event's caller under
    /// `principal: envelope` (in `messaging.inbound`, or in its `subscriptions` entry whose
    /// `subject` is `pattern`); bus headers are always trusted. Its context carries the
    /// event's correlation id as request id, the [authorizer](Self::with_authorizer), and
    /// an [`InboundEvent`](jenny_core::port::InboundEvent). A failure or an undecodable
    /// payload is logged with the event id and name, and not retried.
    ///
    /// ```no_run
    /// # use jenny_core::domain::DomainError;
    /// # use jenny_core::port::Context;
    /// # use jenny_server::JennyServer;
    /// # #[derive(serde::Deserialize)] struct NoteArchived { id: String }
    /// # fn wire(mut server: JennyServer) -> anyhow::Result<()> {
    /// server.handle("my-notes.note.archived", |event: NoteArchived, cx: Context| async move {
    ///     tracing::info!(note = event.id, by = %cx.require_principal()?, "archived");
    ///     Ok::<(), DomainError>(())
    /// })?;
    /// # Ok(()) }
    /// ```
    ///
    /// The raw [`subscribe`](Self::subscribe) stays for listeners that want the message
    /// itself.
    #[cfg(feature = "messaging")]
    pub fn handle<M, K>(
        &mut self,
        pattern: &str,
        handler: impl jenny_messaging::inbound::IntoHandler<M, K>,
    ) -> anyhow::Result<&mut Self> {
        use jenny_messaging::inbound::{BusHandler, Dispatch, Dispatcher};
        self.ensure_broker()?;
        let dispatcher = Dispatcher::new(self.system_principal("events"), true)
            .with_authorizer(self.authorizer.clone())
            .with_policy(self.config.messaging.inbound.principal(pattern))?;
        let handler = BusHandler::new(pattern, Dispatch::new(handler), dispatcher);
        self.broker
            .as_ref()
            .expect("broker was just started")
            .add_topic_consumer(handler)?;
        self.handlers_registered = true;
        info!(pattern, "registered bus event handler");
        Ok(self)
    }

    /// The service's NATS connection, from `messaging.nats`, or `None` when no URL is
    /// configured. The key-value stores and the gateway share it.
    ///
    /// It connects on first use, so it is available during wiring. Before the socket is
    /// bound, the runtime connects — whenever a URL is configured, even if this was never
    /// called — opens or creates the bucket of every store handed out, named
    /// `<namespace>-<kind>` (the namespace being `messaging.namespace` or the service
    /// name), and registers a `nats` readiness check. A server that refuses the
    /// connection stops the service there, with its reason.
    ///
    /// `messaging.nats` configures TLS, a client certificate and credentials (ADR-031);
    /// see [`NatsConnectionConfig`](jenny_messaging::NatsConnectionConfig). The
    /// certificates and `.creds` it names are watched, and a rotation forces a reconnect.
    ///
    /// ```no_run
    /// # use std::sync::Arc;
    /// # use jenny_core::memory::Memory;
    /// # use jenny_core::port::Store;
    /// # use jenny_server::JennyServer;
    /// # #[derive(serde::Serialize, serde::Deserialize)] struct Session { id: String }
    /// # impl jenny_core::domain::Aggregate for Session { type Id = String; const KIND: &'static str = "session"; fn id(&self) -> String { self.id.clone() } }
    /// # fn wire(mut server: JennyServer) -> anyhow::Result<()> {
    /// let sessions: Arc<dyn Store<Session>> = match server.nats() {
    ///     Some(nats) => {
    ///         server.subscribe(nats.gateway("my-notes", "my-notes.>"))?; // events to JetStream
    ///         Arc::new(nats.kv::<Session>())
    ///     }
    ///     None => Arc::new(Memory::<Session>::new()),
    /// };
    /// # Ok(())
    /// # }
    /// ```
    #[cfg(feature = "messaging")]
    pub fn nats(&mut self) -> Option<jenny_messaging::kv::Nats> {
        if self.nats.is_none() && self.config.messaging.nats.url.is_some() {
            let prefix = self.namespace();
            info!(prefix, "nats configured");
            let config = self.config.messaging.nats.clone();
            self.nats = Some(match self.nats_options.clone() {
                Some(hook) => {
                    jenny_messaging::kv::Nats::with_options(config, prefix, move |o| hook(o))
                }
                None => jenny_messaging::kv::Nats::new(config, prefix),
            });
        }
        self.nats.clone()
    }

    /// Adjust the [`ConnectOptions`](async_nats::ConnectOptions) built from
    /// `messaging.nats` before the connection is made, for credentials the configuration
    /// cannot name — from Vault, a cloud secret manager, a sidecar's socket.
    ///
    /// The hook receives the options built from the section and returns the ones to
    /// connect with. It may extend or replace them; replacing them is the only way to
    /// set an auth callback, which async-nats offers only as a constructor. It runs
    /// again before each attempt to make the first connection. The files the section
    /// names are the only ones watched for rotation.
    ///
    /// ```no_run
    /// # use std::path::PathBuf;
    /// # use jenny_server::JennyServer;
    /// use async_nats::ConnectOptions;
    /// # mod vault { pub async fn nats_auth(_: Vec<u8>) -> Result<async_nats::Auth, async_nats::AuthError> { unimplemented!() } }
    /// # fn wire(mut server: JennyServer, cert: PathBuf, key: PathBuf) -> anyhow::Result<()> {
    /// server.nats_options(move |_options: ConnectOptions| {
    ///     ConnectOptions::with_auth_callback(|nonce| vault::nats_auth(nonce))
    ///         .require_tls(true)
    ///         .add_client_certificate(cert.clone(), key.clone())
    /// })?;
    /// let nats = server.nats();
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// Fails when [`nats`](Self::nats) has already built the connection: the stores and
    /// gateways handed out would use options this hook never saw.
    ///
    /// `ConnectOptions` is async-nats's own type, so a new async-nats major is a breaking
    /// change of this method.
    #[cfg(feature = "messaging")]
    pub fn nats_options(
        &mut self,
        options: impl Fn(async_nats::ConnectOptions) -> async_nats::ConnectOptions
        + Send
        + Sync
        + 'static,
    ) -> anyhow::Result<&mut Self> {
        if self.nats.is_some() {
            anyhow::bail!(
                "nats_options() must come before nats(): the NATS connection was already \
                 built, and the stores and gateways handed out would not use these options"
            );
        }
        self.nats_options = Some(Arc::new(options));
        Ok(self)
    }

    /// The object store blobs live in, from `objects.url`, or `None` when no URL is
    /// configured.
    ///
    /// `s3://bucket[/prefix]` for S3 and everything that speaks it, configured by
    /// `objects.options` over the standard `AWS_*` variables; `file:///directory` or
    /// `memory://` for development. The first call builds it and registers an `objects`
    /// readiness check, which fails while the bucket cannot be listed.
    ///
    /// ```no_run
    /// # use std::sync::Arc;
    /// # use jenny_core::memory::MemoryBlobs;
    /// # use jenny_core::port::Blobs;
    /// # use jenny_server::JennyServer;
    /// # fn wire(mut server: JennyServer) -> anyhow::Result<()> {
    /// let attachments: Arc<dyn Blobs> = match server.objects()? {
    ///     Some(objects) => objects,
    ///     None => Arc::new(MemoryBlobs::new()),
    /// };
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// Fails when the URL or an option does not parse.
    #[cfg(feature = "objects")]
    pub fn objects(&mut self) -> anyhow::Result<Option<Arc<jenny_objects::ObjectBlobs>>> {
        if self.objects.is_none() && self.config.objects.url.is_some() {
            let objects = Arc::new(
                jenny_objects::ObjectBlobs::from_config(&self.config.objects)
                    .map_err(|e| e.context("configuring the object store from `objects`"))?,
            );
            info!(store = ?objects, "object store configured");
            self.checks.push(Arc::new(objects.health_check()));
            self.objects = Some(objects);
        }
        Ok(self.objects.clone())
    }

    /// The prefix for event topics and NATS buckets: `messaging.namespace`, else the
    /// service name.
    ///
    /// An empty namespace would produce topics like `.note.created`, which the broker
    /// rejects as having an empty segment, so a blank one falls back too; the service
    /// name never is blank.
    #[cfg(feature = "messaging")]
    pub fn namespace(&self) -> String {
        self.config
            .messaging
            .namespace
            .clone()
            .filter(|n| !n.trim().is_empty())
            .unwrap_or_else(|| self.service.name.clone())
    }

    /// Start the broker if it is not running yet.
    #[cfg(feature = "messaging")]
    fn ensure_broker(&mut self) -> anyhow::Result<()> {
        if self.broker.is_some() {
            return Ok(());
        }
        let namespace = self.namespace();
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
    /// With `persistence.postgres.outbox`, the events a change raises are committed with
    /// it and forwarded to the event bus by a relay the runtime runs until shutdown
    /// (ADR-016).
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
            if self.config.persistence.postgres.outbox {
                #[cfg(feature = "messaging")]
                postgres.with_outbox(self.publisher()?);
                #[cfg(not(feature = "messaging"))]
                anyhow::bail!(
                    "persistence.postgres.outbox forwards to the event bus, which needs the \
                     `messaging` feature; without it, call `Postgres::with_outbox` with a \
                     publisher of your own"
                );
            }
            info!(
                max_connections = self.config.persistence.postgres.max_connections,
                outbox = postgres.has_outbox(),
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
    /// Every subject bound with [`MessagingAdapter::on`] is consumed: at `bind` the
    /// runtime creates or updates a durable pull consumer, `<service_name>-<adapter>`, on
    /// the adapter's [`stream`](MessagingAdapter::stream), over the shared NATS connection,
    /// and runs it until shutdown, configured by `messaging.inbound`. `bind` fails when
    /// such an adapter has no stream, the stream does not exist, or `messaging.nats.url` is
    /// unset. Subjects given to [`subscribe`](MessagingAdapter::subscribe) are only
    /// declared, and listed in `/health/status`.
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
    ///
    /// Every run gets a fresh [`Context`](jenny_core::port::Context) as the system
    /// principal `System[<service_name>/tasks]`, with `system.roles`, the
    /// [authorizer](Self::with_authorizer) when there is one, and the label `task=<name>`.
    pub fn schedule(mut self, task: ScheduledTask) -> Self {
        self.scheduler = self.scheduler.with_task(task);
        self
    }

    /// Assemble the router and bind the socket, without serving yet.
    pub async fn bind(mut self) -> anyhow::Result<ServerParts> {
        // Whenever NATS is configured, even if nothing asked for the connection: a
        // server that refuses it should stop the service now, not at its first event.
        #[cfg(feature = "messaging")]
        self.nats();
        // Every run as the service itself, checked by the same policies as a request.
        let scheduler =
            std::mem::take(&mut self.scheduler).running_as(self.system_principal("tasks"));
        let scheduler = match self.authorizer.clone() {
            Some(authorizer) => scheduler.with_authorizer(authorizer),
            None => scheduler,
        };
        let Self {
            service,
            config,
            context: _,
            telemetry,
            mut rest_adapters,
            #[cfg(feature = "messaging")]
            messaging_adapters,
            scheduler: _,
            #[allow(unused_mut)]
            mut checks,
            authorizer,
            handlers_registered: _,
            #[cfg(feature = "messaging")]
            events,
            #[cfg(feature = "messaging")]
            broker,
            #[cfg(feature = "persistence")]
            postgres,
            #[cfg(feature = "messaging")]
            nats,
            #[cfg(feature = "messaging")]
                nats_options: _,
            #[cfg(feature = "objects")]
                objects: _,
        } = self;

        // Only scheduled runs (above) and inbound consumers (below) need it.
        #[cfg(not(feature = "messaging"))]
        let _ = authorizer;

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

        // Where each of the framework's endpoint groups goes: decided, and refused when
        // it cannot work, before anything connects or binds.
        #[cfg(feature = "openapi")]
        let document = merged_openapi(&rest_adapters);
        #[cfg(feature = "telemetry-prometheus")]
        let registry = telemetry
            .as_ref()
            .and_then(|handle| handle.prometheus_registry().cloned());
        let layout = crate::endpoints::resolve(
            &config.server,
            Available {
                #[cfg(feature = "telemetry-prometheus")]
                metrics: registry.is_some(),
                #[cfg(not(feature = "telemetry-prometheus"))]
                metrics: false,
                #[cfg(feature = "openapi")]
                openapi: document.is_some(),
                #[cfg(not(feature = "openapi"))]
                openapi: false,
                swagger: cfg!(feature = "swagger-ui"),
            },
            &rest_adapters
                .iter()
                .map(|adapter| AdapterRoutes {
                    name: adapter.name().to_string(),
                    mount: adapter.path().to_string(),
                    routes: adapter
                        .resources()
                        .iter()
                        .flat_map(|resource| resource.endpoints())
                        .map(|endpoint| match (adapter.path(), endpoint.path.as_str()) {
                            ("", path) => path.to_string(),
                            (mount, "/") => mount.to_string(),
                            (mount, path) => format!("{mount}{path}"),
                        })
                        .collect(),
                })
                .collect::<Vec<_>>(),
        )?;

        // Buckets too: a store whose bucket cannot be opened fails here, not later.
        #[cfg(feature = "messaging")]
        if let Some(nats) = nats.as_ref() {
            nats.prepare()
                .await
                .map_err(|e| e.context("preparing the NATS connection (`messaging.nats`)"))?;
            info!("nats connected, buckets prepared");
            checks.push(Arc::new(nats.health_check()));
        }

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
        // Started now, so events committed by the first request go out at once; stopped
        // in `serve` only once the last request is done.
        #[cfg(feature = "persistence")]
        let relay = postgres.as_ref().and_then(|p| p.relay()).map(|relay| {
            let (stop, mut stopping) = watch::channel(false);
            let handle = tokio::spawn(relay.run(async move {
                let _ = stopping.wait_for(|stop| *stop).await;
            }));
            (stop, handle)
        });

        #[cfg(feature = "messaging")]
        for adapter in &messaging_adapters {
            for binding in adapter.subjects() {
                info!(
                    adapter = adapter.name(),
                    subject = binding.subject,
                    "declared messaging subject"
                );
            }
        }

        // Created before serving, so a missing stream or a refused consumer stops the
        // service now; they consume from here on, and stop as soon as shutdown begins.
        let (shutdown, _) = watch::channel(false);
        #[cfg(feature = "messaging")]
        let mut consumer_status = Vec::new();
        #[cfg(feature = "messaging")]
        let consumers = {
            let mut handles = Vec::new();
            for adapter in &messaging_adapters {
                if adapter.handlers().is_empty() {
                    continue;
                }
                let Some(nats) = nats.as_ref() else {
                    anyhow::bail!(
                        "messaging adapter `{}` consumes from JetStream, but `messaging.nats.url` \
                         is not set",
                        adapter.name()
                    );
                };
                let consumer = jenny_messaging::consumer::JetStreamConsumer::bind(
                    nats,
                    &service.name,
                    adapter,
                    &config.messaging.inbound,
                    jenny_core::domain::Principal::system(&service.name, "events")
                        .with_roles(config.system.roles.iter().cloned()),
                    authorizer.clone(),
                )
                .await?
                .expect("the adapter has handlers");
                let status = consumer.status();
                consumer_status.push(Arc::new(move || {
                    let status = status.clone();
                    Box::pin(async move { status.report().await })
                        as futures::future::BoxFuture<'static, serde_json::Value>
                }) as crate::health::StatusProbe);
                handles.push(consumer);
            }
            handles
        };

        // Checks for what the runtime itself owns, the Postgres pool among them (above).
        // Anything it does not own is registered by the composition root.
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
            service: service.name.clone(),
            service_version: service.version.clone(),
            framework_version: crate::health::FRAMEWORK_VERSION,
            #[cfg(feature = "messaging")]
            consumers: consumer_status,
            #[cfg(not(feature = "messaging"))]
            consumers: Vec::new(),
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

        // The REST adapters, traced, with the problem documents for a wrong method.
        let mut api = Router::new();
        for adapter in rest_adapters {
            api = api.merge(adapter.into_router());
        }
        let mut api = api
            .method_not_allowed_fallback(jenny_rest::method_not_allowed)
            .layer(trace_layer());
        let mut management = config.server.management.as_ref().map(|_| Router::new());

        // Each of the framework's groups is a router of its own, traced only when its
        // `log_requests` says so, merged into the router of its listener.
        let mut place = |group: Group, router: Router| {
            let Some(placement) = layout.get(group) else {
                return;
            };
            let router = router.method_not_allowed_fallback(jenny_rest::method_not_allowed);
            let router = if placement.log_requests {
                router.layer(trace_layer())
            } else {
                router
            };
            match (placement.listener, management.as_mut()) {
                (Listener::Management, Some(management)) => {
                    *management = std::mem::take(management).merge(router);
                }
                _ => api = std::mem::take(&mut api).merge(router),
            }
        };
        // Only the groups that are served are built: a disabled one may have no path.
        let path = |group: Group| layout.get(group).map(|p| p.path.clone());
        if let Some(prefix) = path(Group::Probes) {
            place(
                Group::Probes,
                crate::health::probes_router(health.clone(), &prefix),
            );
        }
        if let Some(route) = path(Group::Status) {
            place(
                Group::Status,
                crate::health::status_router(health.clone(), status, &route),
            );
        }
        if let Some(route) = path(Group::PublicStatus) {
            place(
                Group::PublicStatus,
                crate::health::public_status_router(health.clone(), &route),
            );
        }
        #[cfg(feature = "telemetry-prometheus")]
        if let (Some(registry), Some(route)) = (registry, path(Group::Metrics)) {
            let mut metrics = crate::metrics::metrics_router(registry, &route);
            if let Some(auth) = config.server.endpoints.metrics.auth.as_ref() {
                info!("metrics endpoint requires basic authentication");
                metrics = metrics.layer(
                    jenny_rest::security::BasicAuth::new(&auth.username, &auth.password)
                        .with_realm("metrics"),
                );
            }
            place(Group::Metrics, metrics);
        }
        #[cfg(feature = "openapi")]
        if let (Some(document), Some(route)) = (document, path(Group::Openapi)) {
            #[cfg(feature = "swagger-ui")]
            if let Some(prefix) = path(Group::Swagger) {
                place(
                    Group::Swagger,
                    crate::openapi::swagger_router(&prefix, &route),
                );
            }
            place(
                Group::Openapi,
                crate::openapi::document_router(document, &route),
            );
        }
        drop(place);

        // Everything below applies to every route of a listener, including the
        // fallback, so that a 404 looks like every other failure rather than like a
        // hole in the API.
        let body_limit = config.server.body_limit;
        let finish = |router: Router| {
            router
                .fallback(axum::handler::Handler::layer(
                    jenny_rest::not_found,
                    trace_layer(),
                ))
                .layer(DefaultBodyLimit::max(body_limit))
                // Outermost, so the trace span, every layer and every problem document
                // see the same id, traced or not.
                .layer(RequestIdLayer)
        };
        let router = finish(api);
        let management = management.map(finish);

        // Both sockets before the server counts as started; if either cannot be bound,
        // neither serves.
        let listener = TcpListener::bind(&config.server.address)
            .await
            .map_err(|e| {
                anyhow::Error::from(e).context(format!(
                    "binding `server.address` (`{}`)",
                    config.server.address
                ))
            })?;
        let management = match (config.server.management.as_ref(), management) {
            (Some(settings), Some(router)) => Some((
                TcpListener::bind(&settings.address).await.map_err(|e| {
                    anyhow::Error::from(e).context(format!(
                        "binding `server.management.address` (`{}`)",
                        settings.address
                    ))
                })?,
                router,
            )),
            _ => None,
        };

        // Consuming starts only once nothing else can fail startup, so no message is acked
        // by a service that then does not come up.
        #[cfg(feature = "messaging")]
        let consumers: Vec<_> = consumers
            .into_iter()
            .map(|consumer| {
                let mut requested = shutdown.subscribe();
                let (stop, stopping) = watch::channel(false);
                tokio::spawn(async move {
                    let _ = requested.wait_for(|stopping| *stopping).await;
                    let _ = stop.send(true);
                });
                consumer.spawn(stopping)
            })
            .collect();

        let scheduler_handle = (!scheduler.is_empty()).then(|| {
            let mut requested = shutdown.subscribe();
            tokio::spawn(scheduler.run_until(async move {
                let _ = requested.wait_for(|stopping| *stopping).await;
            }))
        });

        match management.as_ref() {
            Some((management, _)) => info!(
                address = %listener.local_addr()?,
                management = %management.local_addr()?,
                "jenny server bound"
            ),
            None => info!(address = %listener.local_addr()?, "jenny server bound"),
        }
        health.mark_started();

        Ok(ServerParts {
            listener,
            router,
            management,
            shutdown,
            shutdown_delay: Duration::from_secs(config.server.shutdown_delay_seconds),
            shutdown_timeout: Duration::from_secs(config.server.shutdown_timeout_seconds),
            telemetry,
            scheduler: scheduler_handle,
            health,
            #[cfg(feature = "messaging")]
            broker,
            #[cfg(feature = "messaging")]
            nats,
            #[cfg(feature = "messaging")]
            consumers,
            #[cfg(feature = "persistence")]
            postgres,
            #[cfg(feature = "persistence")]
            relay,
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

/// The request span and access log, for the REST adapters and every group whose
/// `log_requests` is on.
fn trace_layer() -> tower_http::trace::TraceLayer<
    tower_http::classify::SharedClassifier<tower_http::classify::ServerErrorsAsFailures>,
    fn(&axum::extract::Request) -> tracing::Span,
> {
    TraceLayer::new_for_http()
        .make_span_with(request_span as fn(&axum::extract::Request) -> tracing::Span)
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
        JennyServer::from_config(
            config,
            source,
            jenny_core::Service::new("test-service", "9.9.9"),
        )
        .unwrap()
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

    /// A server with `server.management` on a free port.
    fn managed_server() -> JennyServer {
        let mut server = server_with("127.0.0.1:0");
        server.config.server.management = Some(crate::config::ManagementConfig {
            address: "127.0.0.1:0".into(),
        });
        server
    }

    #[tokio::test]
    async fn without_management_there_is_one_listener() {
        let parts = server_with("127.0.0.1:0").bind().await.unwrap();
        assert_eq!(parts.management_addr().unwrap(), None);
    }

    #[tokio::test]
    async fn with_management_the_framework_endpoints_move_to_their_own_listener() {
        let parts = managed_server()
            .register(notes())
            .unwrap()
            .bind()
            .await
            .unwrap();
        let api = parts.local_addr().unwrap();
        let management = parts.management_addr().unwrap().expect("a second listener");
        assert_ne!(api, management);
        assert_ne!(management.port(), 0);
        let handle = parts.handle();
        let serving = tokio::spawn(parts.serve());

        for path in ["/j/health/live", "/j/health/ready", "/j/health/status"] {
            let response = raw_get(&format!("http://{management}{path}")).await;
            assert!(response.starts_with("HTTP/1.0 200"), "{path}: {response}");
            let response = raw_get(&format!("http://{api}{path}")).await;
            assert!(response.starts_with("HTTP/1.0 404"), "{path}: {response}");
            assert!(
                response.contains("urn:jenny:problem:not-found"),
                "a problem document: {response}"
            );
        }
        let response = raw_get(&format!("http://{api}/api/notes")).await;
        assert!(response.starts_with("HTTP/1.0 200"), "{response}");
        let response = raw_get(&format!("http://{management}/api/notes")).await;
        assert!(response.starts_with("HTTP/1.0 404"), "{response}");
        assert!(
            response.contains("urn:jenny:problem:not-found"),
            "{response}"
        );
        assert!(
            response
                .to_lowercase()
                .contains(jenny_rest::REQUEST_ID_HEADER),
            "{response}"
        );

        handle.shutdown();
        serving.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn a_group_can_stay_on_the_api_listener_at_a_path_of_its_own() {
        use crate::config::Listener;

        let mut server = managed_server();
        let public = &mut server.config.server.endpoints.public_status;
        public.enabled = true;
        public.listener = Some(Listener::Api);
        public.path = Some("/status".into());
        let parts = server.register(notes()).unwrap().bind().await.unwrap();
        let management = parts.management.as_ref().unwrap().1.clone();

        let (status, body) = call(parts.router.clone(), "/status").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, serde_json::json!({"status": "up"}));
        assert_eq!(
            call(management.clone(), "/status").await.0,
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            call(management.clone(), "/j/health/public").await.0,
            StatusCode::NOT_FOUND
        );
        // Everything else stayed on the management listener.
        assert_eq!(call(management, "/j/health/ready").await.0, StatusCode::OK);
        assert_eq!(
            call(parts.router.clone(), "/j/health/ready").await.0,
            StatusCode::NOT_FOUND
        );
    }

    #[tokio::test]
    async fn naming_a_management_listener_that_is_not_configured_stops_the_service() {
        let mut server = server_with("127.0.0.1:0");
        server.config.server.endpoints.status.listener = Some(crate::config::Listener::Management);
        let Err(err) = server.bind().await else {
            panic!("there is no management listener");
        };
        assert!(
            err.to_string()
                .contains("`server.endpoints.status.listener`"),
            "{err}"
        );
    }

    #[tokio::test]
    async fn a_group_inside_an_adapters_mount_stops_the_service() {
        let mut server = server_with("127.0.0.1:0");
        server.config.server.endpoints.status.path = Some("/api/status".into());
        let Err(err) = server.register(notes()).unwrap().bind().await else {
            panic!("`/api/status` is inside `my-notes`, at `/api`");
        };
        let err = err.to_string();
        assert!(err.contains("`server.endpoints.status.path`"), "{err}");
        assert!(err.contains("`my-notes`"), "{err}");
    }

    #[tokio::test]
    async fn a_taken_management_port_fails_bind_and_leaves_the_api_port_unbound() {
        let taken = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let free = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap();
        let mut server = server_with(&free.to_string());
        server.config.server.management = Some(crate::config::ManagementConfig {
            address: taken.local_addr().unwrap().to_string(),
        });
        let Err(err) = server.bind().await else {
            panic!("the management port is in use");
        };
        assert!(
            format!("{err:#}").contains("`server.management.address`"),
            "{err:#}"
        );
        assert!(
            std::net::TcpListener::bind(free).is_ok(),
            "the API listener was released"
        );
    }

    #[tokio::test]
    async fn the_management_listener_reports_stopping_while_the_api_drains_and_closes_last() {
        let mut server = managed_server();
        server.config.server.shutdown_delay_seconds = 1;
        let server = server
            .register(
                RestAdapter::new("slow")
                    .unwrap()
                    .mount(Routes::new("slow").get("/slow", || async {
                        tokio::time::sleep(Duration::from_secs(3)).await;
                        "done"
                    }))
                    .unwrap(),
            )
            .unwrap();
        let parts = server.bind().await.unwrap();
        let api = parts.local_addr().unwrap();
        let management = parts.management_addr().unwrap().unwrap();
        let handle = parts.handle();
        let serving = tokio::spawn(parts.serve());

        let ready = format!("http://{management}/j/health/ready");
        assert!(raw_get(&ready).await.starts_with("HTTP/1.0 200"));
        let slow = tokio::spawn(async move { raw_get(&format!("http://{api}/api/slow")).await });
        tokio::time::sleep(Duration::from_millis(100)).await;
        handle.shutdown();
        tokio::time::sleep(Duration::from_millis(100)).await;

        // During the delay: not ready, and the API still answers.
        let during_delay = raw_get(&ready).await;
        assert!(during_delay.starts_with("HTTP/1.0 503"), "{during_delay}");
        assert!(during_delay.contains("\"stopping\""), "{during_delay}");

        // After it the API listener is closed and draining the slow request, while
        // the management listener still says why.
        tokio::time::sleep(Duration::from_millis(1_400)).await;
        assert!(
            tokio::net::TcpStream::connect(api).await.is_err(),
            "the API listener is closed"
        );
        let draining = raw_get(&ready).await;
        assert!(draining.starts_with("HTTP/1.0 503"), "{draining}");
        assert!(draining.contains("\"stopping\""), "{draining}");

        serving.await.unwrap().unwrap();
        assert!(
            slow.await.unwrap().contains("done"),
            "the drain finished it"
        );
        assert!(
            tokio::net::TcpStream::connect(management).await.is_err(),
            "the management listener closed last"
        );
    }

    /// Every span opened and every event emitted, by name and target.
    #[derive(Clone, Default)]
    struct Recorded(Arc<std::sync::Mutex<Vec<String>>>);

    impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for Recorded {
        fn on_new_span(
            &self,
            attributes: &tracing::span::Attributes<'_>,
            _: &tracing::span::Id,
            _: tracing_subscriber::layer::Context<'_, S>,
        ) {
            let name = attributes.metadata().name();
            self.0.lock().unwrap().push(format!("span {name}"));
        }

        fn on_event(
            &self,
            event: &tracing::Event<'_>,
            _: tracing_subscriber::layer::Context<'_, S>,
        ) {
            let target = event.metadata().target();
            self.0.lock().unwrap().push(format!("event {target}"));
        }
    }

    impl Recorded {
        fn take(&self) -> Vec<String> {
            std::mem::take(&mut *self.0.lock().unwrap())
        }

        fn traced(seen: &[String]) -> bool {
            seen.iter().any(|s| s == "span request")
                && seen.iter().any(|s| s.contains("tower_http"))
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn probes_are_not_traced_unless_asked_but_still_carry_a_request_id() {
        use tracing_subscriber::layer::SubscriberExt as _;

        let recorded = Recorded::default();
        let _guard =
            tracing::subscriber::set_default(tracing_subscriber::registry().with(recorded.clone()));
        // Callsites other tests registered first, under no subscriber, cached that
        // they are never interesting; this one wants them.
        let rebuild = tracing::callsite::rebuild_interest_cache;
        rebuild();

        let get = |router: Router, uri: &'static str| async move {
            router
                .oneshot(http::Request::get(uri).body(Body::empty()).unwrap())
                .await
                .unwrap()
        };

        let parts = server_with("127.0.0.1:0")
            .register(notes())
            .unwrap()
            .bind()
            .await
            .unwrap();
        rebuild();
        recorded.take();
        let response = get(parts.router.clone(), "/j/health/ready").await;
        assert_eq!(response.status(), StatusCode::OK);
        assert!(
            response
                .headers()
                .contains_key(jenny_rest::REQUEST_ID_HEADER)
        );
        let seen = recorded.take();
        assert!(
            !seen
                .iter()
                .any(|s| s == "span request" || s.contains("tower_http")),
            "a probe left no span and no access log: {seen:?}"
        );

        let response = get(parts.router.clone(), "/api/notes").await;
        assert_eq!(response.status(), StatusCode::OK);
        let seen = recorded.take();
        assert!(Recorded::traced(&seen), "the API is traced: {seen:?}");
        let response = get(parts.router.clone(), "/nowhere").await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert!(Recorded::traced(&recorded.take()), "and so is a 404");

        let mut server = server_with("127.0.0.1:0");
        server.config.server.endpoints.probes.log_requests = true;
        let parts = server.bind().await.unwrap();
        rebuild();
        recorded.take();
        let response = get(parts.router.clone(), "/j/health/ready").await;
        assert!(
            response
                .headers()
                .contains_key(jenny_rest::REQUEST_ID_HEADER)
        );
        assert!(Recorded::traced(&recorded.take()), "log_requests: true");
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
    async fn status_reports_the_top_level_identity_not_the_telemetry_one() {
        let mut server = server_with("127.0.0.1:0");
        server.config.telemetry.service_name = Some("renamed-in-traces".into());
        server.config.telemetry.service_version = Some("0.0.0-traces".into());
        let parts = server.bind().await.unwrap();
        let (_, body) = call(parts.router.clone(), "/j/health/status").await;
        assert_eq!(body["service"], "test-service");
        assert_eq!(body["service_version"], "9.9.9");
    }

    #[tokio::test(start_paused = true)]
    async fn a_task_runs_as_the_system_principal_with_the_configured_roles() {
        use jenny_core::port::Context;
        use jenny_core::task::Task;
        use std::sync::Mutex;

        /// The principal, its roles, and whether an authorizer was attached, per run.
        type Runs = Vec<(String, Vec<String>, bool)>;
        struct Seen(Arc<Mutex<Runs>>);

        #[async_trait::async_trait]
        impl Task for Seen {
            async fn execute(&self, cx: &Context) -> anyhow::Result<()> {
                let principal = cx.require_principal()?;
                self.0.lock().unwrap().push((
                    principal.to_string(),
                    principal.roles(),
                    cx.authorizer().is_some(),
                ));
                Ok(())
            }
        }

        let seen = Arc::new(Mutex::new(Vec::new()));
        let mut server = server_with("127.0.0.1:0");
        server.config.system.roles = vec!["Maintenance".into()];
        server.with_authorizer(Arc::new(AllowAll)).unwrap();
        let parts = server
            .schedule(ScheduledTask::new("tidy", Seen(seen.clone()), "*/1 * * * * * *").unwrap())
            .bind()
            .await
            .unwrap();
        let handle = parts.handle();
        let serving = tokio::spawn(parts.serve());
        tokio::time::sleep(Duration::from_millis(1_500)).await;
        handle.shutdown();
        serving.await.unwrap().unwrap();

        let seen = seen.lock().unwrap();
        assert!(!seen.is_empty());
        assert_eq!(
            seen[0],
            (
                "System[test-service/tasks]".to_string(),
                vec!["Maintenance".to_string()],
                true
            )
        );
    }

    #[tokio::test]
    async fn a_disabled_group_is_not_served_and_the_others_are() {
        let mut server = server_with("127.0.0.1:0");
        server.config.server.endpoints.probes.enabled = false;
        let parts = server.bind().await.unwrap();

        let (status, body) = call(parts.router.clone(), "/j/health/live").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(body["type"], "urn:jenny:problem:not-found");
        let (status, _) = call(parts.router.clone(), "/j/health/status").await;
        assert_eq!(status, StatusCode::OK);
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
            .publisher()
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
    async fn events_carry_the_configured_headers_first_then_those_main_adds() {
        use jenny_core::port::{Context, Headers};

        #[derive(serde::Serialize)]
        struct Created;
        impl jenny_core::domain::DomainEvent for Created {
            fn name(&self) -> &str {
                "note.created"
            }
        }
        let cx =
            Context::new().with_principal(jenny_core::domain::Principal::builder("u-7").build());

        // Off by default.
        let mut server = server_with("127.0.0.1:0");
        let seen = HeaderRecorder::subscribe(&mut server);
        server.events().unwrap().publish(&Created, &cx).await;
        let headers = seen.next().await;
        assert!(!headers.contains_key("caller-subject"), "{headers:?}");
        assert!(!headers.contains_key("source-service"), "{headers:?}");

        let mut server = server_with("127.0.0.1:0");
        server.config.messaging.events.headers.caller = true;
        server.config.messaging.events.headers.source = true;
        let seen = HeaderRecorder::subscribe(&mut server);
        let events =
            server
                .events()
                .unwrap()
                .enriched_with(|_: &Context, headers: &mut Headers| {
                    // The configured ones have run already.
                    let after = headers
                        .get("caller-subject")
                        .unwrap_or("not yet")
                        .to_string();
                    headers.insert("ran-after", after);
                });
        events.publish(&Created, &cx).await;
        let headers = seen.next().await;
        assert_eq!(headers["caller-subject"], "u-7");
        assert_eq!(headers["source-service"], "test-service");
        assert_eq!(headers["source-version"], "9.9.9");
        assert_eq!(headers["ran-after"], "u-7");
    }

    #[cfg(feature = "messaging")]
    #[tokio::test]
    async fn a_bus_handler_runs_as_the_service_with_the_events_context() {
        use jenny_core::domain::{DomainError, Principal};
        use jenny_core::port::{Context, EventCaller, InboundEvent};
        use std::sync::Mutex;

        #[derive(serde::Serialize, serde::Deserialize)]
        struct NoteArchived {
            id: u32,
        }
        impl jenny_core::domain::DomainEvent for NoteArchived {
            fn name(&self) -> &str {
                "note.archived"
            }
        }

        type Seen = Vec<(u32, String, uuid::Uuid, String, Option<EventCaller>, bool)>;
        let seen: Arc<Mutex<Seen>> = Arc::default();
        let record = seen.clone();
        let mut server = server_with("127.0.0.1:0");
        server.config.messaging.events.headers.caller = true;
        server.config.system.roles = vec!["Maintenance".into()];
        server
            .handle(
                "test-service.note.*",
                move |event: NoteArchived, cx: Context| {
                    let record = record.clone();
                    async move {
                        let principal = cx.require_principal()?;
                        assert!(principal.has_role("Maintenance"));
                        record.lock().unwrap().push((
                            event.id,
                            principal.to_string(),
                            cx.request_id(),
                            cx.get::<InboundEvent>().unwrap().name.clone(),
                            cx.get::<EventCaller>().cloned(),
                            cx.authorizer().is_some(),
                        ));
                        Ok::<(), DomainError>(())
                    }
                },
            )
            .unwrap();
        let err = server
            .with_authorizer(Arc::new(AllowAll))
            .err()
            .expect("the handler was built without it");
        assert!(err.to_string().contains("before handle()"), "{err}");

        let cx = Context::new().with_principal(Principal::builder("u-7").build());
        server
            .events()
            .unwrap()
            .publish(&NoteArchived { id: 7 }, &cx)
            .await;
        for _ in 0..200 {
            if !seen.lock().unwrap().is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 1);
        let (id, principal, request_id, name, caller, authorized) = &seen[0];
        assert_eq!(*id, 7);
        assert_eq!(principal, "System[test-service/events]");
        assert_eq!(*request_id, cx.request_id(), "correlated with the request");
        assert_eq!(name, "note.archived");
        assert_eq!(caller.as_ref().unwrap().subject, "u-7");
        assert!(!authorized);
    }

    #[cfg(feature = "messaging")]
    #[tokio::test]
    async fn the_envelope_policy_restores_the_caller_on_the_bus() {
        use jenny_core::domain::{DomainError, Principal};
        use jenny_core::port::{Context, RestoredCaller};
        use std::sync::Mutex;

        #[derive(serde::Serialize, serde::Deserialize)]
        struct NoteArchived {
            id: u32,
        }
        impl jenny_core::domain::DomainEvent for NoteArchived {
            fn name(&self) -> &str {
                "note.archived"
            }
        }

        let seen: Arc<Mutex<Vec<(String, bool)>>> = Arc::default();
        let record = seen.clone();
        let mut server = server_with("127.0.0.1:0");
        server.config.messaging.events.headers.caller = true;
        server.config.messaging.inbound = serde_json::from_value(serde_json::json!({
            "subscriptions": [{ "subject": "test-service.note.*", "principal": "envelope" }]
        }))
        .unwrap();
        server
            .handle(
                "test-service.note.*",
                move |_: NoteArchived, cx: Context| {
                    let record = record.clone();
                    async move {
                        record.lock().unwrap().push((
                            cx.require_principal()?.subject(),
                            cx.get::<RestoredCaller>().is_some(),
                        ));
                        Ok::<(), DomainError>(())
                    }
                },
            )
            .unwrap();

        let cx = Context::new().with_principal(Principal::builder("u-7").build());
        server
            .events()
            .unwrap()
            .publish(&NoteArchived { id: 7 }, &cx)
            .await;
        for _ in 0..200 {
            if !seen.lock().unwrap().is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(seen.lock().unwrap().as_slice(), [("u-7".to_string(), true)]);
    }

    #[cfg(feature = "messaging")]
    #[tokio::test]
    async fn a_jetstream_adapter_without_a_nats_url_stops_the_service() {
        use jenny_core::domain::DomainError;
        use jenny_messaging::{MessagingAdapter, SubjectBinding};

        let adapter = MessagingAdapter::new("orders", "shop")
            .unwrap()
            .stream("SHOP")
            .on(
                SubjectBinding::new("orders.created"),
                |_: serde_json::Value, _: jenny_core::port::Context| async {
                    Ok::<(), DomainError>(())
                },
            );
        let Err(err) = server_with("127.0.0.1:0")
            .register_messaging(adapter)
            .bind()
            .await
        else {
            panic!("there is no NATS to consume from");
        };
        assert!(err.to_string().contains("`messaging.nats.url`"), "{err}");
    }

    /// Allows everything, to tell an attached authorizer from none.
    struct AllowAll;

    #[async_trait::async_trait]
    impl jenny_core::port::outbound::Authorizer for AllowAll {
        async fn authorize(
            &self,
            _: &jenny_core::domain::Principal,
            _: &jenny_core::port::AccessRequest,
        ) -> Result<(), jenny_core::domain::AuthorizationError> {
            Ok(())
        }
    }

    /// A bus listener that keeps the headers of every message it gets.
    #[cfg(feature = "messaging")]
    #[derive(Clone, Debug, Default)]
    struct HeaderRecorder(
        Arc<
            std::sync::Mutex<
                std::collections::VecDeque<std::collections::BTreeMap<String, String>>,
            >,
        >,
    );

    #[cfg(feature = "messaging")]
    impl HeaderRecorder {
        fn subscribe(server: &mut JennyServer) -> Self {
            let recorder = Self::default();
            server.subscribe(recorder.clone()).unwrap();
            recorder
        }

        async fn next(&self) -> std::collections::BTreeMap<String, String> {
            for _ in 0..200 {
                if let Some(headers) = self.0.lock().unwrap().pop_front() {
                    return headers;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            panic!("no message reached the listener");
        }
    }

    #[cfg(feature = "messaging")]
    #[async_trait::async_trait]
    impl topmesys::EventConsumer for HeaderRecorder {
        type Topic = ();

        fn subscriptions(&self) -> topmesys::Subscriptions<()> {
            "test-service.note.*".into()
        }

        async fn handle_event(
            &self,
            _: &(),
            delivery: &topmesys::Delivery,
        ) -> Result<(), topmesys::HandlerError> {
            let headers = delivery
                .message()
                .headers()
                .iter()
                .map(|(name, value)| (name.to_string(), value.to_string()))
                .collect();
            self.0.lock().unwrap().push_back(headers);
            Ok(())
        }
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
        assert_eq!(server.service().name, "test-service");

        let seen = Arc::new(Mutex::new(Vec::new()));
        server.subscribe(Recorder(seen.clone())).unwrap();

        let events = server.events().unwrap();
        events
            .publisher()
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

    #[cfg(feature = "messaging")]
    #[tokio::test]
    async fn without_a_nats_url_there_are_no_buckets() {
        let mut server = server_with("127.0.0.1:0");
        assert!(server.nats().is_none());
    }

    /// Needs `JENNY_TEST_NATS_URL`.
    #[cfg(feature = "messaging")]
    #[tokio::test]
    async fn a_configured_nats_opens_its_buckets_before_serving_and_reports_ready() {
        use jenny_core::port::{Store, StoreExt};

        let url = match std::env::var("JENNY_TEST_NATS_URL") {
            Ok(url) => url,
            Err(_) if std::env::var_os("CI").is_some() => {
                panic!("JENNY_TEST_NATS_URL must be set in CI")
            }
            Err(_) => return eprintln!("skipping: JENNY_TEST_NATS_URL is not set"),
        };

        #[derive(serde::Serialize, serde::Deserialize)]
        struct Session {
            id: u32,
        }
        impl jenny_core::domain::Aggregate for Session {
            type Id = u32;
            const KIND: &'static str = "session";
            fn id(&self) -> u32 {
                self.id
            }
        }

        let namespace = format!("servertest-{}", uuid::Uuid::new_v4().simple());
        let mut server = server_with("127.0.0.1:0");
        server.config.messaging.namespace = Some(namespace.clone());
        server.config.messaging.nats.url = Some(url.clone());
        let sessions = server.nats().expect("a URL is configured").kv::<Session>();

        let parts = server.register(notes()).unwrap().bind().await.unwrap();
        sessions.insert(Session { id: 1 }).await.unwrap();
        assert_eq!(sessions.load(&1).await.unwrap().id, 1);

        let (status, ready) = call(parts.router.clone(), "/j/health/ready").await;
        assert_eq!(status, StatusCode::OK);
        let check = ready["checks"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["name"] == "nats")
            .expect("the runtime registers the NATS readiness check");
        assert_eq!(check["status"], "up");

        let client = async_nats::connect(&url).await.unwrap();
        async_nats::jetstream::new(client)
            .delete_key_value(format!("{namespace}-session"))
            .await
            .unwrap();
    }

    #[cfg(feature = "messaging")]
    #[test]
    fn nats_options_must_come_before_nats() {
        let mut server = server_with("127.0.0.1:0");
        server.config.messaging.nats.url = Some("nats://127.0.0.1:1".into());
        server.nats_options(|options| options).unwrap();
        assert!(server.nats().is_some());

        let Err(err) = server.nats_options(|options| options) else {
            panic!("nats_options() after nats() should be refused");
        };
        assert!(err.to_string().contains("must come before nats()"), "{err}");
    }

    #[cfg(feature = "messaging")]
    #[tokio::test]
    async fn the_nats_options_hook_decides_what_bind_connects_with() {
        let mut server = server_with("127.0.0.1:0");
        server.config.messaging.nats.url = Some("nats://127.0.0.1:1".into());
        // A replacement that cannot connect anywhere: TLS against a plain port.
        server
            .nats_options(|_| async_nats::ConnectOptions::new().tls_first())
            .unwrap();
        let Err(err) = server.bind().await else {
            panic!("nothing listens on port 1");
        };
        assert!(format!("{err:#}").contains("connecting to NATS"), "{err:#}");
    }

    #[cfg(feature = "messaging")]
    #[tokio::test]
    async fn bind_connects_whenever_a_nats_url_is_configured() {
        let mut server = server_with("127.0.0.1:0");
        // Nothing listens there, and nothing called `nats()`: binding still connects.
        server.config.messaging.nats.url = Some("nats://127.0.0.1:1".into());
        let Err(err) = server.bind().await else {
            panic!("a NATS that cannot be reached should stop the service at startup");
        };
        let err = format!("{err:#}");
        assert!(err.contains("messaging.nats"), "{err}");
        assert!(
            err.contains("connecting to NATS at `nats://127.0.0.1:1`"),
            "{err}"
        );
    }

    /// Needs `JENNY_TEST_NATS_URL`.
    #[cfg(feature = "messaging")]
    #[tokio::test]
    async fn a_configured_nats_is_connected_and_checked_even_if_nothing_asked_for_it() {
        let Some(url) = nats_url() else { return };
        let mut server = server_with("127.0.0.1:0");
        server.config.messaging.nats.url = Some(url);
        let parts = server.bind().await.unwrap();
        let check = nats_check(parts.router.clone()).await;
        assert_eq!(check["status"], "up", "{check}");
    }

    /// Needs `JENNY_TEST_NATS_TLS_URL` and `JENNY_TEST_NATS_TLS_DIR`.
    #[cfg(feature = "messaging")]
    #[tokio::test]
    async fn bind_connects_to_nats_with_mtls_and_refuses_what_the_server_refuses() {
        let (url, dir) = match (
            std::env::var("JENNY_TEST_NATS_TLS_URL"),
            std::env::var("JENNY_TEST_NATS_TLS_DIR"),
        ) {
            (Ok(url), Ok(dir)) => (url, std::path::PathBuf::from(dir)),
            _ if std::env::var_os("CI").is_some() => {
                panic!("JENNY_TEST_NATS_TLS_URL and JENNY_TEST_NATS_TLS_DIR must be set in CI")
            }
            _ => {
                return eprintln!(
                    "skipping: JENNY_TEST_NATS_TLS_URL / JENNY_TEST_NATS_TLS_DIR are not set"
                );
            }
        };
        let tls = |client: Option<&str>| jenny_messaging::connection::NatsTlsConfig {
            ca: Some(dir.join("ca.crt")),
            cert: client.map(|name| dir.join(format!("{name}.crt"))),
            key: client.map(|name| dir.join(format!("{name}.key"))),
            ..Default::default()
        };

        let mut server = server_with("127.0.0.1:0");
        server.config.messaging.nats.url = Some(url.clone());
        server.config.messaging.nats.tls = Some(tls(Some("client-a")));
        let parts = server.bind().await.unwrap();
        let check = nats_check(parts.router.clone()).await;
        assert_eq!(check["status"], "up", "{check}");

        for (client, reason) in [(None, "CertificateRequired"), (Some("other"), "UnknownCA")] {
            let mut server = server_with("127.0.0.1:0");
            server.config.messaging.nats.url = Some(url.clone());
            server.config.messaging.nats.tls = Some(tls(client));
            let Err(err) = server.bind().await else {
                panic!("{client:?}: the server refuses this client");
            };
            let err = format!("{err:#}");
            assert!(err.contains("connecting to NATS"), "{err}");
            assert!(err.contains(reason), "{reason} in {err}");
        }
    }

    #[cfg(feature = "messaging")]
    #[tokio::test]
    async fn a_jetstream_handler_over_nats_consumes_and_is_listed_in_status() {
        use jenny_core::domain::DomainError;
        use jenny_core::port::Context;
        use jenny_messaging::{MessagingAdapter, SubjectBinding};

        let Some(url) = nats_url() else {
            return;
        };
        let prefix = format!("srv{}", &uuid::Uuid::new_v4().simple().to_string()[..10]);
        let stream = prefix.to_uppercase();
        let client = async_nats::connect(&url).await.unwrap();
        let jetstream = async_nats::jetstream::new(client);
        jetstream
            .create_stream(async_nats::jetstream::stream::Config {
                name: stream.clone(),
                subjects: vec![format!("{prefix}.>")],
                ..Default::default()
            })
            .await
            .unwrap();

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let adapter = MessagingAdapter::new("orders", &prefix)
            .unwrap()
            .stream(&stream)
            .on(
                SubjectBinding::new("orders.created"),
                move |order: serde_json::Value, cx: Context| {
                    let tx = tx.clone();
                    async move {
                        let _ = tx.send((order, cx.principal().map(ToString::to_string)));
                        Ok::<(), DomainError>(())
                    }
                },
            );
        let mut server = server_with("127.0.0.1:0");
        server.config.messaging.nats.url = Some(url);
        let parts = server.register_messaging(adapter).bind().await.unwrap();
        let router = parts.router.clone();
        let handle = parts.handle();
        let serving = tokio::spawn(parts.serve());

        jetstream
            .publish(format!("{prefix}.orders.created"), r#"{"id":1}"#.into())
            .await
            .unwrap()
            .await
            .unwrap();
        let (order, principal) = tokio::time::timeout(Duration::from_secs(10), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(order, serde_json::json!({ "id": 1 }));
        assert_eq!(principal.as_deref(), Some("System[test-service/events]"));

        let (_, body) = call(router, "/j/health/status").await;
        let consumer = &body["consumers"][0];
        assert_eq!(consumer["name"], "test-service-orders");
        assert_eq!(consumer["stream"], stream);
        assert_eq!(
            consumer["filter_subjects"][0],
            format!("{prefix}.orders.created")
        );
        assert!(consumer["pending"].is_u64(), "{consumer}");

        handle.shutdown();
        serving.await.unwrap().unwrap();
        jetstream.delete_stream(&stream).await.unwrap();
    }

    #[cfg(feature = "messaging")]
    fn nats_url() -> Option<String> {
        match std::env::var("JENNY_TEST_NATS_URL") {
            Ok(url) => Some(url),
            Err(_) if std::env::var_os("CI").is_some() => {
                panic!("JENNY_TEST_NATS_URL must be set in CI")
            }
            Err(_) => {
                eprintln!("skipping: JENNY_TEST_NATS_URL is not set");
                None
            }
        }
    }

    /// The `nats` entry of the readiness report.
    #[cfg(feature = "messaging")]
    async fn nats_check(router: Router) -> serde_json::Value {
        let (_, ready) = call(router, "/j/health/ready").await;
        ready["checks"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["name"] == "nats")
            .expect("the runtime registers the NATS readiness check")
            .clone()
    }

    #[cfg(feature = "objects")]
    #[tokio::test]
    async fn without_an_objects_url_there_is_no_object_store() {
        let mut server = server_with("127.0.0.1:0");
        assert!(server.objects().unwrap().is_none());
    }

    #[cfg(feature = "objects")]
    #[tokio::test]
    async fn a_configured_object_store_is_built_once_and_reports_ready() {
        use jenny_core::port::{Blob, Blobs};

        let directory = tempfile::tempdir().unwrap();
        let mut server = server_with("127.0.0.1:0");
        server.config.objects.url = Some(format!("file://{}", directory.path().display()));
        let blobs = server.objects().unwrap().expect("a URL is configured");
        let again = server.objects().unwrap().unwrap();
        assert!(
            Arc::ptr_eq(&blobs, &again),
            "one store, one readiness check"
        );
        blobs.put("a/b", Blob::new(b"x".to_vec())).await.unwrap();

        let parts = server.register(notes()).unwrap().bind().await.unwrap();
        let (status, ready) = call(parts.router.clone(), "/j/health/ready").await;
        assert_eq!(status, StatusCode::OK);
        let checks: Vec<_> = ready["checks"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|c| c["name"] == "objects")
            .collect();
        assert_eq!(checks.len(), 1, "{ready}");
        assert_eq!(checks[0]["status"], "up");
    }

    #[cfg(feature = "objects")]
    #[tokio::test]
    async fn an_object_store_url_that_does_not_parse_is_a_wiring_error() {
        let mut server = server_with("127.0.0.1:0");
        server.config.objects.url = Some("ftp://elsewhere".into());
        let err = format!("{:#}", server.objects().unwrap_err());
        assert!(err.contains("objects"), "{err}");
    }

    #[cfg(all(feature = "persistence", not(feature = "messaging")))]
    #[tokio::test]
    async fn an_outbox_without_a_bus_to_forward_to_is_a_wiring_error() {
        let mut server = server_with("127.0.0.1:0");
        server.config.persistence.postgres.url = Some("postgres://nobody@127.0.0.1:1/x".into());
        server.config.persistence.postgres.outbox = true;
        let err = server.postgres().unwrap_err();
        assert!(err.to_string().contains("messaging"), "{err}");
    }

    /// Needs `JENNY_TEST_POSTGRES_URL`.
    #[cfg(all(feature = "persistence", feature = "messaging"))]
    #[tokio::test]
    async fn with_the_outbox_a_committed_event_reaches_the_bus_through_the_relay() {
        use jenny_core::port::{Context, StoreExt};
        use std::sync::Mutex;
        use topmesys::{Delivery, EventConsumer, HandlerError, Subscriptions};

        let url = match std::env::var("JENNY_TEST_POSTGRES_URL") {
            Ok(url) => url,
            Err(_) if std::env::var_os("CI").is_some() => {
                panic!("JENNY_TEST_POSTGRES_URL must be set in CI")
            }
            Err(_) => return eprintln!("skipping: JENNY_TEST_POSTGRES_URL is not set"),
        };

        #[derive(Debug)]
        struct Recorder(Arc<Mutex<Vec<String>>>);

        #[async_trait::async_trait]
        impl EventConsumer for Recorder {
            type Topic = ();

            fn subscriptions(&self) -> Subscriptions<()> {
                "outbox-test.note.*".into()
            }

            async fn handle_event(&self, _: &(), delivery: &Delivery) -> Result<(), HandlerError> {
                self.0
                    .lock()
                    .unwrap()
                    .push(delivery.message().topic().text().to_string());
                Ok(())
            }
        }

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
        #[derive(serde::Serialize)]
        struct NoteCreated {
            id: u32,
        }
        impl jenny_core::domain::DomainEvent for NoteCreated {
            fn name(&self) -> &str {
                "note.created"
            }
        }

        let mut server = server_with("127.0.0.1:0");
        server.config.messaging.namespace = Some("outbox-test".to_string());
        server.config.persistence.postgres.url = Some(url);
        server.config.persistence.postgres.outbox = true;
        let seen = Arc::new(Mutex::new(Vec::new()));
        server.subscribe(Recorder(seen.clone())).unwrap();

        let postgres = server.postgres().unwrap().expect("a URL is configured");
        assert!(postgres.has_outbox());
        let table = format!("server_outbox_{}", uuid::Uuid::new_v4().simple());
        let store = postgres.documents_in::<Note>(&table);
        let events = server.events().unwrap();

        let parts = server.register(notes()).unwrap().bind().await.unwrap();
        let handle = parts.handle();
        let serving = tokio::spawn(parts.serve());

        store
            .add(Note { id: 1 }, NoteCreated { id: 1 })
            .raising(&events, &Context::new())
            .await
            .unwrap();
        for _ in 0..100 {
            if !seen.lock().unwrap().is_empty() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(
            seen.lock()
                .unwrap()
                .contains(&"outbox-test.note.created".to_string()),
            "the relay forwarded the committed event to the bus"
        );

        jenny_persistence::sqlx::query(jenny_persistence::sqlx::AssertSqlSafe(format!(
            "DROP TABLE \"{table}\""
        )))
        .execute(postgres.pool())
        .await
        .unwrap();
        handle.shutdown();
        serving.await.unwrap().unwrap();
    }

    /// What `from_args` gets from telemetry init, minus the global subscriber.
    #[cfg(feature = "telemetry-prometheus")]
    fn with_a_registry(mut server: JennyServer) -> JennyServer {
        server.telemetry = Some(TelemetryHandle {
            tracer_provider: None,
            logger_provider: None,
            prometheus_registry: Some(prometheus::Registry::new()),
            loki_task: None,
        });
        server
    }

    #[cfg(feature = "telemetry-prometheus")]
    #[tokio::test]
    async fn the_metrics_endpoint_can_require_basic_auth() {
        use crate::config::BasicAuthConfig;

        let mut server = with_a_registry(server_with("127.0.0.1:0"));
        server.config.server.endpoints.metrics.auth = Some(BasicAuthConfig {
            username: "prometheus".into(),
            password: "s3cret".into(),
        });
        let metrics = server.bind().await.unwrap().router;

        let unauthorized = metrics
            .clone()
            .oneshot(
                http::Request::get("/j/metrics")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(unauthorized.status(), StatusCode::UNAUTHORIZED);

        use base64::Engine as _;
        let credential = base64::engine::general_purpose::STANDARD.encode("prometheus:s3cret");
        let authorized = metrics
            .oneshot(
                http::Request::get("/j/metrics")
                    .header(http::header::AUTHORIZATION, format!("Basic {credential}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(authorized.status(), StatusCode::OK);
    }

    #[cfg(feature = "telemetry-prometheus")]
    #[tokio::test]
    async fn metrics_move_to_the_management_listener_at_a_path_of_their_own() {
        let mut server = with_a_registry(managed_server());
        server.config.server.endpoints.metrics.endpoint.path = Some("/scrape".into());
        let parts = server.bind().await.unwrap();
        let management = parts.management.as_ref().unwrap().1.clone();

        let (status, _) = call(management.clone(), "/scrape").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            call(management, "/j/metrics").await.0,
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            call(parts.router.clone(), "/scrape").await.0,
            StatusCode::NOT_FOUND
        );
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

    #[cfg(feature = "openapi")]
    fn documented_notes() -> RestAdapter {
        use utoipa::OpenApi as _;

        #[derive(utoipa::OpenApi)]
        #[openapi(info(title = "notes"))]
        struct NotesApi;

        RestAdapter::new("my-notes")
            .unwrap()
            .with_openapi(NotesApi::openapi())
            .mount(Routes::new("notes").get("/notes", || async { "[]" }))
            .unwrap()
    }

    #[cfg(feature = "openapi")]
    #[tokio::test]
    async fn the_openapi_document_can_move_to_the_api_listener_or_be_switched_off() {
        use crate::config::Listener;

        let mut server = managed_server();
        let endpoints = &mut server.config.server.endpoints;
        endpoints.openapi.listener = Some(Listener::Api);
        endpoints.openapi.path = Some("/docs/openapi.json".into());
        endpoints.swagger.enabled = false;
        let parts = server
            .register(documented_notes())
            .unwrap()
            .bind()
            .await
            .unwrap();
        let management = parts.management.as_ref().unwrap().1.clone();

        let (status, body) = call(parts.router.clone(), "/docs/openapi.json").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["info"]["title"], "notes");
        assert_eq!(
            call(management.clone(), "/j/openapi.json").await.0,
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            call(management, "/j/swagger/").await.0,
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            call(parts.router.clone(), "/j/swagger/").await.0,
            StatusCode::NOT_FOUND
        );

        let mut server = server_with("127.0.0.1:0");
        server.config.server.endpoints.openapi.enabled = false;
        server.config.server.endpoints.swagger.enabled = false;
        let parts = server
            .register(documented_notes())
            .unwrap()
            .bind()
            .await
            .unwrap();
        assert_eq!(
            call(parts.router.clone(), "/j/openapi.json").await.0,
            StatusCode::NOT_FOUND
        );
    }

    #[cfg(feature = "swagger-ui")]
    #[tokio::test]
    async fn swagger_ui_loads_the_document_from_the_openapi_group() {
        let mut server = server_with("127.0.0.1:0");
        let endpoints = &mut server.config.server.endpoints;
        endpoints.openapi.path = Some("/docs/openapi.json".into());
        endpoints.swagger.path = Some("/docs/ui".into());
        let parts = server
            .register(documented_notes())
            .unwrap()
            .bind()
            .await
            .unwrap();

        let response = parts
            .router
            .clone()
            .oneshot(
                http::Request::get("/docs/ui/swagger-initializer.js")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let script = String::from_utf8(bytes.to_vec()).unwrap();
        assert!(script.contains("\"/docs/openapi.json\""), "{script}");
        assert_eq!(
            call(parts.router.clone(), "/j/openapi.json").await.0,
            StatusCode::NOT_FOUND,
            "no copy at the old place"
        );

        let mut server = server_with("127.0.0.1:0");
        server.config.server.endpoints.openapi.enabled = false;
        let Err(err) = server.register(documented_notes()).unwrap().bind().await else {
            panic!("swagger without openapi");
        };
        assert!(
            err.to_string().contains("`server.endpoints.openapi`"),
            "{err}"
        );
    }
}
