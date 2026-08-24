//! Driven ports — what the application needs from the outside world.
//!
//! Only the genuinely application-independent ports live here. Repositories do not:
//! a `NoteRepository` phrased in your domain's own language, returning your own types
//! and errors, is worth more than any `Repository<T, Id>` the framework could invent,
//! and it belongs in your `domain/port/` module. What the framework can usefully own
//! is the handful of ports whose shape does not vary between applications:
//!
//! | Port | Driven adapter that implements it |
//! |---|---|
//! | [`EventPublisher`] | `adapter/outbound/eventbus` — the in-process bus, a broker, an outbox relay |
//! | [`Authenticator`] | `adapter/outbound/security` — JWT/JWK verification, API-key lookup |
//! | [`Authorizer`] | `adapter/outbound/security` — RBAC, policy engine |
//! | [`HealthCheck`] | any driven adapter reporting whether it is usable |
//!
//! All three are object-safe, so the composition root can choose between
//! `Arc<dyn EventPublisher>` and a monomorphised generic without the port changing.

use std::fmt;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::Serialize;
use thiserror::Error;

use crate::domain::{AuthenticationError, AuthorizationError, Credentials, Principal};

/// Publishing an event failed.
#[derive(Debug, Error)]
pub enum PublishError {
    /// The event's payload could not be encoded.
    #[error("could not encode event payload: {0}")]
    Encoding(#[from] serde_json::Error),
    /// The bus or broker rejected or could not accept the event.
    #[error("could not publish event: {0}")]
    Transport(String),
}

impl PublishError {
    pub fn transport(detail: impl fmt::Display) -> Self {
        Self::Transport(detail.to_string())
    }
}

/// A domain event, encoded and ready to leave the process.
///
/// The domain defines events as plain types (see
/// [`DomainEvent`](crate::domain::DomainEvent)); the application layer turns one into
/// an envelope on its way to the [`EventPublisher`]. That conversion is where serde
/// enters — one ring out from the domain, mirroring how a DTO is converted into a
/// command on the way in.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EventEnvelope {
    name: String,
    occurred_at: DateTime<Utc>,
    payload: Vec<u8>,
}

impl EventEnvelope {
    /// Wrap an already-encoded payload.
    pub fn new(name: impl Into<String>, payload: impl Into<Vec<u8>>) -> Self {
        Self {
            name: name.into(),
            occurred_at: Utc::now(),
            payload: payload.into(),
        }
    }

    /// Encode `payload` as JSON and wrap it.
    pub fn json(name: impl Into<String>, payload: &impl Serialize) -> Result<Self, PublishError> {
        Ok(Self::new(name, serde_json::to_vec(payload)?))
    }

    /// Override the timestamp, which otherwise defaults to now. Useful when replaying
    /// events from an outbox, where "when it happened" is not "when it is published".
    pub fn occurred_at(mut self, at: DateTime<Utc>) -> Self {
        self.occurred_at = at;
        self
    }

    /// The routing name, taken from the domain event.
    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn timestamp(&self) -> DateTime<Utc> {
        self.occurred_at
    }

    pub fn payload(&self) -> &[u8] {
        &self.payload
    }

    /// Take the encoded payload, consuming the envelope.
    pub fn into_payload(self) -> Vec<u8> {
        self.payload
    }
}

/// Where domain events go.
///
/// The application layer collects the events an aggregate raised, then hands them
/// here after the state change has been committed. Whether that lands on an in-process
/// bus, a broker, or an outbox table is the driven adapter's business — which is the
/// point, because it means moving from a direct publish to a transactional outbox
/// later is a change in the composition root and nowhere else.
///
/// **On atomicity:** `repository.save()` followed by `publisher.publish()` is two
/// writes and can diverge if the process dies between them. Start direct; when your
/// consistency requirements actually demand it, put a transactional outbox behind this
/// port — write the event to an outbox table inside the same database transaction and
/// let a relay drain it. The application layer does not change.
#[async_trait]
pub trait EventPublisher: Send + Sync + 'static {
    async fn publish(&self, event: EventEnvelope) -> Result<(), PublishError>;

    /// Publish a batch. The default publishes one at a time; adapters that can submit
    /// a batch atomically should override it.
    async fn publish_all(&self, events: Vec<EventEnvelope>) -> Result<(), PublishError> {
        for event in events {
            self.publish(event).await?;
        }
        Ok(())
    }
}

/// Turns raw credentials into a known identity.
///
/// Lifting credentials off the wire is per-protocol and belongs to the inbound
/// adapter; deciding whether they are *valid* means talking to a JWKS endpoint, a key
/// store or a directory, which makes it a driven concern.
#[async_trait]
pub trait Authenticator: Send + Sync + 'static {
    async fn authenticate(
        &self,
        credentials: &Credentials,
    ) -> Result<Principal, AuthenticationError>;
}

/// Reports whether something the service depends on is usable right now.
///
/// A port rather than a runtime concern, because the things worth checking — a
/// connection pool, a broker client, a remote endpoint — live in driven adapters, and
/// those crates must be able to answer for themselves without depending on the
/// composition root. `jenny-persistence` and `jenny-messaging` each ship
/// implementations; the runtime only aggregates them.
///
/// Failures return a human-readable reason rather than an error type, because the
/// reason's only destination is a readiness response and an operator's eyes.
///
/// ```
/// use async_trait::async_trait;
/// use jenny_core::port::outbound::HealthCheck;
///
/// struct CacheWarmed(std::sync::Arc<std::sync::atomic::AtomicBool>);
///
/// #[async_trait]
/// impl HealthCheck for CacheWarmed {
///     fn name(&self) -> &str { "cache" }
///
///     async fn check(&self) -> Result<(), String> {
///         if self.0.load(std::sync::atomic::Ordering::Relaxed) {
///             Ok(())
///         } else {
///             Err("still warming up".to_string())
///         }
///     }
/// }
/// ```
#[async_trait]
pub trait HealthCheck: Send + Sync + 'static {
    /// Name reported in readiness and status responses.
    fn name(&self) -> &str;

    /// What kind of dependency this is, for grouping in the status report.
    ///
    /// Defaults to `"dependency"`. Adapters override it with `"database"`, `"broker"`,
    /// `"telemetry"` and so on.
    fn kind(&self) -> &str {
        "dependency"
    }

    /// `Ok(())` when healthy, `Err(reason)` otherwise.
    async fn check(&self) -> Result<(), String>;
}

/// Decides whether a known identity may perform an action on a resource.
///
/// This is the *coarse*, mechanism-backed check an inbound adapter can run before
/// dispatching — is this caller allowed to `POST` to this endpoint at all. Fine-grained
/// rules that depend on the data itself ("may this user edit *this* note") are domain
/// logic and belong in a domain service, written once and enforced on every transport.
#[async_trait]
pub trait Authorizer: Send + Sync + 'static {
    /// `action` is the protocol's verb (an HTTP method, a message kind); `resource`
    /// identifies what is being acted on (a matched path, a [`Urn`](crate::domain::Urn)).
    async fn authorize(
        &self,
        principal: &Principal,
        action: &str,
        resource: &str,
    ) -> Result<(), AuthorizationError>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[derive(Serialize)]
    struct NoteCreatedPayload {
        id: u32,
    }

    #[derive(Debug, Default)]
    struct RecordingPublisher(Mutex<Vec<EventEnvelope>>);

    #[async_trait]
    impl EventPublisher for RecordingPublisher {
        async fn publish(&self, event: EventEnvelope) -> Result<(), PublishError> {
            self.0.lock().unwrap().push(event);
            Ok(())
        }
    }

    #[test]
    fn json_envelopes_carry_the_encoded_payload() {
        let envelope = EventEnvelope::json("note.created", &NoteCreatedPayload { id: 7 }).unwrap();
        assert_eq!(envelope.name(), "note.created");
        assert_eq!(envelope.payload(), br#"{"id":7}"#);
    }

    #[test]
    fn the_timestamp_can_be_overridden_for_replay() {
        let then = DateTime::parse_from_rfc3339("2020-01-01T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let envelope = EventEnvelope::new("note.created", b"{}".to_vec()).occurred_at(then);
        assert_eq!(envelope.timestamp(), then);
    }

    #[tokio::test]
    async fn publish_all_defaults_to_publishing_one_at_a_time() {
        let publisher = RecordingPublisher::default();
        publisher
            .publish_all(vec![
                EventEnvelope::new("note.created", b"{}".to_vec()),
                EventEnvelope::new("note.archived", b"{}".to_vec()),
            ])
            .await
            .unwrap();

        let published = publisher.0.lock().unwrap();
        assert_eq!(published.len(), 2);
        assert_eq!(published[0].name(), "note.created");
        assert_eq!(published[1].name(), "note.archived");
    }

    #[tokio::test]
    async fn the_publisher_port_is_object_safe() {
        let erased: std::sync::Arc<dyn EventPublisher> =
            std::sync::Arc::new(RecordingPublisher::default());
        erased
            .publish(EventEnvelope::new("note.created", b"{}".to_vec()))
            .await
            .unwrap();
    }
}
