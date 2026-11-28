//! Messaging adapters, on both sides of the hexagon.
//!
//! | Module | Side | What it is |
//! |---|---|---|
//! | [`eventbus`] | driven | [`EventBus`] — the in-process bus behind the [`EventPublisher`] port |
//! | [`connection`] | driven | [`NatsConnectionConfig`] — the `messaging.nats` section: servers, TLS, credentials |
//! | [`kv`] | driven | [`Nats`](kv::Nats) — the one connection, shared — and [`NatsKv`](kv::NatsKv), a [`Store`](jenny_core::port::Store) over a JetStream key-value bucket |
//! | [`lease`] | driven | [`NatsLease`](lease::NatsLease) — the [`Lease`](jenny_core::port::Lease) port over a key-value bucket, `jenny_leases` (ADR-029) |
//! | [`nats`] | driven | [`NatsGateway`](nats::NatsGateway) — a bus listener that forwards events to JetStream |
//! | [`subscription`] | driving | [`MessagingAdapter`] — the subjects this service consumes from JetStream, and their handlers |
//! | [`inbound`] | driving | the dispatch core: a received event becomes a [`Context`](jenny_core::port::Context) and a typed [`Handler`](inbound::Handler) call |
//! | [`consumer`] | driving | [`JetStreamConsumer`](consumer::JetStreamConsumer) — one durable pull consumer per adapter: ack, nak with backoff, term, dead letters |
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
//! ### One NATS connection
//!
//! A service has one NATS connection, [`Nats`](kv::Nats), configured by
//! [`NatsConnectionConfig`]: plain, TLS, mTLS with a client certificate, a `.creds` file,
//! an NKey, a user and password, or a token (ADR-031). The key-value stores and the
//! gateway ([`Nats::gateway`](kv::Nats::gateway)) share it. Certificates and `.creds` are
//! read again at each connect, and a watcher forces a reconnect when they rotate.
//!
//! ### Inbound
//!
//! A service consumes events through typed handlers, from two sources that share one
//! dispatch core ([`inbound`]): its own in-process bus (`JennyServer::handle`), and
//! JetStream, through a [`MessagingAdapter`] whose handlers get a durable pull consumer
//! ([`consumer`]). Either way a handler gets the payload decoded into its DTO and a
//! [`Context`](jenny_core::port::Context) whose request id is the event's correlation id,
//! and runs as the service itself, `System[<service>/events]`, unless configured to run
//! as the event's caller (ADR-032, ADR-033).
//!
//! [`EventPublisher`]: jenny_core::port::outbound::EventPublisher

pub mod connection;
pub mod consumer;
pub mod eventbus;
pub mod health;
pub mod inbound;
pub mod kv;
pub mod lease;
pub mod nats;
mod rotation;

/// The headers [`EventBus`] puts on every message, from the
/// [`EventEnvelope`](jenny_core::port::outbound::EventEnvelope)'s metadata, and
/// [`NatsGateway`](nats::NatsGateway) carries over to NATS.
pub mod headers {
    /// The envelope's [`id`](jenny_core::port::outbound::EventEnvelope::id). Also sent to
    /// JetStream as `Nats-Msg-Id`, so a replayed event is deduplicated there.
    pub const EVENT_ID: &str = "event-id";
    /// The event name without the bus namespace, such as `note.created`.
    pub const EVENT_NAME: &str = "event-name";
    /// When the event occurred, RFC 3339.
    pub const OCCURRED_AT: &str = "event-occurred-at";
    /// The request id of the invocation that raised the event, when it was correlated.
    pub const CORRELATION_ID: &str = "correlation-id";
}

/// The NATS client, re-exported because its `Client` and `ConnectOptions` are part of
/// this crate's public surface (and, through `JennyServer::nats_options`, the runtime's):
/// a service can reach them without depending on the same async-nats major itself.
pub use async_nats;
/// The in-process broker, re-exported because its `EventConsumer` trait is part of
/// this crate's public surface: subscribing a listener means implementing it.
pub use topmesys;
pub mod subscription;

#[doc(inline)]
pub use connection::NatsConnectionConfig;
#[doc(inline)]
pub use eventbus::EventBus;
#[doc(inline)]
pub use health::{EventBusCheck, NatsCheck};
#[doc(inline)]
pub use subscription::{MessagingAdapter, SubjectBinding};
