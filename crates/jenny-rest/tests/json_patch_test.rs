//! The JSON Patch mode, end to end: a real resource over an in-memory store, mounted on
//! a nested adapter, patched through the `PATCH` the adapter derives from its `GET` and
//! `PUT`.

use std::sync::Arc;

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::extract::{Path, State};
use axum::middleware::{self, Next};
use axum::response::Response;
use jenny_core::domain::{Aggregate, DomainError, DomainResult};
use jenny_core::memory::Memory;
use jenny_core::port::{Store, StoreExt};
use jenny_rest::{
    ApiResult, IfMatch, JSON_PATCH_CONTENT_TYPE, Json, RestAdapter, RouteId, Routes, StatusCode,
    Versioned,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tower::ServiceExt;

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Note {
    id: u32,
    title: String,
    tags: Vec<String>,
    archived: bool,
}

impl Aggregate for Note {
    type Id = u32;
    const KIND: &'static str = "note";
    fn id(&self) -> u32 {
        self.id
    }
}

/// What GET returns: everything, including fields PUT does not take.
#[derive(Serialize)]
struct NoteResponse {
    id: u32,
    title: String,
    tags: Vec<String>,
    archived: bool,
}

/// What PUT takes. It accepts the GET representation and ignores `id` and `archived`.
#[derive(Deserialize)]
struct NoteRequest {
    title: String,
    #[serde(default)]
    tags: Vec<String>,
}

type Notes = Arc<dyn Store<Note>>;

async fn fetch(
    State(notes): State<Notes>,
    Path(id): Path<u32>,
) -> ApiResult<Versioned<Json<NoteResponse>>> {
    let note = notes.load(&id).await?;
    Ok(Versioned::new(
        note.version(),
        Json(NoteResponse {
            id: note.id,
            title: note.title.clone(),
            tags: note.tags.clone(),
            archived: note.archived,
        }),
    ))
}

async fn replace(
    State(notes): State<Notes>,
    Path(id): Path<u32>,
    IfMatch(expected): IfMatch,
    route: RouteId,
    Json(dto): Json<NoteRequest>,
) -> ApiResult<(StatusCode, [(&'static str, String); 1])> {
    if dto.title.trim().is_empty() {
        return Err(DomainError::invalid("title must not be empty").into());
    }
    notes
        .change_at(&id, expected, |note| -> DomainResult<()> {
            note.title = dto.title;
            note.tags = dto.tags;
            Ok(())
        })
        .await?;
    // Which resource served it, to show a forwarded PUT still knows.
    Ok((
        StatusCode::NO_CONTENT,
        [("x-resource", route.urn().to_string())],
    ))
}

/// A resource-level layer, to show the derived PATCH reaches GET and PUT through it.
async fn stamp(request: axum::extract::Request, next: Next) -> Response {
    let method = request.method().clone();
    let mut response = next.run(request).await;
    response
        .headers_mut()
        .append("x-layered", method.as_str().parse().unwrap());
    response
}

fn resource(notes: Notes) -> Routes {
    Routes::new("notes")
        .get("/notes/{id}", fetch)
        .put("/notes/{id}", replace)
        .layer(middleware::from_fn(stamp))
        .with_state(notes)
}

async fn service(json_patch: bool) -> (Router, Notes) {
    let notes: Notes = Arc::new(Memory::<Note>::new());
    notes
        .insert(Note {
            id: 1,
            title: "Shopping".into(),
            tags: vec!["home".into()],
            archived: false,
        })
        .await
        .unwrap();

    let mut adapter = RestAdapter::new("my-notes").unwrap();
    if json_patch {
        adapter = adapter.accept_json_patch().unwrap();
    }
    let router = adapter
        .mount(resource(notes.clone()))
        .unwrap()
        .into_router();
    (router, notes)
}

struct Reply {
    status: StatusCode,
    headers: http::HeaderMap,
    body: Value,
}

async fn send(router: &Router, request: http::Request<Body>) -> Reply {
    let response = router.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    Reply {
        status,
        headers,
        body: serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    }
}

fn patch() -> http::request::Builder {
    http::Request::patch("/api/notes/1").header("content-type", JSON_PATCH_CONTENT_TYPE)
}

async fn patch_with(router: &Router, operations: Value) -> Reply {
    send(
        router,
        patch().body(Body::from(operations.to_string())).unwrap(),
    )
    .await
}

async fn get(router: &Router) -> Reply {
    send(
        router,
        http::Request::get("/api/notes/1")
            .body(Body::empty())
            .unwrap(),
    )
    .await
}

#[tokio::test]
async fn a_patch_changes_what_it_names_and_nothing_else() {
    let (router, _) = service(true).await;
    assert_eq!(get(&router).await.headers["etag"], r#""1""#);

    let reply = patch_with(
        &router,
        json!([
            { "op": "replace", "path": "/title", "value": "Groceries" },
            { "op": "add", "path": "/tags/-", "value": "weekly" },
        ]),
    )
    .await;
    assert_eq!(reply.status, StatusCode::NO_CONTENT);
    // The PUT ran inside the resource, layers and URN included.
    assert_eq!(reply.headers["x-resource"], "urn:my-notes:notes");
    let layered: Vec<_> = reply.headers.get_all("x-layered").iter().collect();
    assert_eq!(layered, ["PUT"]);

    let after = get(&router).await;
    assert_eq!(after.body["title"], "Groceries");
    assert_eq!(after.body["tags"], json!(["home", "weekly"]));
    assert_eq!(after.body["archived"], false);
    assert_eq!(after.headers["etag"], r#""2""#);
}

#[tokio::test]
async fn a_patch_to_a_field_the_put_ignores_is_refused_rather_than_dropped() {
    let (router, notes) = service(true).await;

    for (path, value) in [
        ("/archived", json!(true)),
        ("/id", json!(7)),
        ("/invented", json!("x")),
    ] {
        let op = if path == "/invented" {
            "add"
        } else {
            "replace"
        };
        let reply = patch_with(&router, json!([{ "op": op, "path": path, "value": value }])).await;
        assert_eq!(reply.status, StatusCode::UNPROCESSABLE_ENTITY, "{path}");
        assert!(
            reply.body["detail"].as_str().unwrap().contains(path),
            "{path}: {}",
            reply.body["detail"]
        );
    }
    let note = notes.load(&1).await.unwrap();
    assert!(!note.archived);
    assert_eq!(note.version().get(), 1, "nothing was written");
}

#[tokio::test]
async fn a_patch_that_cannot_apply_is_unprocessable_and_a_failed_test_is_a_conflict() {
    let (router, _) = service(true).await;

    let reply = patch_with(
        &router,
        json!([{ "op": "replace", "path": "/nope/deeper", "value": 1 }]),
    )
    .await;
    assert_eq!(reply.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(reply.body["type"], "urn:jenny:problem:unprocessable");

    let reply = patch_with(
        &router,
        json!([
            { "op": "test", "path": "/title", "value": "Not the title" },
            { "op": "replace", "path": "/title", "value": "Groceries" },
        ]),
    )
    .await;
    assert_eq!(reply.status, StatusCode::CONFLICT);
    assert_eq!(get(&router).await.body["title"], "Shopping");
}

#[tokio::test]
async fn the_puts_own_validation_still_applies() {
    let (router, _) = service(true).await;
    let reply = patch_with(
        &router,
        json!([{ "op": "replace", "path": "/title", "value": "  " }]),
    )
    .await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST);

    let reply = patch_with(
        &router,
        json!([{ "op": "replace", "path": "/title", "value": 5 }]),
    )
    .await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn if_match_is_checked_against_the_current_version() {
    let (router, _) = service(true).await;
    let rename = json!([{ "op": "replace", "path": "/title", "value": "Groceries" }]);
    let conditional = |tag: &str| {
        patch()
            .header("if-match", tag)
            .body(Body::from(rename.to_string()))
            .unwrap()
    };

    let stale = send(&router, conditional(r#""7""#)).await;
    assert_eq!(stale.status, StatusCode::PRECONDITION_FAILED);
    assert_eq!(stale.body["type"], "urn:jenny:problem:precondition-failed");

    let current = send(&router, conditional(r#""1""#)).await;
    assert_eq!(current.status, StatusCode::NO_CONTENT);

    // The version moved on, so the same precondition now fails.
    let again = send(&router, conditional(r#""1""#)).await;
    assert_eq!(again.status, StatusCode::PRECONDITION_FAILED);
}

#[tokio::test]
async fn a_put_with_a_stale_if_match_is_refused_by_the_store() {
    let (router, _) = service(true).await;
    let put = |tag: &str| {
        http::Request::put("/api/notes/1")
            .header("content-type", "application/json")
            .header("if-match", tag)
            .body(Body::from(json!({ "title": "Groceries" }).to_string()))
            .unwrap()
    };
    assert_eq!(
        send(&router, put(r#""9""#)).await.status,
        StatusCode::PRECONDITION_FAILED
    );
    assert_eq!(
        send(&router, put(r#""1""#)).await.status,
        StatusCode::NO_CONTENT
    );
}

#[tokio::test]
async fn only_json_patch_documents_are_accepted() {
    let (router, _) = service(true).await;

    let wrong_type = send(
        &router,
        http::Request::patch("/api/notes/1")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"title":"x"}"#))
            .unwrap(),
    )
    .await;
    assert_eq!(wrong_type.status, StatusCode::UNSUPPORTED_MEDIA_TYPE);
    assert_eq!(wrong_type.headers["accept-patch"], JSON_PATCH_CONTENT_TYPE);

    let malformed = send(
        &router,
        http::Request::patch("/api/notes/1")
            .header("content-type", JSON_PATCH_CONTENT_TYPE)
            .body(Body::from(r#"{"op":"replace"}"#))
            .unwrap(),
    )
    .await;
    assert_eq!(malformed.status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn patching_something_absent_is_whatever_get_says() {
    let (router, _) = service(true).await;
    let reply = send(
        &router,
        http::Request::patch("/api/notes/42")
            .header("content-type", JSON_PATCH_CONTENT_TYPE)
            .body(Body::from("[]"))
            .unwrap(),
    )
    .await;
    assert_eq!(reply.status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn without_the_mode_patch_is_not_allowed() {
    let (router, _) = service(false).await;
    let reply = patch_with(
        &router,
        json!([{ "op": "replace", "path": "/title", "value": "Groceries" }]),
    )
    .await;
    assert_eq!(reply.status, StatusCode::METHOD_NOT_ALLOWED);
}

#[tokio::test]
async fn a_resource_with_its_own_patch_keeps_it() {
    let routes: Routes = Routes::new("notes")
        .get("/notes/{id}", || async { Json(json!({ "title": "t" })) })
        .put("/notes/{id}", || async { StatusCode::NO_CONTENT })
        .patch("/notes/{id}", || async { StatusCode::ACCEPTED });
    let router = RestAdapter::new("my-notes")
        .unwrap()
        .accept_json_patch()
        .unwrap()
        .mount(routes)
        .unwrap()
        .into_router();

    let reply = patch_with(&router, json!([])).await;
    assert_eq!(reply.status, StatusCode::ACCEPTED);
}

#[test]
fn the_mode_must_be_chosen_before_mounting() {
    let routes: Routes = Routes::new("notes").get("/notes", || async { "[]" });
    let mounted = RestAdapter::new("my-notes").unwrap().mount(routes).unwrap();
    let err = mounted.accept_json_patch().err().unwrap();
    assert!(err.to_string().contains("before mount()"), "{err}");
}

#[tokio::test]
async fn the_derived_patch_is_listed_with_the_resource() {
    let notes: Notes = Arc::new(Memory::<Note>::new());
    let adapter = RestAdapter::new("my-notes")
        .unwrap()
        .accept_json_patch()
        .unwrap()
        .mount(resource(notes))
        .unwrap();
    let endpoints: Vec<String> = adapter.resources()[0]
        .endpoints()
        .iter()
        .map(ToString::to_string)
        .collect();
    assert!(
        endpoints.contains(&"PATCH /notes/{id}".to_string()),
        "{endpoints:?}"
    );
}
