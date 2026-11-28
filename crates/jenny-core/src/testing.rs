//! Test doubles for the ports jenny ships.
//!
//! Owning a port is what makes a double this small: implement the trait, record what
//! arrives. These are the ones every service would otherwise write for itself. For
//! storage, [`Memory`](crate::memory::Memory) is already the double, and
//! [`MemoryBlobs`](crate::memory::MemoryBlobs) for files.

use std::sync::Mutex;

use async_trait::async_trait;

use crate::port::outbound::{EventEnvelope, EventPublisher, PublishError};

/// An [`EventPublisher`] that keeps every envelope it is given.
///
/// ```
/// use std::sync::Arc;
/// use jenny_core::port::{Context, Events};
/// use jenny_core::testing::RecordedEvents;
/// # #[derive(serde::Serialize)] struct NoteCreated;
/// # impl jenny_core::domain::DomainEvent for NoteCreated { fn name(&self) -> &str { "note.created" } }
///
/// # #[tokio::main(flavor = "current_thread")] async fn main() {
/// let recorded = Arc::new(RecordedEvents::default());
/// let events = Events::new(recorded.clone());   // what the use case under test gets
///
/// events.publish(&NoteCreated, &Context::new()).await;
/// assert_eq!(recorded.names(), ["note.created"]);
/// # }
/// ```
#[derive(Debug, Default)]
pub struct RecordedEvents(Mutex<Vec<EventEnvelope>>);

impl RecordedEvents {
    /// The name of every event published so far, in order.
    pub fn names(&self) -> Vec<String> {
        self.lock().iter().map(|e| e.name().to_string()).collect()
    }

    /// Every envelope published so far, in order.
    pub fn envelopes(&self) -> Vec<EventEnvelope> {
        self.lock().clone()
    }

    /// The payload of every event with this name, decoded as JSON.
    pub fn payloads(&self, name: &str) -> Vec<serde_json::Value> {
        self.lock()
            .iter()
            .filter(|e| e.name() == name)
            .map(|e| serde_json::from_slice(e.payload()).unwrap_or(serde_json::Value::Null))
            .collect()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<EventEnvelope>> {
        // A test that panicked mid-publish has failed already; keep reporting.
        self.0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

#[async_trait]
impl EventPublisher for RecordedEvents {
    async fn publish(&self, event: EventEnvelope) -> Result<(), PublishError> {
        self.lock().push(event);
        Ok(())
    }
}

/// The contract every [`Lease`](crate::port::Lease) keeps, as one function each adapter's
/// tests call against its backend. Not API: it panics on the first broken rule.
#[doc(hidden)]
pub mod lease {
    use std::time::Duration;

    use crate::port::Lease;

    /// Check `lease` with leases of `ttl`, waiting `ttl + settle` for one to expire.
    /// Every key starts with `prefix`, so runs against a shared backend do not meet.
    ///
    /// `settle` covers the backend's expiry granularity; with `tokio::time::pause` and
    /// [`MemoryLease`](crate::memory::MemoryLease), zero will do.
    pub async fn conformance(lease: &dyn Lease, ttl: Duration, settle: Duration, prefix: &str) {
        let a = format!("{prefix}/a");
        let b = format!("{prefix}/b");
        let c = format!("{prefix}/c weird key: *>.");

        // Acquire, and contention, from someone else and from the holder itself.
        let held_a = lease
            .acquire(&a, "one", ttl)
            .await
            .unwrap()
            .expect("a free key is acquired");
        assert_eq!(held_a.key(), a);
        assert_eq!(held_a.holder(), "one");
        assert!(lease.acquire(&a, "two", ttl).await.unwrap().is_none());
        assert!(
            lease.acquire(&a, "one", ttl).await.unwrap().is_none(),
            "a held key is refused to its own holder too"
        );

        // Tokens increase across keys, and stay put across a renewal.
        let held_b = lease.acquire(&b, "one", ttl).await.unwrap().unwrap();
        assert!(
            held_b.token() > held_a.token(),
            "tokens increase across keys"
        );
        let token_a = held_a.token();
        assert!(
            lease.renew(&held_a, ttl).await.unwrap(),
            "a held lease renews"
        );
        assert_eq!(held_a.token(), token_a);

        // Release frees the key at once, for a larger token.
        let token_b = held_b.token();
        lease.release(held_b).await.unwrap();
        let again_b = lease
            .acquire(&b, "two", ttl)
            .await
            .unwrap()
            .expect("a released key is free");
        assert!(again_b.token() > token_b);

        // Any key the backend is given works, whatever characters it has.
        let held_c = lease
            .acquire(&c, "one", ttl)
            .await
            .unwrap()
            .expect("keys are free strings");

        // Expiry: a takeover with a larger token, and the old holder told it lost.
        tokio::time::sleep(ttl + settle).await;
        let taken = lease
            .acquire(&a, "two", ttl)
            .await
            .unwrap()
            .expect("an expired lease can be taken over");
        assert!(taken.token() > again_b.token());
        assert!(
            !lease.renew(&held_a, ttl).await.unwrap(),
            "a lease taken over does not renew"
        );
        assert!(
            !lease.renew(&held_c, ttl).await.unwrap(),
            "an expired lease does not renew, even untaken"
        );

        // Releasing a lost lease leaves the new holder alone.
        lease.release(held_a).await.unwrap();
        assert!(
            lease.acquire(&a, "three", ttl).await.unwrap().is_none(),
            "the new holder still holds it"
        );
        assert!(lease.renew(&taken, ttl).await.unwrap());

        // Leave nothing behind.
        lease.release(taken).await.unwrap();
        lease.release(again_b).await.unwrap();
        lease.release(held_c).await.unwrap();
    }
}
