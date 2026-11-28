use std::fmt::Debug;

use async_nats::{
    Client,
    jetstream::{self, Context},
};
use async_trait::async_trait;
use tokio::sync::OnceCell;
use tracing::instrument;

use crate::kv::Nats;

use topmesys::{Delivery, EventConsumer, EventMessage, HandlerError, Subscription, Subscriptions};

/// A bus listener that forwards events to NATS JetStream.
///
/// This is one of the small outbound effects an event fans out to: register it with
/// the broker for the topics that should leave the process, and every matching event
/// published through the [`EventPublisher`](jenny_core::port::outbound::EventPublisher)
/// port ends up on the configured stream. Nothing in the application layer knows it
/// exists.
///
/// `subscription` is a topic pattern, or a [`Subscription`] when failed publishes should
/// be retried — a broker that is briefly unreachable is exactly the case retries are for.
///
/// Get one from the service's connection with [`Nats::gateway`], so it publishes with the
/// credentials `messaging.nats` configures, over the connection the key-value stores
/// use. [`new`](Self::new) takes a client of your own instead, such as one for another
/// NATS account.
#[derive(Debug)]
pub struct NatsGateway {
    stream: String,
    client: Source,
    subscription: Subscription,
}

/// Where a gateway's JetStream context comes from.
#[derive(Debug)]
enum Source {
    /// A client of the service's own, from [`NatsGateway::new`].
    Own(Context),
    /// The shared connection, resolved on the first publish.
    Shared(Nats, OnceCell<Context>),
}

impl Nats {
    /// A gateway publishing to `stream` over this connection, for the events
    /// `subscription` matches.
    ///
    /// Wiring is synchronous, so this does not connect: the runtime connects before
    /// serving, and the gateway takes the connected client on its first publish.
    pub fn gateway(
        &self,
        stream: impl Into<String>,
        subscription: impl Into<Subscription>,
    ) -> NatsGateway {
        NatsGateway {
            stream: stream.into(),
            client: Source::Shared(self.clone(), OnceCell::new()),
            subscription: subscription.into(),
        }
    }
}

impl NatsGateway {
    /// A gateway over a client of your own.
    pub fn new(
        client: Client,
        stream: impl Into<String>,
        subscription: impl Into<Subscription>,
    ) -> Self {
        Self {
            stream: stream.into(),
            client: Source::Own(jetstream::new(client)),
            subscription: subscription.into(),
        }
    }

    /// Publish received [EventMessage]s to [jetstream], with their headers.
    #[instrument(skip(self), level = "debug")]
    pub async fn publish_to_stream(&self, msg: &EventMessage) -> anyhow::Result<()> {
        self.ctx()
            .await?
            .publish_with_headers(
                format!("{}.{}", self.stream, msg.topic().text()),
                nats_headers(msg),
                msg.content().clone(),
            )
            .await?
            .await?;
        Ok(())
    }

    pub fn stream(&self) -> &str {
        &self.stream
    }

    /// The JetStream context it publishes with, connecting a gateway from a [`Nats`]
    /// when it has not yet.
    pub async fn ctx(&self) -> anyhow::Result<&Context> {
        match &self.client {
            Source::Own(ctx) => Ok(ctx),
            Source::Shared(nats, ctx) => {
                ctx.get_or_try_init(|| async { Ok(jetstream::new(nats.client().await?)) })
                    .await
            }
        }
    }
}

/// A message's headers as NATS headers, with the event id doubling as `Nats-Msg-Id`.
fn nats_headers(msg: &EventMessage) -> async_nats::HeaderMap {
    let mut headers = async_nats::HeaderMap::new();
    for (name, value) in msg.headers() {
        headers.insert(name.as_str(), value.as_str());
    }
    if let Some(id) = msg.header(crate::headers::EVENT_ID) {
        headers.insert(async_nats::header::NATS_MESSAGE_ID, id);
    }
    headers
}

#[async_trait]
impl EventConsumer for NatsGateway {
    type Topic = ();

    fn subscriptions(&self) -> Subscriptions<()> {
        self.subscription.clone().into()
    }

    async fn handle_event(&self, _: &(), delivery: &Delivery) -> Result<(), HandlerError> {
        Ok(self.publish_to_stream(delivery.message()).await?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_gateway_from_the_connection_does_not_connect_while_wiring() {
        let nats = Nats::new(
            crate::connection::NatsConnectionConfig::new("nats://127.0.0.1:1"),
            "notes",
        );
        let gateway = nats.gateway("notes", "notes.>");
        assert_eq!(gateway.stream(), "notes");
        assert!(!format!("{nats:?}").contains("connected: true"));
        // Nothing listens there: the first publish is what fails.
        let err = gateway.ctx().await.unwrap_err();
        assert!(err.to_string().contains("connecting to NATS"), "{err}");
    }

    #[test]
    fn headers_are_carried_over_and_the_event_id_deduplicates() {
        let msg = EventMessage::new("note.created", "{}")
            .unwrap()
            .with_header(crate::headers::EVENT_ID, "e-1")
            .with_header(crate::headers::CORRELATION_ID, "c-1");
        let headers = nats_headers(&msg);
        assert_eq!(
            headers
                .get(crate::headers::CORRELATION_ID)
                .unwrap()
                .as_str(),
            "c-1"
        );
        assert_eq!(
            headers
                .get(async_nats::header::NATS_MESSAGE_ID)
                .unwrap()
                .as_str(),
            "e-1"
        );
    }
}
