//! The object-storage port: named blobs of bytes, written conditionally if asked, and
//! handed out by signed URL.

use std::fmt;
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use chrono::{DateTime, Utc};

use crate::domain::{DomainError, DomainResult};

/// What an object store holds under a key: bytes, and what they are.
#[derive(Clone, PartialEq, Eq)]
pub struct Blob {
    bytes: Bytes,
    content_type: Option<String>,
}

impl Blob {
    pub fn new(bytes: impl Into<Bytes>) -> Self {
        Self {
            bytes: bytes.into(),
            content_type: None,
        }
    }

    /// The media type to store with it — `image/png` — and serve it as.
    pub fn with_content_type(mut self, content_type: impl Into<String>) -> Self {
        self.content_type = Some(content_type.into());
        self
    }

    pub fn bytes(&self) -> &Bytes {
        &self.bytes
    }

    pub fn into_bytes(self) -> Bytes {
        self.bytes
    }

    pub fn content_type(&self) -> Option<&str> {
        self.content_type.as_deref()
    }
}

impl fmt::Debug for Blob {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Blob")
            .field("size", &self.bytes.len())
            .field("content_type", &self.content_type)
            .finish()
    }
}

/// What a store knows about a blob without reading it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BlobInfo {
    pub key: String,
    pub size: u64,
    /// Changes with every write; what [`Blobs::put_if_match`] compares. Some stores
    /// report none for a blob written without one.
    pub etag: Option<String>,
    pub modified: DateTime<Utc>,
}

/// What a [signed URL](Blobs::signed_url) lets its holder do.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Access {
    /// `GET` the blob.
    Read,
    /// `PUT` a new blob under the key.
    Write,
}

/// Where blobs live: an S3 bucket, a directory, memory.
///
/// Files, images, exports, anything too big or too opaque for an aggregate. The port is
/// small on purpose — `object_store` has shown that this much is common to every object
/// store worth supporting (ADR-013) — and in-memory, so a large blob is read and written
/// whole.
///
/// Keys are paths: segments separated by `/`, none empty, `.` or `..`.
/// [`list`](Self::list) takes a path too, and lists what is under it, not every key that
/// happens to start with the same characters.
///
/// The conditional writes are what make blobs safe to share: `put_if_absent` for "create,
/// but never clobber", `put_if_match` for "replace what I read, unless someone replaced
/// it first". Not every store can do them — see the adapter — and one that cannot says
/// so rather than writing unconditionally.
#[async_trait]
pub trait Blobs: Send + Sync + 'static {
    /// Write `blob` under `key`, replacing whatever is there.
    async fn put(&self, key: &str, blob: Blob) -> DomainResult<BlobInfo>;

    /// Write only if nothing is stored under `key`;
    /// [`Conflict`](DomainError::Conflict) otherwise.
    async fn put_if_absent(&self, key: &str, blob: Blob) -> DomainResult<BlobInfo>;

    /// Write only if what is stored under `key` still has `etag`;
    /// [`Stale`](DomainError::Stale) when it changed, [`NotFound`](DomainError::NotFound)
    /// when it is gone.
    async fn put_if_match(&self, key: &str, blob: Blob, etag: &str) -> DomainResult<BlobInfo>;

    /// The blob under `key`, or `None`.
    async fn get(&self, key: &str) -> DomainResult<Option<(Blob, BlobInfo)>>;

    /// What is known about the blob under `key`, without reading it.
    async fn head(&self, key: &str) -> DomainResult<Option<BlobInfo>>;

    /// Remove the blob under `key`. Removing what is not there is not an error, so a
    /// retried delete succeeds.
    async fn delete(&self, key: &str) -> DomainResult<()>;

    /// Every blob under the path `prefix`, in key order; `""` lists everything.
    async fn list(&self, prefix: &str) -> DomainResult<Vec<BlobInfo>>;

    /// A URL that lets whoever holds it read or write `key` for `valid_for`, without
    /// credentials — so a browser uploads or downloads straight from the store instead
    /// of through the service.
    async fn signed_url(
        &self,
        key: &str,
        access: Access,
        valid_for: Duration,
    ) -> DomainResult<String>;
}

/// Refuse a key no store would take the same way: empty segments, `.` and `..`, control
/// characters. For adapters and the in-memory store, so every store refuses alike.
pub fn check_key(key: &str) -> DomainResult<()> {
    let valid = !key.is_empty()
        && key
            .split('/')
            .all(|segment| !segment.is_empty() && segment != "." && segment != "..")
        && !key.chars().any(char::is_control);
    if valid {
        Ok(())
    } else {
        Err(DomainError::invalid(format!(
            "`{key}` is not a blob key: use `/`-separated segments, none empty, `.` or `..`"
        )))
    }
}

/// Whether `key` lies under the path `prefix`.
pub fn is_under(key: &str, prefix: &str) -> bool {
    let prefix = prefix.trim_matches('/');
    prefix.is_empty() || key == prefix || key.starts_with(&format!("{prefix}/"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_are_paths_of_real_segments() {
        for good in ["a", "notes/7/attachment.pdf", "a b/ü"] {
            assert!(check_key(good).is_ok(), "{good}");
        }
        for bad in ["", "/a", "a/", "a//b", "a/./b", "../a", "a\nb"] {
            assert!(check_key(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn under_means_within_the_path_not_sharing_a_prefix() {
        assert!(is_under("avatars/1.png", "avatars"));
        assert!(is_under("avatars/1.png", "avatars/"));
        assert!(is_under("anything", ""));
        assert!(!is_under("avatarsx/1.png", "avatars"));
    }
}
