//! Outbound ports the domain owns.
//!
//! This is the half of hexagonal architecture that inverts the dependency. The domain
//! declares what it needs in its own vocabulary, and a driven adapter supplies it.
//!
//! Notes are only ever found by id or listed, so the framework's [`Store`] is the whole
//! port: load, save with a version check, remove, list. When the domain needs a query
//! of its own, it is declared here on a trait that extends it —
//!
//! ```ignore
//! #[async_trait]
//! pub trait NoteRepository: Store<Note> {
//!     async fn archived_before(&self, cutoff: DateTime<Utc>) -> DomainResult<Vec<Note>>;
//! }
//! ```
//!
//! — and the use cases hold an `Arc<dyn NoteRepository>` instead. Nothing else changes.

use jenny::Store;

use super::model::Note;

/// Where notes live.
pub type NoteStore = dyn Store<Note>;
