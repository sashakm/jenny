//! An in-memory `NoteRepository`.
//!
//! Stands in for what would be `adapter/outbound/persistence/` in a real service. It
//! keeps the same two-part structure a sqlx-backed one has, because that structure is
//! the point:
//!
//! 1. a **storage-shaped** row type ([`NoteRow`]) — flat, primitive fields, the shape
//!    the store wants,
//! 2. **mappers** between it and the aggregate, in both directions.
//!
//! The store never holds a `Note`. That is what keeps storage concerns from creeping
//! into the domain: a column rename changes `NoteRow` and the two `From` impls, and
//! nothing inside the hexagon notices. Swapping this whole module for Postgres is one
//! line in `main.rs`.

use std::collections::HashMap;
use std::sync::Mutex;

use async_trait::async_trait;
use jenny::{DomainError, DomainResult};
use uuid::Uuid;

use crate::domain::model::{Note, NoteId, NoteTitle};
use crate::domain::port::NoteRepository;

/// The storage shape — what a database row would look like.
#[derive(Clone, Debug)]
struct NoteRow {
    id: Uuid,
    title: String,
    body: String,
    archived: bool,
}

/// Aggregate to row, on the way out.
impl From<&Note> for NoteRow {
    fn from(note: &Note) -> Self {
        Self {
            id: note.id().as_uuid(),
            title: note.title().to_string(),
            body: note.body().to_string(),
            archived: note.is_archived(),
        }
    }
}

/// Row to aggregate, on the way back in.
///
/// Fallible, and deliberately so: stored data can violate an invariant the code now
/// enforces — an older release wrote a blank title, or someone edited the table by
/// hand. That is an internal failure, not the caller's fault, so it maps to
/// [`DomainError::Internal`] rather than to a 400.
impl TryFrom<NoteRow> for Note {
    type Error = DomainError;

    fn try_from(row: NoteRow) -> Result<Self, Self::Error> {
        let title = NoteTitle::new(row.title).map_err(|e| {
            DomainError::internal(format!("stored note {} has an unusable title: {e}", row.id))
        })?;
        Ok(Note::rehydrate(
            NoteId::from_uuid(row.id),
            title,
            row.body,
            row.archived,
        ))
    }
}

#[derive(Debug, Default)]
pub struct InMemoryNotes {
    rows: Mutex<HashMap<Uuid, NoteRow>>,
}

impl InMemoryNotes {
    /// Whether the store is reachable — the readiness signal `main` registers, in
    /// place of the connection-pool ping a real repository would do.
    pub fn is_healthy(&self) -> bool {
        self.rows.lock().is_ok()
    }

    fn lock(&self) -> DomainResult<std::sync::MutexGuard<'_, HashMap<Uuid, NoteRow>>> {
        // A poisoned lock means another thread panicked mid-write. Internal failure,
        // not something the caller got wrong.
        self.rows
            .lock()
            .map_err(|e| DomainError::internal(format!("note store is poisoned: {e}")))
    }
}

#[async_trait]
impl NoteRepository for InMemoryNotes {
    async fn save(&self, note: Note) -> DomainResult<()> {
        let row = NoteRow::from(&note);
        self.lock()?.insert(row.id, row);
        Ok(())
    }

    async fn fetch(&self, id: NoteId) -> DomainResult<Option<Note>> {
        let row = self.lock()?.get(&id.as_uuid()).cloned();
        row.map(Note::try_from).transpose()
    }

    async fn list(&self) -> DomainResult<Vec<Note>> {
        let rows: Vec<NoteRow> = self.lock()?.values().cloned().collect();
        rows.into_iter().map(Note::try_from).collect()
    }

    async fn delete(&self, id: NoteId) -> DomainResult<Option<Note>> {
        let row = self.lock()?.remove(&id.as_uuid());
        row.map(Note::try_from).transpose()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn note(title: &str) -> Note {
        Note::create(NoteTitle::new(title).unwrap(), "body".into()).0
    }

    #[tokio::test]
    async fn saved_notes_come_back_through_the_mappers() {
        let repo = InMemoryNotes::default();
        let note = note("Shopping");
        let id = note.id();

        repo.save(note).await.unwrap();

        let found = repo.fetch(id).await.unwrap().unwrap();
        assert_eq!(found.id(), id);
        assert_eq!(found.title().as_str(), "Shopping");
        assert_eq!(found.body(), "body");
        assert_eq!(repo.list().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn archived_state_survives_the_round_trip() {
        let repo = InMemoryNotes::default();
        let mut note = note("Shopping");
        let id = note.id();
        note.archive().unwrap();

        repo.save(note).await.unwrap();
        assert!(repo.fetch(id).await.unwrap().unwrap().is_archived());
    }

    #[tokio::test]
    async fn saving_the_same_id_replaces_rather_than_duplicates() {
        let repo = InMemoryNotes::default();
        let mut note = note("Shopping");
        let id = note.id();
        repo.save(note.clone()).await.unwrap();

        note.edit(NoteTitle::new("Groceries").unwrap(), "eggs".into())
            .unwrap();
        repo.save(note).await.unwrap();

        assert_eq!(repo.list().await.unwrap().len(), 1);
        assert_eq!(
            repo.fetch(id).await.unwrap().unwrap().title().as_str(),
            "Groceries"
        );
    }

    #[tokio::test]
    async fn missing_notes_are_none_not_errors() {
        let repo = InMemoryNotes::default();
        let id = NoteId::generate();
        assert!(repo.fetch(id).await.unwrap().is_none());
        assert!(repo.delete(id).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn deleting_returns_the_removed_note() {
        let repo = InMemoryNotes::default();
        let note = note("Shopping");
        let id = note.id();
        repo.save(note).await.unwrap();

        assert_eq!(repo.delete(id).await.unwrap().unwrap().id(), id);
        assert!(repo.list().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_stored_row_that_breaks_an_invariant_is_an_internal_error() {
        let repo = InMemoryNotes::default();
        let id = Uuid::new_v4();
        // As if an older release, or a hand-edited table, had written this.
        repo.lock().unwrap().insert(
            id,
            NoteRow {
                id,
                title: "   ".to_string(),
                body: String::new(),
                archived: false,
            },
        );

        let err = repo.fetch(NoteId::from_uuid(id)).await.unwrap_err();
        assert!(
            matches!(err, DomainError::Internal { .. }),
            "corrupt storage is not the caller's fault"
        );
    }
}
