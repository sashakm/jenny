//! Domain event definitions — no dispatch.
//!
//! Event *types* live in the domain and are raised by aggregates. Getting them onto
//! a bus is somebody else's job: the application layer collects raised events after
//! a state change and hands them to the
//! [`EventPublisher`](crate::port::outbound::EventPublisher) outbound port, and a
//! driven adapter behind that port does the actual publishing.
//!
//! That split is why this module is so small. A domain event needs to say what it is
//! called; it does not need to know how to serialise itself, which broker exists, or
//! whether anyone is listening.
//!
//! ### Example
//!
//! ```
//! use jenny_core::domain::DomainEvent;
//!
//! pub struct NoteCreated {
//!     pub id: uuid::Uuid,
//!     pub title: String,
//! }
//!
//! impl DomainEvent for NoteCreated {
//!     fn name(&self) -> &str { "note.created" }
//! }
//! ```
//!
//! Turning that into something publishable happens one ring out, where serde is
//! allowed:
//!
//! ```
//! # use jenny_core::domain::DomainEvent;
//! # use jenny_core::port::outbound::EventEnvelope;
//! # pub struct NoteCreated { pub id: uuid::Uuid, pub title: String }
//! # impl DomainEvent for NoteCreated { fn name(&self) -> &str { "note.created" } }
//! # let event = NoteCreated { id: uuid::Uuid::nil(), title: "hi".into() };
//! let envelope = EventEnvelope::json(
//!     event.name(),
//!     &serde_json::json!({ "id": event.id, "title": event.title }),
//! )?;
//! # Ok::<(), jenny_core::port::outbound::PublishError>(())
//! ```

/// A thing that happened in the domain.
///
/// Implemented by the plain structs and enums in a service's `domain/event/` module.
/// The only thing the framework needs from an event is its name, which becomes the
/// topic it is routed on.
pub trait DomainEvent: Send + Sync + 'static {
    /// Dotted, past-tense name identifying the kind of event — `"note.created"`,
    /// `"order.cancelled"`. Bus adapters route on this.
    fn name(&self) -> &str;
}

#[cfg(test)]
mod tests {
    use super::*;

    struct NoteArchived;

    impl DomainEvent for NoteArchived {
        fn name(&self) -> &str {
            "note.archived"
        }
    }

    #[test]
    fn events_expose_a_routing_name() {
        let event: &dyn DomainEvent = &NoteArchived;
        assert_eq!(event.name(), "note.archived");
    }
}
