//! Conditional requests: an `ETag` out, an `If-Match` in, both backed by the version a
//! [`Store`](jenny_core::port::Store) keeps for every aggregate.
//!
//! ```
//! use axum::extract::{Path, State};
//! use jenny_rest::{ApiResult, Ctx, IfMatch, Json, StatusCode, Versioned};
//! # use jenny_core::port::{Loaded, Version};
//! # use jenny_core::domain::DomainResult;
//! # #[derive(Clone)] struct Notes;
//! # impl Notes {
//! #     async fn fetch(&self, _: u32) -> DomainResult<(String, Version)> { Ok(("note".into(), Version::FIRST)) }
//! #     async fn edit(&self, _: u32, _: String, _: Option<Version>) -> DomainResult<()> { Ok(()) }
//! # }
//! /// `ETag: "3"` on the way out…
//! async fn fetch(State(notes): State<Notes>, Path(id): Path<u32>) -> ApiResult<Versioned<Json<String>>> {
//!     let (note, version) = notes.fetch(id).await?;
//!     Ok(Versioned::new(version, Json(note)))
//! }
//!
//! /// …and `If-Match: "3"` on the way back in, handed to the use case as the version
//! /// its `change_at` must find.
//! async fn update(
//!     State(notes): State<Notes>,
//!     Path(id): Path<u32>,
//!     IfMatch(expected): IfMatch,
//!     Json(title): Json<String>,
//! ) -> ApiResult<StatusCode> {
//!     notes.edit(id, title, expected).await?;
//!     Ok(StatusCode::NO_CONTENT)
//! }
//! ```
//!
//! The check is the store's compare-and-swap, not a comparison in the handler, so there
//! is no window between "the version matched" and "the write happened" for another
//! writer to slip into. A mismatch is [`DomainError::Stale`](jenny_core::domain::DomainError::Stale),
//! which is a `412 Precondition Failed`.

use axum::extract::FromRequestParts;
use axum::response::{IntoResponse, Response};
use http::header::{ETAG, IF_MATCH};
use http::request::Parts;
use http::{HeaderMap, HeaderValue};

use jenny_core::port::Version;

use crate::problem::Problem;

/// A response body together with the version it represents, sent as a strong `ETag`.
///
/// Only a successful response carries the tag: a failure is not a representation of the
/// resource, and tagging one would let a client "match" an error.
#[derive(Clone, Debug)]
pub struct Versioned<T> {
    version: Version,
    body: T,
}

impl<T> Versioned<T> {
    pub fn new(version: Version, body: T) -> Self {
        Self { version, body }
    }
}

impl<T: IntoResponse> IntoResponse for Versioned<T> {
    fn into_response(self) -> Response {
        let mut response = self.body.into_response();
        if response.status().is_success() {
            response
                .headers_mut()
                .insert(ETAG, entity_tag(self.version));
        }
        response
    }
}

/// The version an `If-Match` header requires, if it requires one.
///
/// `IfMatch(None)` when the header is absent or `*`; `*` asks only that the resource
/// exists, which loading it establishes anyway. `IfMatch(Some(version))` for a single
/// strong entity tag, which is what [`Versioned`] sends.
///
/// Anything else is answered with `412 Precondition Failed` before the handler runs: a
/// weak tag never matches under the strong comparison `If-Match` uses, a tag that is
/// not a version cannot be one of ours, and a list of several cannot be checked
/// against a single compare-and-swap. Refusing is the safe answer to a precondition
/// that cannot be evaluated; ignoring it would let the write through.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct IfMatch(pub Option<Version>);

impl<S: Send + Sync> FromRequestParts<S> for IfMatch {
    type Rejection = Problem;

    async fn from_request_parts(parts: &mut Parts, _: &S) -> Result<Self, Self::Rejection> {
        match Condition::from_headers(&parts.headers)? {
            None | Some(Condition::Any) => Ok(Self(None)),
            Some(Condition::Tags(tags)) => match tags.as_slice() {
                [tag] if !tag.weak => tag
                    .opaque
                    .parse::<u64>()
                    .map(|version| Self(Some(Version::new(version))))
                    .map_err(|_| unmatchable(&parts.headers)),
                _ => Err(unmatchable(&parts.headers)),
            },
        }
    }
}

fn unmatchable(headers: &HeaderMap) -> Problem {
    let raw = headers
        .get_all(IF_MATCH)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .collect::<Vec<_>>()
        .join(", ");
    Problem::precondition_failed(format!(
        "`If-Match: {raw}` can never match: this resource's entity tags are single, \
         strong versions such as \"3\""
    ))
}

/// `"3"` for version 3.
pub(crate) fn entity_tag(version: Version) -> HeaderValue {
    HeaderValue::from_str(&format!("\"{version}\""))
        .expect("digits and quotes are a valid header value")
}

/// A parsed `If-Match`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Condition {
    /// `*`: any current representation.
    Any,
    Tags(Vec<EntityTag>),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct EntityTag {
    weak: bool,
    /// Between the quotes.
    opaque: String,
}

impl Condition {
    /// `None` when there is no `If-Match` at all.
    pub(crate) fn from_headers(headers: &HeaderMap) -> Result<Option<Self>, Problem> {
        let mut values = headers.get_all(IF_MATCH).iter().peekable();
        if values.peek().is_none() {
            return Ok(None);
        }
        let mut tags = Vec::new();
        for value in values {
            let text = value
                .to_str()
                .map_err(|_| Problem::invalid("`If-Match` is not valid text"))?;
            if text.trim() == "*" {
                return Ok(Some(Self::Any));
            }
            tags.extend(parse_tags(text)?);
        }
        Ok(Some(Self::Tags(tags)))
    }

    /// Strong comparison against the current tag, as RFC 9110 §13.1.1 requires.
    pub(crate) fn matches(&self, current: Option<&HeaderValue>) -> bool {
        let Some(current) = current
            .and_then(|value| value.to_str().ok())
            .and_then(|text| parse_tags(text).ok())
            .and_then(|tags| tags.into_iter().next())
        else {
            // Nothing to compare against: only "any representation" can hold.
            return matches!(self, Self::Any);
        };
        match self {
            Self::Any => true,
            Self::Tags(tags) => {
                !current.weak
                    && tags
                        .iter()
                        .any(|tag| !tag.weak && tag.opaque == current.opaque)
            }
        }
    }
}

/// A comma-separated list of entity tags. Commas may appear inside the quotes.
fn parse_tags(text: &str) -> Result<Vec<EntityTag>, Problem> {
    let malformed = || Problem::invalid(format!("malformed entity tag list `{text}`"));
    let mut tags = Vec::new();
    let mut rest = text.trim();
    while !rest.is_empty() {
        let (weak, quoted) = match rest.strip_prefix("W/") {
            Some(after) => (true, after),
            None => (false, rest),
        };
        let inner = quoted.strip_prefix('"').ok_or_else(malformed)?;
        let end = inner.find('"').ok_or_else(malformed)?;
        tags.push(EntityTag {
            weak,
            opaque: inner[..end].to_string(),
        });
        rest = inner[end + 1..].trim_start();
        rest = match rest.strip_prefix(',') {
            Some(after) => after.trim_start(),
            None if rest.is_empty() => rest,
            None => return Err(malformed()),
        };
    }
    Ok(tags)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::Router;
    use axum::body::Body;
    use axum::routing::get;
    use http::StatusCode;
    use tower::ServiceExt;

    fn headers(values: &[&str]) -> HeaderMap {
        let mut headers = HeaderMap::new();
        for value in values {
            headers.append(IF_MATCH, HeaderValue::from_str(value).unwrap());
        }
        headers
    }

    fn tag(opaque: &str) -> EntityTag {
        EntityTag {
            weak: false,
            opaque: opaque.into(),
        }
    }

    #[test]
    fn lists_parse_with_commas_inside_quotes_and_across_headers() {
        let parsed = Condition::from_headers(&headers(&[r#""a,b", W/"c""#, r#""d""#]))
            .unwrap()
            .unwrap();
        assert_eq!(
            parsed,
            Condition::Tags(vec![
                tag("a,b"),
                EntityTag {
                    weak: true,
                    opaque: "c".into()
                },
                tag("d"),
            ])
        );
        assert_eq!(
            Condition::from_headers(&headers(&["*"])).unwrap(),
            Some(Condition::Any)
        );
        assert_eq!(Condition::from_headers(&HeaderMap::new()).unwrap(), None);
    }

    #[test]
    fn malformed_lists_are_refused() {
        for bad in ["3", r#""3"x"#, r#""unterminated"#, r#""a" "b""#] {
            assert!(
                Condition::from_headers(&headers(&[bad])).is_err(),
                "{bad} should not parse"
            );
        }
    }

    #[test]
    fn comparison_is_strong() {
        let current = HeaderValue::from_static(r#""3""#);
        let weak_current = HeaderValue::from_static(r#"W/"3""#);
        let wants = |v: &str| Condition::from_headers(&headers(&[v])).unwrap().unwrap();

        assert!(wants(r#""3""#).matches(Some(&current)));
        assert!(wants(r#""2", "3""#).matches(Some(&current)));
        assert!(!wants(r#""2""#).matches(Some(&current)));
        assert!(!wants(r#"W/"3""#).matches(Some(&current)));
        assert!(!wants(r#""3""#).matches(Some(&weak_current)));
        assert!(wants("*").matches(Some(&current)));
        // No tag to compare against: only `*` can hold.
        assert!(!wants(r#""3""#).matches(None));
        assert!(wants("*").matches(None));
    }

    async fn if_match(values: &[&str]) -> Result<IfMatch, StatusCode> {
        let app = Router::new().route(
            "/",
            get(|condition: IfMatch| async move {
                condition.0.map_or("none".to_string(), |v| v.to_string())
            }),
        );
        let mut request = http::Request::get("/").body(Body::empty()).unwrap();
        *request.headers_mut() = headers(values);
        let response = app.oneshot(request).await.unwrap();
        if !response.status().is_success() {
            return Err(response.status());
        }
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        Ok(match std::str::from_utf8(&bytes).unwrap() {
            "none" => IfMatch(None),
            n => IfMatch(Some(Version::new(n.parse().unwrap()))),
        })
    }

    #[tokio::test]
    async fn a_single_strong_version_is_what_the_use_case_gets() {
        assert_eq!(if_match(&[]).await, Ok(IfMatch(None)));
        assert_eq!(if_match(&["*"]).await, Ok(IfMatch(None)));
        assert_eq!(
            if_match(&[r#""3""#]).await,
            Ok(IfMatch(Some(Version::new(3))))
        );
    }

    #[tokio::test]
    async fn a_precondition_that_cannot_be_evaluated_is_refused() {
        for unmatchable in [r#"W/"3""#, r#""abc""#, r#""3", "4""#] {
            assert_eq!(
                if_match(&[unmatchable]).await,
                Err(StatusCode::PRECONDITION_FAILED),
                "{unmatchable}"
            );
        }
        assert_eq!(if_match(&["3"]).await, Err(StatusCode::BAD_REQUEST));
    }

    #[tokio::test]
    async fn only_a_success_carries_the_tag() {
        let ok = Versioned::new(Version::new(4), "body").into_response();
        assert_eq!(ok.headers().get(ETAG).unwrap(), r#""4""#);

        let failed = Versioned::new(Version::new(4), Problem::not_found()).into_response();
        assert!(failed.headers().get(ETAG).is_none());
    }
}
