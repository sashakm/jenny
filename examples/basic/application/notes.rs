//! The note use cases.

use std::sync::Arc;

use jenny::port::Blob;
use jenny::{
    Context, Criteria, DomainError, DomainResult, Events, Loaded, StoreExt, Version, use_cases,
};

use crate::domain::model::{Note, NoteId};
use crate::domain::port::{Attachments, Links, NoteLinks, NoteStore};

use super::command::{CreateNote, EditNote};

/// Everything you can do with a note.
///
/// Cheap to clone — every field is a shared handle — so a driving adapter can hold its
/// own copy. Swapping the in-memory store for Postgres, memory for S3, or a set of links
/// for a graph, is a change in `main.rs` and nowhere else.
#[derive(Clone)]
pub struct Notes {
    notes: Arc<NoteStore>,
    attachments: Arc<Attachments>,
    links: Arc<dyn NoteLinks>,
    events: Events,
}

/// Where a note's attachment is kept. One per note, so the note's id is the key.
fn attachment_key(id: NoteId) -> String {
    format!("notes/{id}/attachment")
}

#[use_cases]
impl Notes {
    pub fn new(
        notes: Arc<NoteStore>,
        attachments: Arc<Attachments>,
        links: Arc<dyn NoteLinks>,
        events: Events,
    ) -> Self {
        Self {
            notes,
            attachments,
            links,
            events,
        }
    }

    pub async fn create(&self, cmd: CreateNote, cx: &Context) -> DomainResult<NoteId> {
        let (note, created) = Note::create(cmd.title, cmd.body);
        // The note and the event that says so, together: in one transaction when the
        // store keeps an outbox, published right after the insert otherwise.
        let note = self
            .notes
            .add(note, created)
            .raising(&self.events, cx)
            .await?;
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
        self.notes
            .change(&id, |note| note.edit(cmd.title, cmd.body))
            .expecting(expected)
            .raising(&self.events, cx)
            .announcing_changes()
            .await?;
        Ok(())
    }

    pub async fn archive(&self, id: NoteId, cx: &Context) -> DomainResult<()> {
        self.notes
            .change(&id, Note::archive)
            .raising(&self.events, cx)
            .await?;
        Ok(())
    }

    /// Delete a note, its links and its attachment.
    ///
    /// The note goes first: if what follows fails, what is left behind is a link or a
    /// blob that points at nothing, never a note that lost its links or its file.
    pub async fn delete(
        &self,
        id: NoteId,
        expected: Option<Version>,
        _cx: &Context,
    ) -> DomainResult<Note> {
        let removed = self.notes.remove(&id, expected).await?;
        self.links.forget(id).await?;
        self.attachments.delete(&attachment_key(id)).await?;
        Ok(removed)
    }

    /// `from` refers to `to` from now on. Both must exist, and which links a note may
    /// have is the note's rule.
    pub async fn link(&self, from: NoteId, to: NoteId, _cx: &Context) -> DomainResult<()> {
        self.notes.load(&from).await?.ensure_linkable_to(to)?;
        self.notes.load(&to).await?;
        self.links.link(from, to).await
    }

    /// `from` no longer refers to `to`. Nothing happens if it did not.
    pub async fn unlink(&self, from: NoteId, to: NoteId, _cx: &Context) -> DomainResult<()> {
        self.links.unlink(from, to).await
    }

    /// What a note refers to, and what refers to it.
    pub async fn links(&self, id: NoteId, _cx: &Context) -> DomainResult<Links> {
        self.notes.load(&id).await?;
        self.links.links(id).await
    }

    /// Attach a file to a note, replacing the one it had. The note itself does not
    /// change; whether it may take one is the note's rule.
    pub async fn attach(&self, id: NoteId, file: Blob, _cx: &Context) -> DomainResult<()> {
        self.notes.load(&id).await?.ensure_attachable()?;
        self.attachments.put(&attachment_key(id), file).await?;
        Ok(())
    }

    /// The file attached to a note.
    pub async fn attachment(&self, id: NoteId, _cx: &Context) -> DomainResult<Blob> {
        match self.attachments.get(&attachment_key(id)).await? {
            Some((file, _)) => Ok(file),
            None => Err(DomainError::not_found()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::model::NoteTitle;
    use jenny::domain::ValueKind;
    use jenny::memory::{Memory, MemoryBlobs};
    use jenny::testing::RecordedEvents;

    use crate::adapter::outbound::links::MemoryLinks;

    fn notes() -> (Notes, Arc<RecordedEvents>) {
        let events = Arc::new(RecordedEvents::default());
        let notes = Notes::new(
            Arc::new(Memory::<Note>::new().queryable([("title", ValueKind::Text)])),
            Arc::new(MemoryBlobs::new()),
            Arc::new(MemoryLinks::default()),
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

    #[tokio::test]
    async fn an_attachment_is_kept_beside_its_note_and_goes_with_it() {
        let (notes, _) = notes();
        let cx = Context::new();
        let id = notes.create(create("Shopping"), &cx).await.unwrap();

        let file = Blob::new(b"milk, eggs".to_vec()).with_content_type("text/plain");
        notes.attach(id, file.clone(), &cx).await.unwrap();
        assert_eq!(notes.attachment(id, &cx).await.unwrap(), file);
        assert_eq!(notes.fetch(id, &cx).await.unwrap().version().get(), 1);

        notes.delete(id, None, &cx).await.unwrap();
        assert!(matches!(
            notes.attachment(id, &cx).await.unwrap_err(),
            DomainError::NotFound
        ));
    }

    #[tokio::test]
    async fn only_a_note_that_exists_and_is_not_archived_takes_an_attachment() {
        let (notes, _) = notes();
        let cx = Context::new();
        let file = || Blob::new(b"x".to_vec());

        let err = notes
            .attach(NoteId::generate(), file(), &cx)
            .await
            .unwrap_err();
        assert!(matches!(err, DomainError::NotFound));

        let id = notes.create(create("Shopping"), &cx).await.unwrap();
        notes.archive(id, &cx).await.unwrap();
        let err = notes.attach(id, file(), &cx).await.unwrap_err();
        assert!(matches!(err, DomainError::Conflict { .. }));
    }

    #[tokio::test]
    async fn links_are_seen_from_both_ends_and_go_with_the_note() {
        let (notes, _) = notes();
        let cx = Context::new();
        let recipe = notes.create(create("Recipe"), &cx).await.unwrap();
        let shopping = notes.create(create("Shopping"), &cx).await.unwrap();

        notes.link(recipe, shopping, &cx).await.unwrap();
        notes.link(recipe, shopping, &cx).await.unwrap();
        assert_eq!(
            notes.links(recipe, &cx).await.unwrap(),
            Links {
                to: vec![shopping],
                from: vec![]
            }
        );
        assert_eq!(notes.links(shopping, &cx).await.unwrap().from, [recipe]);

        notes.delete(shopping, None, &cx).await.unwrap();
        assert_eq!(notes.links(recipe, &cx).await.unwrap(), Links::default());
    }

    #[tokio::test]
    async fn a_link_needs_two_notes_that_exist() {
        let (notes, _) = notes();
        let cx = Context::new();
        let id = notes.create(create("Shopping"), &cx).await.unwrap();

        let err = notes.link(id, id, &cx).await.unwrap_err();
        assert!(matches!(err, DomainError::Invalid { .. }), "{err}");
        let err = notes.link(id, NoteId::generate(), &cx).await.unwrap_err();
        assert!(matches!(err, DomainError::NotFound), "{err}");
        let err = notes.links(NoteId::generate(), &cx).await.unwrap_err();
        assert!(matches!(err, DomainError::NotFound), "{err}");
    }
}
