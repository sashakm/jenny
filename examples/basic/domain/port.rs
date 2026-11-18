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
//!
//! Attachments are bytes, not aggregates, so they go through the other storage port,
//! [`Blobs`]: an S3 bucket, a directory or memory, chosen in `main.rs`.
//!
//! Links between notes are a graph, and what a graph is asked — what refers to this
//! note, what it refers to — is no generic store's question. So that port is the
//! domain's own, [`NoteLinks`], and `adapter/outbound/links.rs` answers it from an
//! Apache AGE graph or from memory.

use async_trait::async_trait;
use jenny::port::Blobs;
use jenny::{DomainResult, Store};

use super::model::{Note, NoteId};

/// Where notes live.
pub type NoteStore = dyn Store<Note>;

/// Where the files attached to notes live.
pub type Attachments = dyn Blobs;

/// Which notes refer to which.
#[async_trait]
pub trait NoteLinks: Send + Sync {
    /// `from` refers to `to`. Linking twice is linking once.
    async fn link(&self, from: NoteId, to: NoteId) -> DomainResult<()>;

    /// `from` no longer refers to `to`. Nothing happens if it did not.
    async fn unlink(&self, from: NoteId, to: NoteId) -> DomainResult<()>;

    /// The notes `id` refers to, and the ones that refer to it.
    async fn links(&self, id: NoteId) -> DomainResult<Links>;

    /// Every link to or from `id`, for when the note goes.
    async fn forget(&self, id: NoteId) -> DomainResult<()>;
}

/// The links around one note, each side in id order.
#[derive(Debug, Default, PartialEq)]
pub struct Links {
    pub to: Vec<NoteId>,
    pub from: Vec<NoteId>,
}
