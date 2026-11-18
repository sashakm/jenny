//! The boundary. Ports are traits; adapters implement them.
//!
//! Which side a port sits on is decided by who calls whom, not by what it does:
//!
//! - [`use_case`] — the **driving** side. The outside world calls in. A use case is a
//!   method on a plain struct; a REST handler, a message consumer or a scheduled task
//!   is a driving adapter that calls it.
//! - [`store`], [`blobs`], [`events`] and [`outbound`] — the **driven** side. The application
//!   calls out. A store, an event publisher, a token verifier: the trait is owned by
//!   the inside, the implementation lives in a driven adapter.
//!
//! The framework ships the driven ports whose shape does not vary between applications.
//! [`Store<A>`] is the one for aggregates: load by id, save with a version check.
//! Anything phrased in your domain's own language — `archived_before(cutoff)` — goes on
//! a port of yours that extends it, in `domain/port/` (ADR-012). [`Blobs`] is the one
//! for files (ADR-018).
//!
//! ### Dispatch
//!
//! Use cases hold their ports as `Arc<dyn …>`. Every port is `#[async_trait]`, which
//! boxes each call's future whatever the dispatch, so a generic parameter would save
//! one vtable lookup on a call that has already allocated and is about to do I/O — and
//! it would cost a type parameter on every handler that touches the use case.
//!
//! ```
//! # use jenny_core::domain::DomainResult;
//! # use std::sync::Arc;
//! # trait NoteRepository: Send + Sync { fn count(&self) -> usize; }
//! fn count_notes(repo: &Arc<dyn NoteRepository>) -> usize { repo.count() }
//! ```

pub mod blobs;
pub mod context;
pub mod events;
pub mod outbound;
pub mod store;
pub mod use_case;

#[doc(inline)]
pub use blobs::{Access, Blob, BlobInfo, Blobs};
#[doc(inline)]
pub use context::{Context, Extensions};
#[doc(inline)]
pub use events::{Events, Raised};
#[doc(inline)]
pub use outbound::{
    AccessRequest, Authenticator, Authorizer, EventEnvelope, EventPublisher, HealthCheck,
    PublishError, Resource,
};
#[doc(inline)]
pub use store::{
    Add, Change, Changed, Loaded, RaisingAdd, RaisingChange, Store, StoreExt, Updated, Version,
};
#[doc(inline)]
pub use use_case::{UseCaseError, UseCaseObserver, observe_use_cases};
