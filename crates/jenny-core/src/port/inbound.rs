//! What an inbound event handler finds in its [`Context`](super::Context), besides the
//! principal.
//!
//! The messaging adapter builds the context from the received envelope and attaches
//! these as extensions; a use case reads them with `cx.get::<InboundEvent>()`.

use chrono::{DateTime, Utc};
use uuid::Uuid;

/// The event being handled: for idempotency, because delivery is at least once.
///
/// jenny keeps no deduplication store; a handler whose effect is not idempotent records
/// [`id`](Self::id) next to it and skips an id it has seen.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InboundEvent {
    /// The envelope's id, kept across outbox replays and redeliveries.
    pub id: Option<Uuid>,
    /// The event name without the bus namespace, such as `note.created`.
    pub name: String,
    /// When the event occurred, when the message said.
    pub occurred_at: Option<DateTime<Utc>>,
    /// Which delivery this is, from 1.
    pub attempt: u64,
}

/// Who the event was raised for, from its `caller-subject` and `caller-actor` headers.
///
/// Only attached when the transport is trusted: always on the in-process bus, and on a
/// JetStream consumer only over an authenticated NATS connection. Even then it is only as
/// trustworthy as the permissions on who may publish to the subject.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EventCaller {
    pub subject: String,
    pub actor: Option<String>,
}

/// Marks a context whose principal was restored from an event's headers (the `envelope`
/// principal policy) rather than authenticated by a credential.
///
/// A restored principal is a plain user with the subject's id, and `act` when there was
/// an actor, and nothing else: no roles, no claims. Role-based policies therefore deny
/// it, which is intended until identities carry their provenance (ADR-024).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RestoredCaller;
