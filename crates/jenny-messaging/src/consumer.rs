//! Consuming from JetStream: one durable pull consumer per [`MessagingAdapter`], its
//! messages dispatched to the adapter's handlers (ADR-006).
//!
//! At startup the runtime creates or updates, on the adapter's
//! [stream](MessagingAdapter::stream), a durable pull consumer named
//! `<service_name>-<adapter name>` whose filter subjects are the adapter's handled
//! subjects. It never creates the stream: a missing one stops the service, naming it.
//!
//! ### What becomes of a message
//!
//! | The handler | The message |
//! |---|---|
//! | succeeded | ack |
//! | the payload did not decode; `Invalid`, `NotFound`, `Forbidden`, `Unauthenticated` | term, with a warning |
//! | `Internal`, `Stale`, `Conflict` | nak, redelivered after `backoff`, until `max_deliver`; then term |
//!
//! Before any term, a `dead_letter` subject, when configured, gets the original payload
//! and headers (without the `Nats-*` ones, so JetStream neither deduplicates the copy
//! against the original nor checks its expectations), plus `dead-letter-reason`,
//! `dead-letter-subject` and `dead-letter-deliveries`. The copy is published to
//! JetStream and its acknowledgement awaited, so the subject must be captured by a
//! stream; if that publish fails, the message is nak'd instead, and never lost silently.
//!
//! A handler that runs long tells JetStream it is still working (a progress ack every
//! half `ack_wait`, default 30 seconds), so the message is not redelivered meanwhile.
//!
//! Delivery is at least once. A handler whose effect is not idempotent deduplicates on
//! the [`InboundEvent`](jenny_core::port::InboundEvent)'s id; jenny keeps no dedup store.
//!
//! ### Order and concurrency
//!
//! `max_in_flight` (default 1) is how many messages a consumer holds at a time. At 1 the
//! messages are handled one after the other, in stream order, as long as they succeed
//! (a nak'd message is redelivered after its delay, behind the ones that followed it).
//! Larger values handle that many concurrently, and give up order.
//!
//! ### Trust
//!
//! The caller headers (`caller-subject`, `caller-actor`) are only trusted when the NATS
//! server required the service to authenticate ([`Nats::is_authenticated`]), which it
//! then requires of every publisher too. Over an anonymous connection they are dropped,
//! and `principal: envelope` is a startup error. Trusted headers are only as trustworthy
//! as the NATS publish permissions on the subject: restrict who may publish there. A
//! server with `no_auth_user` reports that it requires authentication but accepts
//! anonymous clients as that user, so give that user no publish permission on subjects
//! whose caller headers a service trusts.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use async_nats::jetstream::{self, AckKind, consumer::pull};
use futures::stream::FuturesUnordered;
use futures::{FutureExt, StreamExt};
use jenny_core::domain::Principal;
use jenny_core::port::outbound::Authorizer;
use serde::Deserialize;
use tokio::sync::watch;
use tokio::task::JoinHandle;

use crate::MessagingAdapter;
use crate::inbound::{Dispatch, Dispatcher, Outcome, PrincipalPolicy, Received};
use crate::kv::Nats;

/// The header a dead-lettered message carries its failure in.
pub const DEAD_LETTER_REASON: &str = "dead-letter-reason";
/// The subject a dead-lettered message was consumed from.
pub const DEAD_LETTER_SUBJECT: &str = "dead-letter-subject";
/// How many times it was delivered before it was given up.
pub const DEAD_LETTER_DELIVERIES: &str = "dead-letter-deliveries";

/// Default for `messaging.inbound.max_deliver`.
pub const DEFAULT_MAX_DELIVER: u32 = 5;
/// Default for `messaging.inbound.ack_wait`, in seconds.
pub const DEFAULT_ACK_WAIT_SECONDS: u64 = 30;
/// Default for `messaging.inbound.backoff`, in seconds.
pub const DEFAULT_BACKOFF_SECONDS: [u64; 3] = [1, 5, 30];

/// `messaging.inbound`: how inbound handlers run and what becomes of a failed message.
///
/// ```yaml
/// messaging:
///   inbound:
///     principal: system         # or envelope: run as the event's caller
///     max_deliver: 5
///     backoff: [1, 5, 30]       # seconds before each redelivery; the last repeats
///     dead_letter: SHOP.dead    # optional
///     max_in_flight: 1          # per consumer; 1 keeps order
///     subscriptions:            # a list, because keys cannot contain dots
///       - subject: shop.orders.created
///         max_deliver: 10
///         principal: envelope
/// ```
#[derive(Clone, Debug, Deserialize)]
#[serde(default)]
pub struct InboundConfig {
    pub principal: PrincipalPolicy,
    pub max_deliver: u32,
    pub backoff: Vec<u64>,
    pub dead_letter: Option<String>,
    pub max_in_flight: usize,
    /// Seconds JetStream waits for an ack before redelivering (default 30). A running
    /// handler sends a progress ack every half of it.
    pub ack_wait: u64,
    /// Overrides for one subject each, matched on the full subject a handler was bound
    /// to (prefix included). In-process bus handlers match on their topic pattern; only
    /// `principal` applies to them.
    pub subscriptions: Vec<SubscriptionConfig>,
}

impl Default for InboundConfig {
    fn default() -> Self {
        Self {
            principal: PrincipalPolicy::default(),
            max_deliver: DEFAULT_MAX_DELIVER,
            backoff: DEFAULT_BACKOFF_SECONDS.to_vec(),
            dead_letter: None,
            max_in_flight: 1,
            ack_wait: DEFAULT_ACK_WAIT_SECONDS,
            subscriptions: Vec::new(),
        }
    }
}

/// One entry of `messaging.inbound.subscriptions`. What is unset takes the default.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct SubscriptionConfig {
    pub subject: String,
    pub principal: Option<PrincipalPolicy>,
    pub max_deliver: Option<u32>,
    pub backoff: Option<Vec<u64>>,
    pub dead_letter: Option<String>,
}

impl InboundConfig {
    /// Find every mistake at once, naming the keys relative to this section.
    pub fn validate(&self) -> anyhow::Result<()> {
        let mut problems = Vec::new();
        if self.max_in_flight == 0 {
            problems.push("`max_in_flight` must be at least 1".to_string());
        }
        if self.ack_wait < 2 {
            problems.push("`ack_wait` must be at least 2 (seconds)".to_string());
        }
        let blank = |value: &Option<String>| value.as_deref().is_some_and(|v| v.trim().is_empty());
        if blank(&self.dead_letter) {
            problems.push("`dead_letter` is set but empty".to_string());
        }
        let check = |problems: &mut Vec<String>,
                     key: &str,
                     max_deliver: Option<u32>,
                     backoff: Option<&Vec<u64>>| {
            if max_deliver == Some(0) {
                problems.push(format!("`{key}max_deliver` must be at least 1"));
            }
            if backoff.is_some_and(Vec::is_empty) {
                problems.push(format!("`{key}backoff` needs at least one delay"));
            }
        };
        check(
            &mut problems,
            "",
            Some(self.max_deliver),
            Some(&self.backoff),
        );
        for (index, subscription) in self.subscriptions.iter().enumerate() {
            let key = format!("subscriptions[{index}].");
            if subscription.subject.trim().is_empty() {
                problems.push(format!("`{key}subject` is empty"));
            }
            if blank(&subscription.dead_letter) {
                problems.push(format!("`{key}dead_letter` is set but empty"));
            }
            check(
                &mut problems,
                &key,
                subscription.max_deliver,
                subscription.backoff.as_ref(),
            );
        }
        if problems.is_empty() {
            Ok(())
        } else {
            anyhow::bail!("{}", problems.join("; "))
        }
    }

    /// The override for exactly `subject`, if there is one.
    fn subscription(&self, subject: &str) -> Option<&SubscriptionConfig> {
        self.subscriptions.iter().find(|s| s.subject == subject)
    }

    /// Who handlers of `subject` run as.
    pub fn principal(&self, subject: &str) -> PrincipalPolicy {
        self.subscription(subject)
            .and_then(|s| s.principal)
            .unwrap_or(self.principal)
    }

    /// What becomes of a failed message on `subject`.
    pub fn failures(&self, subject: &str) -> FailurePolicy {
        let over = self.subscription(subject);
        FailurePolicy {
            max_deliver: over.and_then(|s| s.max_deliver).unwrap_or(self.max_deliver),
            backoff: over
                .and_then(|s| s.backoff.clone())
                .unwrap_or_else(|| self.backoff.clone())
                .into_iter()
                .map(Duration::from_secs)
                .collect(),
            dead_letter: over
                .and_then(|s| s.dead_letter.clone())
                .or_else(|| self.dead_letter.clone()),
        }
    }
}

/// Retries and dead-lettering for one subject.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FailurePolicy {
    pub max_deliver: u32,
    pub backoff: Vec<Duration>,
    pub dead_letter: Option<String>,
}

/// What to tell JetStream about a message.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Decision {
    Ack,
    /// Redeliver after the delay.
    Nak(Duration),
    /// Never redeliver.
    Term,
}

impl FailurePolicy {
    /// What `outcome` on the `delivered`-th delivery (from 1) calls for.
    pub fn decide(&self, outcome: &Outcome, delivered: u64) -> Decision {
        match outcome {
            Outcome::Handled => Decision::Ack,
            outcome if !outcome.is_retryable() => Decision::Term,
            _ if delivered >= u64::from(self.max_deliver) => Decision::Term,
            _ => Decision::Nak(self.delay(delivered)),
        }
    }

    /// The wait before the redelivery that follows the `delivered`-th; the last delay
    /// repeats.
    pub fn delay(&self, delivered: u64) -> Duration {
        let index = usize::try_from(delivered.saturating_sub(1)).unwrap_or(usize::MAX);
        self.backoff
            .get(index)
            .or(self.backoff.last())
            .copied()
            .unwrap_or_default()
    }
}

/// The headers a dead-lettered copy carries: the original ones except NATS's own
/// (`Nats-Msg-Id` would make JetStream drop the copy as a duplicate of the original), plus
/// why, where from and after how many deliveries.
pub fn dead_letter_headers(
    original: &BTreeMap<String, String>,
    reason: &str,
    subject: &str,
    deliveries: u64,
) -> BTreeMap<String, String> {
    let mut headers: BTreeMap<String, String> = original
        .iter()
        .filter(|(name, _)| !name.to_ascii_lowercase().starts_with("nats-"))
        .map(|(name, value)| (name.clone(), value.clone()))
        .collect();
    headers.insert(DEAD_LETTER_REASON.to_string(), reason.to_string());
    headers.insert(DEAD_LETTER_SUBJECT.to_string(), subject.to_string());
    headers.insert(DEAD_LETTER_DELIVERIES.to_string(), deliveries.to_string());
    headers
}

/// Whether `subject` matches the NATS filter `filter` (`*` one token, `>` the rest).
pub fn subject_matches(filter: &str, subject: &str) -> bool {
    let mut filter = filter.split('.');
    let mut subject = subject.split('.');
    loop {
        match (filter.next(), subject.next()) {
            (Some(">"), Some(_)) => return true,
            (Some("*"), Some(_)) => {}
            (Some(f), Some(s)) if f == s => {}
            (None, None) => return true,
            _ => return false,
        }
    }
}

/// Resolves once `stop` turns true (or its sender is gone).
async fn stopped(stop: &mut watch::Receiver<bool>) {
    let _ = stop.wait_for(|stop| *stop).await;
}

/// One handled subject of a consumer.
#[derive(Clone, Debug)]
struct Route {
    subject: String,
    dispatch: Dispatch,
    dispatcher: Dispatcher,
    failures: FailurePolicy,
}

/// An adapter's durable pull consumer, created and ready to run.
#[derive(Debug)]
pub struct JetStreamConsumer {
    name: String,
    stream: String,
    routes: Vec<Route>,
    max_in_flight: usize,
    ack_wait: Duration,
    jetstream: jetstream::Context,
    consumer: jetstream::consumer::Consumer<pull::Config>,
}

/// What `/health/status` shows of a consumer.
#[derive(Clone, Debug)]
pub struct ConsumerStatus {
    name: String,
    stream: String,
    filter_subjects: Vec<String>,
    consumer: jetstream::consumer::Consumer<pull::Config>,
}

impl ConsumerStatus {
    /// Name, stream, filter subjects and, when the server answers, its pending count.
    pub async fn report(&self) -> serde_json::Value {
        let mut consumer = self.consumer.clone();
        let pending = match consumer.info().await {
            Ok(info) => serde_json::json!(info.num_pending),
            Err(e) => {
                tracing::debug!(consumer = self.name, error = %e, "reading consumer info");
                serde_json::Value::Null
            }
        };
        serde_json::json!({
            "name": self.name,
            "stream": self.stream,
            "filter_subjects": self.filter_subjects,
            "pending": pending,
        })
    }
}

impl JetStreamConsumer {
    /// Create or update the consumer of `adapter` over `nats`, or `None` when the adapter
    /// handles nothing.
    ///
    /// Fails when the adapter has handlers but no stream, the stream does not exist, a
    /// subscription asks for `principal: envelope` over an anonymous connection, or the
    /// server refuses the consumer.
    pub async fn bind(
        nats: &Nats,
        service: &str,
        adapter: &MessagingAdapter,
        config: &InboundConfig,
        system: Principal,
        authorizer: Option<Arc<dyn Authorizer>>,
    ) -> anyhow::Result<Option<Self>> {
        let handled = adapter.handlers();
        if handled.is_empty() {
            return Ok(None);
        }
        let stream = adapter.stream_name().ok_or_else(|| {
            anyhow::anyhow!(
                "messaging adapter `{}` has handlers but no stream: call `.stream(\"…\")`",
                adapter.name()
            )
        })?;
        let name = format!("{service}-{}", adapter.name());
        if name
            .chars()
            .any(|c| c.is_whitespace() || matches!(c, '.' | '*' | '>' | '/' | '\\'))
        {
            anyhow::bail!(
                "`{name}` is not a valid JetStream consumer name: the service and adapter \
                 names may not contain whitespace, `.`, `*`, `>`, `/` or `\\`"
            );
        }

        let client = nats.client().await?;
        let trusted = nats.is_authenticated().await?;
        let mut routes = Vec::with_capacity(handled.len());
        for (subject, dispatch) in handled {
            let dispatcher = Dispatcher::new(system.clone(), trusted)
                .with_authorizer(authorizer.clone())
                .with_policy(config.principal(&subject))
                .map_err(|e| {
                    anyhow::anyhow!(
                        "subscription `{subject}` of messaging adapter `{}`: {e}",
                        adapter.name()
                    )
                })?;
            routes.push(Route {
                failures: config.failures(&subject),
                subject,
                dispatch,
                dispatcher,
            });
        }
        if !trusted {
            tracing::info!(
                consumer = name,
                "the NATS server does not require authentication: caller headers are dropped"
            );
        }

        let jetstream = jetstream::new(client.clone());
        let found = jetstream.get_stream(stream).await.map_err(|e| {
            anyhow::anyhow!(
                "JetStream stream `{stream}` of messaging adapter `{}` is not there ({e}); \
                 jenny never creates streams",
                adapter.name()
            )
        })?;
        let filter_subjects: Vec<String> = routes.iter().map(|r| r.subject.clone()).collect();
        let max_in_flight = config.max_in_flight.max(1);
        let ack_wait = Duration::from_secs(config.ack_wait.max(2));
        let consumer = found
            .create_consumer(pull::Config {
                durable_name: Some(name.clone()),
                description: adapter.description().map(str::to_string),
                filter_subjects,
                ack_policy: jetstream::consumer::AckPolicy::Explicit,
                // jenny counts deliveries itself, so a message is never dropped by the
                // server without going through the dead-letter path.
                max_deliver: -1,
                ack_wait,
                max_ack_pending: i64::try_from(max_in_flight).unwrap_or(i64::MAX),
                ..Default::default()
            })
            .await
            .map_err(|e| {
                anyhow::anyhow!("creating JetStream consumer `{name}` on `{stream}`: {e}")
            })?;
        tracing::info!(consumer = name, stream, trusted, "jetstream consumer ready");
        Ok(Some(Self {
            name,
            stream: stream.to_string(),
            routes,
            max_in_flight,
            ack_wait,
            jetstream,
            consumer,
        }))
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    /// For `/health/status`.
    pub fn status(&self) -> ConsumerStatus {
        ConsumerStatus {
            name: self.name.clone(),
            stream: self.stream.clone(),
            filter_subjects: self.routes.iter().map(|r| r.subject.clone()).collect(),
            consumer: self.consumer.clone(),
        }
    }

    /// Consume until `stop` turns true, then stop fetching, finish the messages held,
    /// and return.
    pub fn spawn(self, stop: watch::Receiver<bool>) -> JoinHandle<()> {
        tokio::spawn(self.run(stop))
    }

    async fn run(self, mut stop: watch::Receiver<bool>) {
        let this = Arc::new(self);
        loop {
            if *stop.borrow() {
                break;
            }
            let batch = this
                .consumer
                .batch()
                .max_messages(this.max_in_flight)
                .expires(Duration::from_secs(5))
                .messages()
                .await;
            let mut batch = match batch {
                Ok(batch) => batch,
                Err(e) => {
                    tracing::warn!(consumer = this.name, error = %e, "fetching from JetStream");
                    tokio::select! {
                        () = stopped(&mut stop) => break,
                        () = tokio::time::sleep(Duration::from_secs(1)) => continue,
                    }
                }
            };
            let mut running = FuturesUnordered::new();
            let mut stopping = false;
            loop {
                tokio::select! {
                    biased;
                    () = stopped(&mut stop) => {
                        stopping = true;
                        break;
                    }
                    next = batch.next() => match next {
                        Some(Ok(message)) if this.max_in_flight == 1 => {
                            this.process(message).await;
                        }
                        Some(Ok(message)) => running.push(this.clone().process_owned(message)),
                        Some(Err(e)) => {
                            tracing::debug!(consumer = this.name, error = %e, "batch ended");
                            break;
                        }
                        None => break,
                    },
                    Some(()) = running.next(), if !running.is_empty() => {}
                }
            }
            // Whatever was taken is finished, and acked or nak'd, before stopping.
            while running.next().await.is_some() {}
            if stopping {
                // Pulled but never started: back to JetStream at once, for the next one.
                while let Some(Some(Ok(message))) = batch.next().now_or_never() {
                    this.settle(&message, AckKind::Nak(None)).await;
                }
            }
        }
        tracing::info!(consumer = this.name, "jetstream consumer stopped");
    }

    async fn process_owned(self: Arc<Self>, message: jetstream::Message) {
        self.process(message).await;
    }

    async fn process(&self, message: jetstream::Message) {
        let subject = message.subject.to_string();
        let delivered = message
            .info()
            .map(|info| u64::try_from(info.delivered).unwrap_or(1))
            .unwrap_or(1);
        let headers: BTreeMap<String, String> = message
            .headers
            .iter()
            .flat_map(|map| map.iter())
            .filter_map(|(name, values)| {
                values
                    .first()
                    .map(|value| (name.to_string(), value.as_str().to_string()))
            })
            .collect();
        let received = Received {
            headers,
            payload: message.payload.to_vec(),
            attempt: delivered,
            subject: subject.clone(),
        };

        let Some(route) = self
            .routes
            .iter()
            .find(|route| subject_matches(&route.subject, &subject))
        else {
            tracing::warn!(
                consumer = self.name,
                subject,
                "no handler for the subject; terminating"
            );
            self.settle(&message, AckKind::Term).await;
            return;
        };
        let outcome = {
            // Tell JetStream the handler is still at it, so it does not redeliver meanwhile.
            let handling = route.dispatcher.dispatch(&route.dispatch, &received);
            tokio::pin!(handling);
            let mut progress = tokio::time::interval(self.ack_wait / 2);
            progress.tick().await;
            loop {
                tokio::select! {
                    outcome = &mut handling => break outcome,
                    _ = progress.tick() => self.settle(&message, AckKind::Progress).await,
                }
            }
        };
        let event_id = received.headers.get(crate::headers::EVENT_ID).cloned();
        let event = received.headers.get(crate::headers::EVENT_NAME).cloned();
        match route.failures.decide(&outcome, delivered) {
            Decision::Ack => self.settle(&message, AckKind::Ack).await,
            Decision::Nak(delay) => {
                tracing::info!(
                    consumer = self.name,
                    subject,
                    event_id,
                    event,
                    delivered,
                    retry_in_seconds = delay.as_secs(),
                    "inbound handler failed, redelivering: {}",
                    outcome.reason().unwrap_or_default()
                );
                self.settle(&message, AckKind::Nak(Some(delay))).await;
            }
            Decision::Term => {
                let reason = outcome.reason().unwrap_or_default();
                tracing::warn!(
                    consumer = self.name,
                    subject,
                    event_id,
                    event,
                    delivered,
                    "inbound message given up: {reason}"
                );
                if let Some(dead_letter) = &route.failures.dead_letter
                    && let Err(e) = self
                        .dead_letter(dead_letter, &received, &reason, delivered)
                        .await
                {
                    tracing::error!(
                        consumer = self.name, subject, dead_letter, error = %e,
                        "could not dead-letter the message; redelivering it instead"
                    );
                    let delay = route.failures.delay(delivered);
                    self.settle(&message, AckKind::Nak(Some(delay))).await;
                    return;
                }
                self.settle(&message, AckKind::Term).await;
            }
        }
    }

    async fn dead_letter(
        &self,
        subject: &str,
        received: &Received,
        reason: &str,
        deliveries: u64,
    ) -> anyhow::Result<()> {
        let mut headers = async_nats::HeaderMap::new();
        for (name, value) in
            dead_letter_headers(&received.headers, reason, &received.subject, deliveries)
        {
            headers.insert(name.as_str(), value.as_str());
        }
        // Through JetStream, awaiting its ack: a subject no stream captures fails here,
        // and the original is redelivered rather than lost.
        self.jetstream
            .publish_with_headers(
                subject.to_string(),
                headers,
                received.payload.clone().into(),
            )
            .await?
            .await?;
        Ok(())
    }

    async fn settle(&self, message: &jetstream::Message, kind: AckKind) {
        if let Err(e) = message.ack_with(kind).await {
            tracing::warn!(consumer = self.name, error = %e, "settling a JetStream message");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use jenny_core::domain::DomainError;

    #[test]
    fn subjects_match_nats_wildcards() {
        assert!(subject_matches(
            "shop.orders.created",
            "shop.orders.created"
        ));
        assert!(!subject_matches(
            "shop.orders.created",
            "shop.orders.cancelled"
        ));
        assert!(subject_matches("shop.orders.*", "shop.orders.created"));
        assert!(!subject_matches("shop.orders.*", "shop.orders.created.eu"));
        assert!(subject_matches("shop.>", "shop.orders.created.eu"));
        assert!(!subject_matches("shop.>", "shop"));
        assert!(!subject_matches("shop.orders", "shop.orders.created"));
    }

    #[test]
    fn the_defaults_and_an_override_per_full_subject() {
        let config: InboundConfig = serde_json::from_value(serde_json::json!({
            "dead_letter": "SHOP.dead",
            "subscriptions": [
                { "subject": "shop.orders.created", "max_deliver": 10, "backoff": [2], "principal": "envelope" }
            ]
        }))
        .unwrap();
        config.validate().unwrap();
        assert_eq!(config.max_in_flight, 1);

        let default = config.failures("shop.orders.cancelled");
        assert_eq!(default.max_deliver, 5);
        assert_eq!(default.backoff, [1, 5, 30].map(Duration::from_secs));
        assert_eq!(default.dead_letter.as_deref(), Some("SHOP.dead"));
        assert_eq!(
            config.principal("shop.orders.cancelled"),
            PrincipalPolicy::System
        );

        let over = config.failures("shop.orders.created");
        assert_eq!(over.max_deliver, 10);
        assert_eq!(over.backoff, [Duration::from_secs(2)]);
        assert_eq!(over.dead_letter.as_deref(), Some("SHOP.dead"));
        assert_eq!(
            config.principal("shop.orders.created"),
            PrincipalPolicy::Envelope
        );
    }

    #[test]
    fn mistakes_are_named() {
        let config: InboundConfig = serde_json::from_value(serde_json::json!({
            "max_in_flight": 0,
            "ack_wait": 1,
            "dead_letter": " ",
            "backoff": [],
            "subscriptions": [{ "subject": "", "max_deliver": 0 }]
        }))
        .unwrap();
        let err = config.validate().unwrap_err().to_string();
        for key in [
            "`max_in_flight`",
            "`backoff`",
            "`subscriptions[0].subject`",
            "`subscriptions[0].max_deliver`",
            "`dead_letter`",
            "`ack_wait`",
        ] {
            assert!(err.contains(key), "{key} in {err}");
        }
    }

    #[test]
    fn errors_map_to_ack_nak_or_term() {
        let policy = InboundConfig::default().failures("x");
        assert_eq!(policy.decide(&Outcome::Handled, 1), Decision::Ack);
        for permanent in [
            Outcome::Undecodable("nope".into()),
            Outcome::Failed(DomainError::invalid("bad")),
            Outcome::Failed(DomainError::not_found()),
            Outcome::Failed(DomainError::forbidden("no")),
            Outcome::Failed(DomainError::unauthenticated("who")),
        ] {
            assert_eq!(
                policy.decide(&permanent, 1),
                Decision::Term,
                "{permanent:?}"
            );
        }
        for transient in [
            DomainError::internal("db"),
            DomainError::stale("moved"),
            DomainError::conflict("raced"),
        ] {
            assert_eq!(
                policy.decide(&Outcome::Failed(transient), 1),
                Decision::Nak(Duration::from_secs(1))
            );
        }
    }

    #[test]
    fn the_backoff_repeats_its_last_delay_until_max_deliver() {
        let policy = InboundConfig::default().failures("x");
        let failed = Outcome::Failed(DomainError::internal("db"));
        let decisions: Vec<Decision> = (1..=6).map(|n| policy.decide(&failed, n)).collect();
        assert_eq!(
            decisions,
            [
                Decision::Nak(Duration::from_secs(1)),
                Decision::Nak(Duration::from_secs(5)),
                Decision::Nak(Duration::from_secs(30)),
                Decision::Nak(Duration::from_secs(30)),
                Decision::Term,
                Decision::Term,
            ]
        );
    }

    #[test]
    fn a_dead_letter_keeps_the_original_headers_and_says_why() {
        let original = BTreeMap::from([
            ("Nats-Msg-Id".to_string(), "e-1".to_string()),
            ("Nats-Expected-Stream".to_string(), "SHOP".to_string()),
            ("event-id".to_string(), "e-1".to_string()),
            ("caller-subject".to_string(), "u-7".to_string()),
        ]);
        let headers =
            dead_letter_headers(&original, "invalid: zero", "SHOP.shop.orders.created", 5);
        assert_eq!(headers["event-id"], "e-1");
        assert_eq!(headers["caller-subject"], "u-7");
        assert_eq!(headers[DEAD_LETTER_REASON], "invalid: zero");
        assert_eq!(headers[DEAD_LETTER_SUBJECT], "SHOP.shop.orders.created");
        assert_eq!(headers[DEAD_LETTER_DELIVERIES], "5");
        assert!(
            !headers.contains_key("Nats-Msg-Id"),
            "or JetStream drops it as a duplicate"
        );
        assert!(!headers.contains_key("Nats-Expected-Stream"));
    }
}
