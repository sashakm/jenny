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
//! The framework reads the sections it owns — `server`, `telemetry`, and `messaging`
//! when that feature is on. Everything else passes through untyped, so it imposes no schema on your configuration:
//! an adapter deserialises its own `applications.<name>` section into whatever type
//! it likes, via [`ServerContext::app_section`](crate::ServerContext::app_section).
//!
//! ```yaml
//! server:
//!   address: "[::]:8000"
//!   api_path: /api        # where REST adapters mount
//!   framework_path: /j    # where health, metrics and OpenAPI live
//!   shutdown_delay_seconds: 5     # keep serving, reported not ready, before closing
//!   shutdown_timeout_seconds: 30  # then wait at most this long for in-flight requests
//!   metrics:
//!     auth:               # optional; omit to leave the scrape endpoint open
//!       username: prometheus
//!       password: s3cret
//! telemetry:
//!   service_name: my-service
//!   service_version: "1.2.3"
//!   log_format: pretty
//! messaging:
//!   buffer_size: 64
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
/// Topic prefix used when neither `messaging.namespace` nor `telemetry.service_name`
/// is set. Topics may not contain empty segments, so there has to be something.
pub const DEFAULT_EVENT_NAMESPACE: &str = "jenny";

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
    /// Where the framework's own endpoints live: health, metrics, OpenAPI.
    ///
    /// Namespaced away from `api_path` so the framework can never collide with a route
    /// your service wanted. `/j` is short because it appears in every probe URL.
    pub framework_path: String,
    /// Whether to expose `<framework_path>/health/*`.
    pub health: bool,
    /// The metrics scrape endpoint.
    pub metrics: MetricsConfig,
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
            health: true,
            metrics: MetricsConfig::default(),
            shutdown_delay_seconds: 0,
            shutdown_timeout_seconds: DEFAULT_SHUTDOWN_TIMEOUT_SECONDS,
        }
    }
}

/// The `server.metrics` section.
#[derive(Clone, Debug, Deserialize)]
#[serde(default)]
pub struct MetricsConfig {
    /// Whether to expose `<framework_path>/metrics` at all.
    pub enabled: bool,
    /// Optional HTTP Basic credentials the scraper must present.
    ///
    /// Left unset the endpoint is open, which is usually right when it is only
    /// reachable from inside the cluster and wrong the moment it is not. Metrics leak
    /// more than people expect: route names, tenant counts, error rates, deploy times.
    pub auth: Option<BasicAuthConfig>,
}

impl Default for MetricsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            auth: None,
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
    /// Prefix for every event topic. Defaults to `telemetry.service_name`.
    pub namespace: Option<String>,
}

impl Default for MessagingConfig {
    fn default() -> Self {
        Self {
            buffer_size: DEFAULT_EVENT_BUFFER,
            namespace: None,
        }
    }
}

/// The part of the configuration tree the framework itself interprets.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct ServerConfig {
    pub server: HttpConfig,
    pub telemetry: TelemetryConfig,
    pub messaging: MessagingConfig,
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
    pub fn server_config(&self, args: &ServerArgs) -> anyhow::Result<ServerConfig> {
        let mut config: ServerConfig = self.0.clone().try_deserialize()?;
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
        assert!(config.server.health);
        assert!(!config.telemetry.debug);
    }

    #[test]
    fn each_layer_overrides_the_one_before() {
        let file = write_yaml(
            r#"
server:
  address: "127.0.0.1:7000"
  body_limit: 4096
telemetry:
  service_name: from-file
applications:
  my-notes:
    greeting: "hi"
"#,
        );
        let prefix = "JENNY_TEST_LAYERING";
        // SAFETY: the variable name is unique to this test.
        unsafe { std::env::set_var(format!("{prefix}__TELEMETRY__SERVICE_NAME"), "from-env") };

        let args = ServerArgs {
            address: Some("127.0.0.1:9000".into()),
            debug: true,
            config_path: Some(file.path().to_string_lossy().into_owned()),
            env_prefix: Some(prefix.into()),
        };
        let source = ConfigSource::load(&args).unwrap();
        let config = source.server_config(&args).unwrap();

        assert_eq!(config.telemetry.service_name, "from-env", "env beats file");
        assert_eq!(config.server.address, "127.0.0.1:9000", "cli beats file");
        assert_eq!(config.server.body_limit, 4096, "file beats the default");
        assert!(config.telemetry.debug, "--debug is applied");

        unsafe { std::env::remove_var(format!("{prefix}__TELEMETRY__SERVICE_NAME")) };
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
}
