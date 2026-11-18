//! The REST driving adapter for notes.
//!
//! Everything HTTP-shaped lives here and nowhere else: the JSON DTOs, the conversions
//! into domain commands, and the status codes. Each handler is three lines — extract,
//! convert, call — because anything longer would be business logic in the wrong ring.
//!
//! There is no PATCH handler, and the adapter still answers PATCH: it accepts JSON
//! Patch (see `main.rs`), so it derives one from `fetch` and `update`. Two things here
//! make that safe. `fetch` sends the note's version as an `ETag`, and `update` takes
//! `If-Match` and hands it to the use case, so a patch fails with 412 instead of
//! overwriting a change made in between. And `NoteRequest` accepts the whole
//! [`NoteResponse`], ignoring `id` and `archived`, so a patch applies to exactly what a
//! client read — and one that tries to change `archived` is refused with 422.

use axum::extract::{Path, State};
use jenny::rest::{ApiResult, Ctx, IfMatch, Json, Listing, Routes, StatusCode, Versioned};
use jenny::{DomainError, Dto, Validate};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::application::Notes;
use crate::application::command::{CreateNote, EditNote};
use crate::domain::model::{Note, NoteId, NoteTitle};

/// A note to create or replace.
///
/// Deliberately *not* shared with any other adapter — a gRPC adapter gets its own
/// protobuf type, and both convert into [`CreateNote`] or [`EditNote`].
///
/// `#[derive(Dto)]` turns these attributes into a `Validate` impl and, with the
/// `openapi` feature, the matching OpenAPI schema. One place to change a bound, and
/// the documentation cannot drift from what the code enforces.
#[derive(Debug, Deserialize, Dto)]
pub struct NoteRequest {
    #[dto(not_empty, max_len = 120, example = "Shopping")]
    title: String,

    #[dto(max_len = 10_000, description = "Free-text body")]
    #[serde(default)]
    body: String,
}

/// What this protocol returns.
#[derive(Debug, Serialize)]
pub struct NoteResponse {
    id: Uuid,
    title: String,
    body: String,
    archived: bool,
}

impl From<&Note> for NoteResponse {
    fn from(note: &Note) -> Self {
        Self {
            id: note.id().as_uuid(),
            title: note.title().to_string(),
            body: note.body().to_string(),
            archived: note.is_archived(),
        }
    }
}

/// The `TryFrom` **is** the input validation.
///
/// It is the one place where untrusted strings become domain types, and it fails with
/// a [`DomainError`] that the REST layer already knows how to turn into a 400. No
/// handler ever validates anything itself.
impl TryFrom<NoteRequest> for CreateNote {
    type Error = DomainError;

    fn try_from(dto: NoteRequest) -> Result<Self, Self::Error> {
        // Format rules first, from the derive; then the domain's own constructor,
        // which is what actually makes an invalid title unrepresentable.
        dto.validate()?;
        Ok(CreateNote {
            title: NoteTitle::new(dto.title)?,
            body: dto.body,
        })
    }
}

impl TryFrom<NoteRequest> for EditNote {
    type Error = DomainError;

    fn try_from(dto: NoteRequest) -> Result<Self, Self::Error> {
        let CreateNote { title, body } = dto.try_into()?;
        Ok(EditNote { title, body })
    }
}

async fn list(
    State(notes): State<Notes>,
    listing: Listing,
    cx: Ctx,
) -> ApiResult<Json<Vec<NoteResponse>>> {
    let found = notes.list(&listing, &cx).await?;
    Ok(Json(found.iter().map(NoteResponse::from).collect()))
}

async fn create(
    State(notes): State<Notes>,
    cx: Ctx,
    Json(dto): Json<NoteRequest>,
) -> ApiResult<(StatusCode, Json<Uuid>)> {
    let id = notes.create(dto.try_into()?, &cx).await?;
    Ok((StatusCode::CREATED, Json(id.as_uuid())))
}

async fn fetch(
    State(notes): State<Notes>,
    Path(id): Path<Uuid>,
    cx: Ctx,
) -> ApiResult<Versioned<Json<NoteResponse>>> {
    let note = notes.fetch(NoteId::from_uuid(id), &cx).await?;
    Ok(Versioned::new(
        note.version(),
        Json(NoteResponse::from(&*note)),
    ))
}

async fn update(
    State(notes): State<Notes>,
    Path(id): Path<Uuid>,
    IfMatch(expected): IfMatch,
    cx: Ctx,
    Json(dto): Json<NoteRequest>,
) -> ApiResult<StatusCode> {
    notes
        .edit(NoteId::from_uuid(id), dto.try_into()?, expected, &cx)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn archive(
    State(notes): State<Notes>,
    Path(id): Path<Uuid>,
    cx: Ctx,
) -> ApiResult<StatusCode> {
    notes.archive(NoteId::from_uuid(id), &cx).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn delete(
    State(notes): State<Notes>,
    Path(id): Path<Uuid>,
    IfMatch(expected): IfMatch,
    cx: Ctx,
) -> ApiResult<Json<NoteResponse>> {
    let removed = notes.delete(NoteId::from_uuid(id), expected, &cx).await?;
    Ok(Json(NoteResponse::from(&removed)))
}

/// The whole adapter, as one value. No trait to implement, nothing to erase.
pub fn routes(notes: Notes) -> Routes {
    Routes::new("notes")
        .describe("Create, read, update, archive and delete notes")
        .label("kind", "crud")
        .get("/notes", list)
        .post("/notes", create)
        .get("/notes/{id}", fetch)
        .put("/notes/{id}", update)
        .delete("/notes/{id}", delete)
        .post("/notes/{id}/archive", archive)
        .with_state(notes)
}
