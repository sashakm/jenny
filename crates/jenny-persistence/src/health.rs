//! Is the database reachable?

use async_trait::async_trait;
use jenny_core::port::outbound::HealthCheck;
use sqlx::{Pool, Postgres};

/// Readiness backed by an actual round trip to Postgres.
///
/// Checks by running `SELECT 1`, not by asking the pool whether it *thinks* it is
/// connected. A pool can hold handles to a server that has since gone away, so the only
/// answer worth reporting to a load balancer is one that came back over the wire.
///
/// Register it in the composition root, where the pool lives:
///
/// ```ignore
/// // in main.rs, where the pool is constructed
/// server.register_health_check(DatabaseCheck::new("notes-db", pool.clone()))
/// ```
///
/// The runtime cannot register this for you. It has no way to reach your pool without
/// either a dependency-injection container or a dependency on `sqlx`, and both would
/// cost more than the one line above.
pub struct DatabaseCheck {
    name: String,
    pool: Pool<Postgres>,
}

impl DatabaseCheck {
    pub fn new(name: impl Into<String>, pool: Pool<Postgres>) -> Self {
        Self {
            name: name.into(),
            pool,
        }
    }
}

#[async_trait]
impl HealthCheck for DatabaseCheck {
    fn name(&self) -> &str {
        &self.name
    }

    fn kind(&self) -> &str {
        "database"
    }

    async fn check(&self) -> Result<(), String> {
        sqlx::query("SELECT 1")
            .execute(&self.pool)
            .await
            .map(|_| ())
            .map_err(|e| e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // A lazy pool still needs a tokio context to construct.
    #[tokio::test]
    async fn it_reports_its_name_and_kind() {
        // Constructing a pool without connecting is enough to check the metadata; the
        // query path needs a live server and belongs in an integration test.
        let pool = sqlx::pool::PoolOptions::<Postgres>::new()
            .connect_lazy("postgres://localhost/does-not-matter")
            .unwrap();
        let check = DatabaseCheck::new("notes-db", pool);
        assert_eq!(check.name(), "notes-db");
        assert_eq!(check.kind(), "database");
    }

    #[tokio::test]
    async fn an_unreachable_database_reports_why() {
        let pool = sqlx::pool::PoolOptions::<Postgres>::new()
            .acquire_timeout(std::time::Duration::from_millis(150))
            .connect_lazy("postgres://127.0.0.1:1/nothing-here")
            .unwrap();
        let err = DatabaseCheck::new("notes-db", pool)
            .check()
            .await
            .unwrap_err();
        assert!(!err.is_empty(), "a failure must carry a reason");
    }
}
