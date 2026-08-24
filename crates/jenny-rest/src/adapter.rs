//! The REST driving adapter.

use axum::Router;
use axum::routing::Route;

use jenny_core::domain::{Label, Urn};

use crate::routes::{Endpoint, Routes};

/// A description of one mounted resource, for `/health/status` and OpenAPI.
#[derive(Clone, Debug)]
pub struct Resource {
    urn: Urn,
    description: Option<String>,
    labels: Vec<Label>,
    endpoints: Vec<Endpoint>,
}

impl Resource {
    pub fn urn(&self) -> &Urn {
        &self.urn
    }

    pub fn description(&self) -> Option<&str> {
        self.description.as_deref()
    }

    pub fn labels(&self) -> &[Label] {
        &self.labels
    }

    pub fn endpoints(&self) -> &[Endpoint] {
        &self.endpoints
    }
}

/// Exposes a bounded piece of an application over HTTP.
///
/// The adapter owns its own identity — there is no separate container type to
/// construct. Its `name` does three jobs: it roots the [`Urn`] of every resource
/// mounted on it, it keys the adapter's `applications.<name>` configuration section,
/// and it is what `/health/status` reports.
///
/// ```
/// use jenny_rest::{RestAdapter, Routes};
///
/// let notes: Routes = Routes::new("notes").get("/notes", || async { "[]" });
/// let health: Routes = Routes::new("ping").get("/ping", || async { "pong" });
///
/// let adapter = RestAdapter::new("my-notes")?
///     .describe("Note taking")
///     .mount(notes)?
///     .mount(health)?;
///
/// assert_eq!(adapter.urn().to_string(), "urn:my-notes");
/// assert_eq!(adapter.path(), "/api");                 // the default
/// assert_eq!(adapter.resources()[0].urn().to_string(), "urn:my-notes:notes");
/// # Ok::<(), anyhow::Error>(())
/// ```
///
/// ### Where it ends up
///
/// The mount path resolves in this order, most specific first:
///
/// 1. an explicit [`at`](Self::at) call,
/// 2. `server.api_path` in the configuration file,
/// 3. [`DEFAULT_API_PATH`] — `/api`.
///
/// Code beats configuration deliberately. An operator changing `server.api_path` is
/// moving the *convention*; a developer who wrote `.at("/v2")` meant that path
/// specifically, and having a config file silently relocate it would be baffling to
/// debug.
///
/// A service can host several adapters — one per bounded context. They cannot all take
/// the default, and the runtime rejects the collision rather than letting the second
/// one's routes silently never match.
pub struct RestAdapter {
    urn: Urn,
    path: String,
    /// Whether `path` came from an explicit [`at`](RestAdapter::at) call, so the
    /// runtime knows if it may apply the configured default.
    path_is_explicit: bool,
    description: Option<String>,
    resources: Vec<Resource>,
    router: Router,
    #[cfg(feature = "openapi")]
    openapi: Option<utoipa::openapi::OpenApi>,
}

/// Where REST adapters mount unless told otherwise.
pub const DEFAULT_API_PATH: &str = "/api";

impl RestAdapter {
    /// Fails when `name` is not a valid URN segment.
    pub fn new(name: impl Into<String>) -> anyhow::Result<Self> {
        Ok(Self {
            urn: Urn::root(name)?,
            path: DEFAULT_API_PATH.to_string(),
            path_is_explicit: false,
            description: None,
            resources: Vec::new(),
            router: Router::new(),
            #[cfg(feature = "openapi")]
            openapi: None,
        })
    }

    /// Attach this adapter's OpenAPI document, which the runtime serves and merges with
    /// any others.
    ///
    /// The document comes from your own `#[derive(OpenApi)]` type rather than being
    /// inferred from [`Routes`]. Inferring it would mean either annotating every handler
    /// or guessing at request and response shapes, and a guessed API document is worse
    /// than none — clients trust it.
    ///
    /// ```no_run
    /// use utoipa::OpenApi;
    /// # use jenny_rest::{RestAdapter, Routes};
    /// # #[derive(serde::Deserialize, utoipa::ToSchema)] struct NoteRequest { title: String }
    ///
    /// #[derive(OpenApi)]
    /// #[openapi(components(schemas(NoteRequest)))]
    /// struct NotesApi;
    ///
    /// # fn build() -> anyhow::Result<RestAdapter> {
    /// let adapter = RestAdapter::new("my-notes")?.with_openapi(NotesApi::openapi());
    /// # Ok(adapter)
    /// # }
    /// ```
    #[cfg(feature = "openapi")]
    pub fn with_openapi(mut self, openapi: utoipa::openapi::OpenApi) -> Self {
        self.openapi = Some(openapi);
        self
    }

    /// This adapter's OpenAPI document, if it supplied one.
    #[cfg(feature = "openapi")]
    pub fn openapi(&self) -> Option<&utoipa::openapi::OpenApi> {
        self.openapi.as_ref()
    }

    /// Mount somewhere other than the default, overriding configuration too.
    pub fn at(mut self, path: impl Into<String>) -> Self {
        self.path = normalize_mount_path(path.into());
        self.path_is_explicit = true;
        self
    }

    /// Apply a configured default mount path. Ignored when [`at`](Self::at) was called.
    ///
    /// The runtime calls this with `server.api_path` while assembling the router.
    pub fn apply_default_path(&mut self, path: &str) {
        if !self.path_is_explicit {
            self.path = normalize_mount_path(path.to_string());
        }
    }

    /// Whether the mount path was chosen in code rather than defaulted.
    pub fn path_is_explicit(&self) -> bool {
        self.path_is_explicit
    }

    pub fn describe(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }

    /// Mount a resource. Fails when the resource's name is not a valid URN segment.
    pub fn mount(mut self, routes: Routes) -> anyhow::Result<Self> {
        let urn = self.urn.clone().child(routes.name())?;
        self.resources.push(Resource {
            urn: urn.clone(),
            description: routes.description().map(ToOwned::to_owned),
            labels: routes.labels().to_vec(),
            endpoints: routes.endpoints().to_vec(),
        });
        self.router = self.router.merge(routes.into_router(urn));
        Ok(self)
    }

    /// Apply a tower [`Layer`](tower::Layer) across every resource on this adapter —
    /// authentication, per-adapter CORS, a body limit.
    ///
    /// Layers apply to the routes mounted *before* the call, which is axum's own
    /// ordering rule; mount first, then layer.
    pub fn layer<L>(mut self, layer: L) -> Self
    where
        L: tower::Layer<Route> + Clone + Send + Sync + 'static,
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

    pub fn name(&self) -> &str {
        self.urn.name()
    }

    pub fn urn(&self) -> &Urn {
        &self.urn
    }

    /// The base path everything on this adapter is mounted under.
    pub fn path(&self) -> &str {
        &self.path
    }

    pub fn description(&self) -> Option<&str> {
        self.description.as_deref()
    }

    pub fn resources(&self) -> &[Resource] {
        &self.resources
    }

    /// The finished router, nested under [`path`](Self::path).
    pub fn into_router(self) -> Router {
        if self.path.is_empty() {
            self.router
        } else {
            Router::new().nest(&self.path, self.router)
        }
    }
}

/// Leading slash on, trailing slash off. A mount path of `"/"` or `""` means "mount at
/// the root", which axum expresses by not nesting at all rather than by nesting under
/// `"/"`.
fn normalize_mount_path(path: String) -> String {
    let trimmed = path.trim_end_matches('/');
    if trimmed.is_empty() {
        String::new()
    } else if trimmed.starts_with('/') {
        trimmed.to_string()
    } else {
        format!("/{trimmed}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::{Body, to_bytes};
    use http::StatusCode;
    use tower::ServiceExt;

    fn notes() -> Routes {
        Routes::new("notes")
            .describe("Note lifecycle")
            .label("kind", "crud")
            .get("/notes", || async { "list" })
            .get("/notes/{id}", || async { "fetch" })
    }

    #[test]
    fn the_adapter_name_roots_every_resource_urn() {
        let adapter = RestAdapter::new("my-notes")
            .unwrap()
            .mount(notes())
            .unwrap();
        assert_eq!(adapter.name(), "my-notes");
        assert_eq!(adapter.urn().to_string(), "urn:my-notes");
        assert_eq!(adapter.resources().len(), 1);
        assert_eq!(
            adapter.resources()[0].urn().to_string(),
            "urn:my-notes:notes"
        );
        assert_eq!(adapter.resources()[0].description(), Some("Note lifecycle"));
        assert_eq!(adapter.resources()[0].endpoints().len(), 2);
    }

    #[test]
    fn an_invalid_name_is_rejected_at_construction() {
        assert!(RestAdapter::new("My-Notes").is_ok());
        assert!(RestAdapter::new("my notes/v1").is_err());
    }

    #[test]
    fn an_invalid_resource_name_is_rejected_at_mount() {
        let adapter = RestAdapter::new("my-notes").unwrap();
        assert!(adapter.mount(Routes::new("not a segment")).is_err());
    }

    #[test]
    fn the_mount_path_defaults_and_can_be_overridden_in_code() {
        let default = RestAdapter::new("my-notes").unwrap();
        assert_eq!(default.path(), DEFAULT_API_PATH);
        assert!(!default.path_is_explicit());

        let explicit = RestAdapter::new("my-notes").unwrap().at("/v2");
        assert_eq!(explicit.path(), "/v2");
        assert!(explicit.path_is_explicit());
    }

    #[test]
    fn a_configured_default_applies_only_when_code_did_not_choose() {
        let mut defaulted = RestAdapter::new("my-notes").unwrap();
        defaulted.apply_default_path("/service");
        assert_eq!(defaulted.path(), "/service");

        let mut explicit = RestAdapter::new("my-notes").unwrap().at("/v2");
        explicit.apply_default_path("/service");
        assert_eq!(
            explicit.path(),
            "/v2",
            "an explicit `at` must win over configuration"
        );
    }

    #[test]
    fn mount_paths_are_normalised() {
        assert_eq!(normalize_mount_path("api".into()), "/api");
        assert_eq!(normalize_mount_path("/api/".into()), "/api");
        assert_eq!(normalize_mount_path("/".into()), "");
        assert_eq!(normalize_mount_path(String::new()), "");
    }

    #[tokio::test]
    async fn resources_are_served_under_the_mount_path() {
        let app = RestAdapter::new("my-notes")
            .unwrap()
            .mount(notes())
            .unwrap()
            .into_router();

        let response = app
            .clone()
            .oneshot(
                http::Request::get("/api/notes")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        assert_eq!(String::from_utf8(bytes.to_vec()).unwrap(), "list");

        // ...and not at the bare resource path.
        let response = app
            .oneshot(http::Request::get("/notes").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn mounting_at_the_root_needs_no_prefix() {
        let app = RestAdapter::new("my-notes")
            .unwrap()
            .at("/")
            .mount(notes())
            .unwrap()
            .into_router();
        let response = app
            .oneshot(http::Request::get("/notes").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn several_resources_coexist_on_one_adapter() {
        let app = RestAdapter::new("shop")
            .unwrap()
            .mount(Routes::new("orders").get("/orders", || async { "orders" }))
            .unwrap()
            .mount(Routes::new("payments").get("/payments", || async { "payments" }))
            .unwrap()
            .into_router();

        for (uri, expected) in [("/api/orders", "orders"), ("/api/payments", "payments")] {
            let response = app
                .clone()
                .oneshot(http::Request::get(uri).body(Body::empty()).unwrap())
                .await
                .unwrap();
            let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
            assert_eq!(String::from_utf8(bytes.to_vec()).unwrap(), expected);
        }
    }
}
