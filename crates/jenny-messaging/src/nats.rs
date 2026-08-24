use std::{fmt::Debug, sync::Arc};

use async_nats::{
    Client, ServerAddr,
    jetstream::{self, Context},
};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tracing::instrument;

use topmesys::{EventConsumer, EventMessage};

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
    pub async fn create_client(&self) -> anyhow::Result<async_nats::Client> {
        let addrs: Vec<ServerAddr> = self
            .url
            .split(",")
            .filter_map(|url| url.parse::<ServerAddr>().ok())
            .collect();
        Ok(async_nats::connect(addrs).await?)
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
#[derive(Debug)]
pub struct NatsGateway {
    stream: String,
    ctx: Context,
    broker_topic: String,
}

impl NatsGateway {
    #[instrument]
    pub fn new<S: Into<String> + Debug>(client: Client, stream: S, broker_topic: S) -> Self {
        Self {
            stream: stream.into(),
            ctx: jetstream::new(client.clone()),
            broker_topic: broker_topic.into(),
        }
    }

    /// Publish received [EventMessage]s to [jetstream]
    #[instrument(skip(self), level = "debug")]
    pub async fn publish_to_stream(&self, msg: &EventMessage) -> anyhow::Result<()> {
        self.ctx
            .publish(
                format!("{}.{}", self.stream, msg.topic().text()),
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

#[async_trait]
impl EventConsumer for NatsGateway {
    fn consumes(&self) -> &str {
        &self.broker_topic
    }

    async fn handle_event(&self, event: Arc<EventMessage>) -> anyhow::Result<()> {
        self.publish_to_stream(event.as_ref()).await
    }
}
