//! Provides authentication related functionality, such as session user representation and authentication middleware
use arc_swap::ArcSwap;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::Value;

use std::{
    collections::HashMap,
    hash::{Hash, Hasher},
    sync::Arc,
};

#[derive(Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
pub struct ApiKeyClaims {
    #[serde(alias = "id")]
    pub user_id: String,
    #[serde(default)]
    pub expiration: Option<u64>,
    #[serde(default, flatten)]
    pub attributes: HashMap<String, Value>,
}

/// This is a configuration type for for Api Key authentication.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct ApiKeyConfig {
    /// api key store file path
    pub path: String,
    /// load file once at startup and don't monitor for changes. Default: false
    #[serde(default)]
    pub static_load: bool,
}

impl ApiKeyConfig {
    pub fn auth_context_config(&self) -> anyhow::Result<ApiKeyAuthContextConfig> {
        if !self.static_load {
            Ok(ApiKeyAuthContextConfig::new_file_backed(&self.path))
        } else {
            let json_data = std::fs::read(&self.path)?;
            Ok(ApiKeyAuthContextConfig::new_static(
                ApiKeyAuthContextConfig::load_store_from_json(&json_data)?,
            ))
        }
    }
}

/// The API-key half of [`AuthenticationConfig`](crate::AuthenticationConfig).
///
/// Keys are stored hashed and looked up by the SHA-256 of what the caller presented, so
/// a leaked store does not leak usable keys. The store can either be loaded once at
/// startup or backed by a file that is re-read when it changes on disk — the latter lets
/// keys be rotated without a restart.
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
    store_file_metadata_hash: Arc<ArcSwap<u64>>,
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
            store_file_metadata_hash: Arc::new(ArcSwap::from_pointee(0)),
        }
    }

    /// Create new with static store data and no runtime refreshes
    pub fn new_static(store_data: HashMap<String, Box<str>>) -> Self {
        Self {
            store: Arc::new(ArcSwap::from_pointee(store_data)),
            store_path: String::new(),
            store_file_metadata_hash: Arc::new(ArcSwap::from_pointee(0)),
        }
    }

    async fn hash_store_file_metadata(&self) -> anyhow::Result<u64> {
        let metadata = tokio::fs::metadata(&self.store_path).await?;
        let modified = metadata
            .modified()?
            .duration_since(std::time::UNIX_EPOCH)?
            .as_secs();
        let len = metadata.len();
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        modified.hash(&mut hasher);
        len.hash(&mut hasher);
        Ok(hasher.finish())
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

    /// Atomically replaces the store with the provided one and update the store-file metadata hash.
    pub async fn update_store(&self) -> anyhow::Result<()> {
        let store_hash = self.hash_store_file_metadata().await?;
        if &store_hash != self.store_file_metadata_hash.load().as_ref() {
            let new_store = Self::load_store_from_json(&tokio::fs::read(&self.store_path).await?)?;
            self.store_file_metadata_hash.store(Arc::new(store_hash));
            self.store.store(Arc::new(new_store));
        }
        Ok(())
    }
}
