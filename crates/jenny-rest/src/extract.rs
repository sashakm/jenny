//! Extractors: the HTTP-to-domain half of a driving adapter.

use std::ops::{Deref, DerefMut};

use axum::extract::{FromRequest, FromRequestParts, Request};
use axum::response::{IntoResponse, Response};
use http::{Method, StatusCode, request::Parts};
use serde::Serialize;
use serde::de::DeserializeOwned;
use uuid::Uuid;

use jenny_core::domain::{Criteria, DomainError, Page, Principal, Urn};
use jenny_core::port::Context;

use crate::problem::Problem;

/// Header carrying (or receiving) the id that correlates a request across logs,
/// traces and the `instance` field of a [`Problem`].
pub const REQUEST_ID_HEADER: &str = "x-request-id";

/// The invocation [`Context`], extracted from the request.
///
/// A thin newtype rather than an impl on `Context` itself, because both the type and
/// axum's extractor traits are foreign to this crate. It [`Deref`]s to `Context`, so
/// handlers just pass `&cx` where a use case wants `&Context`:
///
/// ```
/// # use std::sync::Arc;
/// # use jenny_core::domain::DomainError;
/// # use jenny_core::port::Context;
/// use jenny_rest::{ApiResult, Ctx, Json};
/// # struct ArchiveNote;
/// # impl ArchiveNote { async fn invoke(&self, _: u32, _: &Context) -> Result<(), DomainError> { Ok(()) } }
/// async fn archive(uc: Arc<ArchiveNote>, cx: Ctx) -> ApiResult<Json<()>> {
///     Ok(Json(uc.invoke(1, &cx).await?))
/// }
/// ```
///
/// It picks up the request id from an inbound `x-request-id` header when there is one
/// — so a correlation id set by a gateway or an upstream service survives the hop —
/// and mints a fresh one otherwise. The [`Principal`] comes from the request
/// extensions, where the authentication layer put it; without that layer the context
/// is simply anonymous, which is what
/// [`require_principal`](jenny_core::port::Context::require_principal) then reports.
#[derive(Debug)]
pub struct Ctx(Context);

impl Ctx {
    pub fn into_inner(self) -> Context {
        self.0
    }
}

impl Deref for Ctx {
    type Target = Context;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl DerefMut for Ctx {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl From<Ctx> for Context {
    fn from(ctx: Ctx) -> Self {
        ctx.0
    }
}

impl<S: Send + Sync> FromRequestParts<S> for Ctx {
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(parts: &mut Parts, _: &S) -> Result<Self, Self::Rejection> {
        let request_id = parts
            .headers
            .get(REQUEST_ID_HEADER)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| Uuid::parse_str(value).ok())
            .unwrap_or_else(Uuid::new_v4);

        let mut context = Context::new().with_request_id(request_id);
        if let Some(principal) = parts.extensions.get::<Principal>() {
            context = context.with_principal(principal.clone());
        }
        Ok(Ctx(context))
    }
}

/// Which resource, and what is being done to it.
///
/// The URN is the one the [`Routes`](crate::Routes) group was mounted under, so it
/// identifies the resource the same way whatever the request path looked like. Useful
/// for authorization rules and telemetry that should not be re-derived from string
/// paths.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct RouteId {
    method: Method,
    urn: Urn,
}

impl RouteId {
    pub fn new(method: Method, urn: Urn) -> Self {
        Self { method, urn }
    }

    pub fn method(&self) -> &Method {
        &self.method
    }

    pub fn urn(&self) -> &Urn {
        &self.urn
    }
}

impl std::fmt::Display for RouteId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}#{}", self.urn, self.method)
    }
}

/// Set by [`Routes`](crate::Routes) on every route it installs.
#[derive(Clone, Debug)]
pub(crate) struct ResourceUrn(pub(crate) Urn);

impl<S: Send + Sync> FromRequestParts<S> for RouteId {
    type Rejection = Problem;

    async fn from_request_parts(parts: &mut Parts, _: &S) -> Result<Self, Self::Rejection> {
        let urn = parts
            .extensions
            .get::<ResourceUrn>()
            .map(|resource| resource.0.clone())
            .ok_or_else(|| {
                // Only reachable if a handler is mounted outside a `Routes` group.
                Problem::internal("route is not part of a jenny resource")
            })?;
        Ok(RouteId::new(parts.method.clone(), urn))
    }
}

/// JSON in, JSON out — with jenny's failure shape on the way in.
///
/// Behaves like [`axum::Json`] except that a malformed body produces a
/// [`Problem`] rather than axum's plain-text rejection, so *every* failure a client
/// can provoke has the same content type and the same fields. That consistency is
/// the entire reason this newtype exists.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Json<T>(pub T);

impl<T> Json<T> {
    pub fn into_inner(self) -> T {
        self.0
    }
}

impl<T> Deref for Json<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl<T, S> FromRequest<S> for Json<T>
where
    T: DeserializeOwned,
    S: Send + Sync,
{
    type Rejection = Problem;

    async fn from_request(req: Request, state: &S) -> Result<Self, Self::Rejection> {
        match axum::Json::<T>::from_request(req, state).await {
            Ok(axum::Json(value)) => Ok(Json(value)),
            Err(rejection) => {
                let problem = Problem::invalid(rejection.body_text());
                // A body too large is a distinct failure and deserves its own status.
                Err(if rejection.status() == StatusCode::PAYLOAD_TOO_LARGE {
                    Problem::new(
                        StatusCode::PAYLOAD_TOO_LARGE,
                        "urn:jenny:problem:payload-too-large",
                        "Payload Too Large",
                    )
                    .with_detail(rejection.body_text())
                } else {
                    problem
                })
            }
        }
    }
}

impl<T: Serialize> IntoResponse for Json<T> {
    fn into_response(self) -> Response {
        axum::Json(self.0).into_response()
    }
}

/// What the client asked for about a listing: paging, sorting and filtering.
///
/// The HTTP spelling of [`Criteria`]. Parameters are:
///
/// | Parameter | Repeatable | Form |
/// |---|---|---|
/// | `page` | no | 1-based page number |
/// | `page_size` | no | entries per page |
/// | `sort` | **yes** | `path`, `path:asc`, `path:desc`, or `-path` |
/// | `filter` | **yes** | `path:check:value`, or `path:value` for equality |
///
/// ```text
/// GET /notes?page=2&page_size=20
///           &sort=-created_at&sort=title
///           &filter=status:eq:active&filter=title:contains:milk
/// ```
///
/// Repeated parameters are why this is a hand-written extractor rather than
/// `Query<Criteria>`: `serde_urlencoded` keeps only one value per key, so the second
/// `sort=` would be silently dropped. Any parameter this does not recognise is left
/// alone, so a handler can take both `Listing` and its own `Query<T>`.
///
/// Malformed input is a [`Problem`] with the reason, never a silent misparse.
///
/// ```
/// # use std::sync::Arc;
/// use jenny_rest::{ApiResult, Ctx, Json, Listing};
/// # use jenny_core::domain::{DomainError, Criteria};
/// # use jenny_core::port::Context;
/// # struct Notes;
/// # impl Notes { async fn list(&self, _: &Criteria, _: &Context) -> Result<Vec<String>, DomainError> { Ok(vec![]) } }
/// async fn list(notes: Arc<Notes>, listing: Listing, cx: Ctx) -> ApiResult<Json<Vec<String>>> {
///     Ok(Json(notes.list(&listing, &cx).await?))
/// }
/// ```
#[derive(Clone, Debug, Default)]
pub struct Listing(Criteria);

impl Listing {
    pub fn into_inner(self) -> Criteria {
        self.0
    }

    /// Parse listing parameters out of a raw query string.
    ///
    /// Public because the same spelling is worth reusing from a driving adapter this
    /// crate does not own.
    pub fn parse(query: &str) -> Result<Self, DomainError> {
        let mut criteria = Criteria::new();
        let mut number = None;
        let mut size = None;

        for (key, value) in form_urlencoded::parse(query.as_bytes()) {
            match key.as_ref() {
                "page" => number = Some(parse_number(&key, &value)?),
                "page_size" => size = Some(parse_number(&key, &value)?),
                "sort" => criteria.sort.push(value.parse()?),
                "filter" => criteria.filter.push(value.parse()?),
                // Not ours — the handler may have its own parameters.
                _ => {}
            }
        }

        Ok(Self(criteria.with_page(Page::new(number, size))))
    }
}

fn parse_number(key: &str, value: &str) -> Result<u32, DomainError> {
    value
        .parse()
        .map_err(|_| DomainError::invalid(format!("`{key}` must be a whole number, got `{value}`")))
}

impl Deref for Listing {
    type Target = Criteria;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl From<Listing> for Criteria {
    fn from(listing: Listing) -> Self {
        listing.0
    }
}

impl<S: Send + Sync> FromRequestParts<S> for Listing {
    type Rejection = Problem;

    async fn from_request_parts(parts: &mut Parts, _: &S) -> Result<Self, Self::Rejection> {
        Listing::parse(parts.uri.query().unwrap_or_default()).map_err(Problem::from)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::Router;
    use axum::body::{Body, to_bytes};
    use axum::routing::{get, post};
    use http::header;
    use jenny_core::domain::{Check, Ordering};
    use tower::ServiceExt;

    async fn call(router: Router, request: http::Request<Body>) -> (StatusCode, serde_json::Value) {
        let response = router.oneshot(request).await.unwrap();
        let status = response.status();
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let body = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
        (status, body)
    }

    #[tokio::test]
    async fn the_context_reuses_an_inbound_request_id() {
        let id = Uuid::new_v4();
        let app = Router::new().route(
            "/",
            get(|cx: Ctx| async move { cx.request_id().to_string() }),
        );
        let response = app
            .oneshot(
                http::Request::get("/")
                    .header(REQUEST_ID_HEADER, id.to_string())
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        assert_eq!(String::from_utf8(bytes.to_vec()).unwrap(), id.to_string());
    }

    #[tokio::test]
    async fn a_missing_or_unparseable_request_id_gets_a_fresh_one() {
        let app = Router::new().route(
            "/",
            get(|cx: Ctx| async move { cx.request_id().to_string() }),
        );
        for request in [
            http::Request::get("/").body(Body::empty()).unwrap(),
            http::Request::get("/")
                .header(REQUEST_ID_HEADER, "not-a-uuid")
                .body(Body::empty())
                .unwrap(),
        ] {
            let response = app.clone().oneshot(request).await.unwrap();
            let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
            let id: Uuid = String::from_utf8(bytes.to_vec()).unwrap().parse().unwrap();
            assert_ne!(id, Uuid::nil());
        }
    }

    #[tokio::test]
    async fn the_context_picks_up_a_principal_from_the_extensions() {
        let app = Router::new()
            .route(
                "/",
                get(|cx: Ctx| async move {
                    cx.principal()
                        .map(|p| p.id().to_string())
                        .unwrap_or_default()
                }),
            )
            .layer(axum::Extension(Principal::builder("u-1").build()));

        let response = app
            .oneshot(http::Request::get("/").body(Body::empty()).unwrap())
            .await
            .unwrap();
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        assert_eq!(String::from_utf8(bytes.to_vec()).unwrap(), "u-1");
    }

    #[tokio::test]
    async fn a_malformed_json_body_is_rejected_as_a_problem() {
        let app = Router::new().route(
            "/",
            post(|Json(_): Json<serde_json::Value>| async { StatusCode::OK }),
        );
        let (status, body) = call(
            app,
            http::Request::post("/")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from("{ not json"))
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["type"], "urn:jenny:problem:invalid");
        assert!(body["detail"].is_string());
    }

    #[tokio::test]
    async fn a_valid_json_body_round_trips() {
        let app = Router::new().route(
            "/",
            post(|Json(value): Json<serde_json::Value>| async move { Json(value) }),
        );
        let (status, body) = call(
            app,
            http::Request::post("/")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"title":"hi"}"#))
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["title"], "hi");
    }

    #[tokio::test]
    async fn a_route_id_outside_a_resource_group_is_an_internal_problem() {
        let app = Router::new().route("/", get(|_: RouteId| async { StatusCode::OK }));
        let (status, body) = call(app, http::Request::get("/").body(Body::empty()).unwrap()).await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(body["type"], "urn:jenny:problem:internal");
    }

    #[test]
    fn route_ids_render_as_urn_and_method() {
        let id = RouteId::new(
            Method::GET,
            Urn::root("shop").unwrap().child("orders").unwrap(),
        );
        assert_eq!(id.to_string(), "urn:shop:orders#GET");
    }

    #[test]
    fn listing_parses_paging_sorting_and_filtering() {
        let listing = Listing::parse(
            "page=2&page_size=20&sort=-created_at&sort=title\
             &filter=status:eq:active&filter=title:contains:milk",
        )
        .unwrap();

        assert_eq!(listing.page.number(), Some(2));
        assert_eq!(listing.page.size(), Some(20));
        assert_eq!(listing.page.offset(), 20);

        assert_eq!(listing.sort.len(), 2);
        assert_eq!(listing.sort[0].path(), "created_at");
        assert_eq!(listing.sort[0].ordering(), Ordering::Desc);
        assert_eq!(listing.sort[1].ordering(), Ordering::Asc);

        assert_eq!(listing.filter.len(), 2);
        assert_eq!(listing.filter[0].check(), Check::Eq);
        assert_eq!(listing.filter[1].check(), Check::Contains);
        assert_eq!(listing.filter[1].value(), "milk");
    }

    #[test]
    fn an_empty_query_is_an_empty_listing() {
        let listing = Listing::parse("").unwrap();
        assert!(listing.page.is_unset());
        assert!(listing.sort.is_empty());
        assert!(listing.filter.is_empty());
    }

    #[test]
    fn unrecognised_parameters_are_left_for_the_handler() {
        let listing = Listing::parse("q=milk&tenant=acme&sort=title").unwrap();
        assert_eq!(listing.sort.len(), 1);
        assert!(listing.filter.is_empty());
    }

    #[test]
    fn percent_encoded_values_are_decoded() {
        let listing = Listing::parse("filter=title%3Acontains%3Ahello%20world").unwrap();
        assert_eq!(listing.filter[0].path(), "title");
        assert_eq!(listing.filter[0].value(), "hello world");
    }

    #[tokio::test]
    async fn a_malformed_listing_is_rejected_as_a_problem() {
        let app = Router::new().route(
            "/notes",
            get(|listing: Listing| async move { listing.sort.len().to_string() }),
        );

        for (query, expected) in [
            ("?sort=title:sideways", "asc, desc"),
            ("?filter=a:nope:b", "unknown check"),
            ("?filter=justapath", "missing a value"),
            ("?page=many", "whole number"),
            // The injection attempt never reaches an allowlist — it fails at parsing.
            ("?sort=title;DROP%20TABLE%20notes", "attribute path"),
        ] {
            let (status, body) = call(
                app.clone(),
                http::Request::get(format!("/notes{query}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "for {query}");
            assert_eq!(body["type"], "urn:jenny:problem:invalid", "for {query}");
            assert!(
                body["detail"].as_str().unwrap().contains(expected),
                "for {query}: expected {expected:?} in {}",
                body["detail"]
            );
        }
    }

    #[tokio::test]
    async fn a_valid_listing_reaches_the_handler() {
        let app = Router::new().route(
            "/notes",
            get(|listing: Listing| async move {
                format!("{} {}", listing.sort.len(), listing.filter.len())
            }),
        );
        let response = app
            .oneshot(
                http::Request::get("/notes?sort=title&filter=a:eq:b&filter=c:gt:1")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        assert_eq!(String::from_utf8(bytes.to_vec()).unwrap(), "1 2");
    }
}
