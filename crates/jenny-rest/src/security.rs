//! HTTP-side security: lifting credentials, gating requests, CORS.
//!
//! The split here follows the rule that authentication is per-protocol but
//! verification is not. Getting a bearer token out of an `Authorization` header is an
//! HTTP concern and lives in this module; deciding whether that token is genuine is an
//! [`Authenticator`] implementation in a driven adapter. That is why these layers take
//! the *port* — swapping JWT verification for something else never touches this file.

use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context as TaskContext, Poll};
use std::time::Duration;

use axum::extract::Request;
use axum::response::{IntoResponse, Response};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use http::{Method, header};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use tower::{Layer, Service};
use tower_http::cors::{Any, CorsLayer};
use tracing::debug;

use jenny_core::domain::{AuthenticationError, Credentials, Principal};
use jenny_core::port::outbound::{Authenticator, Authorizer};

use crate::problem::Problem;

type BoxFuture<T> = Pin<Box<dyn Future<Output = T> + Send>>;

/// Read `Authorization: Bearer …` or `Authorization: ApiKey …` off a request.
///
/// Anything else — a missing header, an unknown scheme, a malformed value — is an
/// authentication *failure* rather than a silent pass, so a typo in a scheme name can
/// never be mistaken for an anonymous request.
pub fn credentials_from_headers(
    headers: &header::HeaderMap,
) -> Result<Credentials, AuthenticationError> {
    let raw = headers.get(header::AUTHORIZATION);
    raw.and_then(|value| value.to_str().ok())
        .and_then(|value| value.split_once(' '))
        .and_then(|(scheme, secret)| match scheme {
            "Bearer" => Some(Credentials::JsonWebToken(secret.to_string())),
            "ApiKey" => Some(Credentials::ApiKey(secret.to_string())),
            _ => None,
        })
        .ok_or_else(|| {
            debug!("could not parse Authorization header: {raw:?}");
            AuthenticationError("missing or malformed credentials".to_string())
        })
}

/// Verifies credentials and puts the resulting [`Principal`] into the request
/// extensions, where [`Ctx`](crate::Ctx) picks it up.
///
/// ```no_run
/// use std::sync::Arc;
/// use jenny_rest::{RestAdapter, Routes, security::Authentication};
/// # use jenny_core::port::outbound::Authenticator;
/// # fn build(authenticator: Arc<dyn Authenticator>, notes: Routes) -> anyhow::Result<RestAdapter> {
/// Ok(RestAdapter::new("my-notes")?
///     .mount(notes)?
///     .layer(Authentication::required(authenticator)))
/// # }
/// ```
#[derive(Clone)]
pub struct Authentication {
    authenticator: Arc<dyn Authenticator>,
    required: bool,
}

impl Authentication {
    /// Reject with 401 unless valid credentials are presented.
    pub fn required(authenticator: Arc<dyn Authenticator>) -> Self {
        Self {
            authenticator,
            required: true,
        }
    }

    /// Attach a principal when credentials are present and valid; otherwise carry on
    /// anonymously.
    ///
    /// Note the asymmetry: *absent* credentials are fine, but credentials that are
    /// present and **invalid** still fail with 401. Silently downgrading a bad token
    /// to anonymous access is how authentication bypasses happen.
    pub fn optional(authenticator: Arc<dyn Authenticator>) -> Self {
        Self {
            authenticator,
            required: false,
        }
    }
}

impl<S> Layer<S> for Authentication {
    type Service = AuthenticationService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        AuthenticationService {
            inner,
            authenticator: self.authenticator.clone(),
            required: self.required,
        }
    }
}

#[derive(Clone)]
pub struct AuthenticationService<S> {
    inner: S,
    authenticator: Arc<dyn Authenticator>,
    required: bool,
}

impl<S> Service<Request> for AuthenticationService<S>
where
    S: Service<Request, Response = Response> + Clone + Send + 'static,
    S::Future: Send + 'static,
{
    type Response = Response;
    type Error = S::Error;
    type Future = BoxFuture<Result<Response, S::Error>>;

    fn poll_ready(&mut self, cx: &mut TaskContext<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, mut req: Request) -> Self::Future {
        // The clone is the ready one; keep it and leave a fresh clone behind.
        let clone = self.inner.clone();
        let mut inner = std::mem::replace(&mut self.inner, clone);
        let authenticator = self.authenticator.clone();
        let required = self.required;

        Box::pin(async move {
            let credentials = match credentials_from_headers(req.headers()) {
                Ok(credentials) => credentials,
                Err(_) if !required => return inner.call(req).await,
                Err(e) => return Ok(Problem::unauthenticated(e.to_string()).into_response()),
            };

            match authenticator.authenticate(&credentials).await {
                Ok(principal) => {
                    debug!(scheme = %credentials, principal = %principal, "authenticated");
                    req.extensions_mut().insert(principal);
                    inner.call(req).await
                }
                Err(e) => Ok(Problem::unauthenticated(e.to_string()).into_response()),
            }
        })
    }
}

/// Gates requests on an [`Authorizer`], using the HTTP method as the action and the
/// request path as the resource.
///
/// Apply it alongside [`Authentication`] — it rejects with 403 when no principal has
/// been established, because "authorize an unknown caller" has no useful answer.
///
/// This is the coarse, endpoint-level gate. Rules that depend on the data being
/// addressed belong in a domain service, so they hold for calls arriving over
/// messaging and gRPC too.
#[derive(Clone)]
pub struct Authorization {
    authorizer: Arc<dyn Authorizer>,
}

impl Authorization {
    pub fn new(authorizer: Arc<dyn Authorizer>) -> Self {
        Self { authorizer }
    }
}

impl<S> Layer<S> for Authorization {
    type Service = AuthorizationService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        AuthorizationService {
            inner,
            authorizer: self.authorizer.clone(),
        }
    }
}

#[derive(Clone)]
pub struct AuthorizationService<S> {
    inner: S,
    authorizer: Arc<dyn Authorizer>,
}

impl<S> Service<Request> for AuthorizationService<S>
where
    S: Service<Request, Response = Response> + Clone + Send + 'static,
    S::Future: Send + 'static,
{
    type Response = Response;
    type Error = S::Error;
    type Future = BoxFuture<Result<Response, S::Error>>;

    fn poll_ready(&mut self, cx: &mut TaskContext<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: Request) -> Self::Future {
        let clone = self.inner.clone();
        let mut inner = std::mem::replace(&mut self.inner, clone);
        let authorizer = self.authorizer.clone();

        Box::pin(async move {
            let Some(principal) = req.extensions().get::<Principal>().cloned() else {
                return Ok(Problem::forbidden("no authenticated principal").into_response());
            };
            let action = req.method().as_str().to_owned();
            let resource = original_path(&req);

            match authorizer.authorize(&principal, &action, &resource).await {
                Ok(()) => inner.call(req).await,
                Err(e) => Ok(Problem::forbidden(e.to_string()).into_response()),
            }
        })
    }
}

/// Guards a route with HTTP Basic authentication against one fixed credential.
///
/// Meant for operational endpoints — the metrics scrape in particular — where the
/// caller is a Prometheus server rather than a person, and a single shared credential
/// from configuration is the whole requirement. Application traffic wants
/// [`Authentication`] and a real [`Authenticator`] instead.
///
/// **Basic auth sends the credential in every request, base64-encoded, which is not
/// encryption.** Serve it over TLS, or keep the endpoint on a network only your
/// scrapers can reach.
///
/// Both halves are compared in constant time, against SHA-256 digests so that a
/// mismatched *length* cannot be detected from timing either.
///
/// ```
/// use jenny_rest::security::BasicAuth;
///
/// let guard = BasicAuth::new("prometheus", "s3cret").with_realm("metrics");
/// ```
#[derive(Clone)]
pub struct BasicAuth {
    realm: Arc<str>,
    username: Arc<[u8; 32]>,
    password: Arc<[u8; 32]>,
}

impl BasicAuth {
    pub fn new(username: impl AsRef<str>, password: impl AsRef<str>) -> Self {
        Self {
            realm: Arc::from("restricted"),
            username: Arc::new(digest(username.as_ref().as_bytes())),
            password: Arc::new(digest(password.as_ref().as_bytes())),
        }
    }

    /// The realm quoted back in `WWW-Authenticate`.
    pub fn with_realm(mut self, realm: impl AsRef<str>) -> Self {
        self.realm = Arc::from(realm.as_ref());
        self
    }

    fn accepts(&self, header: Option<&header::HeaderValue>) -> bool {
        let Some(encoded) = header
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.strip_prefix("Basic "))
        else {
            return false;
        };
        let Ok(decoded) = BASE64.decode(encoded.trim()) else {
            return false;
        };
        let Ok(decoded) = String::from_utf8(decoded) else {
            return false;
        };
        // A password may itself contain a colon; a username may not.
        let (username, password) = decoded.split_once(':').unwrap_or((decoded.as_str(), ""));

        // Both halves are always compared, and neither comparison short-circuits, so
        // response time carries no information about which half was wrong.
        let user_ok = digest(username.as_bytes()).ct_eq(self.username.as_ref());
        let pass_ok = digest(password.as_bytes()).ct_eq(self.password.as_ref());
        (user_ok & pass_ok).into()
    }

    fn challenge(&self) -> Response {
        let mut response =
            Problem::unauthenticated("basic authentication required").into_response();
        // Replace the bearer/api-key challenge with the one that applies here.
        response.headers_mut().remove(header::WWW_AUTHENTICATE);
        if let Ok(value) = header::HeaderValue::from_str(&format!("Basic realm=\"{}\"", self.realm))
        {
            response
                .headers_mut()
                .insert(header::WWW_AUTHENTICATE, value);
        }
        response
    }
}

fn digest(bytes: &[u8]) -> [u8; 32] {
    let mut out = [0u8; 32];
    out.copy_from_slice(&Sha256::digest(bytes));
    out
}

impl<S> Layer<S> for BasicAuth {
    type Service = BasicAuthService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        BasicAuthService {
            inner,
            guard: self.clone(),
        }
    }
}

#[derive(Clone)]
pub struct BasicAuthService<S> {
    inner: S,
    guard: BasicAuth,
}

impl<S> Service<Request> for BasicAuthService<S>
where
    S: Service<Request, Response = Response> + Clone + Send + 'static,
    S::Future: Send + 'static,
{
    type Response = Response;
    type Error = S::Error;
    type Future = BoxFuture<Result<Response, S::Error>>;

    fn poll_ready(&mut self, cx: &mut TaskContext<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: Request) -> Self::Future {
        let clone = self.inner.clone();
        let mut inner = std::mem::replace(&mut self.inner, clone);
        let guard = self.guard.clone();

        Box::pin(async move {
            if guard.accepts(req.headers().get(header::AUTHORIZATION)) {
                inner.call(req).await
            } else {
                Ok(guard.challenge())
            }
        })
    }
}

/// The path as the *client* wrote it.
///
/// Necessary because [`Router::nest`](axum::Router::nest) rewrites the request URI to
/// strip the mount prefix before the nested router sees it. A layer applied inside a
/// [`RestAdapter`](crate::RestAdapter) mounted at `/api` would therefore be handed
/// `/orders` rather than `/api/orders` — and authorization rules are written against
/// the path clients actually call. axum preserves the original in
/// [`OriginalUri`](axum::extract::OriginalUri); this reads it, falling back to the
/// current URI when the adapter is not nested.
fn original_path(req: &Request) -> String {
    req.extensions()
        .get::<axum::extract::OriginalUri>()
        .map(|original| original.0.path().to_owned())
        .unwrap_or_else(|| req.uri().path().to_owned())
}

/// A CORS layer with REST-shaped defaults: the usual methods, the headers a JSON API
/// needs, and a one-hour preflight cache.
///
/// Origins are deliberately **not** set — call `.allow_origin(…)` yourself. A default
/// that allowed every origin would be a security decision the framework has no
/// business making on your behalf.
///
/// ```
/// use jenny_rest::security::cors;
///
/// let layer = cors()
///     .allow_origin("https://app.example.com".parse::<http::HeaderValue>().unwrap());
/// ```
pub fn cors() -> CorsLayer {
    CorsLayer::new()
        .allow_methods([
            Method::GET,
            Method::POST,
            Method::PUT,
            Method::PATCH,
            Method::DELETE,
        ])
        .allow_headers([
            header::AUTHORIZATION,
            header::ACCEPT,
            header::CONTENT_TYPE,
            header::USER_AGENT,
        ])
        .max_age(Duration::from_secs(3600))
}

/// [`cors`] with every origin allowed. Development only — it turns off the protection
/// CORS exists to provide.
pub fn permissive_cors() -> CorsLayer {
    cors().allow_origin(Any)
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use axum::Router;
    use axum::body::{Body, to_bytes};
    use axum::routing::get;
    use http::StatusCode;
    use jenny_core::domain::AuthorizationError;
    use tower::ServiceExt;

    use crate::Ctx;

    #[derive(Debug)]
    struct StaticAuthenticator;

    #[async_trait]
    impl Authenticator for StaticAuthenticator {
        async fn authenticate(
            &self,
            credentials: &Credentials,
        ) -> Result<Principal, AuthenticationError> {
            match credentials.secret() {
                "good" => Ok(Principal::builder("u-1")
                    .with_attribute("roles", vec!["Reader"])
                    .build()),
                _ => Err(AuthenticationError("bad credentials".to_string())),
            }
        }
    }

    #[derive(Debug)]
    struct ReadOnlyAuthorizer;

    #[async_trait]
    impl Authorizer for ReadOnlyAuthorizer {
        async fn authorize(
            &self,
            _: &Principal,
            action: &str,
            _: &str,
        ) -> Result<(), AuthorizationError> {
            if action == "GET" {
                Ok(())
            } else {
                Err(AuthorizationError("read-only".to_string()))
            }
        }
    }

    fn whoami_app() -> Router {
        Router::new().route(
            "/whoami",
            get(|cx: Ctx| async move {
                cx.principal()
                    .map(|p| p.id().to_string())
                    .unwrap_or_else(|| "anonymous".to_string())
            }),
        )
    }

    async fn body_string(response: Response) -> String {
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        String::from_utf8(bytes.to_vec()).unwrap()
    }

    #[test]
    fn credentials_are_parsed_per_scheme() {
        let mut headers = header::HeaderMap::new();
        headers.insert(header::AUTHORIZATION, "Bearer token".parse().unwrap());
        assert_eq!(
            credentials_from_headers(&headers).unwrap(),
            Credentials::JsonWebToken("token".into())
        );

        headers.insert(header::AUTHORIZATION, "ApiKey secret".parse().unwrap());
        assert_eq!(
            credentials_from_headers(&headers).unwrap(),
            Credentials::ApiKey("secret".into())
        );
    }

    #[test]
    fn an_unknown_or_missing_scheme_is_a_failure_not_a_pass() {
        assert!(credentials_from_headers(&header::HeaderMap::new()).is_err());

        let mut headers = header::HeaderMap::new();
        headers.insert(header::AUTHORIZATION, "Basic abc".parse().unwrap());
        assert!(credentials_from_headers(&headers).is_err());

        headers.insert(header::AUTHORIZATION, "Bearer".parse().unwrap());
        assert!(credentials_from_headers(&headers).is_err());
    }

    #[tokio::test]
    async fn valid_credentials_reach_the_handler_as_a_principal() {
        let app = whoami_app().layer(Authentication::required(Arc::new(StaticAuthenticator)));
        let response = app
            .oneshot(
                http::Request::get("/whoami")
                    .header(header::AUTHORIZATION, "Bearer good")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(body_string(response).await, "u-1");
    }

    #[tokio::test]
    async fn required_authentication_rejects_missing_and_invalid_credentials() {
        let app = whoami_app().layer(Authentication::required(Arc::new(StaticAuthenticator)));

        for request in [
            http::Request::get("/whoami").body(Body::empty()).unwrap(),
            http::Request::get("/whoami")
                .header(header::AUTHORIZATION, "Bearer bad")
                .body(Body::empty())
                .unwrap(),
        ] {
            let response = app.clone().oneshot(request).await.unwrap();
            assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
            assert!(response.headers().contains_key(header::WWW_AUTHENTICATE));
        }
    }

    #[tokio::test]
    async fn optional_authentication_lets_anonymous_callers_through() {
        let app = whoami_app().layer(Authentication::optional(Arc::new(StaticAuthenticator)));
        let response = app
            .oneshot(http::Request::get("/whoami").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(body_string(response).await, "anonymous");
    }

    #[tokio::test]
    async fn optional_authentication_still_rejects_bad_credentials() {
        let app = whoami_app().layer(Authentication::optional(Arc::new(StaticAuthenticator)));
        let response = app
            .oneshot(
                http::Request::get("/whoami")
                    .header(header::AUTHORIZATION, "Bearer bad")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            StatusCode::UNAUTHORIZED,
            "an invalid token must not be downgraded to anonymous access"
        );
    }

    #[tokio::test]
    async fn authorization_gates_on_the_method() {
        let app = Router::new()
            .route("/notes", get(|| async { "ok" }).post(|| async { "ok" }))
            .layer(Authorization::new(Arc::new(ReadOnlyAuthorizer)))
            .layer(Authentication::required(Arc::new(StaticAuthenticator)));

        let allowed = app
            .clone()
            .oneshot(
                http::Request::get("/notes")
                    .header(header::AUTHORIZATION, "Bearer good")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(allowed.status(), StatusCode::OK);

        let denied = app
            .oneshot(
                http::Request::post("/notes")
                    .header(header::AUTHORIZATION, "Bearer good")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(denied.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn authorization_sees_the_path_the_client_asked_for_not_the_nested_one() {
        use crate::{RestAdapter, Routes};

        /// Only grants the full, un-stripped path.
        #[derive(Debug)]
        struct OnlyFullPath;

        #[async_trait]
        impl Authorizer for OnlyFullPath {
            async fn authorize(
                &self,
                _: &Principal,
                _: &str,
                resource: &str,
            ) -> Result<(), AuthorizationError> {
                if resource == "/api/notes" {
                    Ok(())
                } else {
                    Err(AuthorizationError(format!("saw `{resource}`")))
                }
            }
        }

        // `Router::nest` rewrites the URI, so a naive layer would see `/notes` here.
        let app = RestAdapter::new("my-notes")
            .unwrap()
            .mount(Routes::new("notes").get("/notes", || async { "ok" }))
            .unwrap()
            .layer(Authorization::new(Arc::new(OnlyFullPath)))
            .layer(Authentication::required(Arc::new(StaticAuthenticator)))
            .into_router();

        let response = app
            .oneshot(
                http::Request::get("/api/notes")
                    .header(header::AUTHORIZATION, "Bearer good")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(
            response.status(),
            StatusCode::OK,
            "the authorizer must be given `/api/notes`, not the nest-stripped `/notes`"
        );
    }

    fn basic_header(username: &str, password: &str) -> String {
        format!("Basic {}", BASE64.encode(format!("{username}:{password}")))
    }

    async fn basic_auth_status(header_value: Option<&str>) -> StatusCode {
        let app = Router::new()
            .route("/metrics", get(|| async { "up 1" }))
            .layer(BasicAuth::new("prometheus", "s3cret").with_realm("metrics"));

        let mut request = http::Request::get("/metrics");
        if let Some(value) = header_value {
            request = request.header(header::AUTHORIZATION, value);
        }
        app.oneshot(request.body(Body::empty()).unwrap())
            .await
            .unwrap()
            .status()
    }

    #[tokio::test]
    async fn basic_auth_admits_the_configured_credential() {
        assert_eq!(
            basic_auth_status(Some(&basic_header("prometheus", "s3cret"))).await,
            StatusCode::OK
        );
    }

    #[tokio::test]
    async fn basic_auth_rejects_everything_else() {
        for (case, value) in [
            ("no header", None),
            ("wrong password", Some(basic_header("prometheus", "wrong"))),
            ("wrong username", Some(basic_header("someone", "s3cret"))),
            ("both wrong", Some(basic_header("someone", "wrong"))),
            ("empty", Some(basic_header("", ""))),
            ("not base64", Some("Basic !!!not-base64!!!".to_string())),
            ("wrong scheme", Some("Bearer s3cret".to_string())),
            (
                "no colon",
                Some(format!("Basic {}", BASE64.encode("prometheus"))),
            ),
        ] {
            assert_eq!(
                basic_auth_status(value.as_deref()).await,
                StatusCode::UNAUTHORIZED,
                "case: {case}"
            );
        }
    }

    #[tokio::test]
    async fn a_password_may_contain_colons() {
        let app = Router::new()
            .route("/metrics", get(|| async { "up 1" }))
            .layer(BasicAuth::new("prometheus", "a:b:c"));
        let response = app
            .oneshot(
                http::Request::get("/metrics")
                    .header(header::AUTHORIZATION, basic_header("prometheus", "a:b:c"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn a_basic_auth_challenge_names_its_realm_and_only_that_scheme() {
        let app = Router::new()
            .route("/metrics", get(|| async { "up 1" }))
            .layer(BasicAuth::new("prometheus", "s3cret").with_realm("metrics"));
        let response = app
            .oneshot(http::Request::get("/metrics").body(Body::empty()).unwrap())
            .await
            .unwrap();

        let challenges: Vec<_> = response
            .headers()
            .get_all(header::WWW_AUTHENTICATE)
            .iter()
            .map(|v| v.to_str().unwrap())
            .collect();
        assert_eq!(
            challenges,
            ["Basic realm=\"metrics\""],
            "the bearer/api-key challenge must not be offered here"
        );
    }

    #[test]
    fn credentials_are_compared_as_fixed_width_digests() {
        // The guard stores digests, not the secrets, so comparison is over equal-length
        // buffers whatever the caller sends and a length mismatch leaks no timing.
        let guard = BasicAuth::new("prometheus", "s3cret");
        assert_eq!(guard.username.len(), 32);
        assert_eq!(guard.password.len(), 32);

        let header = header::HeaderValue::from_str(&basic_header("prometheus", "s3cret")).unwrap();
        assert!(guard.accepts(Some(&header)));

        // A far shorter and a far longer attempt both simply fail.
        for wrong in ["x", &"x".repeat(4096)] {
            let header = header::HeaderValue::from_str(&basic_header("prometheus", wrong)).unwrap();
            assert!(!guard.accepts(Some(&header)));
        }
    }

    #[tokio::test]
    async fn authorization_without_a_principal_is_forbidden() {
        let app = Router::new()
            .route("/notes", get(|| async { "ok" }))
            .layer(Authorization::new(Arc::new(ReadOnlyAuthorizer)));
        let response = app
            .oneshot(http::Request::get("/notes").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }
}
