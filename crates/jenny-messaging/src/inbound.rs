//! Inbound dispatch: one received event in, a [`Context`] and a typed handler call out.
//!
//! Both sources a service consumes from use this core: the in-process bus
//! (`JennyServer::handle`) and JetStream ([`MessagingAdapter::on`](crate::MessagingAdapter::on)).
//! What differs between them is what they do with the [`Outcome`]: the bus logs a
//! failure, JetStream acks, naks or terminates the message.
//!
//! ### The handler
//!
//! A handler is a driving adapter, as thin as a REST handler: it gets the message
//! decoded into the adapter's own DTO `M`, converts it into the domain command a REST DTO
//! would become, and calls the use case (ADR-001, ADR-006).
//!
//! ```
//! use std::sync::Arc;
//! use async_trait::async_trait;
//! use jenny_core::domain::DomainError;
//! use jenny_core::port::Context;
//! use jenny_messaging::inbound::Handler;
//!
//! # struct Notes; impl Notes { async fn archive(&self, _: u32, _: &Context) -> Result<(), DomainError> { Ok(()) } }
//! #[derive(serde::Deserialize)]
//! struct OrderCancelled { note: u32 }
//!
//! struct ArchiveOnCancel(Arc<Notes>);
//!
//! #[async_trait]
//! impl Handler<OrderCancelled> for ArchiveOnCancel {
//!     async fn handle(&self, event: OrderCancelled, cx: &Context) -> Result<(), DomainError> {
//!         self.0.archive(event.note, cx).await
//!     }
//! }
//! ```
//!
//! A closure works too. It gets the [`Context`] by value, because stable Rust cannot
//! prove that a closure's future borrowing it is `Send`:
//!
//! ```
//! # use std::sync::Arc;
//! # use jenny_core::domain::DomainError;
//! # use jenny_core::port::Context;
//! # struct Notes; impl Notes { async fn archive(&self, _: u32, _: &Context) -> Result<(), DomainError> { Ok(()) } }
//! # #[derive(serde::Deserialize)] struct OrderCancelled { note: u32 }
//! # fn wire(notes: Arc<Notes>) {
//! let handler = move |event: OrderCancelled, cx: Context| {
//!     let notes = notes.clone();
//!     async move { notes.archive(event.note, &cx).await }
//! };
//! # let _ = jenny_messaging::inbound::Dispatch::new(handler);
//! # }
//! ```
//!
//! ### The context
//!
//! - the request id is the event's `correlation-id` when that is a UUID, so what a
//!   handler raises is correlated with the request that started it all;
//! - the server's authorizer;
//! - an [`InboundEvent`] (id, name, occurred-at, attempt), for idempotency;
//! - an [`EventCaller`] from `caller-subject` / `caller-actor`, only when the transport is
//!   trusted;
//! - the principal, per [`PrincipalPolicy`]: the service itself by default.

use std::collections::BTreeMap;
use std::future::Future;
use std::marker::PhantomData;
use std::sync::Arc;

use async_trait::async_trait;
use futures::future::BoxFuture;
use jenny_core::domain::{DomainError, Principal};
use jenny_core::port::outbound::Authorizer;
use jenny_core::port::{
    CALLER_ACTOR, CALLER_SUBJECT, Context, EventCaller, InboundEvent, RestoredCaller,
};
use serde::Deserialize;
use serde::de::DeserializeOwned;

use crate::headers;

/// Handles one kind of inbound message, decoded into `M`.
#[async_trait]
pub trait Handler<M>: Send + Sync + 'static {
    async fn handle(&self, message: M, cx: &Context) -> Result<(), DomainError>;
}

/// Who an inbound handler runs as.
#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum PrincipalPolicy {
    /// The service itself, `System[<service_name>/events]`. The event's caller is still
    /// in the context as an [`EventCaller`] when the transport is trusted.
    #[default]
    System,
    /// The event's caller, restored as a user with the id in `caller-subject` (and
    /// `act: { sub: caller-actor }`), marked [`RestoredCaller`], so the service's policies
    /// apply to whoever the event was raised for. It carries no roles or claims.
    ///
    /// Falls back to the system principal when there is no `caller-subject`, or when it
    /// names a system principal (`System[…]`): the service itself is never restored from
    /// a header. Needs a trusted transport.
    Envelope,
}

/// What became of one message.
#[derive(Debug)]
pub enum Outcome {
    /// The handler succeeded.
    Handled,
    /// The payload did not decode into the handler's type: retrying cannot help.
    Undecodable(String),
    /// The handler failed.
    Failed(DomainError),
}

impl Outcome {
    /// Whether a redelivery could succeed: a failure that depends on the moment
    /// (`Internal`, `Stale`, `Conflict`) rather than on the message.
    pub fn is_retryable(&self) -> bool {
        match self {
            Outcome::Handled | Outcome::Undecodable(_) => false,
            Outcome::Failed(error) => match error {
                DomainError::Internal { .. }
                | DomainError::Stale { .. }
                | DomainError::Conflict { .. } => true,
                DomainError::NotFound
                | DomainError::Invalid { .. }
                | DomainError::Forbidden { .. }
                | DomainError::Unauthenticated { .. } => false,
            },
        }
    }

    /// Why it did not succeed, for logs and the `dead-letter-reason` header.
    pub fn reason(&self) -> Option<String> {
        match self {
            Outcome::Handled => None,
            Outcome::Undecodable(reason) => Some(format!("undecodable: {reason}")),
            Outcome::Failed(error) => Some(error.to_string()),
        }
    }
}

/// A handler, its message type erased: decodes a payload and calls it.
#[derive(Clone)]
pub struct Dispatch(Arc<dyn Erased>);

trait Erased: Send + Sync + 'static {
    fn call(&self, payload: &[u8], cx: Context) -> BoxFuture<'static, Outcome>;
}

impl std::fmt::Debug for Dispatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Dispatch").finish_non_exhaustive()
    }
}

impl Dispatch {
    /// Wrap a [`Handler`] or a closure `Fn(M, Context) -> impl Future<Output =
    /// Result<(), DomainError>>`.
    pub fn new<M, K>(handler: impl IntoHandler<M, K>) -> Self {
        handler.into_dispatch()
    }

    /// Decode `payload` as JSON and run the handler with `cx`.
    pub async fn call(&self, payload: &[u8], cx: Context) -> Outcome {
        self.0.call(payload, cx).await
    }
}

/// What can be a handler: a [`Handler`] (`K` = [`ByRef`]) or a closure taking the
/// context by value (`K` = [`ByValue`]). `K` is inferred.
pub trait IntoHandler<M, K>: Send + Sync + 'static {
    fn into_dispatch(self) -> Dispatch;
}

/// Marks a [`Handler`] implementation for [`IntoHandler`].
pub enum ByRef {}
/// Marks a closure for [`IntoHandler`].
pub enum ByValue {}

struct Typed<M, H>(Arc<H>, PhantomData<fn(M)>);

fn decode<M: DeserializeOwned>(payload: &[u8]) -> Result<M, Outcome> {
    serde_json::from_slice(payload).map_err(|e| Outcome::Undecodable(e.to_string()))
}

impl<M, H> Erased for Typed<M, H>
where
    M: DeserializeOwned + Send + 'static,
    H: Handler<M>,
{
    fn call(&self, payload: &[u8], cx: Context) -> BoxFuture<'static, Outcome> {
        let handler = self.0.clone();
        let message = decode::<M>(payload);
        Box::pin(async move {
            match message {
                Ok(message) => match handler.handle(message, &cx).await {
                    Ok(()) => Outcome::Handled,
                    Err(error) => Outcome::Failed(error),
                },
                Err(outcome) => outcome,
            }
        })
    }
}

impl<M, H> IntoHandler<M, ByRef> for H
where
    M: DeserializeOwned + Send + 'static,
    H: Handler<M>,
{
    fn into_dispatch(self) -> Dispatch {
        Dispatch(Arc::new(Typed::<M, H>(Arc::new(self), PhantomData)))
    }
}

struct Closure<M, F>(Arc<F>, PhantomData<fn(M)>);

impl<M, F, Fut> Erased for Closure<M, F>
where
    M: DeserializeOwned + Send + 'static,
    F: Fn(M, Context) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<(), DomainError>> + Send + 'static,
{
    fn call(&self, payload: &[u8], cx: Context) -> BoxFuture<'static, Outcome> {
        match decode::<M>(payload) {
            Ok(message) => {
                let running = (self.0)(message, cx);
                Box::pin(async move {
                    match running.await {
                        Ok(()) => Outcome::Handled,
                        Err(error) => Outcome::Failed(error),
                    }
                })
            }
            Err(outcome) => Box::pin(async move { outcome }),
        }
    }
}

impl<M, F, Fut> IntoHandler<M, ByValue> for F
where
    M: DeserializeOwned + Send + 'static,
    F: Fn(M, Context) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<(), DomainError>> + Send + 'static,
{
    fn into_dispatch(self) -> Dispatch {
        Dispatch(Arc::new(Closure::<M, F>(Arc::new(self), PhantomData)))
    }
}

/// One received event, whatever it came over.
#[derive(Clone, Debug, Default)]
pub struct Received {
    /// The message's headers: the reserved ones the bus writes and the envelope's own.
    pub headers: BTreeMap<String, String>,
    pub payload: Vec<u8>,
    /// Which delivery this is, from 1.
    pub attempt: u64,
    /// Where it arrived, for the event name when the message carries none.
    pub subject: String,
}

impl Received {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .get(name)
            .map(String::as_str)
            .filter(|value| !value.is_empty())
    }
}

/// Turns a [`Received`] event into a [`Context`] and runs a [`Dispatch`] with it.
#[derive(Clone)]
pub struct Dispatcher {
    system: Principal,
    authorizer: Option<Arc<dyn Authorizer>>,
    policy: PrincipalPolicy,
    trusted: bool,
}

impl std::fmt::Debug for Dispatcher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Dispatcher")
            .field("system", &self.system)
            .field("authorizer", &self.authorizer.is_some())
            .field("policy", &self.policy)
            .field("trusted", &self.trusted)
            .finish()
    }
}

impl Dispatcher {
    /// Handlers run as `system`, usually `Principal::system(service, "events")`, over a
    /// transport whose caller headers are `trusted` or not.
    pub fn new(system: Principal, trusted: bool) -> Self {
        Self {
            system,
            authorizer: None,
            policy: PrincipalPolicy::System,
            trusted,
        }
    }

    pub fn with_authorizer(mut self, authorizer: Option<Arc<dyn Authorizer>>) -> Self {
        self.authorizer = authorizer;
        self
    }

    /// Fails for [`PrincipalPolicy::Envelope`] over an untrusted transport: anyone could
    /// have written the headers it would restore.
    pub fn with_policy(mut self, policy: PrincipalPolicy) -> anyhow::Result<Self> {
        if policy == PrincipalPolicy::Envelope && !self.trusted {
            anyhow::bail!(
                "`principal: envelope` restores the caller from the event's headers, which \
                 an unauthenticated NATS connection cannot trust"
            );
        }
        self.policy = policy;
        Ok(self)
    }

    pub fn policy(&self) -> PrincipalPolicy {
        self.policy
    }

    pub fn is_trusted(&self) -> bool {
        self.trusted
    }

    /// The context a handler of `received` runs with.
    pub fn context(&self, received: &Received) -> Context {
        let request_id = received
            .header(headers::CORRELATION_ID)
            .and_then(|id| uuid::Uuid::parse_str(id).ok())
            .unwrap_or_else(uuid::Uuid::new_v4);
        let event = InboundEvent {
            id: received
                .header(headers::EVENT_ID)
                .and_then(|id| uuid::Uuid::parse_str(id).ok()),
            name: received
                .header(headers::EVENT_NAME)
                .unwrap_or(&received.subject)
                .to_string(),
            occurred_at: received
                .header(headers::OCCURRED_AT)
                .and_then(|at| chrono::DateTime::parse_from_rfc3339(at).ok())
                .map(|at| at.with_timezone(&chrono::Utc)),
            attempt: received.attempt.max(1),
        };
        let caller = self
            .trusted
            .then(|| received.header(CALLER_SUBJECT))
            .flatten()
            .map(|subject| EventCaller {
                subject: subject.to_string(),
                actor: received.header(CALLER_ACTOR).map(str::to_string),
            });

        let mut cx = Context::new().with_request_id(request_id).with(event);
        if let Some(authorizer) = &self.authorizer {
            cx = cx.with_authorizer(authorizer.clone());
        }
        let restored = match (&self.policy, &caller) {
            (PrincipalPolicy::Envelope, Some(caller)) if caller.subject.starts_with("System[") => {
                tracing::debug!(
                    subject = caller.subject,
                    "not restoring a system principal from an event header; running as the service"
                );
                None
            }
            (PrincipalPolicy::Envelope, Some(caller)) => Some(restore(caller)),
            (PrincipalPolicy::Envelope, None) => {
                tracing::debug!(
                    event = %cx.get::<InboundEvent>().map(|e| e.name.as_str()).unwrap_or_default(),
                    "no caller-subject to restore; running as the service"
                );
                None
            }
            (PrincipalPolicy::System, _) => None,
        };
        cx = match restored {
            Some(principal) => cx.with_principal(principal).with(RestoredCaller),
            None => cx.with_principal(self.system.clone()),
        };
        if let Some(caller) = caller {
            cx = cx.with(caller);
        }
        cx
    }

    /// Build the context for `received` and run `handler` with it.
    pub async fn dispatch(&self, handler: &Dispatch, received: &Received) -> Outcome {
        let cx = self.context(received);
        handler.call(&received.payload, cx).await
    }
}

/// A typed handler on the in-process bus, for a topic pattern.
///
/// The bus's headers are written by this process, so they are always trusted and the
/// `envelope` policy is allowed. A failed handler or an undecodable payload is logged
/// with the event's id and name; there is no redelivery.
#[derive(Debug)]
pub struct BusHandler {
    pattern: String,
    dispatch: Dispatch,
    dispatcher: Dispatcher,
}

impl BusHandler {
    /// `dispatcher` must have been made trusted (`Dispatcher::new(system, true)`).
    pub fn new(pattern: impl Into<String>, dispatch: Dispatch, dispatcher: Dispatcher) -> Self {
        Self {
            pattern: pattern.into(),
            dispatch,
            dispatcher,
        }
    }

    pub fn pattern(&self) -> &str {
        &self.pattern
    }
}

#[async_trait]
impl topmesys::EventConsumer for BusHandler {
    type Topic = ();

    fn subscriptions(&self) -> topmesys::Subscriptions<()> {
        // No retries: a bus handler is told once, like any other listener here.
        topmesys::Subscription::new(self.pattern.clone())
            .with_retry_policy(topmesys::RetryPolicy::none())
            .into()
    }

    async fn handle_event(
        &self,
        _: &(),
        delivery: &topmesys::Delivery,
    ) -> Result<(), topmesys::HandlerError> {
        let message = delivery.message();
        let received = Received {
            headers: message.headers().clone(),
            payload: message.content().to_vec(),
            attempt: u64::from(delivery.attempt()).max(1),
            subject: message.topic().text().to_string(),
        };
        let outcome = self.dispatcher.dispatch(&self.dispatch, &received).await;
        if let Some(reason) = outcome.reason() {
            tracing::warn!(
                pattern = self.pattern,
                event_id = received.header(headers::EVENT_ID),
                event = received.header(headers::EVENT_NAME),
                "bus handler failed, not retried: {reason}"
            );
        }
        Ok(())
    }
}

/// A thin user with the subject's id, and `act` when someone acted for them.
fn restore(caller: &EventCaller) -> Principal {
    let builder = Principal::builder(&caller.subject);
    match &caller.actor {
        Some(actor) => builder.with_attribute("act", serde_json::json!({ "sub": actor })),
        None => builder,
    }
    .build()
}

#[cfg(test)]
mod tests {
    use super::*;
    use jenny_core::domain::PrincipalKind;
    use jenny_core::port::{Caller, Events};
    use jenny_core::testing::RecordedEvents;
    use std::sync::Mutex;

    fn received(headers: &[(&str, &str)]) -> Received {
        Received {
            headers: headers
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            payload: br#"{"id":7}"#.to_vec(),
            attempt: 2,
            subject: "SHOP.shop.order.created".into(),
        }
    }

    fn system() -> Principal {
        Principal::system("my-notes", "events")
    }

    #[test]
    fn the_request_id_is_the_correlation_id_when_it_is_a_uuid() {
        let id = uuid::Uuid::new_v4();
        let dispatcher = Dispatcher::new(system(), true);
        let cx = dispatcher.context(&received(&[(headers::CORRELATION_ID, &id.to_string())]));
        assert_eq!(cx.request_id(), id);

        let cx = dispatcher.context(&received(&[(headers::CORRELATION_ID, "not-a-uuid")]));
        assert_ne!(cx.request_id(), uuid::Uuid::nil());
        assert_ne!(cx.request_id().to_string(), "not-a-uuid");
    }

    #[test]
    fn the_inbound_event_describes_the_delivery() {
        let id = uuid::Uuid::new_v4();
        let cx = Dispatcher::new(system(), true).context(&received(&[
            (headers::EVENT_ID, &id.to_string()),
            (headers::EVENT_NAME, "order.created"),
            (headers::OCCURRED_AT, "2026-11-28T10:00:00+00:00"),
        ]));
        let event = cx.get::<InboundEvent>().unwrap();
        assert_eq!(event.id, Some(id));
        assert_eq!(event.name, "order.created");
        assert_eq!(
            event.occurred_at.unwrap().to_rfc3339(),
            "2026-11-28T10:00:00+00:00"
        );
        assert_eq!(event.attempt, 2);

        let bare = Dispatcher::new(system(), true).context(&received(&[]));
        assert_eq!(
            bare.get::<InboundEvent>().unwrap().name,
            "SHOP.shop.order.created"
        );
    }

    #[test]
    fn by_default_the_service_handles_it_and_the_caller_is_only_noted() {
        let cx = Dispatcher::new(system(), true)
            .context(&received(&[(CALLER_SUBJECT, "u-7"), (CALLER_ACTOR, "bff")]));
        assert_eq!(cx.principal().unwrap().subject(), "System[my-notes/events]");
        assert!(cx.get::<RestoredCaller>().is_none());
        assert_eq!(
            cx.get::<EventCaller>(),
            Some(&EventCaller {
                subject: "u-7".into(),
                actor: Some("bff".into())
            })
        );
    }

    #[test]
    fn untrusted_caller_headers_are_dropped() {
        let cx = Dispatcher::new(system(), false).context(&received(&[(CALLER_SUBJECT, "u-7")]));
        assert!(cx.get::<EventCaller>().is_none());
        assert!(cx.principal().unwrap().is_system());
    }

    #[test]
    fn envelope_needs_a_trusted_transport() {
        let err = Dispatcher::new(system(), false)
            .with_policy(PrincipalPolicy::Envelope)
            .unwrap_err();
        assert!(err.to_string().contains("unauthenticated"), "{err}");
        assert!(
            Dispatcher::new(system(), true)
                .with_policy(PrincipalPolicy::Envelope)
                .is_ok()
        );
    }

    #[test]
    fn envelope_restores_a_thin_user_and_marks_it() {
        let dispatcher = Dispatcher::new(system(), true)
            .with_policy(PrincipalPolicy::Envelope)
            .unwrap();
        let cx = dispatcher.context(&received(&[(CALLER_SUBJECT, "u-7"), (CALLER_ACTOR, "bff")]));
        let principal = cx.principal().unwrap();
        assert_eq!(principal.kind(), PrincipalKind::User);
        assert_eq!(principal.id(), "u-7");
        assert_eq!(
            principal.raw_attribute("act"),
            Some(&serde_json::json!({ "sub": "bff" }))
        );
        assert_eq!(principal.attributes().len(), 1, "nothing but act");
        assert!(principal.roles().is_empty());
        assert!(cx.get::<RestoredCaller>().is_some());

        let without_actor = dispatcher.context(&received(&[(CALLER_SUBJECT, "u-7")]));
        assert!(without_actor.principal().unwrap().attributes().is_empty());
    }

    #[test]
    fn envelope_falls_back_to_the_service_without_a_subject_or_for_a_system_one() {
        let dispatcher = Dispatcher::new(system(), true)
            .with_policy(PrincipalPolicy::Envelope)
            .unwrap();
        for headers in [
            vec![],
            vec![(CALLER_SUBJECT, "")],
            vec![(CALLER_SUBJECT, "System[other/tasks]")],
        ] {
            let cx = dispatcher.context(&received(&headers));
            assert!(cx.principal().unwrap().is_system(), "{headers:?}");
            assert_eq!(cx.principal().unwrap().id(), "my-notes/events");
            assert!(cx.get::<RestoredCaller>().is_none());
        }
    }

    #[test]
    fn retryable_failures_are_the_ones_of_the_moment() {
        for retryable in [
            DomainError::internal("db down"),
            DomainError::stale("moved on"),
            DomainError::conflict("raced"),
        ] {
            assert!(Outcome::Failed(retryable).is_retryable());
        }
        for permanent in [
            DomainError::not_found(),
            DomainError::invalid("bad"),
            DomainError::forbidden("no"),
            DomainError::unauthenticated("who"),
        ] {
            assert!(!Outcome::Failed(permanent).is_retryable());
        }
        assert!(!Outcome::Undecodable("x".into()).is_retryable());
        assert!(!Outcome::Handled.is_retryable());
    }

    #[derive(Deserialize)]
    struct Order {
        id: u32,
    }

    struct Recording(Mutex<Vec<(u32, String)>>);

    #[async_trait]
    impl Handler<Order> for Arc<Recording> {
        async fn handle(&self, order: Order, cx: &Context) -> Result<(), DomainError> {
            self.0
                .lock()
                .unwrap()
                .push((order.id, cx.principal().unwrap().subject()));
            if order.id == 0 {
                return Err(DomainError::invalid("zero"));
            }
            Ok(())
        }
    }

    #[tokio::test]
    async fn a_handler_gets_the_decoded_message_and_the_context() {
        let recording = Arc::new(Recording(Mutex::default()));
        let dispatch = Dispatch::new::<Order, _>(recording.clone());
        let dispatcher = Dispatcher::new(system(), true);

        assert!(matches!(
            dispatcher.dispatch(&dispatch, &received(&[])).await,
            Outcome::Handled
        ));
        assert_eq!(
            recording.0.lock().unwrap().as_slice(),
            [(7, "System[my-notes/events]".to_string())]
        );

        let mut zero = received(&[]);
        zero.payload = br#"{"id":0}"#.to_vec();
        assert!(matches!(
            dispatcher.dispatch(&dispatch, &zero).await,
            Outcome::Failed(DomainError::Invalid { .. })
        ));

        let mut garbage = received(&[]);
        garbage.payload = b"not json".to_vec();
        assert!(matches!(
            dispatcher.dispatch(&dispatch, &garbage).await,
            Outcome::Undecodable(_)
        ));
    }

    #[tokio::test]
    async fn a_closure_is_a_handler() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let record = seen.clone();
        let dispatch = Dispatch::new(move |order: Order, cx: Context| {
            let record = record.clone();
            async move {
                record.lock().unwrap().push((order.id, cx.request_id()));
                Ok(())
            }
        });
        let id = uuid::Uuid::new_v4();
        let outcome = Dispatcher::new(system(), true)
            .dispatch(
                &dispatch,
                &received(&[(headers::CORRELATION_ID, &id.to_string())]),
            )
            .await;
        assert!(matches!(outcome, Outcome::Handled));
        assert_eq!(seen.lock().unwrap().as_slice(), [(7, id)]);
    }

    #[derive(serde::Serialize)]
    struct Shipped;

    impl jenny_core::domain::DomainEvent for Shipped {
        fn name(&self) -> &str {
            "order.shipped"
        }
    }

    /// Raises `order.shipped` with the context it was handed.
    fn raising(events: Events) -> Dispatch {
        Dispatch::new(move |_: Order, cx: Context| {
            let events = events.clone();
            async move {
                events.publish(&Shipped, &cx).await;
                Ok(())
            }
        })
    }

    #[tokio::test]
    async fn what_a_handler_raises_is_correlated_and_names_its_principal() {
        let recorded = Arc::new(RecordedEvents::default());
        let dispatch = raising(Events::new(recorded.clone()).enriched_with(Caller));
        let id = uuid::Uuid::new_v4();
        Dispatcher::new(system(), true)
            .dispatch(
                &dispatch,
                &received(&[
                    (headers::CORRELATION_ID, &id.to_string()),
                    (CALLER_SUBJECT, "u-7"),
                ]),
            )
            .await;
        let raised = &recorded.envelopes()[0];
        assert_eq!(raised.correlation_id(), Some(id));
        assert_eq!(raised.headers()[CALLER_SUBJECT], "System[my-notes/events]");
    }

    #[tokio::test]
    async fn a_delegated_chain_keeps_its_subject_and_actor_across_services() {
        // Service A: user u-7, acting through a token exchanged by service-a.
        let recorded_a = Arc::new(RecordedEvents::default());
        let events_a = Events::new(recorded_a.clone()).enriched_with(Caller);
        let cx_a = Context::new().with_principal(
            Principal::builder("u-7")
                .with_attribute("act", serde_json::json!({ "sub": "service-a" }))
                .build(),
        );
        events_a.publish(&Shipped, &cx_a).await;
        let first = &recorded_a.envelopes()[0];
        assert_eq!(first.headers()[CALLER_SUBJECT], "u-7");
        assert_eq!(first.headers()[CALLER_ACTOR], "service-a");

        // Service B, in `envelope` mode, raises another event from it.
        let recorded_b = Arc::new(RecordedEvents::default());
        let dispatch = raising(Events::new(recorded_b.clone()).enriched_with(Caller));
        let dispatcher = Dispatcher::new(Principal::system("service-b", "events"), true)
            .with_policy(PrincipalPolicy::Envelope)
            .unwrap();
        let mut message = received(&[]);
        message.headers = first.headers().clone();
        assert!(matches!(
            dispatcher.dispatch(&dispatch, &message).await,
            Outcome::Handled
        ));
        let second = &recorded_b.envelopes()[0];
        assert_eq!(second.headers()[CALLER_SUBJECT], "u-7");
        assert_eq!(second.headers()[CALLER_ACTOR], "service-a");
    }
}
