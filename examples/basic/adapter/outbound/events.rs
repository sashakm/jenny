//! The driven side of event publishing.
//!
//! Two shapes, and `main.rs` picks between them:
//!
//! - Without the `messaging` feature, [`EventLog`] satisfies the
//!   [`EventPublisher`] port directly — the simplest thing that can.
//! - With it, the runtime starts an in-process bus and hands the port back from
//!   `server.events()`. Publishing then fans out to every subscribed listener, and
//!   [`EventLogListener`] below is one such listener.
//!
//! The application layer cannot tell which it got, because it only ever sees the port.
//! That is the whole reason for the indirection.

use async_trait::async_trait;
#[cfg(feature = "messaging")]
use jenny::messaging::topmesys;
use jenny::{EventEnvelope, EventPublisher, PublishError};

/// Satisfies the port by writing to the log. No bus involved.
#[cfg_attr(feature = "messaging", allow(dead_code))]
#[derive(Debug, Default)]
pub struct EventLog;

#[async_trait]
impl EventPublisher for EventLog {
    async fn publish(&self, event: EventEnvelope) -> Result<(), PublishError> {
        tracing::info!(
            event = event.name(),
            at = %event.timestamp(),
            payload = %String::from_utf8_lossy(event.payload()),
            "domain event"
        );
        Ok(())
    }
}

/// A *listener* on the in-process bus — one small outbound effect among however many
/// are subscribed.
///
/// A real service would sit an audit-log writer, a NATS forwarder and a mailer beside
/// this one. Each is added independently, and nothing that publishes learns about any
/// of them.
#[cfg(feature = "messaging")]
#[derive(Debug)]
pub struct EventLogListener;

#[cfg(feature = "messaging")]
#[async_trait]
impl topmesys::EventConsumer for EventLogListener {
    /// Every note event in this service's namespace.
    fn consumes(&self) -> &str {
        "basic-example.note.*"
    }

    async fn handle_event(
        &self,
        event: std::sync::Arc<topmesys::EventMessage>,
    ) -> anyhow::Result<()> {
        tracing::info!(
            topic = event.topic().text(),
            payload = %String::from_utf8_lossy(event.content()),
            "domain event"
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn it_satisfies_the_port() {
        let publisher: std::sync::Arc<dyn EventPublisher> = std::sync::Arc::new(EventLog);
        publisher
            .publish(EventEnvelope::json("note.created", &serde_json::json!({ "id": 1 })).unwrap())
            .await
            .unwrap();
    }
}
