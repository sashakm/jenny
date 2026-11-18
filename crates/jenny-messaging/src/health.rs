//! Are the bus and the broker reachable?

use async_trait::async_trait;
use jenny_core::port::outbound::HealthCheck;
use tokio::sync::mpsc;
use topmesys::EventMessage;

use crate::eventbus::EventBus;

/// Readiness of the in-process event bus.
///
/// The bus is healthy while its channel is open. A closed channel means the broker's
/// event loop has stopped, which is worth surfacing: every publish through the port now
/// fails, and a service in that state should stop taking traffic.
///
/// The runtime registers this for you once the bus exists, that is after
/// `JennyServer::events` was called, because it owns the broker — unlike a database
/// pool, which is yours.
pub struct EventBusCheck {
    name: String,
    channel: mpsc::Sender<EventMessage>,
}

impl EventBusCheck {
    pub fn new(name: impl Into<String>, bus: &EventBus) -> Self {
        Self {
            name: name.into(),
            channel: bus.sender().clone(),
        }
    }
}

#[async_trait]
impl HealthCheck for EventBusCheck {
    fn name(&self) -> &str {
        &self.name
    }

    fn kind(&self) -> &str {
        "eventbus"
    }

    async fn check(&self) -> Result<(), String> {
        if self.channel.is_closed() {
            Err("the event broker has stopped; published events would be dropped".to_string())
        } else {
            Ok(())
        }
    }
}

/// Readiness of a NATS connection.
///
/// Reports the client's own view of its connection state. `async-nats` reconnects on
/// its own, so a momentarily disconnected client is not necessarily broken — but it is
/// not ready to carry traffic either, and saying so is the point of a readiness probe.
pub struct NatsCheck {
    name: String,
    client: async_nats::Client,
}

impl NatsCheck {
    pub fn new(name: impl Into<String>, client: async_nats::Client) -> Self {
        Self {
            name: name.into(),
            client,
        }
    }
}

#[async_trait]
impl HealthCheck for NatsCheck {
    fn name(&self) -> &str {
        &self.name
    }

    fn kind(&self) -> &str {
        "broker"
    }

    async fn check(&self) -> Result<(), String> {
        match self.client.connection_state() {
            async_nats::connection::State::Connected => Ok(()),
            other => Err(format!("nats connection is {other:?}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn the_bus_is_healthy_while_its_channel_is_open() {
        let (tx, rx) = mpsc::channel(4);
        let bus = EventBus::new(tx, "test");
        let check = EventBusCheck::new("events", &bus);

        assert_eq!(check.name(), "events");
        assert_eq!(check.kind(), "eventbus");
        assert!(check.check().await.is_ok());

        // Dropping the receiver is what a stopped broker looks like from here.
        drop(rx);
        let err = check.check().await.unwrap_err();
        assert!(err.contains("stopped"), "{err}");
    }
}
