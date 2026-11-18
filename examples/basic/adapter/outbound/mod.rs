//! Driven adapters — the application calling out.
//!
//! Each module here implements a port the inside owns. Nothing in `domain/` or
//! `application/` names any of these types; the composition root is the only place
//! they appear, which is what makes them swappable.
//!
//! There is no note store here: `jenny::memory::Memory` already is one, and a
//! Postgres or document store from the persistence crates would be one line in
//! `main.rs` too. A module appears here when the domain declares a query of its own
//! that a generic store cannot answer.

pub mod events;
