//! The [`Lease`] port over a NATS JetStream key-value bucket.

use std::fmt::{self, Write as _};
use std::sync::Arc;
use std::time::Duration;

use async_nats::jetstream::{self, kv};
use async_trait::async_trait;
use jenny_core::domain::{DomainError, DomainResult};
use jenny_core::port::{HeldLease, Lease};
use tokio::sync::OnceCell;

use crate::kv::Inner;

/// The bucket the runtime's leases live in.
pub const LEASE_BUCKET: &str = "jenny_leases";

/// Leases in a JetStream key-value bucket, from [`Nats::lease`](crate::kv::Nats::lease).
///
/// The bucket keeps one revision per key and expires entries after its `max_age`, which
/// is the lease's `ttl`: [`acquire`](Lease::acquire) is a `create`, which fails while the
/// key exists, [`renew`](Lease::renew) an `update` on the revision held (a new write, so
/// the age starts again), and [`release`](Lease::release) a delete on that revision.
/// Expiry is the NATS server's clock. Tokens are the revision of the acquiring write, the
/// sequence of the bucket's stream, so they increase across every key; a renewal moves
/// the revision on but not the token.
///
/// A lease lasts the bucket's `max_age`, whatever `ttl` a call passes: a longer `ttl` is
/// refused, because the lease would expire before its holder expects. The bucket is
/// created when missing and its `max_age` set to this lease's `ttl` when it differs, so
/// services sharing a NATS account and this bucket must agree on `tasks.lease.ttl`.
///
/// Keys are free strings: letters, digits, `-`, `_` and `/` are stored as they are, and
/// every other byte as `=XX`, which no stored character can be mistaken for.
#[derive(Clone)]
pub struct NatsLease {
    pub(crate) nats: Arc<Inner>,
    pub(crate) bucket: String,
    pub(crate) ttl: Duration,
    pub(crate) store: Arc<OnceCell<kv::Store>>,
}

impl fmt::Debug for NatsLease {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("NatsLease")
            .field("bucket", &self.bucket)
            .field("ttl", &self.ttl)
            .finish_non_exhaustive()
    }
}

impl NatsLease {
    /// The bucket's name.
    pub fn bucket(&self) -> &str {
        &self.bucket
    }

    /// How long a lease lasts: the bucket's `max_age`.
    pub fn ttl(&self) -> Duration {
        self.ttl
    }

    /// Open the bucket, creating it or setting its `max_age` as needed.
    pub(crate) async fn store(&self) -> DomainResult<&kv::Store> {
        self.store
            .get_or_try_init(|| async {
                let client = self
                    .nats
                    .client()
                    .await
                    .map_err(|e| DomainError::internal(e.to_string()))?;
                let jetstream = jetstream::new(client.clone());
                let config = kv::Config {
                    bucket: self.bucket.clone(),
                    history: 1,
                    max_age: self.ttl,
                    ..Default::default()
                };
                let failed = |what: &str, e: &dyn fmt::Display| {
                    DomainError::internal(format!("{what} NATS bucket `{}`: {e}", self.bucket))
                };
                // The bucket's stream as it is, so only `max_age` is changed and whatever an
                // operator set (replicas, storage, placement) stays.
                match jetstream.get_stream(format!("KV_{}", self.bucket)).await {
                    Ok(stream) => {
                        let mut existing = stream.cached_info().config.clone();
                        if existing.max_age != self.ttl {
                            tracing::info!(
                                bucket = self.bucket,
                                from = ?existing.max_age,
                                to = ?self.ttl,
                                "setting the lease bucket's max_age to the lease ttl"
                            );
                            existing.max_age = self.ttl;
                            // JetStream refuses a duplicate window longer than max_age, and
                            // a bucket made for a longer ttl has one.
                            existing.duplicate_window = existing.duplicate_window.min(self.ttl);
                            jetstream
                                .update_stream(&existing)
                                .await
                                .map_err(|e| failed("updating", &e))?;
                        }
                        jetstream
                            .get_key_value(&self.bucket)
                            .await
                            .map_err(|e| failed("opening", &e))
                    }
                    Err(e) if is_not_found(&e) => jetstream
                        .create_key_value(config)
                        .await
                        .map_err(|e| failed("creating", &e)),
                    // Permissions, timeouts: reported as they are, not as a missing bucket.
                    Err(e) => Err(failed("opening", &e)),
                }
            })
            .await
    }

    fn check(&self, ttl: Duration) -> DomainResult<()> {
        if ttl > self.ttl {
            return Err(DomainError::internal(format!(
                "a NATS lease lasts its bucket's max_age, {:?}; {ttl:?} was asked for",
                self.ttl
            )));
        }
        Ok(())
    }
}

/// Whether JetStream said the stream does not exist, rather than failing to say.
fn is_not_found(error: &jetstream::context::GetStreamError) -> bool {
    matches!(
        error.kind(),
        jetstream::context::GetStreamErrorKind::JetStream(e)
            if e.error_code() == jetstream::ErrorCode::STREAM_NOT_FOUND
    )
}

/// `key` as a NATS key: unreserved characters as they are, every other byte `=XX`.
pub(crate) fn encode_key(key: &str) -> String {
    let mut encoded = String::with_capacity(key.len());
    for byte in key.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'/' => {
                encoded.push(byte as char)
            }
            other => {
                let _ = write!(encoded, "={other:02X}");
            }
        }
    }
    encoded
}

fn nats(what: &str, error: impl fmt::Display) -> DomainError {
    DomainError::internal(format!("{what} a NATS lease: {error}"))
}

#[async_trait]
impl Lease for NatsLease {
    async fn acquire(
        &self,
        key: &str,
        holder: &str,
        ttl: Duration,
    ) -> DomainResult<Option<HeldLease>> {
        self.check(ttl)?;
        if key.is_empty() {
            return Err(DomainError::internal("a lease key is never empty"));
        }
        let store = self.store().await?;
        match store
            .create(encode_key(key), holder.to_string().into())
            .await
        {
            Ok(revision) => Ok(Some(HeldLease::new(key, holder, revision))),
            Err(e) if e.kind() == kv::CreateErrorKind::AlreadyExists => Ok(None),
            Err(e) => Err(nats("acquiring", e)),
        }
    }

    async fn renew(&self, lease: &HeldLease, ttl: Duration) -> DomainResult<bool> {
        self.check(ttl)?;
        let store = self.store().await?;
        match store
            .update(
                encode_key(lease.key()),
                lease.holder().to_string().into(),
                lease.revision(),
            )
            .await
        {
            Ok(revision) => {
                lease.set_revision(revision);
                Ok(true)
            }
            Err(e) if e.kind() == kv::UpdateErrorKind::WrongLastRevision => Ok(false),
            Err(e) => Err(nats("renewing", e)),
        }
    }

    async fn release(&self, lease: HeldLease) -> DomainResult<()> {
        let store = self.store().await?;
        let key = encode_key(lease.key());
        let Err(error) = store
            .delete_expect_revision(&key, Some(lease.revision()))
            .await
        else {
            return Ok(());
        };
        // A wrong revision is a lease already lost, which release leaves alone.
        match store.entry(&key).await {
            Ok(Some(entry))
                if entry.revision == lease.revision() && entry.operation == kv::Operation::Put =>
            {
                Err(nats("releasing", error))
            }
            Ok(_) => Ok(()),
            Err(e) => Err(nats("releasing", e)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_keys_stay_readable() {
        assert_eq!(
            encode_key("my-notes/tasks/tidy_up/1767225600"),
            "my-notes/tasks/tidy_up/1767225600"
        );
    }

    #[test]
    fn everything_else_is_escaped_without_collisions() {
        assert_eq!(encode_key("a.b c"), "a=2Eb=20c");
        assert_eq!(encode_key("*>"), "=2A=3E");
        assert_eq!(encode_key("ü"), "=C3=BC");
        // `=` itself is escaped, so an escape never collides with a literal.
        assert_eq!(encode_key("=2E"), "=3D2E");
        assert_ne!(encode_key("=2E"), encode_key("."));
        assert_eq!(encode_key(".leading."), "=2Eleading=2E");
    }
}
