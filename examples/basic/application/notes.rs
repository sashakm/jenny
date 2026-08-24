//! The note use cases.

use std::sync::Arc;

use jenny::domain::query::{Check, Criteria, Ordering};
use jenny::{Context, DomainError, DomainEvent, DomainResult, EventEnvelope, EventPublisher};

use crate::domain::model::{Note, NoteId};
use crate::domain::port::NoteRepository;

use super::command::{CreateNote, UpdateNote};

/// Everything you can do with a note.
///
/// Generic over its two outbound ports, so the compiler sees straight through to the
/// concrete adapters the composition root picked. Swapping the in-memory repository
/// for Postgres is a change in `main.rs` and nowhere else.
///
/// `P: ?Sized` is what lets the composition root pass an `Arc<dyn EventPublisher>`
/// here instead of a concrete type — one word, and both dispatch strategies are on
/// the table without the application layer caring which one it got.
pub struct Notes<R, P: ?Sized> {
    repository: Arc<R>,
    events: Arc<P>,
}

impl<R, P> Notes<R, P>
where
    R: NoteRepository,
    P: EventPublisher + ?Sized,
{
    pub fn new(repository: Arc<R>, events: Arc<P>) -> Self {
        Self { repository, events }
    }

    pub async fn create(&self, cmd: CreateNote, cx: &Context) -> DomainResult<NoteId> {
        let (note, event) = Note::create(cmd.title, cmd.body);
        let id = note.id();

        self.repository.save(note).await?;
        // Publish only after the state change has been committed. See the note on
        // dual writes below.
        self.publish(&event, cx).await;

        Ok(id)
    }

    /// The attribute paths this listing may be sorted and filtered on.
    ///
    /// The allowlist is the security boundary for client-supplied criteria: a path
    /// outside it is refused rather than reaching storage. A repository backed by a
    /// database would pass the equivalent `Columns` table to
    /// `jenny_persistence::SqlCriteria` instead, which requires one for the same
    /// reason.
    pub const QUERYABLE: &'static [&'static str] = &["title", "body", "archived"];

    pub async fn list(&self, criteria: &Criteria, _cx: &Context) -> DomainResult<Vec<Note>> {
        criteria.ensure_allowed(Self::QUERYABLE)?;

        let mut notes = self.repository.list().await?;
        notes.retain(|note| criteria.filter.iter().all(|f| matches(note, f)));
        sort_notes(&mut notes, criteria);

        // Paging last, so it slices the result the client actually asked for.
        if criteria.page.limit() > 0 {
            let offset = criteria.page.offset() as usize;
            notes = notes
                .into_iter()
                .skip(offset)
                .take(criteria.page.limit() as usize)
                .collect();
        }
        Ok(notes)
    }

    pub async fn fetch(&self, id: NoteId, _cx: &Context) -> DomainResult<Note> {
        self.repository
            .fetch(id)
            .await?
            .ok_or_else(DomainError::not_found)
    }

    pub async fn update(&self, cmd: UpdateNote, cx: &Context) -> DomainResult<()> {
        let mut note = self.fetch(cmd.id, cx).await?;
        // The rule about editing archived notes lives in the aggregate, not here.
        note.edit(cmd.title, cmd.body)?;
        self.repository.save(note).await
    }

    pub async fn archive(&self, id: NoteId, cx: &Context) -> DomainResult<()> {
        let mut note = self.fetch(id, cx).await?;
        let event = note.archive()?;

        self.repository.save(note).await?;
        self.publish(&event, cx).await;

        Ok(())
    }

    pub async fn delete(&self, id: NoteId, _cx: &Context) -> DomainResult<Note> {
        self.repository
            .delete(id)
            .await?
            .ok_or_else(DomainError::not_found)
    }

    /// Encode a raised event and hand it to the outbound port.
    ///
    /// Serde appears *here*, one ring out from the domain, mirroring how a DTO is
    /// converted into a command on the way in. The domain event stayed a plain struct.
    ///
    /// A failure to publish is logged rather than returned, because the state change
    /// already succeeded and failing the caller's request would misreport what
    /// happened. That is the dual-write problem in miniature: two writes, one
    /// outcome. When your consistency requirements outgrow it, put a transactional
    /// outbox behind [`EventPublisher`] — write the event to an outbox table inside
    /// the same transaction as the state change, and let a relay drain it. This method
    /// does not change.
    async fn publish(&self, event: &(impl DomainEvent + EventPayload), cx: &Context) {
        let envelope = match EventEnvelope::json(event.name(), &event.payload()) {
            Ok(envelope) => envelope,
            Err(e) => {
                tracing::error!(request_id = %cx.request_id(), "could not encode event: {e}");
                return;
            }
        };
        if let Err(e) = self.events.publish(envelope).await {
            tracing::error!(request_id = %cx.request_id(), "could not publish event: {e}");
        }
    }
}

/// Read one filterable attribute off a note.
fn attribute(note: &Note, path: &str) -> Option<String> {
    match path {
        "title" => Some(note.title().to_string()),
        "body" => Some(note.body().to_string()),
        "archived" => Some(note.is_archived().to_string()),
        // Unreachable: `ensure_allowed` ran first.
        _ => None,
    }
}

fn matches(note: &Note, filter: &jenny::domain::query::Filter) -> bool {
    let Some(value) = attribute(note, filter.path()) else {
        return false;
    };
    let (value, wanted) = (value.to_lowercase(), filter.value().to_lowercase());
    match filter.check() {
        Check::Eq => value == wanted,
        Check::Begins => value.starts_with(&wanted),
        Check::Ends => value.ends_with(&wanted),
        Check::Contains => value.contains(&wanted),
        Check::Gt => value > wanted,
        Check::St => value < wanted,
    }
}

fn sort_notes(notes: &mut [Note], criteria: &Criteria) {
    // Later keys are applied first so that earlier ones win, which is what a stable
    // sort gives us and what a client listing `sort=a&sort=b` expects.
    for sort in criteria.sort.iter().rev() {
        notes.sort_by(|left, right| {
            let ordering = attribute(left, sort.path()).cmp(&attribute(right, sort.path()));
            match sort.ordering() {
                Ordering::Asc => ordering,
                Ordering::Desc => ordering.reverse(),
            }
        });
    }
}

/// How a domain event is rendered for the wire.
///
/// Kept in the application layer so the domain's event structs need no serde derive —
/// the same reason DTOs live in the inbound adapters rather than in `domain/`.
pub trait EventPayload {
    fn payload(&self) -> serde_json::Value;
}

impl EventPayload for crate::domain::event::NoteCreated {
    fn payload(&self) -> serde_json::Value {
        serde_json::json!({ "id": self.id.as_uuid(), "title": self.title.as_str() })
    }
}

impl EventPayload for crate::domain::event::NoteArchived {
    fn payload(&self) -> serde_json::Value {
        serde_json::json!({ "id": self.id.as_uuid() })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapter::outbound::memory::InMemoryNotes;
    use crate::domain::model::NoteTitle;
    use std::sync::Mutex;

    /// A test double for the event port — the point of owning the port at all.
    #[derive(Default)]
    struct RecordedEvents(Mutex<Vec<String>>);

    #[async_trait::async_trait]
    impl EventPublisher for RecordedEvents {
        async fn publish(&self, event: EventEnvelope) -> Result<(), jenny::PublishError> {
            self.0.lock().unwrap().push(event.name().to_string());
            Ok(())
        }
    }

    fn notes() -> (Notes<InMemoryNotes, RecordedEvents>, Arc<RecordedEvents>) {
        let events = Arc::new(RecordedEvents::default());
        let notes = Notes::new(Arc::new(InMemoryNotes::default()), events.clone());
        (notes, events)
    }

    fn create(title: &str) -> CreateNote {
        CreateNote {
            title: NoteTitle::new(title).unwrap(),
            body: "body".to_string(),
        }
    }

    #[tokio::test]
    async fn creating_a_note_stores_it_and_publishes() {
        let (notes, events) = notes();
        let cx = Context::new();

        let id = notes.create(create("Shopping"), &cx).await.unwrap();

        let stored = notes.fetch(id, &cx).await.unwrap();
        assert_eq!(stored.title().as_str(), "Shopping");
        assert_eq!(events.0.lock().unwrap().as_slice(), ["note.created"]);
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
    async fn updating_replaces_the_contents() {
        let (notes, _) = notes();
        let cx = Context::new();
        let id = notes.create(create("Shopping"), &cx).await.unwrap();

        notes
            .update(
                UpdateNote {
                    id,
                    title: NoteTitle::new("Groceries").unwrap(),
                    body: "eggs".into(),
                },
                &cx,
            )
            .await
            .unwrap();

        let updated = notes.fetch(id, &cx).await.unwrap();
        assert_eq!(updated.title().as_str(), "Groceries");
        assert_eq!(updated.body(), "eggs");
    }

    #[tokio::test]
    async fn archiving_publishes_once_and_then_conflicts() {
        let (notes, events) = notes();
        let cx = Context::new();
        let id = notes.create(create("Shopping"), &cx).await.unwrap();

        notes.archive(id, &cx).await.unwrap();
        assert_eq!(
            events.0.lock().unwrap().as_slice(),
            ["note.created", "note.archived"]
        );

        let err = notes.archive(id, &cx).await.unwrap_err();
        assert!(matches!(err, DomainError::Conflict { .. }));
        assert_eq!(
            events.0.lock().unwrap().len(),
            2,
            "a rejected archive must not publish anything"
        );
    }

    #[tokio::test]
    async fn an_archived_note_can_no_longer_be_updated() {
        let (notes, _) = notes();
        let cx = Context::new();
        let id = notes.create(create("Shopping"), &cx).await.unwrap();
        notes.archive(id, &cx).await.unwrap();

        let err = notes
            .update(
                UpdateNote {
                    id,
                    title: NoteTitle::new("Nope").unwrap(),
                    body: String::new(),
                },
                &cx,
            )
            .await
            .unwrap_err();
        assert!(matches!(err, DomainError::Conflict { .. }));
    }

    #[tokio::test]
    async fn deleting_returns_what_was_removed_then_reports_not_found() {
        let (notes, _) = notes();
        let cx = Context::new();
        let id = notes.create(create("Shopping"), &cx).await.unwrap();

        let removed = notes.delete(id, &cx).await.unwrap();
        assert_eq!(removed.id(), id);
        assert!(matches!(
            notes.delete(id, &cx).await.unwrap_err(),
            DomainError::NotFound
        ));
        assert!(notes.list(&Criteria::new(), &cx).await.unwrap().is_empty());
    }
}
