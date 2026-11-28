//! Configuration: CLI arguments, layered loading, and the sections the framework reads.
//!
//! Three layers, each overriding the one before:
//!
//! 1. a YAML file (`--config-path`, defaulting to `config.yaml` when it exists),
//! 2. environment variables prefixed `JENNY__`, using `__` as the section separator —
//!    `JENNY__SERVER__ADDRESS=[::]:9000` sets `server.address`,
//! 3. CLI arguments (`--address`, `--debug`).
//!
//! That order is the deployment order: a file for defaults you commit, environment
//! variables for what differs per environment, flags for what differs per run.
//!
//! The framework reads the sections it owns — `server`, `telemetry`, and `messaging`,
//! `persistence` and `objects` when those features are on. Everything else passes
//! through untyped, so it imposes no schema on your configuration: an adapter
//! deserialises its own `applications.<name>` section into whatever type it likes, via
//! [`ServerContext::app_section`](crate::ServerContext::app_section).
//!
//! ```yaml
//! service_name: my-service    # defaults to the package name `jenny::service!()` read
//! service_version: "1.2.3"    # defaults to the package version
//! system:
//!   roles: [Maintenance]      # what tasks and event handlers hold, as System[…]
//! tasks:
//!   lease:                    # optional: each slot of a scheduled task on one replica
//!     backend: postgres       # memory | postgres | nats
//!     ttl: 30                 # seconds; must exceed the clock skew between replicas
//! server:
//!   address: "[::]:8000"
//!   api_path: /api        # where REST adapters mount
//!   framework_path: /j    # the default prefix of health, metrics and OpenAPI
//!   shutdown_delay_seconds: 5     # keep serving, reported not ready, before closing
//!   shutdown_timeout_seconds: 30  # then wait at most this long for in-flight requests
//!   management:           # optional: the framework's endpoints on a port of their own
//!     address: "[::]:8081"
//!   endpoints:            # per group: enabled, listener (api | management), path, log_requests
//!     metrics:
//!       auth:             # optional; omit to leave the scrape endpoint open
//!         username: prometheus
//!         password: s3cret
//!     swagger: { enabled: true }   # off by default; follows `openapi`
//!     public_status: { enabled: true, listener: api, path: /status }
//! telemetry:
//!   log_format: pretty          # service_name and service_version default to the above
//! messaging:
//!   buffer_size: 64
//!   events:
//!     headers:                # what `JennyServer::events()` writes into every event
//!       caller: true          # caller-subject, and caller-actor when delegated
//!       source: true          # source-service, source-version
//!   nats:                     # one connection: key-value stores and the gateway
//!     url: tls://nats:4222    # JENNY__MESSAGING__NATS__URL
//!     tls:                    # optional; see jenny_messaging::connection
//!       cert: /etc/nats-tls/tls.crt
//!       key: /etc/nats-tls/tls.key
//!     auth:                   # optional: creds, nkey, user + password, or token
//!       creds: /etc/nats/service.creds
//!   inbound:                  # event handlers, on the bus and on JetStream
//!     principal: system       # or envelope: run as the event's caller
//!     max_deliver: 5
//!     backoff: [1, 5, 30]
//!     # dead_letter: SHOP.dead
//!     subscriptions:
//!       - subject: shop.orders.created
//!         max_deliver: 10
//! persistence:
//!   postgres:
//!     url: postgres://notes:secret@db/notes   # better: JENNY__PERSISTENCE__POSTGRES__URL
//!     outbox: true
//! objects:
//!   url: s3://attachments/notes   # or file:///dir, memory://; AWS_* for credentials
//! applications:
//!   my-notes:            # your shape, your business
//!     greeting: "hello"
//! ```

use crate::telemetry::TelemetryConfig;
use clap::Parser;
use jenny_rest::DEFAULT_API_PATH;
use serde::{Deserialize, de::DeserializeOwned};

/// Probed when `--config-path` is not given. Absent is fine; unreadable is not.
pub const DEFAULT_CONFIG_PATH: &str = "config.yaml";
/// Default environment-variable prefix.
pub const DEFAULT_ENV_PREFIX: &str = "JENNY";
/// Default maximum request body, in bytes.
///
/// Generous enough for ordinary JSON payloads, small enough that an unbounded upload
/// cannot exhaust memory before a handler ever sees it.
pub const DEFAULT_BODY_LIMIT: usize = 256 * 1024;
/// Default prefix for the framework's own endpoints.
pub const DEFAULT_FRAMEWORK_PATH: &str = "/j";
/// Default bound on draining in-flight requests at shutdown.
pub const DEFAULT_SHUTDOWN_TIMEOUT_SECONDS: u64 = 30;
/// Default in-process event bus buffer.
pub const DEFAULT_EVENT_BUFFER: usize = 64;

/// Command-line arguments of a jenny server.
#[derive(Debug, Default, Parser)]
#[command(about = "A service built with jenny", long_about = None)]
pub struct ServerArgs {
    #[clap(
        short,
        long,
        help = "Address to listen on (overrides the config file and environment)."
    )]
    pub address: Option<String>,
    #[clap(
        short,
        long,
        help = "Emit debug-level logs. WARNING: may expose sensitive values."
    )]
    pub debug: bool,
    #[clap(
        short,
        long,
        help = "Path to the configuration file (default: config.yaml, if present)."
    )]
    pub config_path: Option<String>,
    #[clap(
        short,
        long,
        help = "Read settings from environment variables prefixed <ENV_PREFIX>__ (default: JENNY)."
    )]
    pub env_prefix: Option<String>,
}

/// The `server` section.
#[derive(Clone, Debug, Deserialize)]
#[serde(default)]
pub struct HttpConfig {
    /// Socket address to bind, e.g. `[::]:8000`. Port `0` picks a free one.
    pub address: String,
    /// Maximum accepted request body, in bytes.
    ///
    /// Enforced when a handler *reads* the body, not at the socket — a handler that
    /// ignores the body will happily answer a request that carries an oversized one.
    pub body_limit: usize,
    /// Where REST adapters mount unless they chose a path in code.
    ///
    /// See [`RestAdapter`](jenny_rest::RestAdapter) for the precedence rule: an
    /// explicit `.at(…)` wins over this.
    pub api_path: String,
    /// The prefix of the default paths of the framework's own endpoints: health,
    /// metrics, OpenAPI.
    ///
    /// Namespaced away from `api_path` so the framework can never collide with a route
    /// your service wanted. `/j` is short because it appears in every probe URL. A
    /// group's own [`path`](EndpointConfig::path) replaces its default outright.
    pub framework_path: String,
    /// A second listener for the framework's endpoints (ADR-020). Unset, everything is
    /// served on [`address`](Self::address).
    pub management: Option<ManagementConfig>,
    /// Which of the framework's endpoint groups are served, on which listener and at
    /// which path.
    pub endpoints: EndpointsConfig,
    /// After `SIGTERM` (or `SIGINT`, or [`ServerHandle::shutdown`]), how long to keep
    /// accepting requests while `/health/ready` already reports the server as stopping.
    ///
    /// On Kubernetes the pod is removed from the load balancer at the same time as it
    /// is told to stop, not before, so for a few seconds new requests still arrive.
    /// Closing the listener at once refuses them; a delay of about five seconds lets the
    /// endpoint removal catch up. Defaults to `0`.
    ///
    /// [`ServerHandle::shutdown`]: crate::ServerHandle::shutdown
    pub shutdown_delay_seconds: u64,
    /// Once the listener is closed, how long in-flight requests may take to finish
    /// before the server stops waiting for them. Defaults to `30`, which is what
    /// Kubernetes waits before it kills the pod.
    pub shutdown_timeout_seconds: u64,
}

impl Default for HttpConfig {
    fn default() -> Self {
        Self {
            address: "[::]:8000".to_string(),
            body_limit: DEFAULT_BODY_LIMIT,
            api_path: DEFAULT_API_PATH.to_string(),
            framework_path: DEFAULT_FRAMEWORK_PATH.to_string(),
            management: None,
            endpoints: EndpointsConfig::default(),
            shutdown_delay_seconds: 0,
            shutdown_timeout_seconds: DEFAULT_SHUTDOWN_TIMEOUT_SECONDS,
        }
    }
}

/// `server.management`: the listener the framework's endpoints move to.
///
/// With it set, the API listener serves the REST adapters only and answers
/// `<framework_path>/…` with 404, whatever sits in front of it, unless a group was put
/// on the API listener explicitly. The paths are the same on either listener.
#[derive(Clone, Debug, Deserialize)]
pub struct ManagementConfig {
    /// Socket address to bind, e.g. `[::]:8081`. Port `0` picks a free one.
    pub address: String,
}

/// Which listener an endpoint group is served on.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Listener {
    /// `server.address`, beside the REST adapters.
    Api,
    /// `server.management.address`.
    Management,
}

impl std::fmt::Display for Listener {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Listener::Api => "api",
            Listener::Management => "management",
        })
    }
}

/// One endpoint group under `server.endpoints`.
///
/// A key left out of the configuration keeps the group's default, so
/// `public_status: { listener: api }` leaves that group switched off.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EndpointConfig {
    /// Whether the group is served at all.
    pub enabled: bool,
    /// Where it is served. Unset: on the management listener when `server.management`
    /// is set, else on the API listener. `management` without `server.management` is a
    /// startup error.
    pub listener: Option<Listener>,
    /// Replaces the default path. The full path of the one route for `status`,
    /// `metrics`, `openapi` and `public_status`; a prefix for `probes` (`<path>/live`,
    /// `<path>/ready`) and `swagger` (the UI under `<path>`).
    pub path: Option<String>,
    /// Whether its requests produce a request span and an access-log line. They carry a
    /// request id either way.
    pub log_requests: bool,
}

impl EndpointConfig {
    const fn new(enabled: bool, log_requests: bool) -> Self {
        Self {
            enabled,
            listener: None,
            path: None,
            log_requests,
        }
    }
}

/// `server.endpoints.metrics`: an [`EndpointConfig`], and the scrape's credentials.
#[derive(Clone, Debug)]
pub struct MetricsEndpointConfig {
    pub endpoint: EndpointConfig,
    /// Optional HTTP Basic credentials the scraper must present.
    ///
    /// Left unset the endpoint is open, which is usually right when it is only
    /// reachable from inside the cluster and wrong the moment it is not. Metrics leak
    /// more than people expect: route names, tenant counts, error rates, deploy times.
    pub auth: Option<BasicAuthConfig>,
}

/// `server.endpoints`: the framework's endpoint groups (ADR-020).
///
/// | Group | Default path | Served | `log_requests` |
/// |---|---|---|---|
/// | `probes` | `<framework_path>/health/live`, `…/health/ready` | yes | no |
/// | `status` | `<framework_path>/health/status` | yes | no |
/// | `metrics` | `<framework_path>/metrics` | with `telemetry-prometheus` | no |
/// | `openapi` | `<framework_path>/openapi.json` | with `openapi`, when an adapter supplies a document | yes |
/// | `swagger` | `<framework_path>/swagger` | no, until enabled (with `swagger-ui`); follows `openapi` | yes |
/// | `public_status` | `<framework_path>/health/public` | no, until enabled | no |
///
/// Every group can be switched off and moved to either listener. `swagger` loads the
/// document from the `openapi` group, so it follows it: not served while `openapi` is
/// off, and on `openapi`'s listener unless given one, which must then be the same. `public_status` answers `{"status": "up" | "degraded" | "down"}` and
/// nothing else, for a page the public may see; `status` names every check and its
/// error.
#[derive(Clone, Debug, Deserialize)]
#[serde(from = "RawEndpoints")]
pub struct EndpointsConfig {
    pub probes: EndpointConfig,
    pub status: EndpointConfig,
    pub metrics: MetricsEndpointConfig,
    pub openapi: EndpointConfig,
    pub swagger: EndpointConfig,
    pub public_status: EndpointConfig,
}

impl Default for EndpointsConfig {
    fn default() -> Self {
        RawEndpoints::default().into()
    }
}

/// What the configuration says, before each group's defaults fill the gaps.
#[derive(Default, Deserialize)]
#[serde(default)]
struct RawEndpoints {
    probes: RawEndpoint,
    status: RawEndpoint,
    metrics: RawMetricsEndpoint,
    openapi: RawEndpoint,
    swagger: RawEndpoint,
    public_status: RawEndpoint,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct RawEndpoint {
    enabled: Option<bool>,
    listener: Option<Listener>,
    path: Option<String>,
    log_requests: Option<bool>,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct RawMetricsEndpoint {
    enabled: Option<bool>,
    listener: Option<Listener>,
    path: Option<String>,
    log_requests: Option<bool>,
    auth: Option<BasicAuthConfig>,
}

impl RawEndpoint {
    fn or(self, default: EndpointConfig) -> EndpointConfig {
        EndpointConfig {
            enabled: self.enabled.unwrap_or(default.enabled),
            listener: self.listener,
            path: self.path,
            log_requests: self.log_requests.unwrap_or(default.log_requests),
        }
    }
}

impl From<RawEndpoints> for EndpointsConfig {
    fn from(raw: RawEndpoints) -> Self {
        let RawMetricsEndpoint {
            enabled,
            listener,
            path,
            log_requests,
            auth,
        } = raw.metrics;
        let metrics = RawEndpoint {
            enabled,
            listener,
            path,
            log_requests,
        };
        Self {
            probes: raw.probes.or(EndpointConfig::new(true, false)),
            status: raw.status.or(EndpointConfig::new(true, false)),
            metrics: MetricsEndpointConfig {
                endpoint: metrics.or(EndpointConfig::new(true, false)),
                auth,
            },
            openapi: raw.openapi.or(EndpointConfig::new(true, true)),
            swagger: raw.swagger.or(EndpointConfig::new(false, true)),
            public_status: raw.public_status.or(EndpointConfig::new(false, false)),
        }
    }
}

/// A single Basic-auth credential from configuration.
///
/// `Debug` deliberately redacts the password. This struct is reachable from
/// [`ServerConfig`], and a config dump at startup is exactly how secrets end up in a
/// log aggregator.
#[derive(Clone, Deserialize)]
pub struct BasicAuthConfig {
    pub username: String,
    pub password: String,
}

impl std::fmt::Debug for BasicAuthConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BasicAuthConfig")
            .field("username", &self.username)
            .field("password", &"<redacted>")
            .finish()
    }
}

/// The `messaging` section. Applies with the `messaging` feature.
#[derive(Clone, Debug, Deserialize)]
#[serde(default)]
pub struct MessagingConfig {
    /// How many events the in-process bus buffers before publishers wait.
    pub buffer_size: usize,
    /// Prefix for every event topic, and for the NATS buckets of key-value stores.
    /// Defaults to the top-level `service_name`.
    pub namespace: Option<String>,
    /// What the events the service raises carry.
    pub events: EventsConfig,
    /// How inbound handlers run, on the bus and on JetStream, and what becomes of a
    /// message that fails: principal policy, retries, dead letters. With `messaging`.
    #[cfg(feature = "messaging")]
    pub inbound: jenny_messaging::consumer::InboundConfig,
    /// The service's NATS connection, which the key-value stores and the gateway share:
    /// its servers, TLS and credentials (ADR-031). With the `messaging` feature.
    #[cfg(feature = "messaging")]
    pub nats: jenny_messaging::NatsConnectionConfig,
}

impl Default for MessagingConfig {
    fn default() -> Self {
        Self {
            buffer_size: DEFAULT_EVENT_BUFFER,
            namespace: None,
            events: EventsConfig::default(),
            #[cfg(feature = "messaging")]
            inbound: jenny_messaging::consumer::InboundConfig::default(),
            #[cfg(feature = "messaging")]
            nats: jenny_messaging::NatsConnectionConfig::default(),
        }
    }
}

/// `messaging.events`.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct EventsConfig {
    pub headers: EventHeadersConfig,
}

/// `messaging.events.headers`: the built-in enrichers `JennyServer::events()` adds
/// (ADR-032). Both are off by default: a principal id is personal data, and a stream
/// keeps it for as long as its retention says.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct EventHeadersConfig {
    /// `caller-subject`, and `caller-actor` when someone acted on the subject's behalf
    /// ([`Caller`](jenny_core::port::Caller)).
    pub caller: bool,
    /// `source-service` and `source-version`, the top-level service name and version
    /// ([`Source`](jenny_core::port::Source)).
    pub source: bool,
}

/// The `system` section: the service acting on its own.
///
/// Scheduled tasks run as `System[<service_name>/tasks]` and inbound event handlers as
/// `System[<service_name>/events]`; Cedar sees them as `System::"<service_name>/…"`.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct SystemConfig {
    /// The roles every system principal holds, for policies written against roles.
    /// Empty by default; a policy can also name `System::"my-notes/tasks"` itself.
    pub roles: Vec<String>,
}

/// Default `tasks.lease.ttl`, in seconds.
pub const DEFAULT_LEASE_TTL_SECONDS: u64 = 30;

/// The `tasks` section: how scheduled tasks run.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct TasksConfig {
    /// Run each slot of every scheduled task on one replica only, the one that acquires
    /// its lease (ADR-029). Unset, every replica runs every task, as without a cluster.
    pub lease: Option<LeaseConfig>,
}

/// `tasks.lease`: where leases live and how long they last.
#[derive(Clone, Debug, Deserialize)]
pub struct LeaseConfig {
    pub backend: LeaseBackend,
    /// Seconds a slot's lease outlives the run's last renewal; renewed every third of
    /// it. Must exceed the clock skew between replicas. Defaults to 30; 0 is a startup
    /// error.
    #[serde(default = "default_lease_ttl")]
    pub ttl: u64,
}

fn default_lease_ttl() -> u64 {
    DEFAULT_LEASE_TTL_SECONDS
}

/// Where `tasks.lease` keeps its leases.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LeaseBackend {
    /// In the process: one replica, or tests.
    Memory,
    /// The table `jenny_leases` in `persistence.postgres`. Needs the `persistence`
    /// feature.
    Postgres,
    /// The bucket `jenny_leases` over `messaging.nats`. Needs the `messaging` feature.
    Nats,
}

/// The `persistence` section. Applies with the `persistence` feature.
#[cfg(feature = "persistence")]
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct PersistenceConfig {
    pub postgres: jenny_persistence::PostgresConfig,
}

/// The part of the configuration tree the framework itself interprets.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct ServerConfig {
    /// Who the service is (`JENNY__SERVICE_NAME`). Overrides the name `jenny::service!()`
    /// read from the package, and is the default for `telemetry.service_name`, for
    /// `messaging.namespace` and for `/health/status`. Empty is a startup error.
    pub service_name: Option<String>,
    /// The service's version (`JENNY__SERVICE_VERSION`). Overrides the package version,
    /// and is the default for `telemetry.service_version` and `/health/status`.
    pub service_version: Option<String>,
    pub system: SystemConfig,
    pub tasks: TasksConfig,
    pub server: HttpConfig,
    pub telemetry: TelemetryConfig,
    pub messaging: MessagingConfig,
    #[cfg(feature = "persistence")]
    pub persistence: PersistenceConfig,
    /// Where blobs live. Applies with the `objects` feature.
    #[cfg(feature = "objects")]
    pub objects: jenny_objects::ObjectsConfig,
}

impl ServerConfig {
    /// Settle who the service is: `service_name` and `service_version` from
    /// configuration, else `service`'s, written back here and into the `telemetry`
    /// values that are unset.
    ///
    /// Fails when the name is empty after that, because topics, buckets, consumers and
    /// system principals are all named after it.
    pub fn identify(
        &mut self,
        service: jenny_core::Service,
    ) -> anyhow::Result<jenny_core::Service> {
        let name = self.service_name.clone().unwrap_or(service.name);
        if name.trim().is_empty() {
            anyhow::bail!(
                "`service_name` is empty: set it (JENNY__SERVICE_NAME) or leave it unset to \
                 use the package name"
            );
        }
        let version = self.service_version.clone().unwrap_or(service.version);
        self.service_name = Some(name.clone());
        self.service_version = Some(version.clone());
        self.telemetry
            .service_name
            .get_or_insert_with(|| name.clone());
        self.telemetry
            .service_version
            .get_or_insert_with(|| version.clone());
        Ok(jenny_core::Service::new(name, version))
    }
}

/// The merged configuration tree, file and environment.
///
/// The whole tree is kept so application sections can be deserialised on demand
/// rather than up front.
#[derive(Clone, Debug)]
pub struct ConfigSource(config::Config);

impl ConfigSource {
    pub fn load(args: &ServerArgs) -> anyhow::Result<Self> {
        let mut builder = config::Config::builder();
        // An explicitly requested file that is missing is an error the operator needs
        // to see; the conventional default one simply may not exist.
        let path = args.config_path.clone().or_else(|| {
            std::fs::metadata(DEFAULT_CONFIG_PATH)
                .is_ok()
                .then(|| DEFAULT_CONFIG_PATH.to_string())
        });
        if let Some(path) = path {
            builder = builder.add_source(config::File::with_name(&path));
        }
        let prefix = args.env_prefix.as_deref().unwrap_or(DEFAULT_ENV_PREFIX);
        builder = builder.add_source(
            config::Environment::with_prefix(prefix)
                .prefix_separator("__")
                .separator("__"),
        );
        Ok(Self(builder.build()?))
    }

    /// Deserialise any section, e.g. `"applications.my-notes"`.
    pub fn section<T: DeserializeOwned>(&self, key: &str) -> anyhow::Result<T> {
        Ok(self.0.get(key)?)
    }

    pub fn has_section(&self, key: &str) -> bool {
        self.0.get_table(key).is_ok() || self.0.get_string(key).is_ok()
    }

    /// Deserialise [`ServerConfig`] and apply the CLI overrides on top.
    ///
    /// The service's identity is not resolved yet: [`ServerConfig::identify`] does that
    /// with what `jenny::service!()` read.
    pub fn server_config(&self, args: &ServerArgs) -> anyhow::Result<ServerConfig> {
        let mut config: ServerConfig = self.0.clone().try_deserialize()?;
        // Before anything starts, rather than when the connection is first made.
        #[cfg(feature = "messaging")]
        config
            .messaging
            .nats
            .validate()
            .map_err(|e| anyhow::anyhow!("invalid `messaging.nats` configuration: {e}"))?;
        #[cfg(feature = "messaging")]
        config
            .messaging
            .inbound
            .validate()
            .map_err(|e| anyhow::anyhow!("invalid `messaging.inbound` configuration: {e}"))?;
        if let Some(address) = &args.address {
            config.server.address = address.clone();
        }
        if args.debug {
            config.telemetry.debug = true;
        }
        Ok(config)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_yaml(content: &str) -> tempfile::NamedTempFile {
        let mut file = tempfile::Builder::new().suffix(".yaml").tempfile().unwrap();
        file.write_all(content.as_bytes()).unwrap();
        file.flush().unwrap();
        file
    }

    #[test]
    fn an_explicitly_named_missing_file_is_an_error() {
        let args = ServerArgs {
            env_prefix: Some("JENNY_TEST_NONE".into()),
            config_path: Some("/definitely/not/there.yaml".into()),
            ..Default::default()
        };
        assert!(ConfigSource::load(&args).is_err());
    }

    #[test]
    fn with_no_file_and_no_env_the_defaults_apply() {
        let args = ServerArgs {
            env_prefix: Some("JENNY_TEST_NONE".into()),
            ..Default::default()
        };
        let source = ConfigSource::load(&args).unwrap();
        let config = source.server_config(&args).unwrap();
        assert_eq!(config.server.address, "[::]:8000");
        assert_eq!(config.server.body_limit, DEFAULT_BODY_LIMIT);
        assert!(config.server.management.is_none());
        assert!(!config.telemetry.debug);
    }

    fn server_section(yaml: &str, prefix: &str) -> HttpConfig {
        let file = write_yaml(yaml);
        let args = ServerArgs {
            config_path: Some(file.path().to_string_lossy().into_owned()),
            env_prefix: Some(prefix.into()),
            ..Default::default()
        };
        ConfigSource::load(&args)
            .unwrap()
            .server_config(&args)
            .unwrap()
            .server
    }

    #[test]
    fn every_endpoint_group_has_its_own_defaults() {
        let endpoints = EndpointsConfig::default();
        let on = |log_requests| EndpointConfig::new(true, log_requests);
        assert_eq!(endpoints.probes, on(false));
        assert_eq!(endpoints.status, on(false));
        assert_eq!(endpoints.metrics.endpoint, on(false));
        assert!(endpoints.metrics.auth.is_none());
        assert_eq!(endpoints.openapi, on(true));
        assert_eq!(endpoints.swagger, EndpointConfig::new(false, true));
        assert_eq!(endpoints.public_status, EndpointConfig::new(false, false));

        // A management listener changes no group's settings; it only changes where an
        // unset `listener` resolves to.
        let server = server_section(
            "server:\n  management:\n    address: \"127.0.0.1:0\"\n",
            "JENNY_TEST_ENDPOINTS_MANAGED",
        );
        assert_eq!(server.management.unwrap().address, "127.0.0.1:0");
        assert_eq!(server.endpoints.probes, on(false));
        assert_eq!(server.endpoints.public_status.listener, None);
    }

    #[test]
    fn a_group_takes_what_is_configured_and_keeps_its_defaults_for_the_rest() {
        let server = server_section(
            r#"
server:
  endpoints:
    public_status: { listener: api, path: /status }
    openapi: { listener: management, path: /docs/openapi.json }
    swagger: { enabled: true }
    probes: { log_requests: true }
    metrics:
      path: /scrape
      auth: { username: prometheus, password: s3cret }
"#,
            "JENNY_TEST_ENDPOINTS_SET",
        );
        let endpoints = server.endpoints;
        assert_eq!(
            endpoints.public_status,
            EndpointConfig {
                enabled: false,
                listener: Some(Listener::Api),
                path: Some("/status".into()),
                log_requests: false,
            },
            "enabled stays off unless asked for"
        );
        assert_eq!(endpoints.openapi.listener, Some(Listener::Management));
        assert_eq!(
            endpoints.openapi.path.as_deref(),
            Some("/docs/openapi.json")
        );
        assert!(endpoints.openapi.log_requests);
        assert!(endpoints.swagger.enabled);
        assert!(endpoints.swagger.log_requests);
        assert!(endpoints.probes.enabled);
        assert!(endpoints.probes.log_requests);
        assert_eq!(endpoints.metrics.endpoint.path.as_deref(), Some("/scrape"));
        let auth = endpoints.metrics.auth.unwrap();
        assert_eq!(auth.username, "prometheus");
        assert!(!format!("{auth:?}").contains("s3cret"));
    }

    #[test]
    fn a_group_can_be_switched_on_from_the_environment() {
        let prefix = "JENNY_TEST_ENDPOINTS_ENV";
        // SAFETY: the variable names are unique to this test.
        unsafe {
            std::env::set_var(
                format!("{prefix}__SERVER__ENDPOINTS__PUBLIC_STATUS__ENABLED"),
                "true",
            );
            std::env::set_var(
                format!("{prefix}__SERVER__MANAGEMENT__ADDRESS"),
                "[::]:8081",
            );
        }
        let server = server_section("{}", prefix);
        unsafe {
            std::env::remove_var(format!(
                "{prefix}__SERVER__ENDPOINTS__PUBLIC_STATUS__ENABLED"
            ));
            std::env::remove_var(format!("{prefix}__SERVER__MANAGEMENT__ADDRESS"));
        }
        assert!(server.endpoints.public_status.enabled);
        assert_eq!(server.management.unwrap().address, "[::]:8081");
    }

    #[test]
    fn the_removed_keys_no_longer_mean_anything() {
        // `server.health`, `server.metrics` and `telemetry.prometheus_port` are gone
        // from the types: they are ignored like any unknown key.
        let server = server_section(
            "server:\n  health: false\n  metrics:\n    enabled: false\ntelemetry:\n  prometheus_port: 9090\n",
            "JENNY_TEST_ENDPOINTS_REMOVED",
        );
        assert!(server.endpoints.probes.enabled);
        assert!(server.endpoints.status.enabled);
        assert!(server.endpoints.metrics.endpoint.enabled);
    }

    #[test]
    fn each_layer_overrides_the_one_before() {
        let file = write_yaml(
            r#"
server:
  address: "127.0.0.1:7000"
  body_limit: 4096
service_name: from-file
applications:
  my-notes:
    greeting: "hi"
"#,
        );
        let prefix = "JENNY_TEST_LAYERING";
        // SAFETY: the variable name is unique to this test.
        unsafe { std::env::set_var(format!("{prefix}__SERVICE_NAME"), "from-env") };

        let args = ServerArgs {
            address: Some("127.0.0.1:9000".into()),
            debug: true,
            config_path: Some(file.path().to_string_lossy().into_owned()),
            env_prefix: Some(prefix.into()),
        };
        let source = ConfigSource::load(&args).unwrap();
        let config = source.server_config(&args).unwrap();

        assert_eq!(
            config.service_name.as_deref(),
            Some("from-env"),
            "env beats file"
        );
        assert_eq!(config.server.address, "127.0.0.1:9000", "cli beats file");
        assert_eq!(config.server.body_limit, 4096, "file beats the default");
        assert!(config.telemetry.debug, "--debug is applied");

        unsafe { std::env::remove_var(format!("{prefix}__SERVICE_NAME")) };
    }

    fn identified(yaml: &str, prefix: &str) -> anyhow::Result<(ServerConfig, jenny_core::Service)> {
        let file = write_yaml(yaml);
        let args = ServerArgs {
            config_path: Some(file.path().to_string_lossy().into_owned()),
            env_prefix: Some(prefix.into()),
            ..Default::default()
        };
        let mut config = ConfigSource::load(&args)?.server_config(&args)?;
        let service = config.identify(jenny_core::Service::new("from-package", "0.1.0"))?;
        Ok((config, service))
    }

    #[test]
    fn tasks_lease_is_off_unless_configured_and_ttl_defaults_to_30() {
        let (config, _) = identified("{}", "JENNY_TEST_LEASE_NONE").unwrap();
        assert!(config.tasks.lease.is_none());

        let (config, _) = identified(
            "tasks:\n  lease:\n    backend: nats\n",
            "JENNY_TEST_LEASE_NATS",
        )
        .unwrap();
        let lease = config.tasks.lease.unwrap();
        assert_eq!(lease.backend, LeaseBackend::Nats);
        assert_eq!(lease.ttl, DEFAULT_LEASE_TTL_SECONDS);

        let (config, _) = identified(
            "tasks:\n  lease:\n    backend: postgres\n    ttl: 12\n",
            "JENNY_TEST_LEASE_PG",
        )
        .unwrap();
        let lease = config.tasks.lease.unwrap();
        assert_eq!(lease.backend, LeaseBackend::Postgres);
        assert_eq!(lease.ttl, 12);

        assert!(
            identified(
                "tasks:\n  lease:\n    backend: redis\n",
                "JENNY_TEST_LEASE_BAD"
            )
            .is_err()
        );
    }

    #[test]
    fn the_package_identity_applies_unless_configuration_overrides_it() {
        let (config, service) = identified("{}", "JENNY_TEST_IDENTITY_DEFAULT").unwrap();
        assert_eq!(service, jenny_core::Service::new("from-package", "0.1.0"));
        assert_eq!(
            config.telemetry.service_name.as_deref(),
            Some("from-package")
        );
        assert_eq!(config.telemetry.service_version.as_deref(), Some("0.1.0"));

        let prefix = "JENNY_TEST_IDENTITY_ENV";
        // SAFETY: the variable name is unique to this test.
        unsafe { std::env::set_var(format!("{prefix}__SERVICE_VERSION"), "2.0.0") };
        let (config, service) = identified("service_name: from-file\n", prefix).unwrap();
        unsafe { std::env::remove_var(format!("{prefix}__SERVICE_VERSION")) };
        assert_eq!(service, jenny_core::Service::new("from-file", "2.0.0"));
        assert_eq!(config.service_name.as_deref(), Some("from-file"));
        assert_eq!(config.telemetry.service_name.as_deref(), Some("from-file"));
    }

    #[test]
    fn telemetry_names_apply_to_telemetry_only() {
        let (config, service) = identified(
            "service_name: notes\ntelemetry:\n  service_name: notes-eu\n  service_version: 9.9.9\n",
            "JENNY_TEST_IDENTITY_TELEMETRY",
        )
        .unwrap();
        assert_eq!(service, jenny_core::Service::new("notes", "0.1.0"));
        assert_eq!(config.telemetry.service_name.as_deref(), Some("notes-eu"));
        assert_eq!(config.telemetry.service_version.as_deref(), Some("9.9.9"));
    }

    #[test]
    fn an_empty_service_name_stops_the_service() {
        let err = identified("service_name: \"  \"\n", "JENNY_TEST_IDENTITY_EMPTY")
            .unwrap_err()
            .to_string();
        assert!(err.contains("`service_name` is empty"), "{err}");
    }

    #[test]
    fn application_sections_pass_through_untyped() {
        let file = write_yaml(
            r#"
applications:
  my-notes:
    greeting: "hi"
    retries: 3
"#,
        );
        let args = ServerArgs {
            config_path: Some(file.path().to_string_lossy().into_owned()),
            env_prefix: Some("JENNY_TEST_PASSTHROUGH".into()),
            ..Default::default()
        };
        let source = ConfigSource::load(&args).unwrap();

        #[derive(Deserialize)]
        struct MyNotesConfig {
            greeting: String,
            retries: u8,
        }

        let config: MyNotesConfig = source.section("applications.my-notes").unwrap();
        assert_eq!(config.greeting, "hi");
        assert_eq!(config.retries, 3);

        assert!(source.has_section("applications.my-notes"));
        assert!(!source.has_section("applications.unknown"));
        assert!(
            source
                .section::<MyNotesConfig>("applications.unknown")
                .is_err()
        );
    }

    #[cfg(feature = "messaging")]
    #[test]
    fn the_nats_section_takes_its_secrets_from_the_environment() {
        let file = write_yaml(
            r#"
messaging:
  nats:
    url: tls://nats:4222
    refresh_interval: 5
    tls:
      ca: /etc/nats-tls/ca.crt
      cert: /etc/nats-tls/tls.crt
      key: /etc/nats-tls/tls.key
    auth:
      user: notes
"#,
        );
        let prefix = "JENNY_TEST_NATS_SECRETS";
        // SAFETY: the variable name is unique to this test.
        unsafe {
            std::env::set_var(
                format!("{prefix}__MESSAGING__NATS__AUTH__PASSWORD"),
                "s3cret",
            )
        };
        let args = ServerArgs {
            config_path: Some(file.path().to_string_lossy().into_owned()),
            env_prefix: Some(prefix.into()),
            ..Default::default()
        };
        let config = ConfigSource::load(&args)
            .unwrap()
            .server_config(&args)
            .unwrap();
        unsafe { std::env::remove_var(format!("{prefix}__MESSAGING__NATS__AUTH__PASSWORD")) };

        let nats = &config.messaging.nats;
        assert_eq!(nats.url.as_deref(), Some("tls://nats:4222"));
        assert_eq!(nats.refresh_interval, 5);
        let tls = nats.tls.as_ref().unwrap();
        assert_eq!(
            tls.cert.as_deref(),
            Some(std::path::Path::new("/etc/nats-tls/tls.crt"))
        );
        assert_eq!(nats.auth.password.as_deref(), Some("s3cret"));
        assert!(
            !format!("{config:?}").contains("s3cret"),
            "the dump is redacted"
        );
    }

    #[cfg(feature = "messaging")]
    #[test]
    fn a_bare_tls_key_in_yaml_asks_for_tls() {
        let file = write_yaml(
            r#"
messaging:
  nats:
    url: nats://nats:4222
    tls:
      # ca: /etc/nats-tls/ca.crt
"#,
        );
        let args = ServerArgs {
            config_path: Some(file.path().to_string_lossy().into_owned()),
            env_prefix: Some("JENNY_TEST_NATS_BARE_TLS".into()),
            ..Default::default()
        };
        let config = ConfigSource::load(&args)
            .unwrap()
            .server_config(&args)
            .unwrap();
        assert!(
            config.messaging.nats.tls.is_some(),
            "{:?}",
            config.messaging.nats
        );
    }

    #[cfg(feature = "messaging")]
    #[test]
    fn an_invalid_nats_section_stops_the_service_naming_the_keys() {
        let file = write_yaml(
            r#"
messaging:
  nats:
    url: tls://nats:4222
    tls:
      cert: /etc/nats-tls/tls.crt
    auth:
      token: t
      creds: /etc/nats/service.creds
"#,
        );
        let args = ServerArgs {
            config_path: Some(file.path().to_string_lossy().into_owned()),
            env_prefix: Some("JENNY_TEST_NATS_INVALID".into()),
            ..Default::default()
        };
        let err = ConfigSource::load(&args)
            .unwrap()
            .server_config(&args)
            .unwrap_err()
            .to_string();
        for key in [
            "`messaging.nats`",
            "`tls.key`",
            "`auth.creds`",
            "`auth.token`",
        ] {
            assert!(err.contains(key), "{key} in {err}");
        }
    }
}
