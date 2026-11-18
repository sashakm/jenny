//! The JWKS cache and JWT verification against a local stand-in for an identity
//! provider, so the tests run offline and the provider can fail or rotate on demand.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use jenny_core::domain::Credentials;
use jenny_core::port::outbound::Authenticator;
use jenny_security::AuthenticationConfig;
use jenny_security::authentication::jwk::{JwkError, JwksCache};
use jenny_security::authentication::jwt::{JwtAuthContextConfig, OidcConfig};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// Serves whatever key set it currently holds, counting requests, or 503 while down.
struct Provider {
    url: String,
    hits: Arc<AtomicUsize>,
    up: Arc<AtomicBool>,
    keys: Arc<Mutex<Value>>,
}

impl Provider {
    async fn start(keys: Value) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/certs", listener.local_addr().unwrap());
        let provider = Self {
            url,
            hits: Arc::default(),
            up: Arc::new(AtomicBool::new(true)),
            keys: Arc::new(Mutex::new(keys)),
        };

        let (hits, up, keys) = (
            provider.hits.clone(),
            provider.up.clone(),
            provider.keys.clone(),
        );
        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                let (hits, up, keys) = (hits.clone(), up.clone(), keys.clone());
                tokio::spawn(async move {
                    let mut request = Vec::new();
                    let mut buffer = [0u8; 1024];
                    while !request.windows(4).any(|w| w == b"\r\n\r\n") {
                        match socket.read(&mut buffer).await {
                            Ok(0) | Err(_) => return,
                            Ok(n) => request.extend_from_slice(&buffer[..n]),
                        }
                    }
                    hits.fetch_add(1, Ordering::SeqCst);
                    let (status, body) = if up.load(Ordering::SeqCst) {
                        ("200 OK", keys.lock().unwrap().to_string())
                    } else {
                        ("503 Service Unavailable", "{}".to_string())
                    };
                    let response = format!(
                        "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = socket.write_all(response.as_bytes()).await;
                    let _ = socket.shutdown().await;
                });
            }
        });
        provider
    }

    fn hits(&self) -> usize {
        self.hits.load(Ordering::SeqCst)
    }

    fn go_down(&self) {
        self.up.store(false, Ordering::SeqCst);
    }

    fn serve(&self, keys: Value) {
        *self.keys.lock().unwrap() = keys;
    }
}

fn oct(kid: &str) -> Value {
    json!({ "kty": "oct", "kid": kid, "k": "c2VjcmV0" })
}

fn key_set(kids: &[&str]) -> Value {
    json!({ "keys": kids.iter().map(|kid| oct(kid)).collect::<Vec<_>>() })
}

#[tokio::test]
async fn keys_are_fetched_once_and_then_served_from_the_cache() {
    let provider = Provider::start(key_set(&["k1"])).await;
    let cache = JwksCache::new(&provider.url, 3600);

    cache.read_jwk("k1").await.unwrap();
    cache.read_jwk("k1").await.unwrap();
    assert_eq!(provider.hits(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_first_lookups_share_one_fetch() {
    let provider = Provider::start(key_set(&["k1"])).await;
    let cache = Arc::new(JwksCache::new(&provider.url, 3600));

    let lookups: Vec<_> = (0..20)
        .map(|_| {
            let cache = cache.clone();
            tokio::spawn(async move { cache.read_jwk("k1").await })
        })
        .collect();
    for lookup in lookups {
        lookup.await.unwrap().unwrap();
    }
    assert_eq!(provider.hits(), 1);
}

#[tokio::test]
async fn a_stale_key_set_is_refreshed() {
    let provider = Provider::start(key_set(&["k1"])).await;
    let cache = JwksCache::new(&provider.url, 0);

    cache.read_jwk("k1").await.unwrap();
    cache.read_jwk("k1").await.unwrap();
    assert_eq!(provider.hits(), 2);
}

#[tokio::test]
async fn an_outage_falls_back_to_the_cached_keys_and_backs_off() {
    let provider = Provider::start(key_set(&["k1"])).await;
    let cache = JwksCache::new(&provider.url, 0);
    cache.read_jwk("k1").await.unwrap();

    provider.go_down();
    cache
        .read_jwk("k1")
        .await
        .expect("a stale key must still verify while the provider is down");
    assert_eq!(provider.hits(), 2, "the refresh was attempted");

    cache.read_jwk("k1").await.unwrap();
    assert_eq!(
        provider.hits(),
        2,
        "right after a failure, the next request does not try again"
    );
}

#[tokio::test]
async fn a_rotated_key_is_picked_up() {
    let provider = Provider::start(key_set(&["k1"])).await;
    let cache = JwksCache::new(&provider.url, 3600).with_unknown_key_refetch(Duration::ZERO);
    cache.read_jwk("k1").await.unwrap();

    provider.serve(key_set(&["k1", "k2"]));
    cache.read_jwk("k2").await.unwrap();
    assert_eq!(provider.hits(), 2);
}

#[tokio::test]
async fn forged_key_ids_do_not_each_cost_a_fetch() {
    let provider = Provider::start(key_set(&["k1"])).await;
    let cache = JwksCache::new(&provider.url, 3600);
    cache.read_jwk("k1").await.unwrap();

    for _ in 0..5 {
        assert!(matches!(
            cache.read_jwk("forged").await,
            Err(JwkError::JwksLookupError())
        ));
    }
    assert_eq!(provider.hits(), 1);
}

#[tokio::test]
async fn without_any_keys_a_failing_provider_is_an_error() {
    let provider = Provider::start(key_set(&["k1"])).await;
    provider.go_down();
    let cache = JwksCache::new(&provider.url, 3600);

    assert!(matches!(
        cache.read_jwk("k1").await,
        Err(JwkError::ProviderError(_))
    ));
    assert!(matches!(
        cache.read_jwk("k1").await,
        Err(JwkError::ProviderError(_))
    ));
    assert_eq!(provider.hits(), 1, "the retry waits out the backoff");
}

const ISSUER: &str = "https://id.example.com/";
const AUDIENCE: &str = "notes-api";

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

/// A token signed with the private half of `docs/data/jwk_rsa.json`.
fn token(claims: Value) -> String {
    let key = jsonwebtoken::EncodingKey::from_rsa_pem(include_bytes!(
        "../../../docs/data/private_rsa_key.pem"
    ))
    .unwrap();
    let mut header = jsonwebtoken::Header::new(jsonwebtoken::Algorithm::RS256);
    header.kid = Some("rsa01".to_string());
    jsonwebtoken::encode(&header, &claims, &key).unwrap()
}

fn claims() -> Value {
    json!({
        "sub": "alice",
        "iss": ISSUER,
        "aud": AUDIENCE,
        "iat": now(),
        "exp": now() + 300,
        "roles": ["Reader"],
    })
}

async fn rsa_provider() -> Provider {
    let jwk: Value = serde_json::from_str(include_str!("../../../docs/data/jwk_rsa.json")).unwrap();
    Provider::start(json!({ "keys": [jwk] })).await
}

fn verifier(provider: &Provider) -> JwtAuthContextConfig {
    JwtAuthContextConfig::new(
        JwksCache::new(&provider.url, 3600),
        vec![ISSUER.to_string()],
        vec![AUDIENCE.to_string()],
        vec![],
    )
}

#[tokio::test]
async fn a_valid_token_yields_its_principal() {
    let provider = rsa_provider().await;
    let authenticator = AuthenticationConfig::default().with_jwt_enabled(verifier(&provider));

    let principal = authenticator
        .authenticate(&Credentials::JsonWebToken(token(claims())))
        .await
        .unwrap();
    assert_eq!(principal.id(), "alice");
    assert!(principal.has_role("Reader"));
}

#[tokio::test]
async fn tokens_for_another_audience_issuer_or_time_are_rejected() {
    let provider = rsa_provider().await;
    let verifier = verifier(&provider);

    let mut cases = Vec::new();
    for (field, value) in [
        ("aud", json!("someone-else")),
        ("iss", json!("https://evil.example.com/")),
        ("exp", json!(now() - 3600)),
    ] {
        let mut claims = claims();
        claims[field] = value;
        cases.push((field, claims));
    }
    for (field, claims) in cases {
        assert!(
            verifier.verify_token(&token(claims)).await.is_err(),
            "a token with the wrong `{field}` must be rejected"
        );
    }
}

#[tokio::test]
async fn a_token_signed_with_another_algorithm_than_its_key_declares_is_rejected() {
    let provider = rsa_provider().await;
    let mut header = jsonwebtoken::Header::new(jsonwebtoken::Algorithm::HS256);
    header.kid = Some("rsa01".to_string());
    let forged = jsonwebtoken::encode(
        &header,
        &claims(),
        &jsonwebtoken::EncodingKey::from_secret(b"guess").unwrap(),
    )
    .unwrap();

    let err = verifier(&provider).verify_token(&forged).await.unwrap_err();
    assert!(err.to_string().contains("does not match"), "{err}");
}

#[test]
fn an_oidc_config_must_check_the_audience_unless_told_not_to() {
    let config = |skip: bool, audience: Option<Vec<String>>| -> OidcConfig {
        serde_json::from_value(json!({
            "issuer": [ISSUER],
            "jwks_url": "https://id.example.com/certs",
            "valid_audience": audience,
            "skip_audience_validation": skip,
        }))
        .unwrap()
    };

    assert!(config(false, None).auth_context_config().is_err());
    assert!(config(false, Some(vec![])).auth_context_config().is_err());
    assert!(
        config(false, Some(vec![AUDIENCE.into()]))
            .auth_context_config()
            .is_ok()
    );
    assert!(config(true, None).auth_context_config().is_ok());
}
