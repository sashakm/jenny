//! One id per request, the same in the logs, the [`Context`](jenny_core::port::Context),
//! the response header and the `instance` of any [`Problem`](crate::Problem).

use std::pin::Pin;
use std::task::{Context as TaskContext, Poll};

use axum::extract::Request;
use axum::response::Response;
use http::HeaderValue;
use tower::{Layer, Service};
use uuid::Uuid;

use crate::extract::REQUEST_ID_HEADER;

tokio::task_local! {
    static CURRENT: Uuid;
}

/// The id of the request being served, while [`RequestIdLayer`] serves it.
pub(crate) fn current() -> Option<Uuid> {
    CURRENT.try_with(|id| *id).ok()
}

/// The id [`RequestIdLayer`] assigned to a request, in its extensions.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct RequestId(Uuid);

impl RequestId {
    pub fn get(&self) -> Uuid {
        self.0
    }
}

impl std::fmt::Display for RequestId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

/// Gives every request an id before anything else sees it.
///
/// An inbound `x-request-id` that is a UUID is kept, so a correlation id set by a
/// gateway or an upstream service survives the hop. Anything else is replaced with a
/// fresh one, in the request header too, so every layer and handler agrees on the id.
/// The id is echoed in the response's `x-request-id`, put into the extensions as
/// [`RequestId`] for [`Ctx`](crate::Ctx) and tracing spans, and becomes the `instance`
/// of every [`Problem`](crate::Problem) produced while serving the request.
///
/// The runtime installs it outermost. A hand-rolled app applies it last:
///
/// ```
/// use axum::Router;
/// use jenny_rest::RequestIdLayer;
///
/// let app: Router = Router::new().layer(RequestIdLayer);
/// ```
#[derive(Clone, Copy, Debug, Default)]
pub struct RequestIdLayer;

impl<S> Layer<S> for RequestIdLayer {
    type Service = RequestIdService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        RequestIdService { inner }
    }
}

#[derive(Clone, Debug)]
pub struct RequestIdService<S> {
    inner: S,
}

impl<S> Service<Request> for RequestIdService<S>
where
    S: Service<Request, Response = Response> + Clone + Send + 'static,
    S::Future: Send + 'static,
{
    type Response = Response;
    type Error = S::Error;
    type Future = Pin<Box<dyn Future<Output = Result<Response, S::Error>> + Send>>;

    fn poll_ready(&mut self, cx: &mut TaskContext<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, mut req: Request) -> Self::Future {
        let clone = self.inner.clone();
        let mut inner = std::mem::replace(&mut self.inner, clone);

        let id = req
            .headers()
            .get(REQUEST_ID_HEADER)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| Uuid::parse_str(value).ok())
            .unwrap_or_else(Uuid::new_v4);
        let header = HeaderValue::from_str(&id.to_string()).expect("a UUID is a valid header");
        req.headers_mut().insert(REQUEST_ID_HEADER, header.clone());
        req.extensions_mut().insert(RequestId(id));

        Box::pin(CURRENT.scope(id, async move {
            let mut response = inner.call(req).await?;
            response.headers_mut().insert(REQUEST_ID_HEADER, header);
            Ok(response)
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::Router;
    use axum::body::{Body, to_bytes};
    use axum::routing::get;
    use tower::ServiceExt;

    use crate::{Ctx, Problem};

    fn app() -> Router {
        Router::new()
            .route(
                "/id",
                get(|cx: Ctx| async move { cx.request_id().to_string() }),
            )
            .route("/fail", get(|| async { Problem::not_found() }))
            .fallback(crate::not_found)
            .layer(RequestIdLayer)
    }

    async fn get_with(uri: &str, request_id: Option<&str>) -> (String, String) {
        let mut request = http::Request::get(uri);
        if let Some(id) = request_id {
            request = request.header(REQUEST_ID_HEADER, id);
        }
        let response = app()
            .oneshot(request.body(Body::empty()).unwrap())
            .await
            .unwrap();
        let echoed = response.headers()[REQUEST_ID_HEADER]
            .to_str()
            .unwrap()
            .to_string();
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        (echoed, String::from_utf8(bytes.to_vec()).unwrap())
    }

    #[tokio::test]
    async fn the_handler_and_the_response_agree_on_a_minted_id() {
        let (echoed, seen) = get_with("/id", None).await;
        assert_eq!(echoed, seen);
        assert!(Uuid::parse_str(&echoed).is_ok());
    }

    #[tokio::test]
    async fn an_inbound_uuid_is_kept_and_anything_else_replaced() {
        let upstream = Uuid::new_v4().to_string();
        assert_eq!(get_with("/id", Some(&upstream)).await.1, upstream);

        let (echoed, seen) = get_with("/id", Some("not-a-uuid; drop table")).await;
        assert_eq!(echoed, seen);
        assert!(Uuid::parse_str(&seen).is_ok());
    }

    #[tokio::test]
    async fn a_problem_names_the_request_as_its_instance() {
        for uri in ["/fail", "/no-such-route"] {
            let (echoed, body) = get_with(uri, None).await;
            let problem: serde_json::Value = serde_json::from_str(&body).unwrap();
            assert_eq!(problem["instance"], format!("urn:uuid:{echoed}"), "{uri}");
        }
    }
}
