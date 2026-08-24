//! Use cases: orchestration, and nothing else.
//!
//! Each method here does the same four things in the same order — check permission,
//! call the domain, persist, then publish what the domain raised. The business rules
//! themselves are one ring in, in `domain/`; if a rule ever starts being written
//! *here*, it has leaked out of the hexagon.
//!
//! ### Why these are plain methods and not `UseCase` impls
//!
//! [`jenny::UseCase`] exists, and this example deliberately does not use it. A use
//! case is a unit of behaviour, and in Rust that is a function; the trait earns its
//! keep only when you need the indirection — several driving adapters that must not
//! know the concrete type, a heterogeneous set behind `Arc<dyn …>`, a decorator that
//! wraps any of them. None of that applies to six methods called from one REST
//! adapter, so a trait would be ceremony with no payoff.
//!
//! What this module *does* lean on is generics: [`Notes`] is parameterised over its
//! outbound ports, so dispatch is static and the compiler inlines straight through
//! the boundary. See `main.rs` for the other option — the event publisher there is an
//! `Arc<dyn EventPublisher>`, chosen at the composition root.

pub mod command;
pub mod notes;

pub use notes::Notes;
