//! The REST driving adapter — axum and tower.
//!
//! A driving adapter has exactly one job: translate a protocol into a use-case call,
//! and the result back. Everything in this crate serves that translation and nothing
//! else. There is no business logic here, and no trait for you to implement.
//!
//! ### What an inbound REST module looks like
//!
//! ```
//! use std::sync::Arc;
//! use axum::extract::{Path, State};
//! use jenny_rest::{ApiResult, Ctx, Json, Routes};
//! # use jenny_core::domain::{DomainError, Validate, ValidationErrors};
//! # use jenny_core::port::Context;
//! # use serde::{Deserialize, Serialize};
//! # #[derive(Clone)] struct Notes;
//! # struct CreateNote { title: String }
//! # impl Notes {
//! #     async fn create(&self, _: CreateNote, _: &Context) -> Result<u32, DomainError> { Ok(1) }
//! #     async fn fetch(&self, _: u32, _: &Context) -> Result<String, DomainError> { Ok("note".into()) }
//! # }
//! /// Per-protocol DTO. Deliberately not shared with any other adapter.
//! #[derive(Deserialize)]
//! struct CreateNoteDto { title: String }
//!
//! /// The `TryFrom` *is* the format validation.
//! impl TryFrom<CreateNoteDto> for CreateNote {
//!     type Error = DomainError;
//!     fn try_from(dto: CreateNoteDto) -> Result<Self, Self::Error> {
//!         if dto.title.trim().is_empty() {
//!             return Err(DomainError::invalid("title must not be empty"));
//!         }
//!         Ok(CreateNote { title: dto.title })
//!     }
//! }
//!
//! async fn create(
//!     State(notes): State<Arc<Notes>>,
//!     cx: Ctx,
//!     Json(dto): Json<CreateNoteDto>,
//! ) -> ApiResult<Json<u32>> {
//!     Ok(Json(notes.create(dto.try_into()?, &cx).await?))
//! }
//!
//! async fn fetch(
//!     State(notes): State<Arc<Notes>>,
//!     Path(id): Path<u32>,
//!     cx: Ctx,
//! ) -> ApiResult<Json<String>> {
//!     Ok(Json(notes.fetch(id, &cx).await?))
//! }
//!
//! pub fn routes(notes: Arc<Notes>) -> Routes {
//!     Routes::new("notes")
//!         .describe("Note lifecycle")
//!         .post("/notes", create)
//!         .get("/notes/{id}", fetch)
//!         .with_state(notes)
//! }
//! ```
//!
//! Three things carry the weight there. The DTO is per-protocol, so a gRPC adapter's
//! protobuf type and this JSON type can differ freely — what they share is the
//! `CreateNote` *command* they both convert into. The `?` after `try_into()` and after
//! `await` both work because [`Problem`] converts from [`DomainError`], so status
//! mapping happens at the boundary and never inside a handler. And `routes` is a plain
//! function, so wiring it up is a function call.
//!
//! ### Why axum
//!
//! axum is a thin ergonomic layer over [`tower`] and [`hyper`](https://docs.rs/hyper),
//! which buys three things that matter for a hexagonal framework:
//!
//! - **Middleware is shared with gRPC.** tonic is built on the same stack, so a
//!   [`tower_http`] layer applied here will apply unchanged to a future gRPC driving
//!   adapter. Two inbound adapters, one set of cross-cutting concerns.
//! - **The composition root reads like a dependency graph.** One [`Router`](axum::Router)
//!   is built once and cloned per connection, rather than a factory closure re-running
//!   per worker thread.
//! - **Standard types.** Requests and responses are `http` crate types, so anything
//!   speaking `tower::Service` drops straight in.
//!
//! Middleware is plain tower, so reach for [`tower_http`] first — it already has
//! compression, timeouts, request ids, tracing and CORS.
//!
//! [`DomainError`]: jenny_core::domain::DomainError

mod adapter;
mod extract;
mod problem;
mod request_id;
mod routes;
pub mod security;

/// The underlying web framework, re-exported so a service needs one dependency rather
/// than three. Reach for it whenever you want something jenny does not wrap:
/// `axum::extract::{Path, Query, State}`, `axum::routing::any`, `axum::Router`.
pub use axum;
/// The tower middleware collection — compression, timeouts, request ids, tracing.
pub use tower_http;

#[doc(no_inline)]
pub use http::{HeaderMap, HeaderValue, Method, StatusCode, header};

#[doc(inline)]
pub use adapter::{DEFAULT_API_PATH, MountedResource, RestAdapter};
#[doc(inline)]
pub use extract::{Ctx, Json, Listing, REQUEST_ID_HEADER, RouteId};
#[doc(inline)]
pub use problem::{ApiResult, PROBLEM_CONTENT_TYPE, Problem, ProblemIssue};
#[doc(inline)]
pub use request_id::{RequestId, RequestIdLayer};
#[doc(inline)]
pub use routes::{Endpoint, Routes};

/// Fallback for requests that matched no route, so a 404 has the same shape as every
/// other failure. The runtime installs this; mount it yourself in a hand-rolled app.
pub async fn not_found() -> Problem {
    Problem::not_found()
}

/// Fallback for a path that matched but a method that did not.
pub async fn method_not_allowed() -> Problem {
    Problem::from_status(http::StatusCode::METHOD_NOT_ALLOWED)
}
