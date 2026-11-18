//! A cache of an identity provider's JSON web keys, so verifying a token does not cost a
//! network round trip.
use std::sync::Arc;
use std::time::{Duration, Instant};

use arc_swap::ArcSwap;
use jsonwebtoken::jwk::{Jwk, JwkSet};
use thiserror::Error;
use tracing::{info, warn};

static HTTP_CLIENT_RESPONSE_TIMEOUT: Duration = Duration::from_secs(10);
static HTTP_CLIENT_USER_AGENT: &str =
    concat!(env!("CARGO_PKG_NAME"), "/", env!("CARGO_PKG_VERSION"));

/// How long after a failed fetch the next one may be attempted.
const RETRY_AFTER_FAILURE: Duration = Duration::from_secs(5);
/// How often a token with an unknown key id may trigger a fetch, unless configured
/// otherwise. The key id is chosen by whoever sent the token, so without a limit every
/// forged token would cost a request to the provider.
pub const DEFAULT_UNKNOWN_KEY_REFETCH: Duration = Duration::from_secs(10);

/// Error type for JWK related results
#[derive(Debug, Error)]
pub enum JwkError {
    #[error("Failure while interacting with auth provider: {0}")]
    ProviderError(String),
    #[error("Signing key not found in JWKS")]
    JwksLookupError(),
    #[error("Could not process JWK: {0}")]
    JwkFormatError(String),
    #[error("Failure while processing token: {0}")]
    JwtError(#[from] jsonwebtoken::errors::Error),
}

/// The keys as last fetched, and when.
#[derive(Clone, Debug, Default)]
struct Snapshot {
    keys: JwkSet,
    fetched_at: Option<Instant>,
    attempted_at: Option<Instant>,
}

/// An identity provider's key set, fetched on first use and refreshed when it is older
/// than its time-to-live or a token names a key it does not hold.
///
/// Lookups never wait for each other. One task refreshes at a time, and while it does,
/// or when a refresh fails, the others keep verifying against the keys already held: a
/// provider outage does not take authentication down with it until the provider has
/// rotated its keys. Only the very first fetch, when there is nothing to fall back on,
/// is waited for.
///
/// ```
/// use jenny_security::authentication::jwk::JwksCache;
///
/// let jwks_cache = JwksCache::new("https://www.googleapis.com/oauth2/v3/certs", 3600);
/// ```
#[derive(Debug)]
pub struct JwksCache {
    jwks_url: String,
    ttl: Duration,
    unknown_key_refetch: Duration,
    client: reqwest::Client,
    state: ArcSwap<Snapshot>,
    refreshing: tokio::sync::Mutex<()>,
}

impl JwksCache {
    /// Create new cache with the url of the jwks server and the cache entries time to live
    /// in seconds.
    pub fn new(jwks_url: &str, ttl: u64) -> Self {
        Self {
            jwks_url: jwks_url.to_owned(),
            ttl: Duration::from_secs(ttl),
            unknown_key_refetch: DEFAULT_UNKNOWN_KEY_REFETCH,
            client: reqwest::Client::new(),
            state: ArcSwap::from_pointee(Snapshot::default()),
            refreshing: tokio::sync::Mutex::new(()),
        }
    }

    /// How long after any fetch a token naming an unknown key may trigger another one.
    pub fn with_unknown_key_refetch(mut self, interval: Duration) -> Self {
        self.unknown_key_refetch = interval;
        self
    }

    /// The key with `kid`, fetching or refreshing the key set first when needed.
    pub async fn read_jwk(&self, kid: &str) -> Result<Jwk, JwkError> {
        let seen = self.state.load_full();
        // After a failed fetch, the next one waits a moment rather than every request
        // trying again.
        let backoff = if seen.attempted_at != seen.fetched_at {
            RETRY_AFTER_FAILURE
        } else {
            Duration::ZERO
        };
        match seen.fetched_at {
            None => {
                info!(url = %self.jwks_url, "loading the initial JWKS");
                self.refresh(&seen, true, backoff).await?;
                if self.state.load().fetched_at.is_none() {
                    return Err(JwkError::ProviderError(
                        "the key set could not be loaded yet".to_string(),
                    ));
                }
            }
            Some(fetched_at) if fetched_at.elapsed() >= self.ttl => {
                if let Err(e) = self.refresh(&seen, false, backoff).await {
                    warn!("could not refresh the JWKS, verifying against the cached keys: {e}");
                }
            }
            Some(_) => {}
        }

        if let Some(jwk) = self.find(kid) {
            return Ok(jwk);
        }

        // The provider may have rotated its keys since the last fetch.
        let seen = self.state.load_full();
        if let Err(e) = self.refresh(&seen, true, self.unknown_key_refetch).await {
            warn!("could not refresh the JWKS for an unknown key id: {e}");
        }
        self.find(kid).ok_or(JwkError::JwksLookupError())
    }

    fn find(&self, kid: &str) -> Option<Jwk> {
        self.state.load().keys.find(kid).cloned()
    }

    /// Fetch the key set, unless another task attempted a fetch since `seen` was taken
    /// or the last attempt is less than `min_gap` ago. With `wait` false, a fetch that is
    /// already underway is not waited for.
    async fn refresh(
        &self,
        seen: &Snapshot,
        wait: bool,
        min_gap: Duration,
    ) -> Result<(), JwkError> {
        let _guard = if wait {
            self.refreshing.lock().await
        } else {
            match self.refreshing.try_lock() {
                Ok(guard) => guard,
                Err(_) => return Ok(()),
            }
        };

        let current = self.state.load_full();
        if current.attempted_at != seen.attempted_at
            || current
                .attempted_at
                .is_some_and(|at| at.elapsed() < min_gap)
        {
            return Ok(());
        }

        let attempted_at = Instant::now();
        let fetched = Self::load_jwks_with(&self.client, &self.jwks_url).await;
        let next = match &fetched {
            Ok(keys) => Snapshot {
                keys: keys.clone(),
                fetched_at: Some(attempted_at),
                attempted_at: Some(attempted_at),
            },
            Err(_) => Snapshot {
                attempted_at: Some(attempted_at),
                ..(*current).clone()
            },
        };
        self.state.store(Arc::new(next));
        fetched.map(|_| ())
    }

    /// Fetch JWKS from auth provider
    pub async fn load_jwks(jwks_url: &str) -> Result<JwkSet, JwkError> {
        Self::load_jwks_with(&reqwest::Client::new(), jwks_url).await
    }

    async fn load_jwks_with(client: &reqwest::Client, jwks_url: &str) -> Result<JwkSet, JwkError> {
        client
            .get(jwks_url)
            .header(
                reqwest::header::USER_AGENT,
                reqwest::header::HeaderValue::from_static(HTTP_CLIENT_USER_AGENT),
            )
            .timeout(HTTP_CLIENT_RESPONSE_TIMEOUT)
            .send()
            .await
            .and_then(reqwest::Response::error_for_status)
            .map_err(|e| JwkError::ProviderError(e.to_string()))?
            .json::<JwkSet>()
            .await
            .map_err(|e| JwkError::ProviderError(e.to_string()))
    }
}
