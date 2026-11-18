//! The JSON Patch mode: `PATCH` derived from a resource's own `GET` and `PUT`.
//!
//! An adapter that [accepts JSON Patch](crate::RestAdapter::accept_json_patch) answers
//! `PATCH` on every route template that has both a `GET` and a `PUT`, and nothing else
//! has to be written for it. A patch is a `PUT` computed on the server:
//!
//! 1. the body must be `application/json-patch+json` (else `415`, with `Accept-Patch`),
//!    and an RFC 6902 document (else `400`);
//! 2. the resource's `GET` produces the current representation and its `ETag`; an
//!    `If-Match` that does not match is `412`;
//! 3. the patch is applied to that representation; a path that does not resolve is
//!    `422`, a failed `test` operation `409`;
//! 4. the result goes to the resource's `PUT`, with `If-Match` set to the `ETag` the
//!    `GET` returned, and its response is the response.
//!
//! So a patch passes the same validation, the same use case and the same domain rules a
//! `PUT` does, and — when the `GET` sends an `ETag` and the `PUT` honours `If-Match` — a
//! write that happened in between is a `412`, not a lost update.
//!
//! ### Fields the client may not change
//!
//! The patch applies to what `GET` returns, which includes fields a `PUT` does not take:
//! an `id`, an `archived` flag. A `PUT` body ignores those, so a patch changing one
//! would succeed without effect. To prevent that, the forwarded request carries the
//! paths the patch changed, and [`Json`](crate::Json) refuses with `422` when a field it
//! ignored is one of them. The convention this needs, and the scaffold follows, is that
//! a `PUT` accepts the `GET` representation and ignores what it does not use.
//!
//! ### Authorization
//!
//! The adapter's authorization gate sees the `PATCH` itself, as action `PATCH` on the
//! route template. The `GET` and `PUT` it makes inside pass the resource's own layers
//! but not the adapter-wide gate a second time, so a policy granting `PUT` on a route
//! grants `PATCH` too when patches should be allowed there. Authorization inside the use
//! case, through `Context::authorize`, runs as it would for the `PUT` (ADR-014).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use axum::body::{Body, Bytes};
use axum::extract::{FromRequest, Request};
use axum::response::{IntoResponse, Response};
use axum::routing::MethodRouter;
use http::header::{
    CONTENT_LENGTH, CONTENT_TYPE, ETAG, HeaderName, IF_MATCH, IF_NONE_MATCH, TRANSFER_ENCODING,
};
use http::request::Parts;
use http::{HeaderValue, Method, StatusCode};
use serde_json::Value;
use tower::ServiceExt;

use jenny_core::domain::Changes;

use crate::conditional::Condition;
use crate::extract::body_problem;
use crate::problem::Problem;

/// `application/json-patch+json`, the RFC 6902 media type.
pub const JSON_PATCH_CONTENT_TYPE: &str = "application/json-patch+json";

/// RFC 5789's `Accept-Patch`, which the `http` crate has no constant for.
const ACCEPT_PATCH: HeaderName = HeaderName::from_static("accept-patch");

/// What the derived `PATCH` hands the `PUT` it forwards to: which paths the patch
/// changed, so [`Json`](crate::Json) can refuse one its type ignores.
#[derive(Clone, Debug)]
pub(crate) struct PatchedPaths {
    paths: Arc<[String]>,
    checked: Arc<AtomicBool>,
}

impl PatchedPaths {
    fn new(paths: Vec<String>) -> Self {
        Self {
            paths: paths.into(),
            checked: Arc::default(),
        }
    }

    /// The changed path that collides with a field the body's type ignored, if any. A
    /// collision is the same path, one inside it, or one containing it: a patch that
    /// replaced a whole object may have changed the ignored field within it.
    pub(crate) fn changed_within(&self, ignored: &str) -> Option<&str> {
        self.paths
            .iter()
            .find(|changed| {
                *changed == ignored
                    || changed.starts_with(&format!("{ignored}/"))
                    || ignored.starts_with(&format!("{changed}/"))
            })
            .map(String::as_str)
    }

    /// The `PUT` read its body through [`Json`](crate::Json), which did the check.
    pub(crate) fn mark_checked(&self) {
        self.checked.store(true, Ordering::Relaxed);
    }

    fn was_checked(&self) -> bool {
        self.checked.load(Ordering::Relaxed)
    }
}

/// The handler installed as `PATCH` beside a `GET` and a `PUT`. Both are the
/// resource's own method routers, layered as the resource layered them.
pub(crate) async fn derived(get: MethodRouter, put: MethodRouter, request: Request) -> Response {
    match apply(get, put, request).await {
        Ok(response) => response,
        Err(problem) => {
            let unsupported = problem.status() == StatusCode::UNSUPPORTED_MEDIA_TYPE;
            let mut response = problem.into_response();
            if unsupported {
                // RFC 5789 §2.2: say which patch formats would have been accepted.
                response.headers_mut().insert(
                    ACCEPT_PATCH,
                    HeaderValue::from_static(JSON_PATCH_CONTENT_TYPE),
                );
            }
            response
        }
    }
}

async fn apply(
    get: MethodRouter,
    put: MethodRouter,
    request: Request,
) -> Result<Response, Problem> {
    let (parts, body) = request.into_parts();

    if !is_json_patch(&parts) {
        return Err(Problem::unsupported_media_type(format!(
            "PATCH takes `{JSON_PATCH_CONTENT_TYPE}` (RFC 6902)"
        )));
    }

    // Read through `Bytes` so `DefaultBodyLimit` applies, as it does to every body.
    let bytes = Bytes::from_request(Request::from_parts(parts.clone(), body), &())
        .await
        .map_err(|rejection| body_problem(rejection.status(), rejection.body_text()))?;
    let patch: json_patch::Patch = serde_json::from_slice(&bytes)
        .map_err(|e| Problem::invalid(format!("not a JSON Patch document: {e}")))?;
    let condition = Condition::from_headers(&parts.headers)?;

    // --- 1. the current representation, from the resource's own GET ---------------
    let current = call(get, forwarded(&parts, Method::GET, Body::empty())).await;
    if !current.status().is_success() {
        // Not found, forbidden, whatever it was: the GET's answer is the PATCH's.
        return Ok(current);
    }
    let etag = current.headers().get(ETAG).cloned();
    if let Some(condition) = &condition
        && !condition.matches(etag.as_ref())
    {
        return Err(Problem::precondition_failed(
            "the resource has changed since the version `If-Match` names",
        ));
    }
    let before = representation(current, &parts).await?;

    // --- 2. the patch, applied to it ----------------------------------------------
    let mut after = before.clone();
    json_patch::patch(&mut after, &patch).map_err(|e| rejected(&e))?;
    let patched = PatchedPaths::new(Changes::between(&before, &after).paths().collect());

    // --- 3. the result, through the resource's own PUT -----------------------------
    let body = serde_json::to_vec(&after).expect("a JSON value always serialises");
    let mut request = forwarded(&parts, Method::PUT, Body::from(body));
    request
        .headers_mut()
        .insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    if let Some(etag) = etag {
        // The version the patch was applied to, so the PUT's compare-and-swap refuses
        // a write that landed in between.
        request.headers_mut().insert(IF_MATCH, etag);
    }
    request.extensions_mut().insert(patched.clone());

    let response = call(put, request).await;
    if response.status().is_success() && !patched.was_checked() {
        tracing::warn!(
            path = %parts.uri.path(),
            "PATCH was forwarded to a PUT that does not read its body through \
             jenny_rest::Json, so a patch to a field the PUT ignores went unnoticed"
        );
    }
    Ok(response)
}

fn is_json_patch(parts: &Parts) -> bool {
    parts
        .headers
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .is_some_and(|media| media.trim().eq_ignore_ascii_case(JSON_PATCH_CONTENT_TYPE))
}

/// The client's request, re-addressed to another method of the same route.
///
/// Extensions are kept — the principal, the request id, the path parameters axum
/// already matched — so the GET and PUT run as the same caller, on the same resource.
/// The body headers and the client's own preconditions are not: the body is new, and
/// the precondition has already been checked against the GET.
fn forwarded(parts: &Parts, method: Method, body: Body) -> Request {
    let mut parts = parts.clone();
    parts.method = method;
    for header in [
        CONTENT_TYPE,
        CONTENT_LENGTH,
        TRANSFER_ENCODING,
        IF_MATCH,
        IF_NONE_MATCH,
    ] {
        parts.headers.remove(header);
    }
    Request::from_parts(parts, body)
}

async fn call(router: MethodRouter, request: Request) -> Response {
    match router.oneshot(request).await {
        Ok(response) => response,
        Err(infallible) => match infallible {},
    }
}

async fn representation(response: Response, parts: &Parts) -> Result<Value, Problem> {
    let not_json = || {
        Problem::internal(format!(
            "GET {} did not return a JSON representation, so PATCH cannot apply to it",
            parts.uri.path()
        ))
    };
    let is_json = response
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|media| {
            let media = media.split(';').next().unwrap_or_default().trim();
            media == "application/json" || media.ends_with("+json")
        });
    if !is_json {
        return Err(not_json());
    }
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .map_err(|_| not_json())?;
    serde_json::from_slice(&bytes).map_err(|_| not_json())
}

/// RFC 5789 §2.2: a failed `test` is a conflict with the current state; any other
/// failure to apply is a patch that cannot be processed against this document.
fn rejected(error: &json_patch::PatchError) -> Problem {
    match error.kind {
        json_patch::PatchErrorKind::TestFailed => Problem::conflict(format!(
            "operation {} (`test` at `{}`) did not match the current value",
            error.operation, error.path
        )),
        _ => Problem::unprocessable(format!(
            "operation {} cannot be applied at `{}`: {}",
            error.operation, error.path, error.kind
        )),
    }
}

/// A JSON Pointer for a path `serde_ignored` reports.
pub(crate) fn pointer(path: &serde_ignored::Path<'_>) -> String {
    use serde_ignored::Path;

    fn escape(segment: &str) -> String {
        segment.replace('~', "~0").replace('/', "~1")
    }

    match path {
        Path::Root => String::new(),
        Path::Seq { parent, index } => format!("{}/{index}", pointer(parent)),
        Path::Map { parent, key } => format!("{}/{}", pointer(parent), escape(key)),
        Path::Some { parent }
        | Path::NewtypeStruct { parent }
        | Path::NewtypeVariant { parent } => pointer(parent),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ignored_pointer(json: &str) -> Vec<String> {
        #[derive(serde::Deserialize)]
        struct Takes {
            #[allow(dead_code)]
            title: String,
        }
        let mut ignored = Vec::new();
        let _: Takes =
            serde_ignored::deserialize(&mut serde_json::Deserializer::from_str(json), |path| {
                ignored.push(pointer(&path))
            })
            .unwrap();
        ignored
    }

    #[test]
    fn ignored_fields_become_escaped_json_pointers() {
        assert_eq!(
            ignored_pointer(r#"{"title":"t","a/b":1,"c~d":{"e":2}}"#),
            ["/a~1b", "/c~0d"]
        );
    }

    #[test]
    fn a_collision_is_the_same_path_one_inside_it_or_one_containing_it() {
        let patched = PatchedPaths::new(vec!["/author/name".into(), "/tags".into()]);
        assert_eq!(patched.changed_within("/author/name"), Some("/author/name"));
        assert_eq!(patched.changed_within("/author"), Some("/author/name"));
        assert_eq!(patched.changed_within("/tags/0/colour"), Some("/tags"));
        assert_eq!(
            patched.changed_within("/authors"),
            None,
            "a prefix is not a parent"
        );
        assert_eq!(patched.changed_within("/title"), None);
    }
}
