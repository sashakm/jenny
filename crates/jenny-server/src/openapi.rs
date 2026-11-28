//! Serving the API description: two endpoint groups (`server.endpoints`).
//!
//! | Group | Default path | Feature | |
//! |---|---|---|---|
//! | `openapi` | `<framework_path>/openapi.json` | `openapi` | the document |
//! | `swagger` | `<framework_path>/swagger` | `swagger-ui` | a browsable UI over it |
//!
//! Swagger UI loads the document from the `openapi` group's URL and serves no copy of
//! its own, so switching `openapi` off really takes the document offline. That is why
//! `swagger` needs `openapi` enabled, and on the same listener.
//!
//! Swagger UI's assets are compiled into the binary rather than fetched from a CDN, so
//! the docs page works on an air-gapped cluster and behind a restrictive CSP. That
//! costs roughly two megabytes of binary, which is why it is a feature of its own
//! rather than part of `openapi`.

use axum::{Json, Router, extract::State, response::IntoResponse, routing::get};
use utoipa::openapi::OpenApi;

/// The document at `path`, the full path of its one route.
pub(crate) fn document_router(document: OpenApi, path: &str) -> Router {
    Router::new()
        .route(path, get(document_handler))
        .with_state(document)
}

async fn document_handler(State(document): State<OpenApi>) -> impl IntoResponse {
    Json(document)
}

/// Swagger UI under `prefix`, never the root (`endpoints::resolve` refuses it),
/// loading the document from `document_url`.
#[cfg(feature = "swagger-ui")]
pub(crate) fn swagger_router(prefix: &str, document_url: &str) -> Router {
    utoipa_swagger_ui::SwaggerUi::new(prefix.to_string())
        .config(utoipa_swagger_ui::Config::from(document_url.to_string()))
        .into()
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
        let response = document_router(NotesApi::openapi(), "/openapi.json")
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

    #[cfg(feature = "swagger-ui")]
    #[tokio::test]
    async fn swagger_ui_points_at_the_document_and_serves_no_copy() {
        let router = swagger_router("/docs/swagger", "/docs/openapi.json");
        let get = |uri: &str| {
            router
                .clone()
                .oneshot(http::Request::get(uri).body(Body::empty()).unwrap())
        };

        let response = get("/docs/swagger/swagger-initializer.js").await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let script = String::from_utf8(bytes.to_vec()).unwrap();
        assert!(script.contains("\"/docs/openapi.json\""), "{script}");

        for uri in [
            "/docs/openapi.json",
            "/openapi.json",
            "/docs/swagger/openapi.json",
        ] {
            let response = get(uri).await.unwrap();
            assert_ne!(response.status(), StatusCode::OK, "{uri}");
            let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
            assert!(
                !bytes.starts_with(b"{\"openapi\""),
                "{uri} serves a document"
            );
        }
    }
}
