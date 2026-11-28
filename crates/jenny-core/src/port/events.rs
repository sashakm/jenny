//! Publishing domain events from a use case, in one call.

use std::sync::Arc;

use serde::Serialize;

use crate::domain::DomainEvent;

use super::context::Context;
use super::headers::{Enrich, Headers};
use super::outbound::{EventEnvelope, EventPublisher, PublishError};

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
/// miniature. For events that report a store write, hand them to the write instead —
/// [`StoreExt::change`](super::StoreExt::change)`(…).raising(&events, cx)` — and a store
/// with an outbox records them in the same transaction (ADR-016).
///
/// ### Enrichers
///
/// [`enriched_with`](Self::enriched_with) adds an [`Enrich`]er, which writes headers from
/// the [`Context`] into every event this encodes, in the order they were added: in
/// [`publish`](Self::publish), in `change(…).raising(…)` (with `announcing_changes`) and
/// in `add(…).raising(…)`, before the store writes, so an outbox keeps the enriched
/// envelope. [`deliver`](Self::deliver) hands envelopes on as they are. Under the runtime,
/// `JennyServer::events()` returns an `Events` already carrying what
/// `messaging.events.headers` turns on.
#[derive(Clone)]
pub struct Events {
    publisher: Arc<dyn EventPublisher>,
    enrichers: Vec<Arc<dyn Enrich>>,
}

impl Events {
    pub fn new(publisher: Arc<dyn EventPublisher>) -> Self {
        Self {
            publisher,
            enrichers: Vec::new(),
        }
    }

    /// The same, with `enricher` running after the ones already added.
    pub fn enriched_with(mut self, enricher: impl Enrich) -> Self {
        self.enrichers.push(Arc::new(enricher));
        self
    }

    /// Run every enricher over the headers of `envelopes`, which see nothing else.
    pub(crate) fn enrich(&self, envelopes: &mut [EventEnvelope], cx: &Context) {
        if self.enrichers.is_empty() {
            return;
        }
        for envelope in envelopes {
            let mut headers = Headers::from(std::mem::take(envelope.headers_mut()));
            for enricher in &self.enrichers {
                enricher.enrich(cx, &mut headers);
            }
            *envelope.headers_mut() = headers.into();
        }
    }

    /// Encode `event`, correlate it with `cx`, and hand it to the port.
    pub async fn publish<E>(&self, event: &E, cx: &Context)
    where
        E: DomainEvent + Serialize,
    {
        let mut envelope = match EventEnvelope::json(event.name(), event) {
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
        self.enrich(std::slice::from_mut(&mut envelope), cx);
        self.deliver(vec![envelope], cx).await;
    }

    /// Hand already-built envelopes to the port, logging any that fail. They are not
    /// enriched: a store returns what it did not send, already enriched when it was
    /// encoded.
    pub async fn deliver(&self, envelopes: Vec<EventEnvelope>, cx: &Context) {
        for envelope in envelopes {
            let name = envelope.name().to_string();
            if let Err(e) = self.publisher.publish(envelope).await {
                tracing::error!(
                    request_id = %cx.request_id(),
                    event = name,
                    "could not publish event: {e}"
                );
            }
        }
    }

    /// The port underneath, for the rare caller that wants the raw envelope API.
    pub fn publisher(&self) -> &Arc<dyn EventPublisher> {
        &self.publisher
    }
}

/// What a change can raise: one event, an optional one, several, or nothing.
///
/// Implemented for every `DomainEvent + Serialize`, and for `Option`, `Vec` and `()` of
/// them, so a domain method returns whatever it naturally raises and
/// [`raising`](super::store::Change::raising) publishes it.
pub trait Raised: Send {
    /// The events, encoded and correlated with `cx`, in order.
    fn envelopes(&self, cx: &Context) -> Result<Vec<EventEnvelope>, PublishError>;
}

impl<E: DomainEvent + Serialize> Raised for E {
    fn envelopes(&self, cx: &Context) -> Result<Vec<EventEnvelope>, PublishError> {
        Ok(vec![
            EventEnvelope::json(self.name(), self)?.correlated_with(cx),
        ])
    }
}

impl<E: DomainEvent + Serialize> Raised for Option<E> {
    fn envelopes(&self, cx: &Context) -> Result<Vec<EventEnvelope>, PublishError> {
        self.as_ref()
            .map_or(Ok(Vec::new()), |event| event.envelopes(cx))
    }
}

impl<E: DomainEvent + Serialize> Raised for Vec<E> {
    fn envelopes(&self, cx: &Context) -> Result<Vec<EventEnvelope>, PublishError> {
        let mut all = Vec::with_capacity(self.len());
        for event in self {
            all.extend(event.envelopes(cx)?);
        }
        Ok(all)
    }
}

impl Raised for () {
    fn envelopes(&self, _: &Context) -> Result<Vec<EventEnvelope>, PublishError> {
        Ok(Vec::new())
    }
}

impl std::fmt::Debug for Events {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Events")
            .field("enrichers", &self.enrichers.len())
            .finish_non_exhaustive()
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
