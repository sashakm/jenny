//! What adapters get handed while they are being wired up.

use jenny_core::port::Extensions;
use serde::de::DeserializeOwned;

use crate::config::ConfigSource;

/// Configuration and shared resources, available during wiring only.
///
/// While `main` constructs adapters and use cases, each one reads its own
/// configuration section and picks up shared resources — a connection pool, an HTTP
/// client, a store — that were [`provide`](Self::provide)d earlier. Once wiring is
/// done the context has no further role: use cases own their `Arc`s, and nothing looks
/// anything up at runtime.
///
/// That is the difference between this and a DI container, and it is the point. A
/// missing dependency is a compile error or a startup failure in one readable
/// function, not a runtime surprise on the first request that needs it.
///
/// ```no_run
/// # use jenny_server::JennyServer;
/// # use std::sync::Arc;
/// # #[derive(serde::Deserialize)] struct MyConfig { greeting: String }
/// # #[derive(Clone)] struct Pool;
/// # fn connect() -> Pool { Pool }
/// # fn main() -> anyhow::Result<()> {
/// let mut server = JennyServer::from_args()?;
///
/// let config: MyConfig = server.context().app_section("my-notes")?;
/// let pool = Arc::new(connect());
/// server.context_mut().provide(pool.clone());   // for the next adapter along
/// # Ok(())
/// # }
/// ```
pub struct ServerContext {
    config: ConfigSource,
    extensions: Extensions,
}

impl ServerContext {
    pub(crate) fn new(config: ConfigSource) -> Self {
        Self {
            config,
            extensions: Extensions::new(),
        }
    }

    /// The whole merged configuration tree.
    pub fn config(&self) -> &ConfigSource {
        &self.config
    }

    /// Deserialise the `applications.<name>` section for an adapter.
    pub fn app_section<T: DeserializeOwned>(&self, name: &str) -> anyhow::Result<T> {
        self.config.section(&format!("applications.{name}"))
    }

    /// Share a value with adapters wired after this point. Returns the previous value
    /// of the same type, if there was one.
    pub fn provide<T: Send + Sync + 'static>(&mut self, value: T) -> Option<T> {
        self.extensions.insert(value)
    }

    /// Look up a shared value by type.
    pub fn get<T: Send + Sync + 'static>(&self) -> Option<&T> {
        self.extensions.get()
    }

    /// Look up a shared value by type, failing with a message that names the type
    /// rather than a bare `None`.
    pub fn expect<T: Send + Sync + 'static>(&self) -> anyhow::Result<&T> {
        self.get::<T>().ok_or_else(|| {
            anyhow::anyhow!(
                "no value of type `{}` was provided to the server context",
                std::any::type_name::<T>()
            )
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ServerArgs;

    fn context() -> ServerContext {
        let args = ServerArgs {
            env_prefix: Some("JENNY_TEST_CTX".into()),
            ..Default::default()
        };
        ServerContext::new(ConfigSource::load(&args).unwrap())
    }

    #[test]
    fn shared_values_round_trip_by_type() {
        #[derive(Debug, PartialEq)]
        struct Shared(u32);

        let mut ctx = context();
        assert!(ctx.get::<Shared>().is_none());

        ctx.provide(Shared(7));
        assert_eq!(ctx.get::<Shared>(), Some(&Shared(7)));
        assert_eq!(ctx.expect::<Shared>().unwrap(), &Shared(7));

        assert_eq!(ctx.provide(Shared(8)), Some(Shared(7)));
        assert_eq!(ctx.get::<Shared>(), Some(&Shared(8)));
    }

    #[test]
    fn a_missing_value_names_the_type_it_wanted() {
        #[derive(Debug)]
        struct NeverProvided;

        let err = context().expect::<NeverProvided>().unwrap_err();
        assert!(err.to_string().contains("NeverProvided"), "{err}");
    }
}
