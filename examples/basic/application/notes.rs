//! The note use cases.

use std::sync::Arc;

use jenny::{Context, Criteria, DomainResult, Events, Loaded, StoreExt, Version, use_cases};

use crate::domain::model::{Note, NoteId};
use crate::domain::port::NoteStore;

use super::command::{CreateNote, EditNote};

/// Everything you can do with a note.
///
/// Cheap to clone — both fields are shared handles — so a driving adapter can hold its
/// own copy. Swapping the in-memory store for Postgres is a change in `main.rs` and
/// nowhere else.
#[derive(Clone)]
pub struct Notes {
    notes: Arc<NoteStore>,
    events: Events,
}

#[use_cases]
impl Notes {
    pub fn new(notes: Arc<NoteStore>, events: Events) -> Self {
        Self { notes, events }
    }

    pub async fn create(&self, cmd: CreateNote, cx: &Context) -> DomainResult<NoteId> {
        let (note, created) = Note::create(cmd.title, cmd.body);
        let note = self.notes.insert(note).await?;
        // Publish only once the state change has been committed.
        self.events.publish(&created, cx).await;
        Ok(note.id())
    }

    /// Paging, sorting and filtering are the store's job; which attributes a client may
    /// name is the allowlist it was built with in `main.rs`.
    pub async fn list(&self, criteria: &Criteria, _cx: &Context) -> DomainResult<Vec<Note>> {
        self.notes.list(criteria).await
    }

    /// The note and the version it was read at.
    pub async fn fetch(&self, id: NoteId, _cx: &Context) -> DomainResult<Loaded<Note>> {
        self.notes.load(&id).await
    }

    /// Replace a note's contents — if it is still at `expected`, when the caller says
    /// which version it read (an HTTP `If-Match`).
    ///
    /// Listeners hear a `note.updated` carrying exactly what changed, as a JSON Patch.
    /// An edit that changes nothing writes nothing and publishes nothing.
    pub async fn edit(
        &self,
        id: NoteId,
        cmd: EditNote,
        expected: Option<Version>,
        cx: &Context,
    ) -> DomainResult<()> {
        // The rule about editing archived notes lives in the aggregate, not here.
        let edited = self
            .notes
            .change_at(&id, expected, |note| note.edit(cmd.title, cmd.body))
            .await?;
        if let Some(updated) = edited.updated() {
            self.events.publish(&updated, cx).await;
        }
        Ok(())
    }

    pub async fn archive(&self, id: NoteId, cx: &Context) -> DomainResult<()> {
        let archived = self.notes.change(&id, Note::archive).await?;
        self.events.publish(&archived.outcome, cx).await;
        Ok(())
    }

    pub async fn delete(
        &self,
        id: NoteId,
        expected: Option<Version>,
        _cx: &Context,
    ) -> DomainResult<Note> {
        self.notes.remove(&id, expected).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::model::NoteTitle;
    use jenny::DomainError;
    use jenny::memory::Memory;
    use jenny::testing::RecordedEvents;

    fn notes() -> (Notes, Arc<RecordedEvents>) {
        let events = Arc::new(RecordedEvents::default());
        let notes = Notes::new(
            Arc::new(Memory::<Note>::new().queryable(["title"])),
            Events::new(events.clone()),
        );
        (notes, events)
    }

    fn create(title: &str) -> CreateNote {
        CreateNote {
            title: NoteTitle::new(title).unwrap(),
            body: "body".to_string(),
        }
    }

    fn edit(title: &str, body: &str) -> EditNote {
        EditNote {
            title: NoteTitle::new(title).unwrap(),
            body: body.to_string(),
        }
    }

    #[tokio::test]
    async fn creating_a_note_stores_it_and_publishes() {
        let (notes, events) = notes();
        let cx = Context::new();

        let id = notes.create(create("Shopping"), &cx).await.unwrap();

        let stored = notes.fetch(id, &cx).await.unwrap();
        assert_eq!(stored.title().as_str(), "Shopping");
        assert_eq!(events.names(), ["note.created"]);
    }

    #[tokio::test]
    async fn fetching_something_that_is_not_there_is_not_found() {
        let (notes, _) = notes();
        let err = notes
            .fetch(NoteId::generate(), &Context::new())
            .await
            .unwrap_err();
        assert!(matches!(err, DomainError::NotFound));
    }

    #[tokio::test]
    async fn editing_replaces_the_contents_and_says_what_changed() {
        let (notes, events) = notes();
        let cx = Context::new();
        let id = notes.create(create("Shopping"), &cx).await.unwrap();

        notes
            .edit(id, edit("Groceries", "body"), None, &cx)
            .await
            .unwrap();

        let edited = notes.fetch(id, &cx).await.unwrap();
        assert_eq!(edited.title().as_str(), "Groceries");
        assert_eq!(edited.version().get(), 2);
        assert_eq!(
            events.payloads("note.updated"),
            [serde_json::json!({
                "id": id.to_string(),
                "version": 2,
                "changes": [{ "op": "replace", "path": "/title", "value": "Groceries" }],
            })]
        );
    }

    #[tokio::test]
    async fn an_edit_that_changes_nothing_publishes_nothing() {
        let (notes, events) = notes();
        let cx = Context::new();
        let id = notes.create(create("Shopping"), &cx).await.unwrap();

        notes
            .edit(id, edit("Shopping", "body"), None, &cx)
            .await
            .unwrap();

        assert_eq!(events.names(), ["note.created"]);
        assert_eq!(notes.fetch(id, &cx).await.unwrap().version().get(), 1);
    }

    #[tokio::test]
    async fn archiving_publishes_once_and_then_conflicts() {
        let (notes, events) = notes();
        let cx = Context::new();
        let id = notes.create(create("Shopping"), &cx).await.unwrap();

        notes.archive(id, &cx).await.unwrap();
        assert_eq!(events.names(), ["note.created", "note.archived"]);

        let err = notes.archive(id, &cx).await.unwrap_err();
        assert!(matches!(err, DomainError::Conflict { .. }));
        assert_eq!(
            events.names().len(),
            2,
            "a rejected archive must not publish anything"
        );
    }

    #[tokio::test]
    async fn an_archived_note_can_no_longer_be_edited() {
        let (notes, _) = notes();
        let cx = Context::new();
        let id = notes.create(create("Shopping"), &cx).await.unwrap();
        notes.archive(id, &cx).await.unwrap();

        let err = notes
            .edit(id, edit("Nope", ""), None, &cx)
            .await
            .unwrap_err();
        assert!(matches!(err, DomainError::Conflict { .. }));
    }

    #[tokio::test]
    async fn an_edit_against_a_version_that_moved_on_is_stale() {
        let (notes, _) = notes();
        let cx = Context::new();
        let id = notes.create(create("Shopping"), &cx).await.unwrap();
        let read = notes.fetch(id, &cx).await.unwrap().version();
        notes
            .edit(id, edit("Groceries", "body"), Some(read), &cx)
            .await
            .unwrap();

        let err = notes
            .edit(id, edit("Lost update", "body"), Some(read), &cx)
            .await
            .unwrap_err();
        assert!(matches!(err, DomainError::Stale { .. }), "{err}");
        assert_eq!(
            notes.fetch(id, &cx).await.unwrap().title().as_str(),
            "Groceries"
        );
    }

    #[tokio::test]
    async fn deleting_returns_what_was_removed_then_reports_not_found() {
        let (notes, _) = notes();
        let cx = Context::new();
        let id = notes.create(create("Shopping"), &cx).await.unwrap();

        let removed = notes.delete(id, None, &cx).await.unwrap();
        assert_eq!(removed.id(), id);
        assert!(matches!(
            notes.delete(id, None, &cx).await.unwrap_err(),
            DomainError::NotFound
        ));
        assert!(notes.list(&Criteria::new(), &cx).await.unwrap().is_empty());
    }
}
