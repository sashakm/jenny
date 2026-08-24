//! Driven adapters — the application calling out.
//!
//! Each module here implements a port the inside owns. Nothing in `domain/` or
//! `application/` names any of these types; the composition root is the only place
//! they appear, which is what makes them swappable.

pub mod events;
pub mod memory;
