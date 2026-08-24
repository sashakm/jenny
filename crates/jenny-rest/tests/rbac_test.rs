//! End-to-end authorization: a real router, the real [`Authorization`] layer, and the
//! real RBAC authorizer from `jenny-security`, driven by `docs/data/sample_rbac_config.json`.
//!
//! Nothing here reimplements the rules — the previous version of this test carried its
//! own copy of the matching logic, which meant it could pass while the shipped
//! authorizer was broken. Every assertion below goes through the code a deployed
//! service would run.

use std::sync::Arc;

use async_trait::async_trait;
use axum::body::Body;
use axum::routing::get;
use axum::{Router, response::Response};
use jenny_core::domain::{AuthenticationError, Credentials, Principal};
use jenny_core::port::outbound::Authenticator;
use jenny_rest::security::{Authentication, Authorization};
use jenny_rest::{RestAdapter, Routes, StatusCode};
use jenny_security::RbacConfig;
use tower::ServiceExt;

/// Hands back a principal whose single role is whatever was presented as the API key.
/// Keeps the test about *authorization* — the authentication half is deliberately
/// trivial so a failure can only mean the rules are wrong.
#[derive(Debug)]
struct RoleAsCredential;

#[async_trait]
impl Authenticator for RoleAsCredential {
    async fn authenticate(
        &self,
        credentials: &Credentials,
    ) -> Result<Principal, AuthenticationError> {
        let role = credentials.secret();
        if role.is_empty() {
            return Err(AuthenticationError("no role presented".into()));
        }
        Ok(Principal::builder(format!("user-{role}"))
            .with_attribute("preferred_username", role)
            .with_attribute("roles", vec![role])
            .build())
    }
}

fn shop() -> Router {
    let rbac: Arc<dyn jenny_core::port::outbound::Authorizer> = Arc::new(
        RbacConfig::<String>::load_from_file("../../docs/data/sample_rbac_config.json").unwrap(),
    );

    let ok = || async { StatusCode::OK };

    let routes: Routes = Routes::new("shop")
        .route("/orders", get(ok).post(ok))
        .route(
            "/orders/{id}",
            get(ok).post(ok).put(ok).patch(ok).delete(ok),
        )
        .route(
            "/orders/{id}/status",
            get(ok).post(ok).put(ok).patch(ok).delete(ok),
        )
        .route("/payments", get(ok).post(ok))
        .route(
            "/payments/{id}",
            get(ok).post(ok).put(ok).patch(ok).delete(ok),
        )
        .route("/products", get(ok).post(ok))
        .route(
            "/products/{id}",
            get(ok).post(ok).put(ok).patch(ok).delete(ok),
        )
        .route("/customers", get(ok).post(ok))
        .route(
            "/customers/{id}",
            get(ok).post(ok).put(ok).patch(ok).delete(ok),
        )
        .route(
            "/customers/{id}/account",
            get(ok).post(ok).put(ok).patch(ok).delete(ok),
        );

    RestAdapter::new("shop")
        .unwrap()
        .mount(routes)
        .unwrap()
        .layer(Authorization::new(rbac))
        .layer(Authentication::required(Arc::new(RoleAsCredential)))
        .into_router()
}

const CUSTOMER_ACCOUNT: &str = "/api/customers/a9d2e97a-0f0c-40ff-85ac-1ce6b7a67f3e/account";
const CUSTOMER_COLLECTION: &str = "/api/customers";
const CUSTOMER_ITEM: &str = "/api/customers/a9d2e97a-0f0c-40ff-85ac-1ce6b7a67f3e";
const ORDER_COLLECTION: &str = "/api/orders";
const ORDER_ITEM: &str = "/api/orders/2341";
const ORDER_STATUS: &str = "/api/orders/2341/status";
const PAYMENT_COLLECTION: &str = "/api/payments";
const PAYMENT_ITEM: &str = "/api/payments/2341";
const PRODUCT_COLLECTION: &str = "/api/products";
const PRODUCT_ITEM: &str = "/api/products/2341";

const ALL_METHODS: [&str; 5] = ["GET", "POST", "PUT", "PATCH", "DELETE"];

async fn request(role: Option<&str>, method: &str, uri: &str) -> Response {
    let mut builder = http::Request::builder().method(method).uri(uri);
    if let Some(role) = role {
        builder = builder.header(http::header::AUTHORIZATION, format!("ApiKey {role}"));
    }
    shop()
        .oneshot(builder.body(Body::empty()).unwrap())
        .await
        .unwrap()
}

async fn assert_allowed(role: &str, cases: &[(&str, &str)]) {
    for (method, uri) in cases {
        let status = request(Some(role), method, uri).await.status();
        assert_eq!(
            status,
            StatusCode::OK,
            "{role} should be allowed to {method} {uri}"
        );
    }
}

async fn assert_denied(role: &str, cases: &[(&str, &str)]) {
    for (method, uri) in cases {
        let status = request(Some(role), method, uri).await.status();
        assert_eq!(
            status,
            StatusCode::FORBIDDEN,
            "{role} should be denied {method} {uri}"
        );
    }
}

#[tokio::test]
async fn without_credentials_nothing_is_reachable() {
    let response = request(None, "GET", ORDER_COLLECTION).await;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert!(
        response
            .headers()
            .contains_key(http::header::WWW_AUTHENTICATE)
    );
}

#[tokio::test]
async fn a_denial_is_a_problem_document() {
    let response = request(Some("Dispatch"), "DELETE", PRODUCT_ITEM).await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert_eq!(
        response.headers().get(http::header::CONTENT_TYPE).unwrap(),
        jenny_rest::PROBLEM_CONTENT_TYPE
    );

    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(body["type"], "urn:jenny:problem:forbidden");
    assert!(body["detail"].as_str().unwrap().contains("may not DELETE"));
}

#[tokio::test]
async fn admin_reaches_everything_under_the_api() {
    let mut cases = Vec::new();
    for uri in [
        CUSTOMER_ACCOUNT,
        CUSTOMER_COLLECTION,
        CUSTOMER_ITEM,
        ORDER_COLLECTION,
        ORDER_ITEM,
        ORDER_STATUS,
        PAYMENT_COLLECTION,
        PAYMENT_ITEM,
        PRODUCT_COLLECTION,
        PRODUCT_ITEM,
    ] {
        for method in ALL_METHODS {
            // The collections in this router expose GET and POST only.
            let collection = matches!(
                uri,
                CUSTOMER_COLLECTION | ORDER_COLLECTION | PAYMENT_COLLECTION | PRODUCT_COLLECTION
            );
            if collection && !matches!(method, "GET" | "POST") {
                continue;
            }
            cases.push((method, uri));
        }
    }
    assert_allowed("Admin", &cases).await;
}

#[tokio::test]
async fn dispatch_may_read_orders_and_drive_their_status() {
    assert_allowed(
        "Dispatch",
        &[
            ("GET", ORDER_COLLECTION),
            ("GET", ORDER_ITEM),
            ("GET", ORDER_STATUS),
            ("POST", ORDER_STATUS),
            ("PUT", ORDER_STATUS),
            ("PATCH", ORDER_STATUS),
            ("GET", PAYMENT_ITEM),
        ],
    )
    .await;

    assert_denied(
        "Dispatch",
        &[
            ("POST", ORDER_COLLECTION),
            ("DELETE", ORDER_ITEM),
            ("DELETE", ORDER_STATUS),
            ("PUT", PAYMENT_ITEM),
            ("GET", CUSTOMER_ITEM),
            ("GET", CUSTOMER_COLLECTION),
            ("GET", PRODUCT_ITEM),
        ],
    )
    .await;
}

#[tokio::test]
async fn purchasing_owns_products_and_nothing_else() {
    let mut allowed: Vec<(&str, &str)> = ALL_METHODS
        .iter()
        .map(|method| (*method, PRODUCT_ITEM))
        .collect();
    allowed.push(("GET", PRODUCT_COLLECTION));
    allowed.push(("POST", PRODUCT_COLLECTION));
    assert_allowed("Purchasing", &allowed).await;

    assert_denied(
        "Purchasing",
        &[
            ("GET", ORDER_ITEM),
            ("POST", PAYMENT_ITEM),
            ("DELETE", PAYMENT_ITEM),
            ("PUT", CUSTOMER_ITEM),
            ("GET", CUSTOMER_ACCOUNT),
            ("PATCH", CUSTOMER_ACCOUNT),
        ],
    )
    .await;
}

#[tokio::test]
async fn customer_support_inherits_dispatch_and_adds_customer_access() {
    assert_allowed(
        "CustomerSupport",
        &[
            // inherited from Dispatch
            ("GET", ORDER_COLLECTION),
            ("GET", ORDER_ITEM),
            ("GET", ORDER_STATUS),
            ("PUT", ORDER_STATUS),
            // its own grants
            ("DELETE", ORDER_ITEM),
            ("POST", PAYMENT_ITEM),
            ("PUT", PAYMENT_ITEM),
            ("PATCH", PAYMENT_ITEM),
            ("GET", CUSTOMER_COLLECTION),
            ("GET", CUSTOMER_ITEM),
            ("DELETE", CUSTOMER_ITEM),
        ],
    )
    .await;

    assert_denied(
        "CustomerSupport",
        &[
            ("POST", ORDER_COLLECTION),
            ("POST", PAYMENT_COLLECTION),
            ("PUT", PRODUCT_ITEM),
            ("POST", PRODUCT_COLLECTION),
            ("DELETE", PAYMENT_ITEM),
        ],
    )
    .await;
}

#[tokio::test]
async fn a_customer_may_only_place_orders_and_manage_their_account() {
    assert_allowed(
        "Customer",
        &[
            ("POST", ORDER_COLLECTION),
            ("POST", PAYMENT_COLLECTION),
            ("POST", CUSTOMER_COLLECTION),
            ("GET", PRODUCT_COLLECTION),
            ("GET", PRODUCT_ITEM),
            ("GET", CUSTOMER_ACCOUNT),
            ("POST", CUSTOMER_ACCOUNT),
            ("PUT", CUSTOMER_ACCOUNT),
            ("PATCH", CUSTOMER_ACCOUNT),
        ],
    )
    .await;

    assert_denied(
        "Customer",
        &[
            ("POST", PRODUCT_COLLECTION),
            ("DELETE", PRODUCT_ITEM),
            ("GET", CUSTOMER_ITEM),
            ("POST", ORDER_ITEM),
            ("PUT", ORDER_ITEM),
            ("PATCH", ORDER_ITEM),
            ("DELETE", CUSTOMER_ACCOUNT),
        ],
    )
    .await;
}

#[tokio::test]
async fn an_unknown_role_is_denied_everything() {
    assert_denied(
        "Stranger",
        &[("GET", ORDER_COLLECTION), ("GET", PRODUCT_COLLECTION)],
    )
    .await;
}
