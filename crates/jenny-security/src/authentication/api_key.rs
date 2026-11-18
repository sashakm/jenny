//! API-key authentication against a store of hashed keys.
use arc_swap::ArcSwap;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::Value;
use tracing::warn;

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::{Duration, Instant, SystemTime},
};

use jenny_core::domain::Principal;

/// How often a file-backed store looks at its file, unless configured otherwise.
pub const DEFAULT_REFRESH_INTERVAL: Duration = Duration::from_secs(5);

/// One entry of an API-key store: who the key belongs to, until when, and anything else
/// the principal should carry.
///
/// ```json
/// { "id": "svc-billing", "expiration": 1798761600, "roles": ["Billing"] }
/// ```
#[derive(Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
pub struct ApiKeyClaims {
    #[serde(alias = "sub", alias = "user_id", alias = "userId")]
    pub id: String,
    /// Seconds since the Unix epoch after which the key is no longer accepted.
    #[serde(default, alias = "exp")]
    pub expiration: Option<u64>,
    #[serde(default, flatten)]
    pub attributes: HashMap<String, Value>,
}

impl ApiKeyClaims {
    /// Whether the key had expired at `now`.
    pub fn is_expired_at(&self, now: SystemTime) -> bool {
        let now = now
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|since| since.as_secs())
            .unwrap_or_default();
        self.expiration.is_some_and(|expiration| now >= expiration)
    }
}

impl From<ApiKeyClaims> for Principal {
    fn from(claims: ApiKeyClaims) -> Self {
        let mut principal = Principal::builder(claims.id).with_attributes(claims.attributes);
        if let Some(expiration) = claims.expiration {
            principal = principal.with_attribute("expiration", expiration);
        }
        principal.build()
    }
}

/// This is a configuration type for for Api Key authentication.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct ApiKeyConfig {
    /// api key store file path
    pub path: String,
    /// load file once at startup and don't monitor for changes. Default: false
    #[serde(default)]
    pub static_load: bool,
    /// seconds between looks at the store file for changes. Default: 5
    #[serde(default)]
    pub refresh_interval: Option<u64>,
}

impl ApiKeyConfig {
    pub fn auth_context_config(&self) -> anyhow::Result<ApiKeyAuthContextConfig> {
        if !self.static_load {
            let mut config = ApiKeyAuthContextConfig::new_file_backed(&self.path);
            if let Some(seconds) = self.refresh_interval {
                config = config.with_refresh_interval(Duration::from_secs(seconds));
            }
            Ok(config)
        } else {
            let json_data = std::fs::read(&self.path)?;
            Ok(ApiKeyAuthContextConfig::new_static(
                ApiKeyAuthContextConfig::load_store_from_json(&json_data)?,
            ))
        }
    }
}

/// What a file-backed store knows about its file.
#[derive(Debug, Default)]
struct FileState {
    /// Modification time and length when the store was last loaded.
    loaded: Option<(SystemTime, u64)>,
    checked_at: Option<Instant>,
}

/// The API-key half of [`AuthenticationConfig`](crate::AuthenticationConfig).
///
/// Keys are stored hashed and looked up by the SHA-256 of what the caller presented, so
/// a leaked store does not leak usable keys. The store can either be loaded once at
/// startup or backed by a file that is re-read when it changes on disk. The latter lets
/// keys be rotated or revoked without a restart: the file is looked at no more often
/// than the [refresh interval](Self::with_refresh_interval), and a file that cannot be
/// read or parsed leaves the last good store in place.
/// ```
/// use std::collections::HashMap;
/// use jenny_security::authentication::api_key::ApiKeyAuthContextConfig;
///
/// let api_key_store_entry = ("key1".to_string(), r#"{"user_id": 1234, "name": "Test User"}"#.to_string().into_boxed_str());
///
/// let static_api_key_store = ApiKeyAuthContextConfig::new_static(HashMap::from([api_key_store_entry.clone()]));
/// let file_backed_api_key_store = ApiKeyAuthContextConfig::new_file_backed("path/to/api_key_store_file.json");
///
/// # assert_eq!(tokio::runtime::Runtime::new().unwrap().block_on(static_api_key_store.get::<serde_json::Value>("key1")).unwrap(), Some(serde_json::from_str(&api_key_store_entry.1).unwrap()));
/// ```
#[derive(Debug, Clone)]
pub struct ApiKeyAuthContextConfig {
    store: Arc<ArcSwap<HashMap<String, Box<str>>>>,
    store_path: String,
    refresh_interval: Duration,
    file: Arc<Mutex<FileState>>,
}

impl ApiKeyAuthContextConfig {
    /// Loads the api key store from json bytes.
    pub fn load_store_from_json(json_data: &[u8]) -> anyhow::Result<HashMap<String, Box<str>>> {
        Ok(
            serde_json::from_slice::<serde_json::Map<String, serde_json::Value>>(json_data)?
                .into_iter()
                .try_fold(HashMap::new(), |mut acc, (k, v)| {
                    acc.insert(k, serde_json::to_string(&v)?.into_boxed_str());
                    Ok::<_, serde_json::Error>(acc)
                })?,
        )
    }

    /// Create new and enable store refreshes
    pub fn new_file_backed(store_path: &str) -> Self {
        Self {
            store: Arc::new(ArcSwap::from_pointee(HashMap::new())),
            store_path: store_path.to_string(),
            refresh_interval: DEFAULT_REFRESH_INTERVAL,
            file: Arc::default(),
        }
    }

    /// Create new with static store data and no runtime refreshes
    pub fn new_static(store_data: HashMap<String, Box<str>>) -> Self {
        Self {
            store: Arc::new(ArcSwap::from_pointee(store_data)),
            store_path: String::new(),
            refresh_interval: DEFAULT_REFRESH_INTERVAL,
            file: Arc::default(),
        }
    }

    /// How often a file-backed store looks at its file. `Duration::ZERO` looks on every
    /// lookup.
    pub fn with_refresh_interval(mut self, interval: Duration) -> Self {
        self.refresh_interval = interval;
        self
    }

    /// Retrieves and deserializes entry into the provided type `T` associated with the provided api key.
    pub async fn get<T: DeserializeOwned>(&self, api_key: &str) -> anyhow::Result<Option<T>> {
        if !self.store_path.is_empty() {
            self.update_store().await?;
        }
        if let Some(entry) = self.store.load().get(api_key) {
            Ok(serde_json::from_str(entry).map(Some)?)
        } else {
            Ok(None)
        }
    }

    /// Re-read the store file if it is due for a look and has changed since it was
    /// loaded.
    ///
    /// Fails only while no version of the store has ever been loaded. After that, a
    /// file that is missing or malformed is logged and the previous store kept, so a
    /// botched edit cannot lock every client out.
    pub async fn update_store(&self) -> anyhow::Result<()> {
        let loaded = {
            let mut file = self.file.lock().expect("api key file state lock");
            if file
                .checked_at
                .is_some_and(|at| at.elapsed() < self.refresh_interval)
            {
                return Ok(());
            }
            file.checked_at = Some(Instant::now());
            file.loaded
        };

        match self.reload_if_changed(loaded).await {
            Ok(Some(version)) => {
                self.file.lock().expect("api key file state lock").loaded = Some(version);
                Ok(())
            }
            Ok(None) => Ok(()),
            Err(e) if loaded.is_some() => {
                warn!(path = %self.store_path, "could not reload the api key store, keeping the previous one: {e}");
                Ok(())
            }
            Err(e) => {
                // Looking again on the next request beats waiting out the interval with
                // no keys at all.
                self.file
                    .lock()
                    .expect("api key file state lock")
                    .checked_at = None;
                warn!(path = %self.store_path, "could not load the api key store: {e}");
                anyhow::bail!("the api key store is unavailable")
            }
        }
    }

    /// The new file version, when the file differs from `loaded` and parsed.
    async fn reload_if_changed(
        &self,
        loaded: Option<(SystemTime, u64)>,
    ) -> anyhow::Result<Option<(SystemTime, u64)>> {
        let metadata = tokio::fs::metadata(&self.store_path).await?;
        let version = (metadata.modified()?, metadata.len());
        if loaded == Some(version) {
            return Ok(None);
        }
        let store = Self::load_store_from_json(&tokio::fs::read(&self.store_path).await?)?;
        self.store.store(Arc::new(store));
        Ok(Some(version))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claims_accept_the_usual_id_spellings_and_an_expiry() {
        for json in [
            r#"{"id": "svc"}"#,
            r#"{"sub": "svc"}"#,
            r#"{"user_id": "svc"}"#,
            r#"{"userId": "svc"}"#,
        ] {
            let claims: ApiKeyClaims = serde_json::from_str(json).unwrap();
            assert_eq!(claims.id, "svc", "{json}");
        }
        let claims: ApiKeyClaims =
            serde_json::from_str(r#"{"id": "svc", "exp": 100, "roles": ["A"]}"#).unwrap();
        assert_eq!(claims.expiration, Some(100));

        let principal = Principal::from(claims);
        assert_eq!(principal.id(), "svc");
        assert!(principal.has_role("A"));
        assert_eq!(principal.attribute::<u64>("expiration"), Some(100));
    }

    #[test]
    fn a_key_expires_at_its_expiration_second() {
        let at = |seconds| SystemTime::UNIX_EPOCH + Duration::from_secs(seconds);
        let claims = ApiKeyClaims {
            id: "svc".into(),
            expiration: Some(100),
            ..Default::default()
        };
        assert!(!claims.is_expired_at(at(99)));
        assert!(claims.is_expired_at(at(100)));

        let forever = ApiKeyClaims {
            id: "svc".into(),
            ..Default::default()
        };
        assert!(!forever.is_expired_at(at(u64::MAX / 2)));
    }
}
