//! Object storage for jenny services: S3 and everything that speaks it, local files, or
//! memory, behind the [`Blobs`] port.
//!
//! [`ObjectBlobs`] implements the port over [`object_store`], the Apache Arrow project's
//! common interface to object stores, so one adapter serves AWS S3, MinIO, Garage,
//! SeaweedFS, Ceph RGW and Cloudflare R2, a local directory, and memory. It is configured
//! by a URL and `object_store`'s own option names ([`ObjectsConfig`]):
//!
//! ```yaml
//! objects:
//!   url: s3://attachments/notes        # bucket `attachments`, keys under `notes/`
//!   options:                           # better: JENNY__OBJECTS__OPTIONS__...
//!     endpoint: http://localhost:9000  # for anything that is not AWS itself
//!     region: us-east-1
//!     access_key_id: …
//!     secret_access_key: …
//!     allow_http: "true"
//!     conditional_put: etag            # only for a server that honours If-Match
//! ```
//!
//! The standard `AWS_*` environment variables apply too, beneath the options: on AWS a
//! service needs only the URL, and finds its role's credentials the way the AWS SDKs
//! do.
//!
//! Over S3, the conditional writes (`put_if_absent`, `put_if_match`) are off until
//! `conditional_put: etag` turns them on, and are refused until then. A server that
//! ignored their headers would overwrite instead, and nothing on this side could tell.
//!
//! `file:///var/lib/notes/blobs` keeps blobs in a directory and `memory://` in the
//! process, for development and tests; neither hands out signed URLs.
//!
//! The decisions — the port's shape, conditional writes, readiness — are ADR-018.
//!
//! [`Blobs`]: jenny_core::port::Blobs

use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures::TryStreamExt;
use jenny_core::domain::{DomainError, DomainResult};
use jenny_core::port::blobs::{Access, Blob, BlobInfo, Blobs, check_key};
use jenny_core::port::outbound::HealthCheck;
use object_store::aws::{AmazonS3Builder, AmazonS3ConfigKey, S3ConditionalPut};
use object_store::local::LocalFileSystem;
use object_store::memory::InMemory;
use object_store::path::Path;
use object_store::signer::Signer;
use object_store::{
    Attribute, AttributeValue, Attributes, ObjectMeta, ObjectStore, ObjectStoreExt, PutMode,
    PutOptions, PutPayload, UpdateVersion,
};
use serde::Deserialize;

/// The `objects` configuration section: where blobs live, and how to reach it.
#[derive(Clone, Default, Deserialize)]
#[serde(default)]
pub struct ObjectsConfig {
    /// `s3://bucket[/prefix]`, `file:///absolute/directory` or `memory://`. Without one
    /// there is no object store.
    pub url: Option<String>,
    /// `object_store`'s options for the URL's backend, by their documented names —
    /// `endpoint`, `region`, `access_key_id`, `secret_access_key`, `allow_http`,
    /// `conditional_put` (`etag` turns the conditional writes on; jenny's default is
    /// `disabled`) for S3. Carries secrets, so it is never logged.
    pub options: BTreeMap<String, String>,
}

impl fmt::Debug for ObjectsConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ObjectsConfig")
            .field("url", &self.url)
            .field("options", &self.options.keys().collect::<Vec<_>>())
            .finish()
    }
}

/// [`Blobs`] over any [`object_store`] backend.
///
/// The conditional writes map onto `object_store`'s put modes: `put_if_absent` is
/// `PutMode::Create`, `put_if_match` is `PutMode::Update` with the entity tag. On S3
/// they are `If-None-Match: *` and `If-Match` headers. A server that ignores them would
/// write unconditionally, and nothing on this side can tell, so they are off unless
/// `conditional_put: etag` (in the options, or `AWS_CONDITIONAL_PUT`) turns them on.
/// Until then they are refused with an internal error. AWS S3 and the compatible
/// servers jenny is tested against honour them. Local files support `put_if_absent`
/// only, and memory both.
pub struct ObjectBlobs {
    store: Arc<dyn ObjectStore>,
    signer: Option<Arc<dyn Signer>>,
    /// Joined in front of every key, from an `s3://bucket/prefix` URL.
    prefix: Option<String>,
    describe: String,
}

impl ObjectBlobs {
    /// The store `config.url` names. Fails when there is no URL, or the URL or an
    /// option does not parse.
    pub fn from_config(config: &ObjectsConfig) -> anyhow::Result<Self> {
        let url = config
            .url
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("objects.url is not set"))?;
        Self::from_url(url, &config.options)
    }

    /// The store `url` names, configured with `options`.
    pub fn from_url(url: &str, options: &BTreeMap<String, String>) -> anyhow::Result<Self> {
        if let Some(rest) = url.strip_prefix("s3://") {
            let (bucket, prefix) = rest.split_once('/').unwrap_or((rest, ""));
            // The standard `AWS_*` variables first, so a service on AWS needs no keys
            // in its configuration: its role (IRSA, ECS, instance profile) is found the
            // way every AWS SDK finds it. `objects.options` override them.
            let mut builder = AmazonS3Builder::from_env().with_bucket_name(bucket);
            // Conditional writes are off until a service asks for them (ADR-018):
            // `object_store` would send the headers by default, and a server that ignores
            // them overwrites without anyone noticing.
            if std::env::var_os("AWS_CONDITIONAL_PUT").is_none() {
                builder = builder.with_conditional_put(S3ConditionalPut::Disabled);
            }
            for (key, value) in options {
                let key: AmazonS3ConfigKey = key
                    .parse()
                    .map_err(|e| anyhow::anyhow!("objects.options.{key}: {e}"))?;
                builder = builder.with_config(key, value);
            }
            let s3 = Arc::new(builder.build()?);
            let prefix = prefix.trim_matches('/');
            return Ok(Self {
                store: s3.clone(),
                signer: Some(s3),
                prefix: (!prefix.is_empty()).then(|| prefix.to_string()),
                describe: format!("s3://{bucket}"),
            });
        }
        if let Some(directory) = url.strip_prefix("file://") {
            std::fs::create_dir_all(directory)
                .map_err(|e| anyhow::anyhow!("creating blob directory `{directory}`: {e}"))?;
            return Ok(Self::new(
                Arc::new(LocalFileSystem::new_with_prefix(directory)?),
                format!("file://{directory}"),
            ));
        }
        if url == "memory://" || url == "memory:" {
            return Ok(Self::new(Arc::new(InMemory::new()), "memory://"));
        }
        anyhow::bail!("objects.url `{url}` is not s3://, file:// or memory://")
    }

    /// Any `object_store` backend you built yourself. It hands out no signed URLs.
    pub fn new(store: Arc<dyn ObjectStore>, describe: impl Into<String>) -> Self {
        Self {
            store,
            signer: None,
            prefix: None,
            describe: describe.into(),
        }
    }

    /// Readiness: the store answers a request.
    pub fn health_check(self: &Arc<Self>) -> ObjectsCheck {
        ObjectsCheck(self.clone())
    }

    /// A key as a path in the store, behind the configured prefix.
    fn path(&self, key: &str) -> DomainResult<Path> {
        check_key(key)?;
        let full = match &self.prefix {
            Some(prefix) => format!("{prefix}/{key}"),
            None => key.to_string(),
        };
        Path::parse(&full).map_err(|e| DomainError::invalid(format!("`{key}`: {e}")))
    }

    /// The key a path in the store stands for.
    fn key(&self, location: &Path) -> String {
        let location = location.as_ref();
        match &self.prefix {
            Some(prefix) => location
                .strip_prefix(prefix.as_str())
                .map(|rest| rest.trim_start_matches('/'))
                .unwrap_or(location)
                .to_string(),
            None => location.to_string(),
        }
    }

    fn info(&self, meta: &ObjectMeta) -> BlobInfo {
        BlobInfo {
            key: self.key(&meta.location),
            size: meta.size,
            etag: meta.e_tag.clone(),
            modified: meta.last_modified,
        }
    }

    async fn write(&self, key: &str, blob: Blob, mode: PutMode) -> DomainResult<BlobInfo> {
        let path = self.path(key)?;
        let mut attributes = Attributes::new();
        if let Some(content_type) = blob.content_type() {
            attributes.insert(
                Attribute::ContentType,
                AttributeValue::from(content_type.to_string()),
            );
        }
        let size = blob.bytes().len() as u64;
        let written = self
            .store
            .put_opts(
                &path,
                PutPayload::from(blob.into_bytes()),
                PutOptions {
                    mode,
                    attributes,
                    ..Default::default()
                },
            )
            .await
            .map_err(|e| self.failure(key, e))?;
        Ok(BlobInfo {
            key: key.to_string(),
            size,
            etag: written.e_tag,
            modified: chrono::Utc::now(),
        })
    }

    /// An `object_store` error, phrased as a domain failure.
    fn failure(&self, key: &str, error: object_store::Error) -> DomainError {
        use object_store::Error;
        match error {
            Error::NotFound { .. } => DomainError::not_found(),
            Error::AlreadyExists { .. } => {
                DomainError::conflict(format!("a blob `{key}` already exists"))
            }
            // Also what an `If-Match` write to a missing key gets: gone is as stale as
            // changed, as in HTTP.
            Error::Precondition { .. } => DomainError::stale(format!(
                "blob `{key}` changed, or is gone, since the entity tag it was read with"
            )),
            Error::NotImplemented { .. } | Error::NotSupported { .. } => {
                DomainError::internal(format!(
                    "{} does not write `{key}` conditionally ({error}): over S3 that \
                     takes `objects.options.conditional_put: etag`, for a server that \
                     honours If-None-Match and If-Match; a directory has \
                     `put_if_absent` only",
                    self.describe
                ))
            }
            other => DomainError::internal(format!("{}: {other}", self.describe)),
        }
    }
}

#[async_trait]
impl Blobs for ObjectBlobs {
    async fn put(&self, key: &str, blob: Blob) -> DomainResult<BlobInfo> {
        self.write(key, blob, PutMode::Overwrite).await
    }

    async fn put_if_absent(&self, key: &str, blob: Blob) -> DomainResult<BlobInfo> {
        self.write(key, blob, PutMode::Create).await
    }

    async fn put_if_match(&self, key: &str, blob: Blob, etag: &str) -> DomainResult<BlobInfo> {
        let version = UpdateVersion {
            e_tag: Some(etag.to_string()),
            version: None,
        };
        self.write(key, blob, PutMode::Update(version)).await
    }

    async fn get(&self, key: &str) -> DomainResult<Option<(Blob, BlobInfo)>> {
        let path = self.path(key)?;
        let result = match self.store.get(&path).await {
            Ok(result) => result,
            Err(object_store::Error::NotFound { .. }) => return Ok(None),
            Err(e) => return Err(self.failure(key, e)),
        };
        let info = self.info(&result.meta);
        let content_type = result
            .attributes
            .get(&Attribute::ContentType)
            .map(|value| value.to_string());
        let bytes = result.bytes().await.map_err(|e| self.failure(key, e))?;
        let mut blob = Blob::new(bytes);
        if let Some(content_type) = content_type {
            blob = blob.with_content_type(content_type);
        }
        Ok(Some((blob, info)))
    }

    async fn head(&self, key: &str) -> DomainResult<Option<BlobInfo>> {
        let path = self.path(key)?;
        match self.store.head(&path).await {
            Ok(meta) => Ok(Some(self.info(&meta))),
            Err(object_store::Error::NotFound { .. }) => Ok(None),
            Err(e) => Err(self.failure(key, e)),
        }
    }

    async fn delete(&self, key: &str) -> DomainResult<()> {
        let path = self.path(key)?;
        match self.store.delete(&path).await {
            Ok(()) | Err(object_store::Error::NotFound { .. }) => Ok(()),
            Err(e) => Err(self.failure(key, e)),
        }
    }

    async fn list(&self, prefix: &str) -> DomainResult<Vec<BlobInfo>> {
        let prefix = prefix.trim_matches('/');
        let under = match (&self.prefix, prefix) {
            (None, "") => None,
            (Some(own), "") => Some(own.clone()),
            (None, prefix) => Some(prefix.to_string()),
            (Some(own), prefix) => Some(format!("{own}/{prefix}")),
        };
        let under = under
            .map(|under| {
                Path::parse(&under).map_err(|e| DomainError::invalid(format!("`{prefix}`: {e}")))
            })
            .transpose()?;
        let metas: Vec<ObjectMeta> = self
            .store
            .list(under.as_ref())
            .try_collect()
            .await
            .map_err(|e| self.failure(prefix, e))?;
        let mut infos: Vec<BlobInfo> = metas.iter().map(|meta| self.info(meta)).collect();
        infos.sort_by(|a, b| a.key.cmp(&b.key));
        Ok(infos)
    }

    async fn signed_url(
        &self,
        key: &str,
        access: Access,
        valid_for: Duration,
    ) -> DomainResult<String> {
        let path = self.path(key)?;
        let signer = self.signer.as_ref().ok_or_else(|| {
            DomainError::internal(format!(
                "{} hands out no signed URLs; they need an S3-compatible store",
                self.describe
            ))
        })?;
        let method = match access {
            Access::Read => http::Method::GET,
            Access::Write => http::Method::PUT,
        };
        signer
            .signed_url(method, &path, valid_for)
            .await
            .map(|url| url.to_string())
            .map_err(|e| self.failure(key, e))
    }
}

impl fmt::Debug for ObjectBlobs {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ObjectBlobs")
            .field("store", &self.describe)
            .field("prefix", &self.prefix)
            .finish_non_exhaustive()
    }
}

/// Readiness for an [`ObjectBlobs`]: the bucket answers a listing.
///
/// A listing, not a `HEAD`: S3 answers a `HEAD` in a missing bucket with the same 404 as
/// for a missing key, which would report a store with no bucket as ready. It lists under
/// a path nothing is stored at, so it is one small request however full the bucket is.
pub struct ObjectsCheck(Arc<ObjectBlobs>);

#[async_trait]
impl HealthCheck for ObjectsCheck {
    fn name(&self) -> &str {
        "objects"
    }

    fn kind(&self) -> &str {
        "storage"
    }

    async fn check(&self) -> Result<(), String> {
        let path = self.0.path(".jenny-health").map_err(|e| e.to_string())?;
        match self.0.store.list_with_delimiter(Some(&path)).await {
            Ok(_) => Ok(()),
            Err(e) => Err(format!("{}: {e}", self.0.describe)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn local_files_keep_the_contract_except_signed_urls() {
        let directory = tempfile::tempdir().unwrap();
        let url = format!("file://{}", directory.path().join("blobs").display());
        let blobs = Arc::new(ObjectBlobs::from_url(&url, &BTreeMap::new()).unwrap());
        assert_eq!(blobs.health_check().check().await, Ok(()));

        let first = blobs
            .put_if_absent("a/b.txt", Blob::new(b"one".to_vec()))
            .await
            .unwrap();
        assert!(matches!(
            blobs
                .put_if_absent("a/b.txt", Blob::new(b"two".to_vec()))
                .await
                .unwrap_err(),
            DomainError::Conflict { .. }
        ));
        assert_eq!(first.size, 3);
        let (blob, _) = blobs.get("a/b.txt").await.unwrap().unwrap();
        assert_eq!(blob.bytes().as_ref(), b"one");
        assert_eq!(blobs.list("a").await.unwrap().len(), 1);
        assert!(
            blobs
                .signed_url("a/b.txt", Access::Read, Duration::from_secs(1))
                .await
                .is_err()
        );
        blobs.delete("a/b.txt").await.unwrap();
        assert!(blobs.get("a/b.txt").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn memory_answers_readiness_and_conditional_writes() {
        let blobs = Arc::new(ObjectBlobs::from_url("memory://", &BTreeMap::new()).unwrap());
        assert_eq!(blobs.health_check().check().await, Ok(()));
        let first = blobs.put("k", Blob::new(b"1".to_vec())).await.unwrap();
        let etag = first.etag.unwrap();
        blobs
            .put_if_match("k", Blob::new(b"2".to_vec()), &etag)
            .await
            .unwrap();
        assert!(matches!(
            blobs
                .put_if_match("k", Blob::new(b"3".to_vec()), &etag)
                .await
                .unwrap_err(),
            DomainError::Stale { .. }
        ));
        // A missing key is stale too, the same answer `MemoryBlobs` gives.
        assert!(matches!(
            blobs
                .put_if_match("gone", Blob::new(b"3".to_vec()), &etag)
                .await
                .unwrap_err(),
            DomainError::Stale { .. }
        ));
    }

    #[tokio::test]
    async fn conditional_writes_over_s3_are_refused_until_turned_on() {
        let mut options: BTreeMap<String, String> = [
            ("endpoint".to_string(), "http://127.0.0.1:1".to_string()),
            ("allow_http".to_string(), "true".to_string()),
            ("region".to_string(), "us-east-1".to_string()),
            ("access_key_id".to_string(), "k".to_string()),
            ("secret_access_key".to_string(), "s".to_string()),
        ]
        .into();
        // Off by jenny's default, and off when said so.
        for conditional_put in [None, Some("disabled")] {
            if let Some(value) = conditional_put {
                options.insert("conditional_put".to_string(), value.to_string());
            }
            let blobs = ObjectBlobs::from_url("s3://bucket", &options).unwrap();
            // Refused before any request: the endpoint is unreachable, so a write would
            // fail differently.
            let err = blobs
                .put_if_absent("k", Blob::new(b"1".to_vec()))
                .await
                .unwrap_err();
            assert!(
                matches!(&err, DomainError::Internal { detail, .. } if detail.contains("conditional_put: etag")),
                "{conditional_put:?}: {err:?}"
            );
            let err = blobs
                .put_if_match("k", Blob::new(b"1".to_vec()), "\"1\"")
                .await
                .unwrap_err();
            assert!(
                matches!(err, DomainError::Internal { .. }),
                "{conditional_put:?}: {err:?}"
            );
        }
        // `etag` is what turns them on; the S3 tests show it against a server.
        options.insert("conditional_put".to_string(), "etag".to_string());
        assert!(ObjectBlobs::from_url("s3://bucket", &options).is_ok());
    }

    #[test]
    fn secrets_in_the_options_never_reach_a_log() {
        let config = ObjectsConfig {
            url: Some("s3://bucket".into()),
            options: [("secret_access_key".to_string(), "hunter2".to_string())].into(),
        };
        let debug = format!("{config:?}");
        assert!(!debug.contains("hunter2"), "{debug}");
        assert!(debug.contains("secret_access_key"));
    }

    #[test]
    fn a_url_that_names_no_backend_is_refused() {
        let err = ObjectBlobs::from_url("ftp://somewhere", &BTreeMap::new()).unwrap_err();
        assert!(err.to_string().contains("s3://"), "{err}");
        let err = ObjectBlobs::from_url(
            "s3://bucket",
            &[("no_such_option".to_string(), "x".to_string())].into(),
        )
        .unwrap_err();
        assert!(err.to_string().contains("no_such_option"), "{err}");
    }

    #[test]
    fn a_prefix_is_joined_in_front_and_stripped_on_the_way_back() {
        let blobs = ObjectBlobs::from_url(
            "s3://bucket/tenant-1/",
            &[
                ("region".to_string(), "us-east-1".to_string()),
                ("access_key_id".to_string(), "x".to_string()),
                ("secret_access_key".to_string(), "y".to_string()),
            ]
            .into(),
        )
        .unwrap();
        let path = blobs.path("avatars/1.png").unwrap();
        assert_eq!(path.as_ref(), "tenant-1/avatars/1.png");
        assert_eq!(blobs.key(&path), "avatars/1.png");
    }
}
