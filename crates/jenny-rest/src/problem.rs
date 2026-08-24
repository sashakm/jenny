//! HTTP failure representation: RFC 9457 `application/problem+json`.

use std::borrow::Cow;

use axum::response::{IntoResponse, Response};
use http::{StatusCode, header};
use serde::Serialize;
use uuid::Uuid;

use jenny_core::domain::DomainError;

/// `application/problem+json`
pub const PROBLEM_CONTENT_TYPE: &str = "application/problem+json";

/// What a jenny REST adapter sends when a request fails.
///
/// The shape is [RFC 9457]'s problem details, so clients that already understand that
/// format need no jenny-specific handling:
///
/// ```json
/// {
///   "type": "urn:jenny:problem:not-found",
///   "title": "Not Found",
///   "status": 404,
///   "detail": "no note with id 2341",
///   "instance": "8f14e45f-ea1a-4a2c-9a1e-2d4c1f0b3a77"
/// }
/// ```
///
/// `type` is a URN rather than the `about:blank` placeholder, so the failure kind
/// stays machine-readable without clients having to parse prose. `instance` carries
/// the request id, which is the same id in the logs and traces for that request —
/// a user quoting it from an error message is quoting something you can grep for.
///
/// ### Detail is withheld on 5xx
///
/// For 4xx the `detail` is sent: the caller made a mistake and needs to know which.
/// For 5xx it is logged at `ERROR` and **not** serialised, because the text of an
/// internal failure ("connection to db-primary.internal refused") is for operators,
/// not for whoever is on the other end of the socket.
///
/// [RFC 9457]: https://www.rfc-editor.org/rfc/rfc9457
#[derive(Clone, Debug, Serialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct Problem {
    #[serde(rename = "type")]
    kind: Cow<'static, str>,
    title: Cow<'static, str>,
    status: u16,
    #[serde(skip_serializing_if = "Option::is_none")]
    detail: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    instance: Option<String>,
}

impl Problem {
    pub fn new(
        status: StatusCode,
        kind: impl Into<Cow<'static, str>>,
        title: impl Into<Cow<'static, str>>,
    ) -> Self {
        Self {
            kind: kind.into(),
            title: title.into(),
            status: status.as_u16(),
            detail: None,
            instance: None,
        }
    }

    /// A problem for a bare status code, for failures that never passed through a use
    /// case — a route that matched nothing, a method that is not allowed.
    pub fn from_status(status: StatusCode) -> Self {
        let slug = status
            .canonical_reason()
            .unwrap_or("error")
            .to_lowercase()
            .replace(' ', "-");
        Self::new(
            status,
            format!("urn:jenny:problem:{slug}"),
            status.canonical_reason().unwrap_or("Error").to_string(),
        )
    }

    pub fn not_found() -> Self {
        Self::new(
            StatusCode::NOT_FOUND,
            "urn:jenny:problem:not-found",
            "Not Found",
        )
    }

    pub fn invalid(detail: impl Into<String>) -> Self {
        Self::new(
            StatusCode::BAD_REQUEST,
            "urn:jenny:problem:invalid",
            "Bad Request",
        )
        .with_detail(detail)
    }

    pub fn unauthenticated(detail: impl Into<String>) -> Self {
        Self::new(
            StatusCode::UNAUTHORIZED,
            "urn:jenny:problem:unauthenticated",
            "Unauthorized",
        )
        .with_detail(detail)
    }

    pub fn forbidden(detail: impl Into<String>) -> Self {
        Self::new(
            StatusCode::FORBIDDEN,
            "urn:jenny:problem:forbidden",
            "Forbidden",
        )
        .with_detail(detail)
    }

    pub fn conflict(detail: impl Into<String>) -> Self {
        Self::new(
            StatusCode::CONFLICT,
            "urn:jenny:problem:conflict",
            "Conflict",
        )
        .with_detail(detail)
    }

    pub fn internal(detail: impl Into<String>) -> Self {
        Self::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "urn:jenny:problem:internal",
            "Internal Server Error",
        )
        .with_detail(detail)
    }

    pub fn with_detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = Some(detail.into());
        self
    }

    /// Tie this failure to a request id so the client can quote something greppable.
    pub fn with_instance(mut self, request_id: Uuid) -> Self {
        self.instance = Some(request_id.to_string());
        self
    }

    pub fn status(&self) -> StatusCode {
        StatusCode::from_u16(self.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR)
    }

    pub fn kind(&self) -> &str {
        &self.kind
    }

    pub fn detail(&self) -> Option<&str> {
        self.detail.as_deref()
    }
}

impl From<DomainError> for Problem {
    fn from(error: DomainError) -> Self {
        match error {
            DomainError::NotFound => Self::not_found(),
            DomainError::Invalid { detail } => Self::invalid(detail),
            DomainError::Unauthenticated { detail } => Self::unauthenticated(detail),
            DomainError::Forbidden { detail } => Self::forbidden(detail),
            DomainError::Conflict { detail } => Self::conflict(detail),
            DomainError::Internal { detail } => Self::internal(detail),
        }
    }
}

impl IntoResponse for Problem {
    fn into_response(mut self) -> Response {
        let status = self.status();

        if status.is_server_error() {
            // Operators get the reason; the caller gets the status.
            if let Some(detail) = self.detail.take() {
                tracing::error!(status = status.as_u16(), problem = %self.kind, "{detail}");
            }
        }

        let mut response = (status, axum::Json(&self)).into_response();
        response.headers_mut().insert(
            header::CONTENT_TYPE,
            header::HeaderValue::from_static(PROBLEM_CONTENT_TYPE),
        );
        if status == StatusCode::UNAUTHORIZED {
            response.headers_mut().append(
                header::WWW_AUTHENTICATE,
                header::HeaderValue::from_static("Bearer realm=\"jenny\""),
            );
            response.headers_mut().append(
                header::WWW_AUTHENTICATE,
                header::HeaderValue::from_static("ApiKey realm=\"jenny\""),
            );
        }
        response
    }
}

/// What a jenny REST handler returns.
///
/// Because [`Problem`] implements `From<DomainError>`, a handler can `?` straight
/// through a use-case call and the status mapping happens on the way out:
///
/// ```
/// # use std::sync::Arc;
/// use jenny_rest::{ApiResult, Json};
/// # use jenny_core::domain::DomainError;
/// # use jenny_core::port::Context;
/// # struct FetchNote;
/// # impl FetchNote { async fn invoke(&self, _: u32, _: &Context) -> Result<String, DomainError> { Ok("note".into()) } }
/// async fn fetch(uc: Arc<FetchNote>) -> ApiResult<Json<String>> {
///     Ok(Json(uc.invoke(1, &Context::new()).await?))
/// }
/// ```
pub type ApiResult<T> = Result<T, Problem>;

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::to_bytes;

    async fn body_of(problem: Problem) -> (StatusCode, serde_json::Value) {
        let response = problem.into_response();
        let status = response.status();
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        (status, serde_json::from_slice(&bytes).unwrap())
    }

    #[test]
    fn every_domain_error_has_a_status() {
        for (error, expected) in [
            (DomainError::not_found(), StatusCode::NOT_FOUND),
            (DomainError::invalid("x"), StatusCode::BAD_REQUEST),
            (DomainError::unauthenticated("x"), StatusCode::UNAUTHORIZED),
            (DomainError::forbidden("x"), StatusCode::FORBIDDEN),
            (DomainError::conflict("x"), StatusCode::CONFLICT),
            (
                DomainError::internal("x"),
                StatusCode::INTERNAL_SERVER_ERROR,
            ),
        ] {
            assert_eq!(Problem::from(error).status(), expected);
        }
    }

    #[tokio::test]
    async fn a_client_error_carries_its_detail() {
        let (status, body) = body_of(Problem::from(DomainError::invalid("title is empty"))).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["type"], "urn:jenny:problem:invalid");
        assert_eq!(body["title"], "Bad Request");
        assert_eq!(body["status"], 400);
        assert_eq!(body["detail"], "title is empty");
    }

    #[tokio::test]
    async fn a_server_error_withholds_its_detail() {
        let (status, body) =
            body_of(Problem::from(DomainError::internal("db-primary refused"))).await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(body["type"], "urn:jenny:problem:internal");
        assert!(
            body.get("detail").is_none(),
            "internal detail must not reach the client, got {body}"
        );
    }

    #[tokio::test]
    async fn the_request_id_travels_as_the_instance() {
        let id = Uuid::new_v4();
        let (_, body) = body_of(Problem::not_found().with_instance(id)).await;
        assert_eq!(body["instance"], id.to_string());
    }

    #[tokio::test]
    async fn unauthorized_advertises_the_accepted_schemes() {
        let response = Problem::unauthenticated("no credentials").into_response();
        let schemes: Vec<_> = response
            .headers()
            .get_all(header::WWW_AUTHENTICATE)
            .iter()
            .map(|v| v.to_str().unwrap())
            .collect();
        assert_eq!(
            schemes,
            ["Bearer realm=\"jenny\"", "ApiKey realm=\"jenny\""]
        );
    }

    #[tokio::test]
    async fn problems_are_served_as_problem_json() {
        let response = Problem::not_found().into_response();
        assert_eq!(
            response.headers().get(header::CONTENT_TYPE).unwrap(),
            PROBLEM_CONTENT_TYPE
        );
    }

    #[tokio::test]
    async fn a_bare_status_becomes_a_well_formed_problem() {
        let (status, body) = body_of(Problem::from_status(StatusCode::METHOD_NOT_ALLOWED)).await;
        assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED);
        assert_eq!(body["type"], "urn:jenny:problem:method-not-allowed");
        assert_eq!(body["title"], "Method Not Allowed");
    }
}
