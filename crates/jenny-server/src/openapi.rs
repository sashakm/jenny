//! Serving the API description.
//!
//! | Path | Feature | |
//! |---|---|---|
//! | `<framework_path>/openapi.json` | `openapi` | the document |
//! | `<framework_path>/swagger` | `swagger-ui` | a browsable UI over it |
//!
//! Swagger UI's assets are compiled into the binary rather than fetched from a CDN, so
//! the docs page works on an air-gapped cluster and behind a restrictive CSP. That
//! costs roughly two megabytes of binary, which is why it is a feature of its own
//! rather than part of `openapi`.

use axum::Router;
#[cfg(not(feature = "swagger-ui"))]
use axum::{Json, extract::State, response::IntoResponse, routing::get};
use utoipa::openapi::OpenApi;

/// Both paths are relative: the runtime nests this router under the configured
/// framework prefix, so the UI's link to the document resolves wherever that is.
pub(crate) fn openapi_router(document: OpenApi) -> Router {
    // With the UI enabled, `SwaggerUi` serves the document at the URL it is given —
    // registering our own route for it too would be an overlapping-route panic.
    #[cfg(feature = "swagger-ui")]
    {
        utoipa_swagger_ui::SwaggerUi::new("/swagger")
            .url("/openapi.json", document)
            .into()
    }
    #[cfg(not(feature = "swagger-ui"))]
    {
        Router::new()
            .route("/openapi.json", get(document_handler))
            .with_state(document)
    }
}

#[cfg(not(feature = "swagger-ui"))]
async fn document_handler(State(document): State<OpenApi>) -> impl IntoResponse {
    Json(document)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::{Body, to_bytes};
    use http::StatusCode;
    use tower::ServiceExt;
    use utoipa::OpenApi as _;

    #[derive(serde::Deserialize, utoipa::ToSchema)]
    #[allow(dead_code)]
    struct NoteRequest {
        title: String,
    }

    #[derive(utoipa::OpenApi)]
    #[openapi(components(schemas(NoteRequest)))]
    struct NotesApi;

    #[tokio::test]
    async fn the_document_is_served_as_json() {
        let response = openapi_router(NotesApi::openapi())
            .oneshot(
                http::Request::get("/openapi.json")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert!(body["openapi"].as_str().unwrap().starts_with("3."));
        assert!(
            body["components"]["schemas"]["NoteRequest"].is_object(),
            "the schema should be in the document: {body}"
        );
    }
}
