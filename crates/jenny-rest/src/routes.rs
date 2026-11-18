//! Declaring a resource's HTTP surface.

use axum::handler::Handler;
use axum::routing::{self, MethodRouter};
use axum::{Extension, Router};
use http::Method;

use jenny_core::domain::{Label, Urn};

use crate::extract::ResourceUrn;

/// One method-and-path pair a resource exposes. Introspection only — the router does
/// the actual matching.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Endpoint {
    pub method: String,
    pub path: String,
}

impl std::fmt::Display for Endpoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} {}", self.method, self.path)
    }
}

/// The routes of one resource, and what to call for each.
///
/// This is the whole driving-adapter surface for REST: a module under
/// `adapter/inbound/rest/` exports a function returning `Routes`, and the composition
/// root mounts it. There is no trait to implement and nothing to erase behind
/// `Arc<dyn …>` — a resource *is* its route table plus the handlers it names.
///
/// ```
/// use std::sync::Arc;
/// use jenny_rest::{ApiResult, Ctx, Json, Routes};
/// # use jenny_core::domain::DomainError;
/// # use jenny_core::port::Context;
/// # use axum::extract::{Path, State};
/// # #[derive(Clone)] struct NoteUseCases;
/// # impl NoteUseCases {
/// #     async fn list(&self, _: &Context) -> Result<Vec<String>, DomainError> { Ok(vec![]) }
/// #     async fn fetch(&self, _: u32, _: &Context) -> Result<String, DomainError> { Ok("note".into()) }
/// # }
/// async fn list(State(uc): State<Arc<NoteUseCases>>, cx: Ctx) -> ApiResult<Json<Vec<String>>> {
///     Ok(Json(uc.list(&cx).await?))
/// }
///
/// async fn fetch(
///     State(uc): State<Arc<NoteUseCases>>,
///     Path(id): Path<u32>,
///     cx: Ctx,
/// ) -> ApiResult<Json<String>> {
///     Ok(Json(uc.fetch(id, &cx).await?))
/// }
///
/// pub fn routes(uc: Arc<NoteUseCases>) -> Routes {
///     Routes::new("notes")
///         .describe("Note lifecycle")
///         .get("/notes", list)
///         .get("/notes/{id}", fetch)
///         .with_state(uc)
/// }
/// ```
///
/// The `S` parameter is axum's router state. It disappears once
/// [`with_state`](Self::with_state) has been called, which is what
/// [`RestAdapter::mount`](crate::RestAdapter::mount) requires — a resource must be
/// fully wired before it can be mounted.
pub struct Routes<S = ()> {
    name: String,
    description: Option<String>,
    labels: Vec<Label>,
    endpoints: Vec<Endpoint>,
    router: Router<S>,
}

impl<S> Routes<S>
where
    S: Clone + Send + Sync + 'static,
{
    /// `name` becomes this resource's URN segment under the adapter's root, so it
    /// must be a valid URN segment (alphanumerics, dashes, underscores).
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            description: None,
            labels: Vec::new(),
            endpoints: Vec::new(),
            router: Router::new(),
        }
    }

    pub fn describe(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }

    pub fn label(mut self, key: &str, value: &str) -> Self {
        self.labels.push(Label::new(key, value));
        self
    }

    pub fn get<H, T>(self, path: &str, handler: H) -> Self
    where
        H: Handler<T, S>,
        T: 'static,
    {
        self.method(Method::GET, path, routing::get(handler))
    }

    pub fn post<H, T>(self, path: &str, handler: H) -> Self
    where
        H: Handler<T, S>,
        T: 'static,
    {
        self.method(Method::POST, path, routing::post(handler))
    }

    pub fn put<H, T>(self, path: &str, handler: H) -> Self
    where
        H: Handler<T, S>,
        T: 'static,
    {
        self.method(Method::PUT, path, routing::put(handler))
    }

    pub fn patch<H, T>(self, path: &str, handler: H) -> Self
    where
        H: Handler<T, S>,
        T: 'static,
    {
        self.method(Method::PATCH, path, routing::patch(handler))
    }

    pub fn delete<H, T>(self, path: &str, handler: H) -> Self
    where
        H: Handler<T, S>,
        T: 'static,
    {
        self.method(Method::DELETE, path, routing::delete(handler))
    }

    pub fn head<H, T>(self, path: &str, handler: H) -> Self
    where
        H: Handler<T, S>,
        T: 'static,
    {
        self.method(Method::HEAD, path, routing::head(handler))
    }

    pub fn options<H, T>(self, path: &str, handler: H) -> Self
    where
        H: Handler<T, S>,
        T: 'static,
    {
        self.method(Method::OPTIONS, path, routing::options(handler))
    }

    /// Escape hatch to raw axum, for anything the shorthands do not cover — a
    /// handler shared by several methods, a custom fallback, `any`.
    ///
    /// ```
    /// # use jenny_rest::Routes;
    /// # use axum::routing::any;
    /// # use http::StatusCode;
    /// let routes: Routes = Routes::new("webhooks")
    ///     .route("/hooks/{provider}", any(|| async { StatusCode::ACCEPTED }));
    /// ```
    pub fn route(mut self, path: &str, method_router: MethodRouter<S>) -> Self {
        let path = normalize_path(path);
        self.endpoints.push(Endpoint {
            method: "*".to_string(),
            path: path.clone(),
        });
        self.router = self.router.route(&path, method_router);
        self
    }

    /// Apply a tower [`Layer`](tower::Layer) to the routes declared so far — a stricter
    /// body limit here, an authorization check there.
    ///
    /// axum layers wrap the routes that exist when they are applied, so a route added
    /// after this call is not covered. Declare every route first, then layer.
    pub fn layer<L>(mut self, layer: L) -> Self
    where
        L: tower::Layer<routing::Route> + Clone + Send + Sync + 'static,
        L::Service: tower::Service<axum::extract::Request> + Clone + Send + Sync + 'static,
        <L::Service as tower::Service<axum::extract::Request>>::Response:
            axum::response::IntoResponse + 'static,
        <L::Service as tower::Service<axum::extract::Request>>::Error:
            Into<std::convert::Infallible> + 'static,
        <L::Service as tower::Service<axum::extract::Request>>::Future: Send + 'static,
    {
        self.router = self.router.layer(layer);
        self
    }

    /// Supply the state the handlers extract with [`State`](axum::extract::State),
    /// producing a resource ready to be mounted.
    pub fn with_state(self, state: S) -> Routes<()> {
        Routes {
            name: self.name,
            description: self.description,
            labels: self.labels,
            endpoints: self.endpoints,
            router: self.router.with_state(state),
        }
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn description(&self) -> Option<&str> {
        self.description.as_deref()
    }

    pub fn labels(&self) -> &[Label] {
        &self.labels
    }

    /// Every method/path pair declared so far.
    pub fn endpoints(&self) -> &[Endpoint] {
        &self.endpoints
    }

    fn method(mut self, method: Method, path: &str, method_router: MethodRouter<S>) -> Self {
        let path = normalize_path(path);
        self.endpoints.push(Endpoint {
            method: method.to_string(),
            path: path.clone(),
        });
        // axum merges method routers registered against the same path, so
        // `.get(p, ..).post(p, ..)` builds one route with two methods.
        self.router = self.router.route(&path, method_router);
        self
    }
}

impl Routes<()> {
    /// Stamp this resource's URN onto every request it serves, and hand back the
    /// router. Called by [`RestAdapter::mount`](crate::RestAdapter::mount).
    pub(crate) fn into_router(self, urn: Urn) -> Router {
        self.router.layer(Extension(ResourceUrn(urn)))
    }
}

/// Leading slash on, trailing slash off, so that `"notes"`, `"/notes"` and
/// `"/notes/"` all mean the same route.
fn normalize_path(path: &str) -> String {
    let trimmed = path.trim_end_matches('/');
    if trimmed.is_empty() {
        "/".to_string()
    } else if trimmed.starts_with('/') {
        trimmed.to_string()
    } else {
        format!("/{trimmed}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::body::to_bytes;
    use http::StatusCode;
    use tower::ServiceExt;

    fn sample() -> Routes {
        Routes::new("notes")
            .describe("Note lifecycle")
            .label("kind", "crud")
            .get("/notes", || async { "list" })
            .post("/notes", || async { "create" })
            .get("/notes/{id}", || async { "fetch" })
            .delete("/notes/{id}", || async { "remove" })
    }

    #[test]
    fn a_resource_records_what_it_declares() {
        let routes = sample();
        assert_eq!(routes.name(), "notes");
        assert_eq!(routes.description(), Some("Note lifecycle"));
        assert_eq!(routes.labels()[0].to_string(), "kind: crud");
        assert_eq!(
            routes
                .endpoints()
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>(),
            [
                "GET /notes",
                "POST /notes",
                "GET /notes/{id}",
                "DELETE /notes/{id}"
            ]
        );
    }

    #[test]
    fn paths_are_normalised() {
        assert_eq!(normalize_path("notes"), "/notes");
        assert_eq!(normalize_path("/notes/"), "/notes");
        assert_eq!(normalize_path("/"), "/");
        assert_eq!(normalize_path(""), "/");
    }

    #[tokio::test]
    async fn several_methods_can_share_one_path() {
        let app = sample().into_router(Urn::root("app").unwrap().child("notes").unwrap());

        for (method, expected) in [(Method::GET, "list"), (Method::POST, "create")] {
            let response = app
                .clone()
                .oneshot(
                    http::Request::builder()
                        .method(method)
                        .uri("/notes")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
            assert_eq!(String::from_utf8(bytes.to_vec()).unwrap(), expected);
        }
    }

    #[tokio::test]
    async fn a_method_that_was_not_declared_is_not_allowed() {
        let app = sample().into_router(Urn::root("app").unwrap().child("notes").unwrap());
        let response = app
            .oneshot(
                http::Request::builder()
                    .method(Method::PUT)
                    .uri("/notes")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
    }

    #[tokio::test]
    async fn every_route_carries_the_resource_urn() {
        use crate::extract::RouteId;

        let routes: Routes =
            Routes::new("notes").get("/notes", |id: RouteId| async move { id.to_string() });
        let app = routes.into_router(Urn::root("app").unwrap().child("notes").unwrap());

        let response = app
            .oneshot(http::Request::get("/notes").body(Body::empty()).unwrap())
            .await
            .unwrap();
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        assert_eq!(
            String::from_utf8(bytes.to_vec()).unwrap(),
            "urn:app:notes#GET"
        );
    }

    #[tokio::test]
    async fn state_reaches_the_handlers() {
        use axum::extract::State;

        #[derive(Clone)]
        struct Greeting(&'static str);

        let routes = Routes::new("greet")
            .get("/greet", |State(g): State<Greeting>| async move { g.0 })
            .with_state(Greeting("hello"));
        let app = routes.into_router(Urn::root("app").unwrap().child("greet").unwrap());

        let response = app
            .oneshot(http::Request::get("/greet").body(Body::empty()).unwrap())
            .await
            .unwrap();
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        assert_eq!(String::from_utf8(bytes.to_vec()).unwrap(), "hello");
    }

    #[tokio::test]
    async fn the_raw_axum_escape_hatch_works() {
        let routes: Routes = Routes::new("hooks").route(
            "/hooks/{provider}",
            routing::any(|| async { StatusCode::ACCEPTED }),
        );
        assert_eq!(routes.endpoints()[0].to_string(), "* /hooks/{provider}");

        let app = routes.into_router(Urn::root("app").unwrap().child("hooks").unwrap());
        let response = app
            .oneshot(
                http::Request::put("/hooks/stripe")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::ACCEPTED);
    }
}
