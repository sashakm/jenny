//! The [`Lease`] port over a Postgres table, `jenny_leases`, its tokens from a sequence.

use std::time::Duration;

use async_trait::async_trait;
use jenny_core::domain::{DomainError, DomainResult};
use jenny_core::port::{HeldLease, Lease};
use sqlx::PgPool;

use crate::error::RepositoryError;

/// The sequence first: the table's tokens come from it.
pub(crate) const SEQUENCE: &str = "CREATE SEQUENCE IF NOT EXISTS jenny_lease_tokens";
pub(crate) const TABLE: &str = "CREATE TABLE IF NOT EXISTS jenny_leases (
    key        text        PRIMARY KEY,
    holder     text        NOT NULL,
    token      bigint      NOT NULL,
    expires_at timestamptz NOT NULL
)";

/// Takes a free or expired key, sweeping what expired over a minute ago on the way so
/// the table does not keep a row per slot forever. A conflict whose `WHERE` fails
/// updates nothing and returns no row: the key is held.
const ACQUIRE: &str = "WITH swept AS (
    DELETE FROM jenny_leases WHERE expires_at < now() - interval '1 minute' AND key <> $1
)
INSERT INTO jenny_leases (key, holder, token, expires_at)
VALUES ($1, $2, nextval('jenny_lease_tokens'), now() + make_interval(secs => $3))
ON CONFLICT (key) DO UPDATE
    SET holder = EXCLUDED.holder, token = EXCLUDED.token, expires_at = EXCLUDED.expires_at
    WHERE jenny_leases.expires_at < now()
RETURNING token";

const RENEW: &str = "UPDATE jenny_leases SET expires_at = now() + make_interval(secs => $3)
    WHERE key = $1 AND token = $2 AND expires_at > now()";

const RELEASE: &str = "DELETE FROM jenny_leases WHERE key = $1 AND token = $2";

/// Leases in the `jenny_leases` table, from [`Postgres::lease`](crate::Postgres::lease).
///
/// Every time is the database's `now()`, never a replica's clock, and tokens come from
/// the sequence `jenny_lease_tokens`, so they increase across every key. Nothing ties a
/// lease to a connection: it works through any pool, PgBouncer's transaction pooling
/// included, which is why session advisory locks were not used (ADR-029).
#[derive(Clone, Debug)]
pub struct PgLease {
    pool: PgPool,
}

impl PgLease {
    pub(crate) fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

fn database(error: sqlx::Error) -> DomainError {
    RepositoryError::DatabaseError(error).into()
}

fn token(lease: &HeldLease) -> DomainResult<i64> {
    i64::try_from(lease.token())
        .map_err(|_| DomainError::internal("a Postgres lease token is a bigint"))
}

#[async_trait]
impl Lease for PgLease {
    async fn acquire(
        &self,
        key: &str,
        holder: &str,
        ttl: Duration,
    ) -> DomainResult<Option<HeldLease>> {
        let token: Option<i64> = sqlx::query_scalar(ACQUIRE)
            .bind(key)
            .bind(holder)
            .bind(ttl.as_secs_f64())
            .fetch_optional(&self.pool)
            .await
            .map_err(database)?;
        Ok(token.map(|token| HeldLease::new(key, holder, token as u64)))
    }

    async fn renew(&self, lease: &HeldLease, ttl: Duration) -> DomainResult<bool> {
        let renewed = sqlx::query(RENEW)
            .bind(lease.key())
            .bind(token(lease)?)
            .bind(ttl.as_secs_f64())
            .execute(&self.pool)
            .await
            .map_err(database)?;
        Ok(renewed.rows_affected() == 1)
    }

    async fn release(&self, lease: HeldLease) -> DomainResult<()> {
        sqlx::query(RELEASE)
            .bind(lease.key())
            .bind(token(&lease)?)
            .execute(&self.pool)
            .await
            .map_err(database)?;
        Ok(())
    }
}
