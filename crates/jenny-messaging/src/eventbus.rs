//! The in-process event bus, behind the [`EventPublisher`] port.

use async_trait::async_trait;
use tokio::sync::mpsc;
use topmesys::{EventEmitter, EventMessage, EventSubmission};

use jenny_core::port::outbound::{EventEnvelope, EventPublisher, PublishError};

use crate::headers;

/// Publishes domain events onto a [`topmesys`] broker.
///
/// This is the driven adapter that satisfies [`EventPublisher`]. The application layer
/// only ever sees the port, so swapping this for a broker client is a change in the
/// composition root and nowhere else. With a Postgres outbox, this is also where the
/// relay forwards committed events to (ADR-016).
///
/// Event names are prefixed with the bus's namespace, so an event named
/// `"note.created"` published on a bus prefixed `"my-service"` is routed as
/// `"my-service.note.created"`. Listeners subscribe with topmesys topic patterns
/// (`"my-service.note.*"`), which is what lets one publish fan out to many independent
/// effects.
///
/// The envelope's metadata travels as message [`headers`]: its id, name,
/// timestamp and correlation id, plus every header of its own. A listener reads them
/// with `delivery.message().header(headers::CORRELATION_ID)`.
///
/// ### Wiring it up
///
/// ```no_run
/// use std::sync::Arc;
/// use jenny_core::port::outbound::{EventEnvelope, EventPublisher};
/// use jenny_messaging::EventBus;
/// use topmesys::EventBroker;
///
/// # #[tokio::main] async fn main() -> anyhow::Result<()> {
/// // composition root: start the broker, register listeners, hand the port inward
/// let broker = EventBroker::default();
/// // broker.add_topic_consumer(AuditLogWriter::new(pool.clone()))?;
/// let broker = broker.run()?;
///
/// let publisher: Arc<dyn EventPublisher> =
///     Arc::new(EventBus::new(broker.get_sender(), "my-service"));
///
/// publisher
///     .publish(EventEnvelope::json("note.created", &serde_json::json!({ "id": 1 }))?)
///     .await?;
/// # Ok(())
/// # }
/// ```
#[derive(Clone, Debug)]
pub struct EventBus {
    channel: mpsc::Sender<EventMessage>,
    namespace: String,
}

impl EventBus {
    /// `channel` comes from a running broker's
    /// [`get_sender`](topmesys::EventBroker::get_sender).
    pub fn new(channel: mpsc::Sender<EventMessage>, namespace: impl Into<String>) -> Self {
        Self {
            channel,
            namespace: namespace.into(),
        }
    }

    /// The channel this bus publishes on, for health checks and for handing to
    /// another emitter.
    pub fn sender(&self) -> &mpsc::Sender<EventMessage> {
        &self.channel
    }

    /// The prefix prepended to every event name before routing.
    pub fn namespace(&self) -> &str {
        &self.namespace
    }

    /// The topic an event of this name is routed on.
    pub fn topic_for(&self, event_name: &str) -> String {
        format!("{}.{event_name}", self.namespace)
    }

    fn message(&self, event: EventEnvelope) -> Result<EventMessage, PublishError> {
        let topic = self.topic_for(event.name());
        // The envelope's own headers first, so they cannot overwrite the reserved ones.
        let mut message = EventMessage::new(topic, Vec::new())
            .map_err(PublishError::transport)?
            .with_headers(event.headers().clone())
            .with_header(headers::EVENT_ID, event.id().to_string())
            .with_header(headers::EVENT_NAME, event.name())
            .with_header(headers::OCCURRED_AT, event.timestamp().to_rfc3339());
        if let Some(correlation_id) = event.correlation_id() {
            message = message.with_header(headers::CORRELATION_ID, correlation_id.to_string());
        }
        Ok(message.with_content(event.into_payload()))
    }
}

impl EventEmitter for EventBus {
    fn get_sender(&self) -> &mpsc::Sender<EventMessage> {
        &self.channel
    }
}

#[async_trait]
impl EventPublisher for EventBus {
    async fn publish(&self, event: EventEnvelope) -> Result<(), PublishError> {
        self.submit_event(EventSubmission::Single(self.message(event)?))
            .await
            .map_err(PublishError::transport)
    }

    /// Reserves all the channel permits up front so a batch cannot be torn in half by
    /// a full channel.
    async fn publish_all(&self, events: Vec<EventEnvelope>) -> Result<(), PublishError> {
        let messages = events
            .into_iter()
            .map(|event| self.message(event))
            .collect::<Result<Vec<_>, _>>()?;
        self.submit_event(EventSubmission::from_iter(messages))
            .await
            .map_err(PublishError::transport)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bus(capacity: usize) -> (EventBus, mpsc::Receiver<EventMessage>) {
        let (tx, rx) = mpsc::channel(capacity);
        (EventBus::new(tx, "test"), rx)
    }

    #[test]
    fn event_names_are_namespaced_into_topics() {
        let (bus, _rx) = bus(4);
        assert_eq!(bus.namespace(), "test");
        assert_eq!(bus.topic_for("note.created"), "test.note.created");
    }

    #[tokio::test]
    async fn publishing_puts_a_routed_message_on_the_channel() {
        let (bus, mut rx) = bus(4);
        bus.publish(EventEnvelope::json("note.created", &serde_json::json!({ "id": 1 })).unwrap())
            .await
            .unwrap();

        let msg = rx.recv().await.unwrap();
        assert_eq!(msg.topic().text(), "test.note.created");
        assert_eq!(msg.content().as_ref(), br#"{"id":1}"#);
    }

    #[tokio::test]
    async fn the_envelope_metadata_travels_as_headers() {
        let (bus, mut rx) = bus(4);
        let cx = jenny_core::port::Context::new();
        let envelope = EventEnvelope::new("note.created", b"{}".to_vec())
            .correlated_with(&cx)
            .with_header("tenant", "acme")
            // A header of the envelope's own cannot pass itself off as the event id.
            .with_header(headers::EVENT_ID, "forged");
        let id = envelope.id();
        bus.publish(envelope).await.unwrap();

        let msg = rx.recv().await.unwrap();
        assert_eq!(msg.header(headers::EVENT_ID), Some(id.to_string().as_str()));
        assert_eq!(msg.header(headers::EVENT_NAME), Some("note.created"));
        assert_eq!(
            msg.header(headers::CORRELATION_ID),
            Some(cx.request_id().to_string().as_str())
        );
        assert!(msg.header(headers::OCCURRED_AT).is_some());
        assert_eq!(msg.header("tenant"), Some("acme"));
    }

    #[tokio::test]
    async fn an_uncorrelated_event_carries_no_correlation_header() {
        let (bus, mut rx) = bus(4);
        bus.publish(EventEnvelope::new("note.created", b"{}".to_vec()))
            .await
            .unwrap();
        assert_eq!(
            rx.recv().await.unwrap().header(headers::CORRELATION_ID),
            None
        );
    }

    #[tokio::test]
    async fn a_batch_arrives_whole_and_in_order() {
        let (bus, mut rx) = bus(8);
        bus.publish_all(vec![
            EventEnvelope::new("note.created", b"1".to_vec()),
            EventEnvelope::new("note.updated", b"2".to_vec()),
            EventEnvelope::new("note.archived", b"3".to_vec()),
        ])
        .await
        .unwrap();

        let mut received = Vec::new();
        rx.recv_many(&mut received, 3).await;
        let topics: Vec<&str> = received.iter().map(|m| m.topic().text()).collect();
        assert_eq!(
            topics,
            [
                "test.note.created",
                "test.note.updated",
                "test.note.archived"
            ]
        );
    }

    #[tokio::test]
    async fn an_unroutable_event_name_is_a_transport_error_not_a_panic() {
        let (bus, _rx) = bus(4);
        let err = bus
            .publish(EventEnvelope::new("not a valid topic!", b"{}".to_vec()))
            .await
            .unwrap_err();
        assert!(matches!(err, PublishError::Transport(_)));
    }

    #[tokio::test]
    async fn the_bus_satisfies_the_port() {
        let (bus, mut rx) = bus(4);
        let publisher: std::sync::Arc<dyn EventPublisher> = std::sync::Arc::new(bus);
        publisher
            .publish(EventEnvelope::new("note.created", b"{}".to_vec()))
            .await
            .unwrap();
        assert!(rx.recv().await.is_some());
    }
}
