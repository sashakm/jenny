//! Events in: the in-process bus and JetStream, as driving adapters like `rest`.
//!
//! Each handler owns its DTO, converts it into the command a REST request would become,
//! and calls the use case with the `Context` the runtime built from the event: its
//! correlation id as request id, and `System[basic-example/events]` as the principal.
//! The use case cannot tell an event from a request.

use jenny::messaging::{MessagingAdapter, SubjectBinding};
use jenny::{Context, DomainError, DomainResult};
use serde::Deserialize;
use uuid::Uuid;

use crate::application::Notes;
use crate::domain::model::NoteId;

/// What a `note.archived` event says, as this adapter reads it.
#[derive(Debug, Deserialize)]
pub struct NoteArchived {
    id: Uuid,
}

/// On the bus: who archived what, for an audit trail. `cx` names the service, and the
/// caller the event was raised for when `messaging.events.headers.caller` is on.
pub async fn audit_archived(event: NoteArchived, cx: Context) -> DomainResult<()> {
    let caller = cx
        .get::<jenny::port::EventCaller>()
        .map(|c| c.subject.as_str());
    tracing::info!(
        note = %event.id,
        handled_by = %cx.require_principal()?,
        caller,
        request_id = %cx.request_id(),
        "note archived"
    );
    Ok(())
}

/// A request from elsewhere to archive a note, on JetStream.
#[derive(Debug, Deserialize)]
pub struct ArchiveRequested {
    note: Uuid,
}

/// Consumes `notes.requests.archive` from `stream`: archive the note, as the service.
///
/// An already archived note is a conflict, which would be retried; here it means the
/// work is done, so it is acknowledged instead. A note that does not exist is
/// `NotFound`, which is terminated rather than retried.
pub fn requests(notes: Notes, stream: &str) -> anyhow::Result<MessagingAdapter> {
    Ok(MessagingAdapter::new("note-requests", "notes.requests")?
        .describe("requests from other services")
        .stream(stream)
        .on(
            SubjectBinding::new("archive").with_description("archive a note"),
            move |request: ArchiveRequested, cx: Context| {
                let notes = notes.clone();
                async move {
                    match notes.archive(NoteId::from_uuid(request.note), &cx).await {
                        Err(DomainError::Conflict { .. }) => Ok(()),
                        other => other,
                    }
                }
            },
        ))
}
