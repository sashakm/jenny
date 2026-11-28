//! Consuming from JetStream against a real NATS (ADR-006, ADR-032): a round trip with
//! headers, ack, nak and redelivery, term with a dead-letter copy, order, shutdown with a
//! message in hand, an idempotent consumer across restarts, a missing stream, and trust
//! over an anonymous versus an authenticated connection.
//!
//! Needs `JENNY_TEST_NATS_URL` (an anonymous NATS with JetStream), and for the trust test
//! `JENNY_TEST_NATS_TLS_URL` / `JENNY_TEST_NATS_TLS_DIR` as `tls_test` does. `just
//! test-nats` starts both. Without them the tests say so and pass, except under `CI`.
//! Each test uses a stream and subjects of its own and deletes the stream.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use async_nats::jetstream;
use futures::StreamExt;
use jenny_core::domain::{DomainError, Principal};
use jenny_core::port::{Context, EventCaller, InboundEvent};
use jenny_messaging::NatsConnectionConfig;
use jenny_messaging::connection::NatsTlsConfig;
use jenny_messaging::consumer::{InboundConfig, JetStreamConsumer};
use jenny_messaging::kv::Nats;
use jenny_messaging::{MessagingAdapter, SubjectBinding};
use serde::Deserialize;
use tokio::sync::{mpsc, watch};

fn nats_url() -> Option<String> {
    match std::env::var("JENNY_TEST_NATS_URL") {
        Ok(url) => Some(url),
        Err(_) if std::env::var_os("CI").is_some() => {
            panic!("JENNY_TEST_NATS_URL must be set in CI")
        }
        Err(_) => {
            eprintln!("skipping: JENNY_TEST_NATS_URL is not set (try `just test-nats`)");
            None
        }
    }
}

/// A stream of its own over `<prefix>.>`, on `nats`.
struct Fixture {
    nats: Nats,
    jetstream: jetstream::Context,
    stream: String,
    prefix: String,
}

impl Fixture {
    async fn on(nats: Nats) -> Self {
        let tag = uuid::Uuid::new_v4().simple().to_string();
        let prefix = format!("in{}", &tag[..12]);
        let stream = prefix.to_uppercase();
        let jetstream = jetstream::new(nats.client().await.unwrap());
        jetstream
            .create_stream(jetstream::stream::Config {
                name: stream.clone(),
                subjects: vec![format!("{prefix}.>")],
                ..Default::default()
            })
            .await
            .unwrap();
        Self {
            nats,
            jetstream,
            stream,
            prefix,
        }
    }

    async fn new() -> Option<Self> {
        let url = nats_url()?;
        Some(Self::on(Nats::new(NatsConnectionConfig::new(url), "inbound-test")).await)
    }

    fn subject(&self, rest: &str) -> String {
        format!("{}.{rest}", self.prefix)
    }

    async fn publish(&self, rest: &str, payload: serde_json::Value, headers: &[(&str, &str)]) {
        let mut map = async_nats::HeaderMap::new();
        for (name, value) in headers {
            map.insert(*name, *value);
        }
        self.jetstream
            .publish_with_headers(self.subject(rest), map, payload.to_string().into())
            .await
            .unwrap()
            .await
            .unwrap();
    }

    fn adapter(&self) -> MessagingAdapter {
        MessagingAdapter::new("orders", &self.prefix)
            .unwrap()
            .stream(&self.stream)
    }

    async fn consume(
        &self,
        adapter: &MessagingAdapter,
        config: &InboundConfig,
    ) -> (watch::Sender<bool>, tokio::task::JoinHandle<()>) {
        let consumer = JetStreamConsumer::bind(
            &self.nats,
            &format!("svc{}", &self.prefix[2..]),
            adapter,
            config,
            Principal::system("svc", "events"),
            None,
        )
        .await
        .unwrap()
        .expect("the adapter has handlers");
        let (stop, stopping) = watch::channel(false);
        (stop, consumer.spawn(stopping))
    }

    async fn pending(&self) -> (u64, usize) {
        let stream = self.jetstream.get_stream(&self.stream).await.unwrap();
        let mut consumer: jetstream::consumer::PullConsumer = stream
            .get_consumer(&format!("svc{}-orders", &self.prefix[2..]))
            .await
            .unwrap();
        let info = consumer.info().await.unwrap();
        (info.num_pending, info.num_ack_pending)
    }

    async fn delete(self) {
        let _ = self.jetstream.delete_stream(&self.stream).await;
    }
}

#[derive(Debug, Deserialize)]
struct Order {
    id: u32,
}

/// What a handler saw of one delivery.
#[derive(Debug)]
struct Seen {
    id: u32,
    principal: String,
    request_id: uuid::Uuid,
    event: InboundEvent,
    caller: Option<EventCaller>,
}

/// A handler that reports each delivery and answers with `answer(id, attempt)`.
fn reporting(
    answer: impl Fn(u32, u64) -> Result<(), DomainError> + Send + Sync + 'static,
) -> (
    impl Fn(Order, Context) -> futures::future::BoxFuture<'static, Result<(), DomainError>>
    + Send
    + Sync
    + 'static,
    mpsc::UnboundedReceiver<Seen>,
) {
    let (tx, rx) = mpsc::unbounded_channel();
    let answer = Arc::new(answer);
    let handler = move |order: Order, cx: Context| {
        let tx = tx.clone();
        let answer = answer.clone();
        Box::pin(async move {
            let event = cx.get::<InboundEvent>().cloned().unwrap();
            let attempt = event.attempt;
            let _ = tx.send(Seen {
                id: order.id,
                principal: cx.principal().map(Principal::subject).unwrap_or_default(),
                request_id: cx.request_id(),
                event,
                caller: cx.get::<EventCaller>().cloned(),
            });
            answer(order.id, attempt)
        }) as futures::future::BoxFuture<'static, Result<(), DomainError>>
    };
    (handler, rx)
}

async fn next(rx: &mut mpsc::UnboundedReceiver<Seen>) -> Seen {
    tokio::time::timeout(Duration::from_secs(15), rx.recv())
        .await
        .expect("a delivery in time")
        .expect("the handler is alive")
}

async fn eventually(mut done: impl AsyncFnMut() -> bool) {
    for _ in 0..100 {
        if done().await {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("not in time");
}

#[tokio::test]
async fn a_message_round_trips_with_its_headers_and_is_acked() {
    let Some(db) = Fixture::new().await else {
        return;
    };
    let (handler, mut seen) = reporting(|_, _| Ok(()));
    let adapter = db
        .adapter()
        .on(SubjectBinding::new("orders.created"), handler);
    let (stop, running) = db.consume(&adapter, &InboundConfig::default()).await;

    let correlation = uuid::Uuid::new_v4().to_string();
    let event_id = uuid::Uuid::new_v4().to_string();
    db.publish(
        "orders.created",
        serde_json::json!({ "id": 7 }),
        &[
            ("event-id", &event_id),
            ("event-name", "order.created"),
            ("correlation-id", &correlation),
            ("caller-subject", "u-7"),
        ],
    )
    .await;

    let seen = next(&mut seen).await;
    assert_eq!(seen.id, 7);
    assert_eq!(seen.principal, "System[svc/events]");
    assert_eq!(seen.request_id.to_string(), correlation);
    assert_eq!(seen.event.id.unwrap().to_string(), event_id);
    assert_eq!(seen.event.name, "order.created");
    assert_eq!(seen.event.attempt, 1);
    assert_eq!(
        seen.caller, None,
        "an anonymous connection's caller headers are dropped"
    );
    eventually(async || db.pending().await == (0, 0)).await;

    stop.send(true).unwrap();
    running.await.unwrap();
    db.delete().await;
}

#[tokio::test]
async fn a_transient_failure_is_redelivered_until_it_succeeds() {
    let Some(db) = Fixture::new().await else {
        return;
    };
    let (handler, mut seen) = reporting(|_, attempt| {
        if attempt < 2 {
            Err(DomainError::internal("the database blinked"))
        } else {
            Ok(())
        }
    });
    let adapter = db
        .adapter()
        .on(SubjectBinding::new("orders.created"), handler);
    let config: InboundConfig =
        serde_json::from_value(serde_json::json!({ "backoff": [1] })).unwrap();
    let (stop, running) = db.consume(&adapter, &config).await;

    db.publish("orders.created", serde_json::json!({ "id": 1 }), &[])
        .await;
    assert_eq!(next(&mut seen).await.event.attempt, 1);
    let again = next(&mut seen).await;
    assert_eq!(
        (again.id, again.event.attempt),
        (1, 2),
        "redelivered after the nak"
    );
    eventually(async || db.pending().await == (0, 0)).await;

    stop.send(true).unwrap();
    running.await.unwrap();
    db.delete().await;
}

#[tokio::test]
async fn after_max_deliver_the_message_is_dead_lettered_and_terminated() {
    let Some(db) = Fixture::new().await else {
        return;
    };
    let dead = db.subject("dead");
    let client = db.nats.client().await.unwrap();
    let mut dead_letters = client.subscribe(dead.clone()).await.unwrap();

    let (handler, mut seen) = reporting(|_, _| Err(DomainError::conflict("always")));
    let adapter = db
        .adapter()
        .on(SubjectBinding::new("orders.created"), handler);
    let config: InboundConfig = serde_json::from_value(serde_json::json!({
        "subscriptions": [{
            "subject": db.subject("orders.created"),
            "max_deliver": 2,
            "backoff": [1],
            "dead_letter": dead,
        }]
    }))
    .unwrap();
    let (stop, running) = db.consume(&adapter, &config).await;

    db.publish(
        "orders.created",
        serde_json::json!({ "id": 3 }),
        &[("event-id", "e-3")],
    )
    .await;
    assert_eq!(next(&mut seen).await.event.attempt, 1);
    assert_eq!(next(&mut seen).await.event.attempt, 2);

    let copy = tokio::time::timeout(Duration::from_secs(10), dead_letters.next())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(copy.payload.as_ref(), br#"{"id":3}"#);
    let headers = copy.headers.unwrap();
    assert_eq!(headers.get("event-id").unwrap().as_str(), "e-3");
    assert!(
        headers
            .get("dead-letter-reason")
            .unwrap()
            .as_str()
            .contains("always")
    );
    assert_eq!(
        headers.get("dead-letter-subject").unwrap().as_str(),
        db.subject("orders.created")
    );
    assert_eq!(headers.get("dead-letter-deliveries").unwrap().as_str(), "2");
    eventually(async || db.pending().await == (0, 0)).await;
    // Terminated, so never delivered again.
    assert!(
        tokio::time::timeout(Duration::from_secs(2), seen.recv())
            .await
            .is_err()
    );

    stop.send(true).unwrap();
    running.await.unwrap();
    db.delete().await;
}

#[tokio::test]
async fn an_undecodable_payload_is_terminated_at_once() {
    let Some(db) = Fixture::new().await else {
        return;
    };
    let (handler, mut seen) = reporting(|_, _| Ok(()));
    let adapter = db.adapter().on(SubjectBinding::new("orders.*"), handler);
    let (stop, running) = db.consume(&adapter, &InboundConfig::default()).await;

    db.publish("orders.created", serde_json::json!({ "no": "id" }), &[])
        .await;
    db.publish("orders.cancelled", serde_json::json!({ "id": 2 }), &[])
        .await;
    assert_eq!(
        next(&mut seen).await.id,
        2,
        "the first never reached the handler"
    );
    eventually(async || db.pending().await == (0, 0)).await;

    stop.send(true).unwrap();
    running.await.unwrap();
    db.delete().await;
}

#[tokio::test]
async fn one_in_flight_keeps_the_stream_order() {
    let Some(db) = Fixture::new().await else {
        return;
    };
    let (handler, mut seen) = reporting(|_, _| Ok(()));
    let adapter = db
        .adapter()
        .on(SubjectBinding::new("orders.created"), handler);
    for id in 1..=20 {
        db.publish("orders.created", serde_json::json!({ "id": id }), &[])
            .await;
    }
    let (stop, running) = db.consume(&adapter, &InboundConfig::default()).await;

    let mut order = Vec::new();
    for _ in 1..=20 {
        order.push(next(&mut seen).await.id);
    }
    assert_eq!(order, (1..=20).collect::<Vec<_>>());

    stop.send(true).unwrap();
    running.await.unwrap();
    db.delete().await;
}

#[tokio::test]
async fn shutdown_finishes_and_acks_the_message_in_hand() {
    let Some(db) = Fixture::new().await else {
        return;
    };
    let (started_tx, mut started) = mpsc::unbounded_channel();
    let finished = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let done = finished.clone();
    let adapter = db.adapter().on(
        SubjectBinding::new("orders.created"),
        move |_: Order, _: Context| {
            let started_tx = started_tx.clone();
            let done = done.clone();
            async move {
                let _ = started_tx.send(());
                tokio::time::sleep(Duration::from_millis(800)).await;
                done.store(true, std::sync::atomic::Ordering::SeqCst);
                Ok::<(), DomainError>(())
            }
        },
    );
    let (stop, running) = db.consume(&adapter, &InboundConfig::default()).await;
    db.publish("orders.created", serde_json::json!({ "id": 1 }), &[])
        .await;
    tokio::time::timeout(Duration::from_secs(10), started.recv())
        .await
        .unwrap();

    stop.send(true).unwrap();
    running.await.unwrap();
    assert!(
        finished.load(std::sync::atomic::Ordering::SeqCst),
        "finished before stopping"
    );
    assert_eq!(db.pending().await, (0, 0), "and acked");
    db.delete().await;
}

#[tokio::test]
async fn the_consumer_is_created_once_and_updated_on_restart() {
    let Some(db) = Fixture::new().await else {
        return;
    };
    let (handler, _seen) = reporting(|_, _| Ok(()));
    let first = db
        .adapter()
        .on(SubjectBinding::new("orders.created"), handler);
    let (stop, running) = db.consume(&first, &InboundConfig::default()).await;
    stop.send(true).unwrap();
    running.await.unwrap();

    // The same service and adapter after a deploy: one more subject, same consumer.
    let (handler, mut seen) = reporting(|_, _| Ok(()));
    let (other, _) = reporting(|_, _| Ok(()));
    let second = db
        .adapter()
        .on(SubjectBinding::new("orders.created"), handler)
        .on(SubjectBinding::new("orders.cancelled"), other);
    let (stop, running) = db.consume(&second, &InboundConfig::default()).await;

    let stream = db.jetstream.get_stream(&db.stream).await.unwrap();
    let names: Vec<String> = stream
        .consumer_names()
        .collect::<Vec<_>>()
        .await
        .into_iter()
        .map(Result::unwrap)
        .collect();
    assert_eq!(names.len(), 1, "{names:?}");
    let mut consumer: jetstream::consumer::PullConsumer =
        stream.get_consumer(&names[0]).await.unwrap();
    let mut filters = consumer
        .info()
        .await
        .unwrap()
        .config
        .filter_subjects
        .clone();
    filters.sort();
    assert_eq!(
        filters,
        [db.subject("orders.cancelled"), db.subject("orders.created")]
    );

    db.publish("orders.created", serde_json::json!({ "id": 5 }), &[])
        .await;
    assert_eq!(next(&mut seen).await.id, 5);
    stop.send(true).unwrap();
    running.await.unwrap();
    db.delete().await;
}

#[tokio::test]
async fn a_missing_stream_stops_the_service_naming_it() {
    let Some(url) = nats_url() else {
        return;
    };
    let nats = Nats::new(NatsConnectionConfig::new(url), "inbound-test");
    let (handler, _seen) = reporting(|_, _| Ok(()));
    let adapter = MessagingAdapter::new("orders", "nowhere")
        .unwrap()
        .stream("NO_SUCH_STREAM_FOR_JENNY")
        .on(SubjectBinding::new("orders.created"), handler);
    let err = JetStreamConsumer::bind(
        &nats,
        "svc",
        &adapter,
        &InboundConfig::default(),
        Principal::system("svc", "events"),
        None,
    )
    .await
    .unwrap_err();
    assert!(
        err.to_string().contains("`NO_SUCH_STREAM_FOR_JENNY`"),
        "{err}"
    );
}

#[tokio::test]
async fn envelope_over_an_anonymous_connection_stops_the_service_naming_the_subscription() {
    let Some(db) = Fixture::new().await else {
        return;
    };
    assert!(!db.nats.is_authenticated().await.unwrap());
    let (handler, _seen) = reporting(|_, _| Ok(()));
    let adapter = db
        .adapter()
        .on(SubjectBinding::new("orders.created"), handler);
    let config: InboundConfig =
        serde_json::from_value(serde_json::json!({ "principal": "envelope" })).unwrap();
    let err = JetStreamConsumer::bind(
        &db.nats,
        "svc",
        &adapter,
        &config,
        Principal::system("svc", "events"),
        None,
    )
    .await
    .unwrap_err();
    let message = err.to_string();
    assert!(message.contains(&db.subject("orders.created")), "{message}");
    assert!(message.contains("unauthenticated"), "{message}");
    db.delete().await;
}

fn tls_server() -> Option<(String, PathBuf)> {
    match (
        std::env::var("JENNY_TEST_NATS_TLS_URL"),
        std::env::var("JENNY_TEST_NATS_TLS_DIR"),
    ) {
        (Ok(url), Ok(dir)) => Some((url, PathBuf::from(dir))),
        _ if std::env::var_os("CI").is_some() => {
            panic!("JENNY_TEST_NATS_TLS_URL and JENNY_TEST_NATS_TLS_DIR must be set in CI")
        }
        _ => {
            eprintln!("skipping: JENNY_TEST_NATS_TLS_URL / JENNY_TEST_NATS_TLS_DIR are not set");
            None
        }
    }
}

#[tokio::test]
async fn over_an_authenticated_connection_the_caller_is_trusted_and_can_be_restored() {
    let Some((url, dir)) = tls_server() else {
        return;
    };
    let mut config = NatsConnectionConfig::new(url);
    config.tls = Some(NatsTlsConfig {
        ca: Some(dir.join("ca.crt")),
        cert: Some(dir.join("client-a.crt")),
        key: Some(dir.join("client-a.key")),
        ..NatsTlsConfig::default()
    });
    let db = Fixture::on(Nats::new(config, "inbound-tls-test")).await;
    assert!(db.nats.is_authenticated().await.unwrap());

    let (handler, mut seen) = reporting(|_, _| Ok(()));
    let adapter = db
        .adapter()
        .on(SubjectBinding::new("orders.created"), handler);
    let inbound: InboundConfig =
        serde_json::from_value(serde_json::json!({ "principal": "envelope" })).unwrap();
    let (stop, running) = db.consume(&adapter, &inbound).await;

    db.publish(
        "orders.created",
        serde_json::json!({ "id": 9 }),
        &[("caller-subject", "u-7"), ("caller-actor", "service-a")],
    )
    .await;
    let seen = next(&mut seen).await;
    assert_eq!(seen.principal, "u-7", "restored");
    assert_eq!(
        seen.caller,
        Some(EventCaller {
            subject: "u-7".into(),
            actor: Some("service-a".into())
        })
    );

    stop.send(true).unwrap();
    running.await.unwrap();
    db.delete().await;
}
