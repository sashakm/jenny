//! Outbound ports the domain owns.
//!
//! This is the half of hexagonal architecture that inverts the dependency. The domain
//! declares what it needs in its own vocabulary — `Note`, `NoteId`, `DomainError` —
//! and a driven adapter in `adapter/outbound/` supplies it. Postgres, an in-memory map
//! and a test double are then interchangeable, and none of them appears anywhere
//! inside the hexagon.
//!
//! Note what this trait deliberately is *not*: a generic `Repository<T, Id>`. It talks
//! about notes, returns notes, and fails with domain errors. A generic one would drag
//! storage concepts back across the boundary it exists to defend.

use async_trait::async_trait;
use jenny::DomainResult;

use super::model::{Note, NoteId};

#[async_trait]
pub trait NoteRepository: Send + Sync + 'static {
    /// Insert or replace.
    async fn save(&self, note: Note) -> DomainResult<()>;

    /// `None` when there is no such note.
    async fn fetch(&self, id: NoteId) -> DomainResult<Option<Note>>;

    async fn list(&self) -> DomainResult<Vec<Note>>;

    /// Returns what was removed, or `None` when there was nothing to remove.
    async fn delete(&self, id: NoteId) -> DomainResult<Option<Note>>;
}
