//! Contains a caching construct for JSON web keys. Can be used to reduce required network roundtrips when using oauth2 authentication.
use chrono::{DateTime, Duration, Local};
use jsonwebtoken::jwk::{Jwk, JwkSet};
use thiserror::Error;
use tracing::{info, warn};

static HTTP_CLIENT_RESPONSE_TIMEOUT: u64 = 10;
static HTTP_CLIENT_USER_AGENT: &str =
    concat!(env!("CARGO_PKG_NAME"), "/", env!("CARGO_PKG_VERSION"));

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

/// Holds a [JwkSet] and provides appropriate access methods.
/// Read attempts automatically update cache if last update time >= refresh_interval or
/// an expected signing key is not found in the key set.
/// Example
/// ```
/// use jenny_security::authentication::jwk::JwksCache;
///
/// let jwks_url = "https://www.googleapis.com/oauth2/v3/certs";
/// let mut jwks_cache = JwksCache::new(jwks_url, 3600);
/// ```
#[derive(Clone, Debug)]
pub struct JwksCache {
    jwks_url: String,
    jwks: JwkSet,
    last_update_time: DateTime<Local>,
    ttl: u64,
}

impl JwksCache {
    /// Create new cache with the url of the jwks server and the cache entries time to live
    pub fn new(jwks_url: &str, ttl: u64) -> Self {
        Self {
            jwks_url: jwks_url.to_owned(),
            jwks: JwkSet {
                keys: Vec::default(),
            },
            last_update_time: chrono::Local::now(),
            ttl,
        }
    }

    fn lookup_key_by_kid(&self, kid: &str) -> Result<Jwk, JwkError> {
        if let Some(jwks_sig_key) = self.jwks.find(kid) {
            Ok(jwks_sig_key.clone())
        } else {
            Err(JwkError::JwksLookupError())
        }
    }

    fn is_stale(&self) -> bool {
        Local::now() > self.last_update_time + Duration::seconds(self.ttl as i64)
    }

    fn set_update_time_now(&mut self) {
        self.last_update_time = Local::now();
    }

    fn was_recently_refreshed(&self) -> bool {
        Local::now() < self.last_update_time + Duration::minutes(5)
    }

    fn jwks_is_empty(&self) -> bool {
        self.jwks.keys.is_empty()
    }

    /// Sets the jwks of this [`JwksCache`].
    pub fn set_jwks(&mut self, jwks: &JwkSet) {
        self.jwks = jwks.clone();
    }

    /// Try to read key with `kid` from jwks. Updates cache entry if stale.
    pub async fn read_jwk(&mut self, kid: &str) -> Result<Jwk, JwkError> {
        if self.is_stale() {
            info!("JWKS cache is stale. Refreshing entry.");
            let jwks = Self::load_jwks(&self.jwks_url).await?;
            self.set_jwks(&jwks);
            self.set_update_time_now();
        };
        if self.jwks_is_empty() {
            info!("JWKS cache is empty. Loading initial key set.");
            let jwks = Self::load_jwks(&self.jwks_url).await?;
            self.set_jwks(&jwks);
            self.set_update_time_now();
        }
        match self.lookup_key_by_kid(kid) {
            Ok(jwk) => Ok(jwk),
            Err(_) => {
                if !self.was_recently_refreshed() {
                    warn!("Key with key id {kid} not found in cached JWKS.");
                    warn!("Refreshing entry.");
                    let jwks = Self::load_jwks(&self.jwks_url).await?;
                    self.set_jwks(&jwks);
                    self.set_update_time_now();
                    self.lookup_key_by_kid(kid)
                } else {
                    Err(JwkError::JwksLookupError())
                }
            }
        }
    }

    /// Fetch JWKS from auth provider
    pub async fn load_jwks(jwks_url: &str) -> Result<JwkSet, JwkError> {
        let client = reqwest::Client::default();
        let jwks = client
            .get(jwks_url)
            .header(
                reqwest::header::USER_AGENT,
                reqwest::header::HeaderValue::from_static(HTTP_CLIENT_USER_AGENT),
            )
            .timeout(std::time::Duration::from_secs(HTTP_CLIENT_RESPONSE_TIMEOUT))
            .send()
            .await
            .map_err(|e| JwkError::ProviderError(e.to_string()))?
            .json::<JwkSet>()
            .await
            .map_err(|e| JwkError::ProviderError(e.to_string()))?;
        Ok(jwks)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn create_jwks_cache(ttl: u64) -> JwksCache {
        let jwks_url = "https://www.googleapis.com/oauth2/v3/certs";
        JwksCache::new(jwks_url, ttl)
    }

    async fn get_keyid_from_keyset() -> String {
        let jwks_url = "https://www.googleapis.com/oauth2/v3/certs";
        let keyset = JwksCache::load_jwks(jwks_url).await.unwrap();
        let some_key = keyset.keys.first().unwrap();
        some_key.common.key_id.as_deref().unwrap().to_owned()
    }

    #[tokio::test]
    async fn test_jwks_cache_refresh_by_fetch() {
        let mut jwks_cache = create_jwks_cache(1).await;
        std::thread::sleep(std::time::Duration::new(2, 0));
        assert!(jwks_cache.is_stale());
        let keyid = get_keyid_from_keyset().await;
        let _ = jwks_cache.read_jwk(&keyid).await.unwrap();
        assert!(!jwks_cache.is_stale())
    }
}
