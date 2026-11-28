//! Driven ports — what the application needs from the outside world.
//!
//! Only the genuinely application-independent ports live here, beside the storage
//! port in [`store`](super::store). Queries phrased in your domain's own language
//! belong on a port of yours that extends [`Store`](super::Store), in your
//! `domain/port/` module. What the framework can usefully own is the handful of ports
//! whose shape does not vary between applications:
//!
//! | Port | Driven adapter that implements it |
//! |---|---|
//! | [`EventPublisher`] | `adapter/outbound/eventbus` — the in-process bus, a broker, an outbox relay |
//! | [`Authenticator`] | `adapter/outbound/security` — JWT/JWK verification, API-key lookup |
//! | [`Authorizer`] | `adapter/outbound/security` — a policy engine (jenny ships Cedar) |
//! | [`HealthCheck`] | any driven adapter reporting whether it is usable |
//!
//! All four are object-safe, so the composition root can choose between
//! `Arc<dyn EventPublisher>` and a monomorphised generic without the port changing.

use std::collections::BTreeMap;
use std::fmt;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::{Map, Value};
use thiserror::Error;
use uuid::Uuid;

use crate::domain::{AuthenticationError, AuthorizationError, Credentials, Principal};
use crate::port::Context;

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
///
/// Besides the payload an envelope carries what a consumer needs to correlate and
/// deduplicate: a unique [`id`](Self::id), when it [occurred](Self::timestamp), the
/// [correlation id](Self::correlation_id) of the request that caused it, and free-form
/// [headers](Self::headers) (a `traceparent`, a tenant). They are metadata rather than
/// arguments to [`EventPublisher::publish`] so that an outbox can store and replay an
/// envelope unchanged.
///
/// ```
/// use jenny_core::port::{Context, EventEnvelope};
///
/// let cx = Context::new();
/// let envelope = EventEnvelope::json("note.created", &serde_json::json!({ "id": 7 }))?
///     .correlated_with(&cx)
///     .with_header("tenant", "acme");
///
/// assert_eq!(envelope.correlation_id(), Some(cx.request_id()));
/// assert_eq!(envelope.headers()["tenant"], "acme");
/// # Ok::<(), jenny_core::port::PublishError>(())
/// ```
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EventEnvelope {
    id: Uuid,
    name: String,
    occurred_at: DateTime<Utc>,
    correlation_id: Option<Uuid>,
    headers: BTreeMap<String, String>,
    payload: Vec<u8>,
}

impl EventEnvelope {
    /// Wrap an already-encoded payload, under a fresh id.
    pub fn new(name: impl Into<String>, payload: impl Into<Vec<u8>>) -> Self {
        Self {
            id: Uuid::new_v4(),
            name: name.into(),
            occurred_at: Utc::now(),
            correlation_id: None,
            headers: BTreeMap::new(),
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

    /// Override the id, which otherwise is fresh. An outbox replaying an envelope keeps
    /// its original id, so a consumer can recognise the duplicate.
    pub fn with_id(mut self, id: Uuid) -> Self {
        self.id = id;
        self
    }

    /// Correlate the event with the invocation that raised it, by that invocation's
    /// request id.
    pub fn correlated_with(self, cx: &Context) -> Self {
        self.with_correlation_id(cx.request_id())
    }

    pub fn with_correlation_id(mut self, id: Uuid) -> Self {
        self.correlation_id = Some(id);
        self
    }

    /// Add a header. Setting one again replaces its value.
    pub fn with_header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.insert(name.into(), value.into());
        self
    }

    /// Unique per event, and kept across a replay.
    pub fn id(&self) -> Uuid {
        self.id
    }

    /// The routing name, taken from the domain event.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The request id of the invocation that raised the event, if it was correlated.
    pub fn correlation_id(&self) -> Option<Uuid> {
        self.correlation_id
    }

    pub fn headers(&self) -> &BTreeMap<String, String> {
        &self.headers
    }

    /// For [`Events`](super::Events), which hands them to its enrichers.
    pub(crate) fn headers_mut(&mut self) -> &mut BTreeMap<String, String> {
        &mut self.headers
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
/// A use case hands the events an aggregate raised to the write that persists the
/// change — `change(…).raising(&events, cx)` — and they reach this port from there.
/// Whether that lands on an in-process bus or a broker is the driven adapter's
/// business.
///
/// **On atomicity:** a save and a publish are two writes, and a crash between them
/// loses the event. A store with an outbox makes them one: it commits the events in
/// the same transaction as the change, and a relay forwards them to this port
/// afterwards, at least once (`Postgres::with_outbox`, ADR-016). Other stores publish
/// right after the write.
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

/// A shared adapter reports for itself: the store a use case holds is the same value
/// the runtime asks whether it is usable.
#[async_trait]
impl<T: HealthCheck + ?Sized> HealthCheck for std::sync::Arc<T> {
    fn name(&self) -> &str {
        (**self).name()
    }

    fn kind(&self) -> &str {
        (**self).kind()
    }

    async fn check(&self) -> Result<(), String> {
        (**self).check().await
    }
}

/// Decides whether a known identity may perform an action on a resource.
///
/// One port for both kinds of question a service asks:
///
/// - the **coarse** one an inbound adapter asks before dispatching: may this caller
///   `POST` to this route at all. `jenny-rest`'s `Authorization` layer asks it for every
///   request.
/// - the **fine-grained** one that depends on the data: may this caller archive *this*
///   note, which belongs to someone else. A use case asks it through
///   [`Context::authorize`](crate::port::Context::authorize), after loading the note,
///   so the rule holds on every transport.
///
/// The request is phrased in the principal/action/resource/context shape every policy
/// engine shares, without naming one, so the domain never imports the engine.
/// `jenny-security` implements it with [Cedar](https://www.cedarpolicy.com).
#[async_trait]
pub trait Authorizer: Send + Sync + 'static {
    async fn authorize(
        &self,
        principal: &Principal,
        request: &AccessRequest,
    ) -> Result<(), AuthorizationError>;
}

/// What a caller asks to do: an action on a [`Resource`], plus facts about the request.
///
/// ```
/// use jenny_core::port::outbound::{AccessRequest, Resource};
///
/// let request = AccessRequest::new(
///     "archive",
///     Resource::new("Note", 42)
///         .with_attribute("owner", "u-7")
///         .within(Resource::new("Notebook", "travel")),
/// )
/// .with_context("reason", "cleanup");
///
/// assert_eq!(request.action(), "archive");
/// assert_eq!(request.resource().to_string(), "Note(42)");
/// ```
#[derive(Clone, Debug, PartialEq)]
pub struct AccessRequest {
    action: String,
    resource: Resource,
    context: Map<String, Value>,
}

impl AccessRequest {
    pub fn new(action: impl Into<String>, resource: Resource) -> Self {
        Self {
            action: action.into(),
            resource,
            context: Map::new(),
        }
    }

    /// Add a fact about the request rather than the resource: the concrete path, the
    /// client's address, a reason given. A value that cannot be serialised is skipped.
    pub fn with_context(mut self, key: impl Into<String>, value: impl Serialize) -> Self {
        if let Ok(value) = serde_json::to_value(value) {
            self.context.insert(key.into(), value);
        }
        self
    }

    pub fn action(&self) -> &str {
        &self.action
    }

    pub fn resource(&self) -> &Resource {
        &self.resource
    }

    pub fn context(&self) -> &Map<String, Value> {
        &self.context
    }
}

/// The thing an [`AccessRequest`] acts on.
///
/// A kind and an id identify it (`Note`, `42`). Parents place it in a hierarchy a policy
/// can match as a whole: a note in a notebook, a route in a resource in an adapter.
/// Attributes are the facts a policy may read, such as its owner or its status.
#[derive(Clone, Debug, PartialEq)]
pub struct Resource {
    kind: String,
    id: String,
    parents: Vec<Resource>,
    attributes: Map<String, Value>,
}

impl Resource {
    pub fn new(kind: impl Into<String>, id: impl fmt::Display) -> Self {
        Self {
            kind: kind.into(),
            id: id.to_string(),
            parents: Vec::new(),
            attributes: Map::new(),
        }
    }

    /// Add an attribute a policy may read. A value that cannot be serialised is skipped.
    pub fn with_attribute(mut self, key: impl Into<String>, value: impl Serialize) -> Self {
        if let Ok(value) = serde_json::to_value(value) {
            self.attributes.insert(key.into(), value);
        }
        self
    }

    /// Place this resource inside `parent`. May be called more than once.
    pub fn within(mut self, parent: Resource) -> Self {
        self.parents.push(parent);
        self
    }

    pub fn kind(&self) -> &str {
        &self.kind
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn parents(&self) -> &[Resource] {
        &self.parents
    }

    pub fn attributes(&self) -> &Map<String, Value> {
        &self.attributes
    }
}

impl fmt::Display for Resource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}({})", self.kind, self.id)
    }
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
    fn every_envelope_has_its_own_id_unless_replayed() {
        let first = EventEnvelope::new("note.created", b"{}".to_vec());
        let second = EventEnvelope::new("note.created", b"{}".to_vec());
        assert_ne!(first.id(), second.id());

        let replayed = EventEnvelope::new("note.created", b"{}".to_vec()).with_id(first.id());
        assert_eq!(replayed.id(), first.id());
    }

    #[test]
    fn an_envelope_correlates_with_the_request_that_raised_it() {
        let cx = Context::new();
        let envelope = EventEnvelope::new("note.created", b"{}".to_vec());
        assert_eq!(envelope.correlation_id(), None);
        assert_eq!(
            envelope.correlated_with(&cx).correlation_id(),
            Some(cx.request_id())
        );
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

    #[test]
    fn a_resource_keeps_its_parents_and_attributes() {
        let note = Resource::new("Note", 42)
            .with_attribute("owner", "u-7")
            .with_attribute("tags", ["a", "b"])
            .within(Resource::new("Notebook", "travel").within(Resource::new("Workspace", 1)));

        assert_eq!(note.kind(), "Note");
        assert_eq!(note.id(), "42");
        assert_eq!(note.attributes()["owner"], "u-7");
        assert_eq!(note.attributes()["tags"], serde_json::json!(["a", "b"]));
        assert_eq!(note.parents()[0].to_string(), "Notebook(travel)");
        assert_eq!(note.parents()[0].parents()[0].to_string(), "Workspace(1)");
    }

    #[test]
    fn an_access_request_carries_its_context() {
        let request = AccessRequest::new("GET", Resource::new("Route", "/notes/{id}"))
            .with_context("path", "/notes/7");
        assert_eq!(request.action(), "GET");
        assert_eq!(request.context()["path"], "/notes/7");
    }
}
