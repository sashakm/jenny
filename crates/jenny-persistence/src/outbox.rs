//! The transactional outbox: events committed with the write that raised them, and a
//! relay that forwards them to the bus.

use std::future::Future;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use jenny_core::domain::{DomainError, DomainResult};
use jenny_core::port::outbound::{EventEnvelope, EventPublisher};
use serde_json::Value;
use sqlx::types::chrono::{DateTime, Utc};
use sqlx::{PgConnection, PgPool};
use tokio::sync::Notify;
use uuid::Uuid;

use crate::error::RepositoryError;

/// Where recorded events wait for the relay.
pub(crate) const SCHEMA: &str = "CREATE TABLE IF NOT EXISTS jenny_outbox (
    position       bigserial   PRIMARY KEY,
    id             uuid        NOT NULL,
    name           text        NOT NULL,
    occurred_at    timestamptz NOT NULL,
    correlation_id uuid,
    headers        jsonb       NOT NULL DEFAULT '{}',
    payload        bytea       NOT NULL,
    recorded_at    timestamptz NOT NULL DEFAULT now()
)";

/// How often the relay looks for rows another replica recorded. Rows this process
/// records wake it at once.
const POLL_INTERVAL: Duration = Duration::from_secs(1);

/// How many rows one relay transaction locks and forwards.
const BATCH: i64 = 100;

/// Shared by a [`Postgres`](crate::Postgres) handle, its stores and its relay.
#[derive(Default)]
pub(crate) struct OutboxState {
    /// Set once the outbox is enabled; where the relay forwards to.
    publisher: OnceLock<Arc<dyn EventPublisher>>,
    wake: Notify,
}

impl OutboxState {
    pub(crate) fn enable(&self, publisher: Arc<dyn EventPublisher>) -> bool {
        self.publisher.set(publisher).is_ok()
    }

    pub(crate) fn is_enabled(&self) -> bool {
        self.publisher.get().is_some()
    }

    /// Tell the relay there is something to forward.
    pub(crate) fn wake(&self) {
        self.wake.notify_one();
    }
}

/// Record `events` on `connection`, which is inside the transaction of the write that
/// raised them.
pub(crate) async fn record(
    connection: &mut PgConnection,
    events: &[EventEnvelope],
) -> DomainResult<()> {
    for event in events {
        let headers = serde_json::to_value(event.headers())
            .map_err(|e| DomainError::internal(format!("could not encode event headers: {e}")))?;
        sqlx::query(
            "INSERT INTO jenny_outbox (id, name, occurred_at, correlation_id, headers, payload) \
             VALUES ($1, $2, $3, $4, $5, $6)",
        )
        .bind(event.id())
        .bind(event.name())
        .bind(event.timestamp())
        .bind(event.correlation_id())
        .bind(headers)
        .bind(event.payload())
        .execute(&mut *connection)
        .await
        .map_err(database)?;
    }
    Ok(())
}

/// Forwards recorded events to the [`EventPublisher`] the outbox was enabled with, then
/// deletes them.
///
/// The runtime runs one per process when `persistence.postgres.outbox` is on, from
/// bind until shutdown, and drains once more after the last request finished. Several
/// replicas can run theirs at once: each batch is taken with `FOR UPDATE SKIP LOCKED`,
/// so no row is forwarded by two of them.
///
/// Delivery is **at least once**. A relay that publishes and then dies before deleting
/// the row publishes it again after a restart, so a listener that must not act twice
/// deduplicates on [`EventEnvelope::id`], which is the same on every attempt. Order is
/// kept within a relay; across replicas each takes whichever rows the others have not
/// locked.
pub struct OutboxRelay {
    pool: PgPool,
    state: Arc<OutboxState>,
}

impl OutboxRelay {
    pub(crate) fn new(pool: PgPool, state: Arc<OutboxState>) -> Self {
        Self { pool, state }
    }

    /// Forward until `stop` completes, then drain once more and return.
    pub async fn run(self, stop: impl Future<Output = ()>) {
        tokio::pin!(stop);
        loop {
            self.drain().await;
            tokio::select! {
                () = &mut stop => break,
                () = self.state.wake.notified() => {}
                () = tokio::time::sleep(POLL_INTERVAL) => {}
            }
        }
        // Requests that finished during shutdown may have committed events after the
        // last pass.
        self.drain().await;
    }

    /// Forward everything recorded so far, batch by batch. Returns how many events were
    /// published; a failure is logged and left for the next pass.
    pub async fn drain(&self) -> usize {
        let mut forwarded = 0;
        loop {
            match self.forward_batch().await {
                Ok(count) => {
                    forwarded += count;
                    if (count as i64) < BATCH {
                        return forwarded;
                    }
                }
                Err(e) => {
                    tracing::warn!("outbox relay will retry: {e}");
                    return forwarded;
                }
            }
        }
    }

    /// Lock a batch, publish it in order, delete what was published. A publish that
    /// fails stops the batch there, so the row it failed on is the first one next time.
    async fn forward_batch(&self) -> DomainResult<usize> {
        let Some(publisher) = self.state.publisher.get() else {
            return Ok(0);
        };
        let mut transaction = self.pool.begin().await.map_err(database)?;
        #[allow(clippy::type_complexity)]
        let rows: Vec<(
            i64,
            Uuid,
            String,
            DateTime<Utc>,
            Option<Uuid>,
            Value,
            Vec<u8>,
        )> = sqlx::query_as(
            "SELECT position, id, name, occurred_at, correlation_id, headers, payload \
                 FROM jenny_outbox ORDER BY position LIMIT $1 FOR UPDATE SKIP LOCKED",
        )
        .bind(BATCH)
        .fetch_all(&mut *transaction)
        .await
        .map_err(database)?;

        let mut published = Vec::with_capacity(rows.len());
        let mut failure = None;
        for (position, id, name, occurred_at, correlation_id, headers, payload) in rows {
            let mut envelope = EventEnvelope::new(name, payload)
                .with_id(id)
                .occurred_at(occurred_at);
            if let Some(correlation_id) = correlation_id {
                envelope = envelope.with_correlation_id(correlation_id);
            }
            if let Value::Object(headers) = headers {
                for (header, value) in headers {
                    if let Value::String(value) = value {
                        envelope = envelope.with_header(header, value);
                    }
                }
            }
            match publisher.publish(envelope).await {
                Ok(()) => published.push(position),
                Err(e) => {
                    failure = Some(e);
                    break;
                }
            }
        }

        if !published.is_empty() {
            sqlx::query("DELETE FROM jenny_outbox WHERE position = ANY($1)")
                .bind(&published)
                .execute(&mut *transaction)
                .await
                .map_err(database)?;
        }
        transaction.commit().await.map_err(database)?;
        match failure {
            Some(e) => Err(DomainError::internal(format!(
                "publishing a recorded event failed: {e}"
            ))),
            None => Ok(published.len()),
        }
    }
}

impl std::fmt::Debug for OutboxRelay {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OutboxRelay").finish_non_exhaustive()
    }
}

fn database(error: sqlx::Error) -> DomainError {
    RepositoryError::from(error).into()
}
