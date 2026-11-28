//! The lease port: a key held by one holder at a time, for a while, with a fencing token.
//!
//! A [`Lease`] is how replicas of a service agree that only one of them does something:
//! run one slot of a scheduled task (the scheduler does this for you once `tasks.lease`
//! is configured, ADR-029), or lead, for whatever a service needs a leader for.
//!
//! ```
//! use std::time::Duration;
//! use jenny_core::memory::MemoryLease;
//! use jenny_core::port::Lease;
//!
//! # #[tokio::main(flavor = "current_thread")] async fn main() -> jenny_core::domain::DomainResult<()> {
//! let lease = MemoryLease::new();
//! let ttl = Duration::from_secs(30);
//!
//! let held = lease.acquire("my-notes/leader", "my-notes/pod-a", ttl).await?.unwrap();
//! assert!(lease.acquire("my-notes/leader", "my-notes/pod-b", ttl).await?.is_none());
//!
//! assert!(lease.renew(&held, ttl).await?); // still ours: keep renewing well within `ttl`
//! lease.release(held).await?;
//! # Ok(()) }
//! ```
//!
//! ### Fencing
//!
//! A lease is only as good as the clocks and pauses around it: a holder that stalls for
//! longer than its `ttl` (a GC pause, a suspended VM) wakes up believing it still holds
//! a lease someone else has taken. Every acquisition therefore carries a
//! [`token`](HeldLease::token), strictly increasing across all keys of a backend. Pass
//! it to whatever the holder writes to, and have that refuse a token smaller than the
//! largest it has seen.

use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use async_trait::async_trait;

use crate::domain::DomainResult;

/// Keys held by one holder at a time, until they expire, are renewed or are released.
///
/// Every implementation keeps the same contract (`jenny_core::testing::lease` checks it):
///
/// - [`acquire`](Self::acquire) succeeds only when nobody holds an unexpired lease on
///   the key, whoever asks: a holder that asks again for a key it holds is refused too;
/// - tokens are strictly increasing across every key of the backend, so a token from a
///   later acquisition is always larger;
/// - [`renew`](Self::renew) extends a lease that is still held, and reports `false` once
///   it was lost: expired, or taken over;
/// - [`release`](Self::release) gives a lease up early, and does nothing to a lease that
///   was already lost;
/// - a backend may refuse, with an internal error, a `ttl` it cannot honour: a NATS
///   lease lasts its bucket's `max_age`, so it refuses a longer one.
///
/// Expiry is the backend's clock (the database's `now()`, the NATS server's), never a
/// replica's, except in [`MemoryLease`](crate::memory::MemoryLease), which has one
/// process only.
#[async_trait]
pub trait Lease: Send + Sync + 'static {
    /// Take `key` for `holder` for `ttl`, or `None` while someone holds it.
    async fn acquire(
        &self,
        key: &str,
        holder: &str,
        ttl: Duration,
    ) -> DomainResult<Option<HeldLease>>;

    /// Extend `lease` to `ttl` from now. `false` when it was lost in the meantime.
    async fn renew(&self, lease: &HeldLease, ttl: Duration) -> DomainResult<bool>;

    /// Give `lease` up before it expires. A lease already lost is left alone.
    async fn release(&self, lease: HeldLease) -> DomainResult<()>;
}

/// A lease that was acquired: what was taken, by whom, and its fencing token.
pub struct HeldLease {
    key: String,
    holder: String,
    token: u64,
    /// What the backend compares when renewing or releasing. Starts as the token; an
    /// adapter whose renewals write a new revision moves it on.
    revision: AtomicU64,
}

impl HeldLease {
    /// For adapters: the lease they just acquired.
    pub fn new(key: impl Into<String>, holder: impl Into<String>, token: u64) -> Self {
        Self {
            key: key.into(),
            holder: holder.into(),
            token,
            revision: AtomicU64::new(token),
        }
    }

    pub fn key(&self) -> &str {
        &self.key
    }

    pub fn holder(&self) -> &str {
        &self.holder
    }

    /// The fencing token of this acquisition. It stays the same across renewals.
    pub fn token(&self) -> u64 {
        self.token
    }

    /// For adapters: the backend's revision of the lease, for compare-and-swap.
    #[doc(hidden)]
    pub fn revision(&self) -> u64 {
        self.revision.load(Ordering::Acquire)
    }

    /// For adapters: the revision a renewal wrote.
    #[doc(hidden)]
    pub fn set_revision(&self, revision: u64) {
        self.revision.store(revision, Ordering::Release);
    }
}

impl fmt::Debug for HeldLease {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HeldLease")
            .field("key", &self.key)
            .field("holder", &self.holder)
            .field("token", &self.token)
            .field("revision", &self.revision())
            .finish()
    }
}

/// The lease a scheduled run holds, in its [`Context`](crate::port::Context).
///
/// Present only when the scheduler has a lease backend and the task is not
/// [`per_replica`](crate::task::ScheduledTask::per_replica). Pass `token` on as a
/// fencing token to what the run writes.
///
/// ```
/// # use jenny_core::port::{Context, TaskLease};
/// async fn execute(cx: &Context) -> anyhow::Result<()> {
///     if let Some(lease) = cx.get::<TaskLease>() {
///         tracing::info!(key = %lease.key, token = lease.token, "running this slot");
///     }
///     Ok(())
/// }
/// ```
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TaskLease {
    /// `<service_name>/tasks/<task name>/<slot as unix seconds>`.
    pub key: String,
    /// The fencing token of the acquisition.
    pub token: u64,
}
