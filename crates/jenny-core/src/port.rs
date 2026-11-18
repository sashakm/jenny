//! The boundary. Ports are traits; adapters implement them.
//!
//! Which side a port sits on is decided by who calls whom, not by what it does:
//!
//! - [`inbound`] — **driving** ports. The outside world calls in. A use case is an
//!   inbound port; a REST handler, a message consumer or a scheduled task is a
//!   driving adapter that invokes one.
//! - [`outbound`] — **driven** ports. The application calls out. A repository, an
//!   event publisher, a token verifier: the trait is owned by the inside, the
//!   implementation lives in a driven adapter.
//!
//! The asymmetry between the two modules is intentional. Outbound ports are mostly
//! *yours* — a `NoteRepository` phrased in your domain's language is worth far more
//! than any generic `Repository<T>` the framework could offer, so the framework only
//! ships the ones that are genuinely application-independent. Inbound ports are often
//! not needed at all: a plain generic function is a use case, and
//! [`inbound::UseCase`] exists for when a trait actually earns its keep.
//!
//! ### Static or dynamic dispatch, your call
//!
//! Ports are ordinary traits, so both work and neither is baked in:
//!
//! ```
//! # use jenny_core::domain::DomainResult;
//! # trait NoteRepository: Send + Sync { fn count(&self) -> usize; }
//! # use std::sync::Arc;
//! // monomorphised, zero-cost — the default choice
//! fn count_notes<R: NoteRepository>(repo: &R) -> usize { repo.count() }
//!
//! // erased — when the composition root wants to pick an implementation at runtime
//! fn count_notes_dyn(repo: &Arc<dyn NoteRepository>) -> usize { repo.count() }
//! ```

pub mod context;
pub mod inbound;
pub mod outbound;

#[doc(inline)]
pub use context::{Context, Extensions};
#[doc(inline)]
pub use inbound::UseCase;
#[doc(inline)]
pub use outbound::{
    AccessRequest, Authenticator, Authorizer, EventEnvelope, EventPublisher, HealthCheck,
    PublishError, Resource,
};
