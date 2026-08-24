//! Messaging adapters, on both sides of the hexagon.
//!
//! | Module | Side | What it is |
//! |---|---|---|
//! | [`eventbus`] | driven | [`EventBus`] — the in-process bus behind the [`EventPublisher`] port |
//! | [`nats`] | driven | [`NatsGateway`](nats::NatsGateway) — a bus listener that forwards events to JetStream |
//! | [`subscription`] | driving | [`MessagingAdapter`] — declares the subjects this service consumes |
//!
//! ### One publish, many effects
//!
//! The application layer publishes a domain event exactly once, through the
//! [`EventPublisher`] port. Behind that port the bus fans the event out to every
//! listener whose subscription matches, and each listener is its own small outbound
//! effect — write an audit row, forward to NATS, send an email. Listeners depend on
//! event definitions; nothing depends on listeners. Adding one is additive, and the
//! use case that raised the event never learns it happened.
//!
//! ```text
//!  application ──publish──▶ EventPublisher (port)
//!                                │
//!                          EventBus (adapter)
//!                                ├──▶ audit log writer
//!                                ├──▶ NatsGateway ──▶ JetStream
//!                                └──▶ notification sender
//! ```
//!
//! ### Inbound is declaration-only for now
//!
//! [`MessagingAdapter`] records which subjects a service means to consume, and the
//! runtime logs them at startup. The dispatch path — message in, use case invoked,
//! ack or nack out — is not implemented yet; see the messaging ADR for the design it
//! is being built against.
//!
//! [`EventPublisher`]: jenny_core::port::outbound::EventPublisher

pub mod eventbus;
pub mod health;
pub mod nats;

/// The in-process broker, re-exported because its `EventConsumer` trait is part of
/// this crate's public surface: subscribing a listener means implementing it.
pub use topmesys;
pub mod subscription;

#[doc(inline)]
pub use eventbus::EventBus;
#[doc(inline)]
pub use health::{EventBusCheck, NatsCheck};
#[doc(inline)]
pub use subscription::{MessagingAdapter, SubjectBinding};
