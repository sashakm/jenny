//! The REST driving adapter for notes.
//!
//! Everything HTTP-shaped lives here and nowhere else: the JSON DTOs, the conversions
//! into domain commands, and the status codes. Each handler is three lines — extract,
//! convert, call — because anything longer would be business logic in the wrong ring.

use std::sync::Arc;

use axum::extract::{Path, State};
use jenny::rest::{ApiResult, Ctx, Json, Listing, Routes, StatusCode};
use jenny::{DomainError, Dto, EventPublisher, Validate};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::application::Notes;
use crate::application::command::{CreateNote, UpdateNote};
use crate::domain::model::{Note, NoteId, NoteTitle};
use crate::domain::port::NoteRepository;

/// A note to create or replace.
///
/// Deliberately *not* shared with any other adapter — a gRPC adapter gets its own
/// protobuf type, and both convert into [`CreateNote`].
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

impl From<Note> for NoteResponse {
    fn from(note: Note) -> Self {
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

impl TryFrom<(Uuid, NoteRequest)> for UpdateNote {
    type Error = DomainError;

    fn try_from((id, dto): (Uuid, NoteRequest)) -> Result<Self, Self::Error> {
        dto.validate()?;
        Ok(UpdateNote {
            id: NoteId::from_uuid(id),
            title: NoteTitle::new(dto.title)?,
            body: dto.body,
        })
    }
}

type App<R, P> = State<Arc<Notes<R, P>>>;

async fn list<R: NoteRepository, P: EventPublisher + ?Sized>(
    State(notes): App<R, P>,
    listing: Listing,
    cx: Ctx,
) -> ApiResult<Json<Vec<NoteResponse>>> {
    let found = notes.list(&listing, &cx).await?;
    Ok(Json(found.into_iter().map(NoteResponse::from).collect()))
}

async fn create<R: NoteRepository, P: EventPublisher + ?Sized>(
    State(notes): App<R, P>,
    cx: Ctx,
    Json(dto): Json<NoteRequest>,
) -> ApiResult<(StatusCode, Json<Uuid>)> {
    let id = notes.create(dto.try_into()?, &cx).await?;
    Ok((StatusCode::CREATED, Json(id.as_uuid())))
}

async fn fetch<R: NoteRepository, P: EventPublisher + ?Sized>(
    State(notes): App<R, P>,
    Path(id): Path<Uuid>,
    cx: Ctx,
) -> ApiResult<Json<NoteResponse>> {
    Ok(Json(notes.fetch(NoteId::from_uuid(id), &cx).await?.into()))
}

async fn update<R: NoteRepository, P: EventPublisher + ?Sized>(
    State(notes): App<R, P>,
    Path(id): Path<Uuid>,
    cx: Ctx,
    Json(dto): Json<NoteRequest>,
) -> ApiResult<StatusCode> {
    notes.update((id, dto).try_into()?, &cx).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn archive<R: NoteRepository, P: EventPublisher + ?Sized>(
    State(notes): App<R, P>,
    Path(id): Path<Uuid>,
    cx: Ctx,
) -> ApiResult<StatusCode> {
    notes.archive(NoteId::from_uuid(id), &cx).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn delete<R: NoteRepository, P: EventPublisher + ?Sized>(
    State(notes): App<R, P>,
    Path(id): Path<Uuid>,
    cx: Ctx,
) -> ApiResult<Json<NoteResponse>> {
    Ok(Json(notes.delete(NoteId::from_uuid(id), &cx).await?.into()))
}

/// The whole adapter, as one value. No trait to implement, nothing to erase.
pub fn routes<R, P>(notes: Arc<Notes<R, P>>) -> Routes
where
    R: NoteRepository,
    P: EventPublisher + ?Sized,
{
    Routes::new("notes")
        .describe("Create, read, update, archive and delete notes")
        .label("kind", "crud")
        .get("/notes", list::<R, P>)
        .post("/notes", create::<R, P>)
        .get("/notes/{id}", fetch::<R, P>)
        .put("/notes/{id}", update::<R, P>)
        .delete("/notes/{id}", delete::<R, P>)
        .post("/notes/{id}/archive", archive::<R, P>)
        .with_state(notes)
}
