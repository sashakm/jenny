//! A Postgres connection pool, configured from `persistence.postgres`, and the schema
//! work that has to happen before a service serves.

use std::fmt;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Context as _;
use jenny_core::domain::Aggregate;
use serde::Deserialize;
use sqlx::migrate::Migrator;
use sqlx::postgres::PgPoolOptions;
use sqlx::{AssertSqlSafe, PgPool};

use crate::documents::PgDocuments;
use crate::health::DatabaseCheck;

/// The `persistence.postgres` configuration section.
///
/// ```yaml
/// persistence:
///   postgres:
///     url: postgres://notes:secret@localhost:5432/notes   # or JENNY__PERSISTENCE__POSTGRES__URL
///     max_connections: 10
///     min_connections: 0
///     acquire_timeout_seconds: 5
///     create_tables: true
/// ```
///
/// Without a `url` there is no database, and a service built with the fallback the
/// scaffold generates keeps its data in memory instead. Everything else has a default.
#[derive(Clone, Deserialize)]
#[serde(default)]
pub struct PostgresConfig {
    /// Where to connect. Carries the password, so it is never logged; set it through
    /// the environment rather than a committed file.
    pub url: Option<String>,
    /// The most connections the pool opens. Defaults to 10.
    pub max_connections: u32,
    /// Connections kept open while idle. Defaults to 0.
    pub min_connections: u32,
    /// How long a query waits for a free connection before failing. Defaults to 5.
    pub acquire_timeout_seconds: u64,
    /// Create the tables document stores need, if they are missing, before serving.
    /// Defaults to `true`; turn it off where the schema is owned by migrations alone,
    /// and create them from [`PgDocuments::schema`].
    pub create_tables: bool,
}

impl Default for PostgresConfig {
    fn default() -> Self {
        Self {
            url: None,
            max_connections: 10,
            min_connections: 0,
            acquire_timeout_seconds: 5,
            create_tables: true,
        }
    }
}

impl fmt::Debug for PostgresConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PostgresConfig")
            .field("url", &self.url.as_ref().map(|_| "<redacted>"))
            .field("max_connections", &self.max_connections)
            .field("min_connections", &self.min_connections)
            .field("acquire_timeout_seconds", &self.acquire_timeout_seconds)
            .field("create_tables", &self.create_tables)
            .finish()
    }
}

/// A Postgres pool, and what has to exist in the database before a service serves.
///
/// Cheap to clone. The pool connects lazily, so building one during wiring needs no
/// database yet; [`prepare`](Self::prepare) is where it is first used, and the runtime
/// calls that before binding its socket, so a database that is down or a migration
/// that fails stops the service at startup rather than on its first request.
///
/// ```no_run
/// # use jenny_persistence::{Postgres, PostgresConfig, ValueKind};
/// # #[derive(serde::Serialize, serde::Deserialize)] struct Note { id: u32 }
/// # impl jenny_core::domain::Aggregate for Note { type Id = u32; const KIND: &'static str = "note"; fn id(&self) -> u32 { self.id } }
/// # async fn wire(config: PostgresConfig) -> anyhow::Result<()> {
/// let postgres = Postgres::connect_lazy(&config)?;
/// let notes = postgres.documents::<Note>().queryable([("title", ValueKind::Text)]);
/// postgres.prepare().await?;   // the runtime does this for you before serving
/// # Ok(()) }
/// ```
#[derive(Clone)]
pub struct Postgres {
    inner: Arc<Inner>,
}

struct Inner {
    pool: PgPool,
    create_tables: bool,
    /// `CREATE TABLE IF NOT EXISTS` for every document store handed out.
    tables: Mutex<Vec<String>>,
    migrator: Mutex<Option<Arc<Migrator>>>,
}

impl Postgres {
    /// A pool for `config.url`, not connected yet. Fails when there is no URL or it
    /// does not parse.
    pub fn connect_lazy(config: &PostgresConfig) -> anyhow::Result<Self> {
        let url = config
            .url
            .as_deref()
            .context("persistence.postgres.url is not set")?;
        let pool = PgPoolOptions::new()
            .max_connections(config.max_connections)
            .min_connections(config.min_connections)
            .acquire_timeout(Duration::from_secs(config.acquire_timeout_seconds))
            .connect_lazy(url)
            // The error names the problem, never the URL, which carries the password.
            .context("persistence.postgres.url is not a valid Postgres URL")?;
        Ok(Self::with_pool(pool, config.create_tables))
    }

    /// Wrap a pool built elsewhere — in a test, or by a service with its own options.
    pub fn from_pool(pool: PgPool) -> Self {
        Self::with_pool(pool, true)
    }

    fn with_pool(pool: PgPool, create_tables: bool) -> Self {
        Self {
            inner: Arc::new(Inner {
                pool,
                create_tables,
                tables: Mutex::new(Vec::new()),
                migrator: Mutex::new(None),
            }),
        }
    }

    pub fn pool(&self) -> &PgPool {
        &self.inner.pool
    }

    /// A document store for `A`, in a table named after [`Aggregate::KIND`].
    ///
    /// Panics when the kind is not a plain lower-case SQL identifier: it is a constant
    /// in your code, so that is a mistake to fix there, not a runtime condition.
    pub fn documents<A: Aggregate>(&self) -> PgDocuments<A> {
        self.documents_in(A::KIND)
    }

    /// [`documents`](Self::documents) in a table of your choosing.
    pub fn documents_in<A: Aggregate>(&self, table: &str) -> PgDocuments<A> {
        let store = PgDocuments::new(self.inner.pool.clone(), table);
        self.inner
            .tables
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(store.schema());
        store
    }

    /// Run these migrations in [`prepare`](Self::prepare), before any table is created.
    ///
    /// Pass `sqlx::migrate!()`, which embeds the `migrations/` directory of the crate
    /// it is written in, so the binary carries its schema with it:
    ///
    /// ```ignore
    /// postgres.migrate(sqlx::migrate!());
    /// ```
    pub fn migrate(&self, migrator: Migrator) -> &Self {
        *self
            .inner
            .migrator
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(Arc::new(migrator));
        self
    }

    /// Run the migrations, then create the document tables, once per process start.
    ///
    /// Safe to run from several replicas at once: sqlx's migrator takes its own lock,
    /// and table creation runs in one transaction under an advisory lock, because two
    /// concurrent `CREATE TABLE IF NOT EXISTS` can still collide on the type catalogue.
    pub async fn prepare(&self) -> anyhow::Result<()> {
        let migrator = self
            .inner
            .migrator
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        if let Some(migrator) = migrator {
            migrator
                .run(&self.inner.pool)
                .await
                .context("running database migrations")?;
        }

        if !self.inner.create_tables {
            return Ok(());
        }
        let tables = self
            .inner
            .tables
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        if tables.is_empty() {
            return Ok(());
        }
        let mut transaction = self
            .inner
            .pool
            .begin()
            .await
            .context("connecting to Postgres")?;
        sqlx::query("SELECT pg_advisory_xact_lock(hashtext('jenny.prepare'))")
            .execute(&mut *transaction)
            .await
            .context("taking the schema lock")?;
        for statement in tables {
            // Identifiers in it were validated and quoted by `PgDocuments::new`.
            sqlx::query(AssertSqlSafe(statement))
                .execute(&mut *transaction)
                .await
                .context("creating a document table")?;
        }
        transaction
            .commit()
            .await
            .context("committing the document tables")?;
        Ok(())
    }

    /// Readiness for this pool: a `SELECT 1` round trip.
    pub fn health_check(&self) -> DatabaseCheck {
        DatabaseCheck::new("postgres", self.inner.pool.clone())
    }

    /// Close the pool, waiting for checked-out connections to come back.
    pub async fn close(&self) {
        self.inner.pool.close().await;
    }
}

impl fmt::Debug for Postgres {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Postgres")
            .field("pool_size", &self.inner.pool.size())
            .field("create_tables", &self.inner.create_tables)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_url_never_reaches_a_log() {
        let config = PostgresConfig {
            url: Some("postgres://notes:hunter2@db/notes".into()),
            ..Default::default()
        };
        let debug = format!("{config:?}");
        assert!(!debug.contains("hunter2"), "{debug}");
        assert!(debug.contains("<redacted>"));
    }

    #[tokio::test]
    async fn without_a_url_there_is_no_pool() {
        let err = Postgres::connect_lazy(&PostgresConfig::default()).unwrap_err();
        assert!(
            err.to_string().contains("persistence.postgres.url"),
            "{err}"
        );
    }

    #[tokio::test]
    async fn a_bad_url_is_refused_without_repeating_it() {
        let err = Postgres::connect_lazy(&PostgresConfig {
            url: Some("not a url with a s3cret".into()),
            ..Default::default()
        })
        .unwrap_err();
        assert!(!format!("{err:#}").contains("s3cret"), "{err:#}");
    }

    #[tokio::test]
    async fn a_lazy_pool_needs_no_database_to_build() {
        let postgres = Postgres::connect_lazy(&PostgresConfig {
            url: Some("postgres://nobody@127.0.0.1:1/none".into()),
            ..Default::default()
        })
        .unwrap();
        assert_eq!(postgres.pool().size(), 0);
    }
}
