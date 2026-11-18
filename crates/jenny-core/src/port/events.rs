//! Publishing domain events from a use case, in one call.

use std::sync::Arc;

use serde::Serialize;

use crate::domain::DomainEvent;

use super::context::Context;
use super::outbound::{EventEnvelope, EventPublisher};

/// What a use case holds to publish events: the [`EventPublisher`] port, plus the three
/// things every publish needs and nobody should write twice.
///
/// [`publish`](Self::publish) encodes the event as JSON, correlates it with the request
/// that raised it, and logs rather than returns a failure:
///
/// ```
/// use std::sync::Arc;
/// use jenny_core::domain::DomainEvent;
/// use jenny_core::port::{Context, Events};
/// use jenny_core::testing::RecordedEvents;
///
/// #[derive(serde::Serialize)]
/// struct NoteArchived { id: u32 }
///
/// impl DomainEvent for NoteArchived {
///     fn name(&self) -> &str { "note.archived" }
/// }
///
/// # #[tokio::main(flavor = "current_thread")] async fn main() {
/// let recorded = Arc::new(RecordedEvents::default());
/// let events = Events::new(recorded.clone());
///
/// events.publish(&NoteArchived { id: 7 }, &Context::new()).await;
/// assert_eq!(recorded.names(), ["note.archived"]);
/// # }
/// ```
///
/// Why a failure is logged and not returned: by the time a use case publishes, the
/// state change it reports has been committed. Failing the request would tell the
/// caller their change did not happen when it did. That is the dual-write problem in
/// miniature, and when a service outgrows it the answer is an outbox behind the port
/// (ADR-004) — this call does not change.
#[derive(Clone)]
pub struct Events {
    publisher: Arc<dyn EventPublisher>,
}

impl Events {
    pub fn new(publisher: Arc<dyn EventPublisher>) -> Self {
        Self { publisher }
    }

    /// Encode `event`, correlate it with `cx`, and hand it to the port.
    pub async fn publish<E>(&self, event: &E, cx: &Context)
    where
        E: DomainEvent + Serialize,
    {
        let envelope = match EventEnvelope::json(event.name(), event) {
            // Listeners can trace the event back to the request that raised it.
            Ok(envelope) => envelope.correlated_with(cx),
            Err(e) => {
                tracing::error!(
                    request_id = %cx.request_id(),
                    event = event.name(),
                    "could not encode event: {e}"
                );
                return;
            }
        };
        if let Err(e) = self.publisher.publish(envelope).await {
            tracing::error!(
                request_id = %cx.request_id(),
                event = event.name(),
                "could not publish event: {e}"
            );
        }
    }

    /// The port underneath, for the rare caller that wants the raw envelope API.
    pub fn publisher(&self) -> &Arc<dyn EventPublisher> {
        &self.publisher
    }
}

impl std::fmt::Debug for Events {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Events").finish_non_exhaustive()
    }
}

impl From<Arc<dyn EventPublisher>> for Events {
    fn from(publisher: Arc<dyn EventPublisher>) -> Self {
        Self::new(publisher)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::port::outbound::PublishError;
    use crate::testing::RecordedEvents;
    use async_trait::async_trait;

    #[derive(Serialize)]
    struct Archived {
        id: u32,
    }

    impl DomainEvent for Archived {
        fn name(&self) -> &str {
            "note.archived"
        }
    }

    #[tokio::test]
    async fn an_event_is_encoded_and_correlated() {
        let recorded = Arc::new(RecordedEvents::default());
        let events = Events::new(recorded.clone());
        let cx = Context::new();

        events.publish(&Archived { id: 7 }, &cx).await;

        let published = recorded.envelopes();
        assert_eq!(published.len(), 1);
        assert_eq!(published[0].name(), "note.archived");
        assert_eq!(published[0].correlation_id(), Some(cx.request_id()));
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(published[0].payload()).unwrap(),
            serde_json::json!({ "id": 7 })
        );
    }

    struct Broken;

    #[async_trait]
    impl EventPublisher for Broken {
        async fn publish(&self, _: EventEnvelope) -> Result<(), PublishError> {
            Err(PublishError::transport("bus is down"))
        }
    }

    #[tokio::test]
    async fn a_failed_publish_does_not_fail_the_caller() {
        // Nothing to assert beyond "returns": the state change already happened.
        Events::new(Arc::new(Broken))
            .publish(&Archived { id: 1 }, &Context::new())
            .await;
    }
}
