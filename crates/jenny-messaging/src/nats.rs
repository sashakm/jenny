use std::fmt::Debug;

use async_nats::{
    Client, ServerAddr,
    jetstream::{self, Context},
};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tracing::instrument;

use topmesys::{Delivery, EventConsumer, EventMessage, HandlerError, Subscription, Subscriptions};

/// Holds NATS client configuration. Url string can hold multiple server urls, separated by comma.
/// #### Example:
/// ```
/// # use serde_json::{from_value, json};
/// use jenny_messaging::nats::NatsConfig;
///
/// let nats_cfg: NatsConfig = from_value(json!({
///     "url": "nats://nats-1:4222,nats://nats-2:4222",
///     "stream": "my-stream",
///     "consumer": "my-consumer"
/// })).unwrap();
/// # assert_eq!(nats_cfg.stream(), "my-stream");
/// ```
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct NatsConfig {
    url: String,
    stream: String,
    consumer: String,
}

impl NatsConfig {
    pub fn new(url: String, stream: String, consumer: String) -> Self {
        Self {
            url,
            stream,
            consumer,
        }
    }

    /// Parse NATS server url string and create a connected client.
    ///
    /// Fails on any url that does not parse, rather than connecting to the rest: a typo
    /// in one of three cluster addresses should be found at startup, not during the
    /// outage of the other two.
    pub async fn create_client(&self) -> anyhow::Result<async_nats::Client> {
        Ok(async_nats::connect(self.server_addrs()?).await?)
    }

    fn server_addrs(&self) -> anyhow::Result<Vec<ServerAddr>> {
        let addrs = self
            .url
            .split(',')
            .map(str::trim)
            .filter(|url| !url.is_empty())
            .map(|url| {
                url.parse::<ServerAddr>()
                    .map_err(|e| anyhow::anyhow!("invalid NATS server url `{url}`: {e}"))
            })
            .collect::<anyhow::Result<Vec<_>>>()?;
        if addrs.is_empty() {
            anyhow::bail!("no NATS server url configured");
        }
        Ok(addrs)
    }

    /// Get configured stream name.
    pub fn stream(&self) -> &str {
        &self.stream
    }

    /// Create stream pull consumer from configured stream and consumer names.
    pub async fn generate_consumer(
        &self,
        client: async_nats::Client,
    ) -> anyhow::Result<async_nats::jetstream::consumer::PullConsumer> {
        Ok(async_nats::jetstream::new(client)
            .get_consumer_from_stream(&self.consumer, &self.stream)
            .await?)
    }

    /// Get configured consumer name.
    pub fn consumer(&self) -> &str {
        &self.consumer
    }
}

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
#[derive(Debug)]
pub struct NatsGateway {
    stream: String,
    ctx: Context,
    subscription: Subscription,
}

impl NatsGateway {
    pub fn new(
        client: Client,
        stream: impl Into<String>,
        subscription: impl Into<Subscription>,
    ) -> Self {
        Self {
            stream: stream.into(),
            ctx: jetstream::new(client),
            subscription: subscription.into(),
        }
    }

    /// Publish received [EventMessage]s to [jetstream], with their headers.
    #[instrument(skip(self), level = "debug")]
    pub async fn publish_to_stream(&self, msg: &EventMessage) -> anyhow::Result<()> {
        self.ctx
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

    pub fn ctx(&self) -> &Context {
        &self.ctx
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

    fn config(url: &str) -> NatsConfig {
        NatsConfig::new(url.into(), "stream".into(), "consumer".into())
    }

    #[test]
    fn every_server_url_must_parse() {
        assert_eq!(
            config("nats://a:4222, nats://b:4222")
                .server_addrs()
                .unwrap()
                .len(),
            2
        );
        let err = config("nats://a:4222,nats://b:not-a-port")
            .server_addrs()
            .unwrap_err();
        assert!(err.to_string().contains("nats://b:not-a-port"), "{err}");
        assert!(config(" , ").server_addrs().is_err());
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
