//! `ObjectBlobs` against a real S3-compatible server: the whole `Blobs` contract,
//! conditional writes, prefixes and signed URLs.
//!
//! Needs `JENNY_TEST_S3_URL` (`s3://bucket`, which must exist) and the standard `AWS_*`
//! variables for the endpoint and credentials — the same way a deployed service gets
//! them. `just test-objects` starts a disposable server and sets them all; CI runs one.
//! Without them the tests say so and pass, except under `CI`, where a missing server is
//! a failure rather than a skip.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use jenny_core::domain::DomainError;
use jenny_core::port::outbound::HealthCheck;
use jenny_core::port::{Access, Blob, Blobs};
use jenny_objects::ObjectBlobs;

fn bucket_url() -> Option<String> {
    match std::env::var("JENNY_TEST_S3_URL") {
        Ok(url) => Some(url),
        Err(_) if std::env::var_os("CI").is_some() => {
            panic!(
                "JENNY_TEST_S3_URL must be set in CI; these tests would otherwise pass without running"
            )
        }
        Err(_) => {
            eprintln!("skipping: JENNY_TEST_S3_URL is not set (try `just test-objects`)");
            None
        }
    }
}

/// A store under a prefix of its own, emptied afterwards.
struct Fixture {
    blobs: ObjectBlobs,
}

impl Fixture {
    fn new() -> Option<Self> {
        Self::with(options())
    }

    fn with(options: BTreeMap<String, String>) -> Option<Self> {
        let url = bucket_url()?;
        let prefix = format!("jenny-test-{}", uuid::Uuid::new_v4().simple());
        let blobs = ObjectBlobs::from_url(&format!("{url}/{prefix}"), &options).unwrap();
        Some(Self { blobs })
    }

    async fn clean(self) {
        for info in self.blobs.list("").await.unwrap() {
            self.blobs.delete(&info.key).await.unwrap();
        }
        assert!(self.blobs.list("").await.unwrap().is_empty());
    }
}

/// Nothing but what `AWS_*` supplies, as for a deployed service: conditional writes
/// are off.
fn options() -> BTreeMap<String, String> {
    BTreeMap::new()
}

#[tokio::test]
async fn blobs_round_trip_with_their_type_and_list_by_path() {
    let Some(fixture) = Fixture::new() else {
        return;
    };
    let blobs = &fixture.blobs;

    let written = blobs
        .put(
            "avatars/1.png",
            Blob::new(b"\x89PNG".to_vec()).with_content_type("image/png"),
        )
        .await
        .unwrap();
    assert_eq!(written.key, "avatars/1.png");
    assert_eq!(written.size, 4);
    assert!(written.etag.is_some());

    let (blob, info) = blobs.get("avatars/1.png").await.unwrap().unwrap();
    assert_eq!(blob.bytes().as_ref(), b"\x89PNG");
    assert_eq!(blob.content_type(), Some("image/png"));
    assert_eq!(info.etag, written.etag);
    assert_eq!(blobs.head("avatars/1.png").await.unwrap().unwrap().size, 4);

    blobs
        .put("avatars/2.png", Blob::new(b"2".to_vec()))
        .await
        .unwrap();
    blobs
        .put("avatarsx/3.png", Blob::new(b"3".to_vec()))
        .await
        .unwrap();
    let keys = |infos: Vec<jenny_core::port::BlobInfo>| -> Vec<String> {
        infos.into_iter().map(|info| info.key).collect()
    };
    assert_eq!(
        keys(blobs.list("avatars").await.unwrap()),
        ["avatars/1.png", "avatars/2.png"]
    );
    assert_eq!(blobs.list("").await.unwrap().len(), 3);

    blobs.delete("avatars/1.png").await.unwrap();
    blobs.delete("avatars/1.png").await.unwrap();
    assert!(blobs.get("avatars/1.png").await.unwrap().is_none());
    assert!(blobs.head("avatars/1.png").await.unwrap().is_none());

    fixture.clean().await;
}

#[tokio::test]
async fn conditional_writes_are_refused_until_turned_on() {
    let Some(fixture) = Fixture::new() else {
        return;
    };
    let blobs = &fixture.blobs;

    let err = blobs
        .put_if_absent("report.csv", Blob::new(b"a,b".to_vec()))
        .await
        .unwrap_err();
    assert!(matches!(err, DomainError::Internal { .. }), "{err:?}");
    let written = blobs
        .put("report.csv", Blob::new(b"a,b".to_vec()))
        .await
        .unwrap();
    let err = blobs
        .put_if_match(
            "report.csv",
            Blob::new(b"c,d".to_vec()),
            &written.etag.unwrap(),
        )
        .await
        .unwrap_err();
    assert!(matches!(err, DomainError::Internal { .. }), "{err:?}");
    let (blob, _) = blobs.get("report.csv").await.unwrap().unwrap();
    assert_eq!(blob.bytes().as_ref(), b"a,b");

    fixture.clean().await;
}

/// RustFS honours `If-None-Match` and `If-Match`, so they may be turned on.
#[tokio::test]
async fn conditional_writes_refuse_rather_than_overwrite() {
    let Some(fixture) = Fixture::with([("conditional_put".into(), "etag".into())].into()) else {
        return;
    };
    let blobs = &fixture.blobs;

    let first = blobs
        .put_if_absent("report.csv", Blob::new(b"a,b".to_vec()))
        .await
        .unwrap();
    let err = blobs
        .put_if_absent("report.csv", Blob::new(b"c,d".to_vec()))
        .await
        .unwrap_err();
    assert!(matches!(err, DomainError::Conflict { .. }), "{err:?}");

    let etag = first.etag.unwrap();
    let second = blobs
        .put_if_match("report.csv", Blob::new(b"e,f".to_vec()), &etag)
        .await
        .unwrap();
    assert_ne!(second.etag.as_deref(), Some(etag.as_str()));
    let err = blobs
        .put_if_match("report.csv", Blob::new(b"g,h".to_vec()), &etag)
        .await
        .unwrap_err();
    assert!(matches!(err, DomainError::Stale { .. }), "{err:?}");
    // S3 answers an `If-Match` write to a missing key with 404; that is stale too.
    let err = blobs
        .put_if_match("missing.csv", Blob::new(b"i,j".to_vec()), &etag)
        .await
        .unwrap_err();
    assert!(matches!(err, DomainError::Stale { .. }), "{err:?}");
    assert!(blobs.head("missing.csv").await.unwrap().is_none());

    let (blob, _) = blobs.get("report.csv").await.unwrap().unwrap();
    assert_eq!(blob.bytes().as_ref(), b"e,f");

    fixture.clean().await;
}

#[tokio::test]
async fn signed_urls_let_a_client_skip_the_service() {
    let Some(fixture) = Fixture::new() else {
        return;
    };
    let blobs = &fixture.blobs;
    let http = reqwest::Client::new();

    let upload = blobs
        .signed_url("uploads/photo.jpg", Access::Write, Duration::from_secs(60))
        .await
        .unwrap();
    let response = http.put(&upload).body("jpeg bytes").send().await.unwrap();
    assert!(response.status().is_success(), "{}", response.status());
    let (blob, _) = blobs.get("uploads/photo.jpg").await.unwrap().unwrap();
    assert_eq!(blob.bytes().as_ref(), b"jpeg bytes");

    let download = blobs
        .signed_url("uploads/photo.jpg", Access::Read, Duration::from_secs(60))
        .await
        .unwrap();
    let response = http.get(&download).send().await.unwrap();
    assert!(response.status().is_success(), "{}", response.status());
    assert_eq!(response.text().await.unwrap(), "jpeg bytes");

    // The signature is what let it through: the same URL without it is refused.
    let unsigned = download.split('?').next().unwrap();
    let response = http.get(unsigned).send().await.unwrap();
    assert!(response.status().is_client_error(), "{}", response.status());

    fixture.clean().await;
}

#[tokio::test]
async fn prefixes_keep_two_stores_in_one_bucket_apart() {
    let Some(left) = Fixture::new() else { return };
    let right = Fixture::new().unwrap();

    left.blobs
        .put("same/key", Blob::new(b"left".to_vec()))
        .await
        .unwrap();
    right
        .blobs
        .put("same/key", Blob::new(b"right".to_vec()))
        .await
        .unwrap();

    let (blob, _) = left.blobs.get("same/key").await.unwrap().unwrap();
    assert_eq!(blob.bytes().as_ref(), b"left");
    let listed = right.blobs.list("").await.unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].key, "same/key");

    left.clean().await;
    right.clean().await;
}

#[tokio::test]
async fn readiness_needs_the_bucket_not_just_the_server() {
    let Some(url) = bucket_url() else { return };
    let present = Arc::new(ObjectBlobs::from_url(&url, &options()).unwrap());
    assert_eq!(present.health_check().check().await, Ok(()));

    let missing = Arc::new(
        ObjectBlobs::from_url(
            &format!("{url}-missing-{}", uuid::Uuid::new_v4().simple()),
            &options(),
        )
        .unwrap(),
    );
    assert!(missing.health_check().check().await.is_err());
}
