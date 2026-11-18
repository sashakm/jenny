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
