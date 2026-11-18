//! Domain event definitions — no dispatch.
//!
//! Event *types* live in the domain and are raised by aggregates. Getting them onto
//! a bus is somebody else's job: the use case hands what an aggregate raised to the
//! write that persists the change — `change(…).raising(&events, cx)` — which commits it
//! with the change where the store keeps an outbox, and a driven adapter behind the
//! [`EventPublisher`](crate::port::outbound::EventPublisher) port does the delivery.
//!
//! An event derives `Serialize`, because what it carries *is* its published contract
//! (ADR-012), and `#[derive(DomainEvent)]` names it after its type. It does not know
//! which broker exists or whether anyone is listening.
//!
//! ### Example
//!
//! ```
//! use jenny_core::domain::DomainEvent;
//!
//! // In a service, `#[derive(serde::Serialize, jenny::DomainEvent)]` writes the impl.
//! #[derive(serde::Serialize)]
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
//! Publishing it encodes the fields as the payload and correlates it with the request:
//!
//! ```
//! # use std::sync::Arc;
//! # use jenny_core::domain::DomainEvent;
//! # use jenny_core::port::{Context, Events};
//! # use jenny_core::testing::RecordedEvents;
//! # #[derive(serde::Serialize)] pub struct NoteCreated { pub id: uuid::Uuid, pub title: String }
//! # impl DomainEvent for NoteCreated { fn name(&self) -> &str { "note.created" } }
//! # #[tokio::main(flavor = "current_thread")] async fn main() {
//! # let events = Events::new(Arc::new(RecordedEvents::default()));
//! let created = NoteCreated { id: uuid::Uuid::nil(), title: "Shopping".into() };
//! events.publish(&created, &Context::new()).await;
//! # }
//! ```

/// A thing that happened in the domain.
///
/// Implemented by the structs and enums in a service's `domain/event/` module, usually
/// through `#[derive(DomainEvent)]`. The trait asks only for the name, which becomes the
/// topic it is routed on; publishing through [`Events`](crate::port::Events) also needs
/// `Serialize`, for the payload.
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
