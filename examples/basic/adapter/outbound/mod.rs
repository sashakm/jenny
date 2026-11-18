//! Driven adapters — the application calling out.
//!
//! Each module here implements a port the inside owns. Nothing in `domain/` or
//! `application/` names any of these types; the composition root is the only place
//! they appear, which is what makes them swappable.
//!
//! There is no note store here: `jenny::memory::Memory`, the Postgres document store and
//! the NATS key-value store already are one each, chosen in `main.rs` from
//! configuration. A module appears here when the domain declares a query of its own
//! that a generic store cannot answer.

pub mod events;
