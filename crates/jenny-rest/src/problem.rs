//! HTTP failure representation: RFC 9457 `application/problem+json`.

use std::borrow::Cow;

use axum::response::{IntoResponse, Response};
use http::{StatusCode, header};
use serde::Serialize;
use uuid::Uuid;

use jenny_core::domain::{DomainError, ValidationIssue};

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
///   "instance": "urn:uuid:8f14e45f-ea1a-4a2c-9a1e-2d4c1f0b3a77"
/// }
/// ```
///
/// `type` is a URN rather than the `about:blank` placeholder, so the failure kind
/// stays machine-readable without clients having to parse prose. `instance` carries
/// the request id, which is the same id in the logs and traces for that request and
/// in the response's `x-request-id` header. A user quoting it from an error message is
/// quoting something you can grep for. [`RequestIdLayer`](crate::RequestIdLayer), which
/// the runtime installs, fills it in for every problem produced while serving a request.
///
/// ### Validation failures list every issue
///
/// A [`DomainError::Invalid`] that came from
/// [`Validate`](jenny_core::domain::Validate) carries one entry per issue in an `errors`
/// member, so a client can mark each field rather than parse the `detail` sentence:
///
/// ```json
/// {
///   "type": "urn:jenny:problem:invalid",
///   "title": "Bad Request",
///   "status": 400,
///   "detail": "title: must not be empty; body: must be at most 10000 characters",
///   "errors": [
///     { "field": "title", "message": "must not be empty" },
///     { "field": "body", "message": "must be at most 10000 characters" }
///   ]
/// }
/// ```
///
/// ### Detail is withheld on 5xx
///
/// For 4xx the `detail` is sent: the caller made a mistake and needs to know which.
/// For 5xx it is logged at `ERROR`, with the whole cause chain of a
/// [`DomainError::Internal`], and **not** serialised, because the text of an internal
/// failure ("connection to db-primary.internal refused") is for operators, not for
/// whoever is on the other end of the socket.
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
    #[serde(
        skip_serializing_if = "Option::is_none",
        serialize_with = "serialize_instance"
    )]
    #[cfg_attr(feature = "openapi", schema(value_type = Option<String>))]
    instance: Option<Uuid>,
    // Boxed, like `instance` is a `Uuid` rather than a `String`, to keep `Problem` small:
    // it is the error half of every handler's `Result`.
    #[serde(skip_serializing_if = "no_errors")]
    #[cfg_attr(feature = "openapi", schema(value_type = Vec<ProblemIssue>))]
    errors: Box<[ProblemIssue]>,
}

fn serialize_instance<S: serde::Serializer>(
    instance: &Option<Uuid>,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    match instance {
        Some(id) => serializer.collect_str(&format_args!("urn:uuid:{id}")),
        None => serializer.serialize_none(),
    }
}

fn no_errors(errors: &[ProblemIssue]) -> bool {
    errors.is_empty()
}

/// One entry of a [`Problem`]'s `errors`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct ProblemIssue {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub field: Option<String>,
    pub message: String,
}

impl From<ValidationIssue> for ProblemIssue {
    fn from(issue: ValidationIssue) -> Self {
        Self {
            field: issue.field,
            message: issue.message,
        }
    }
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
            errors: Box::default(),
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

    /// List what was wrong, one entry per issue, in the `errors` member.
    pub fn with_errors(
        mut self,
        errors: impl IntoIterator<Item = impl Into<ProblemIssue>>,
    ) -> Self {
        let mut all = std::mem::take(&mut self.errors).into_vec();
        all.extend(errors.into_iter().map(Into::into));
        self.errors = all.into_boxed_slice();
        self
    }

    pub fn errors(&self) -> &[ProblemIssue] {
        &self.errors
    }

    /// Tie this failure to a request id so the client can quote something greppable.
    /// Only needed outside [`RequestIdLayer`](crate::RequestIdLayer), which does it.
    pub fn with_instance(mut self, request_id: Uuid) -> Self {
        self.instance = Some(request_id);
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
            DomainError::Invalid { detail, issues } => Self::invalid(detail).with_errors(issues),
            DomainError::Unauthenticated { detail } => Self::unauthenticated(detail),
            DomainError::Forbidden { detail } => Self::forbidden(detail),
            DomainError::Conflict { detail } => Self::conflict(detail),
            internal @ DomainError::Internal { .. } => Self::internal(internal.report()),
        }
    }
}

impl IntoResponse for Problem {
    fn into_response(mut self) -> Response {
        let status = self.status();
        if self.instance.is_none()
            && let Some(request_id) = crate::request_id::current()
        {
            self = self.with_instance(request_id);
        }

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
/// # use jenny_core::domain::{DomainError, ValidationIssue};
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

    #[test]
    fn a_problem_stays_small_enough_to_return_by_value() {
        // clippy's `result_large_err` starts at 128 bytes, and `Problem` is the error
        // half of every handler's `ApiResult`, in every service.
        assert!(
            std::mem::size_of::<Problem>() < 128,
            "{}",
            std::mem::size_of::<Problem>()
        );
    }

    #[tokio::test]
    async fn validation_issues_are_listed_one_by_one() {
        let mut errors = jenny_core::domain::ValidationErrors::default();
        errors.push_field("title", "must not be empty");
        errors.push("note is too large");
        let (status, body) = body_of(Problem::from(DomainError::from(errors))).await;

        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(
            body["errors"],
            serde_json::json!([
                { "field": "title", "message": "must not be empty" },
                { "message": "note is too large" },
            ])
        );
    }

    #[tokio::test]
    async fn a_problem_without_issues_has_no_errors_member() {
        let (_, body) = body_of(Problem::from(DomainError::invalid("bad"))).await;
        assert!(body.get("errors").is_none(), "{body}");
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
        assert_eq!(body["instance"], format!("urn:uuid:{id}"));
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
