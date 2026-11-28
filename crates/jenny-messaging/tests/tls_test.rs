//! `Nats` against a NATS that requires client certificates (ADR-031): mTLS connects and
//! serves a key-value store, a client it does not accept is refused with its reason, and
//! a rotated certificate is picked up without a restart — but only once it is complete.
//!
//! Needs `JENNY_TEST_NATS_TLS_URL` and `JENNY_TEST_NATS_TLS_DIR`, the directory
//! `scripts/nats-test-certs.sh` wrote. `just test-nats` starts the server and sets both;
//! CI does the same. Without them the tests say so and pass, except under `CI`.

use std::path::{Path, PathBuf};
use std::time::Duration;

use async_nats::connection::State;
use jenny_core::domain::Aggregate;
use jenny_core::port::Store;
use jenny_core::port::outbound::HealthCheck;
use jenny_messaging::NatsConnectionConfig;
use jenny_messaging::connection::NatsTlsConfig;
use jenny_messaging::kv::Nats;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct Session {
    id: String,
}

impl Aggregate for Session {
    type Id = String;
    const KIND: &'static str = "session";
    fn id(&self) -> String {
        self.id.clone()
    }
}

/// The server's URL and the certificates' directory, or `None` to skip.
fn server() -> Option<(String, PathBuf)> {
    match (
        std::env::var("JENNY_TEST_NATS_TLS_URL"),
        std::env::var("JENNY_TEST_NATS_TLS_DIR"),
    ) {
        (Ok(url), Ok(dir)) => Some((url, PathBuf::from(dir))),
        _ if std::env::var_os("CI").is_some() => {
            panic!("JENNY_TEST_NATS_TLS_URL and JENNY_TEST_NATS_TLS_DIR must be set in CI")
        }
        _ => {
            eprintln!("skipping: JENNY_TEST_NATS_TLS_URL / JENNY_TEST_NATS_TLS_DIR are not set");
            None
        }
    }
}

/// A connection presenting `<client>.crt` and `<client>.key` from `dir`.
fn config(url: &str, ca: &Path, client: Option<(&Path, &str)>) -> NatsConnectionConfig {
    let mut config = NatsConnectionConfig::new(url);
    config.refresh_interval = 1;
    config.tls = Some(NatsTlsConfig {
        ca: Some(ca.join("ca.crt")),
        cert: client.map(|(dir, name)| dir.join(format!("{name}.crt"))),
        key: client.map(|(dir, name)| dir.join(format!("{name}.key"))),
        ..NatsTlsConfig::default()
    });
    config
}

/// `client-a`'s files in a directory of the test's own, to rotate without disturbing the
/// other tests.
fn own_copy(dir: &Path) -> tempfile::TempDir {
    let own = tempfile::tempdir().unwrap();
    for file in ["client-a.crt", "client-a.key"] {
        std::fs::copy(dir.join(file), own.path().join(file)).unwrap();
    }
    own
}

async fn wait_for(what: &str, mut done: impl FnMut() -> bool) {
    for _ in 0..100 {
        if done() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("timed out waiting for {what}");
}

#[tokio::test]
async fn mtls_connects_and_serves_a_key_value_store() {
    let Some((url, dir)) = server() else { return };
    let prefix = format!("tls-{}", uuid::Uuid::new_v4().simple());
    let nats = Nats::new(config(&url, &dir, Some((&dir, "client-a"))), prefix.clone());
    let sessions = nats.kv::<Session>();
    nats.prepare().await.unwrap();

    let session = Session { id: "s-1".into() };
    sessions.insert(session.clone()).await.unwrap();
    assert_eq!(
        sessions
            .find(&"s-1".to_string())
            .await
            .unwrap()
            .unwrap()
            .into_inner(),
        session
    );
    nats.health_check().check().await.unwrap();

    async_nats::jetstream::new(nats.client().await.unwrap())
        .delete_key_value(format!("{prefix}-session"))
        .await
        .unwrap();
    nats.close().await;
}

#[tokio::test]
async fn a_client_the_server_does_not_accept_is_refused_with_its_reason() {
    let Some((url, dir)) = server() else { return };

    // No client certificate: `verify_and_map` requires one.
    let err = Nats::new(config(&url, &dir, None), "tls-refused")
        .prepare()
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("connecting to NATS"), "{err}");
    assert!(err.contains("CertificateRequired"), "{err}");

    // A certificate from a CA the server does not trust.
    let err = Nats::new(config(&url, &dir, Some((&dir, "other"))), "tls-refused")
        .prepare()
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("connecting to NATS"), "{err}");
    assert!(err.contains("UnknownCA"), "{err}");
}

#[tokio::test]
async fn a_rotated_certificate_reconnects_and_keeps_working() {
    let Some((url, dir)) = server() else { return };
    let own = own_copy(&dir);
    let nats = Nats::new(
        config(&url, &dir, Some((own.path(), "client-a"))),
        "tls-rotated",
    );
    let client = nats.client().await.unwrap();
    let connects = || {
        client
            .statistics()
            .connects
            .load(std::sync::atomic::Ordering::Relaxed)
    };
    assert_eq!(connects(), 1);

    // What cert-manager does, both halves.
    for half in ["crt", "key"] {
        std::fs::copy(
            dir.join(format!("client-b.{half}")),
            own.path().join(format!("client-a.{half}")),
        )
        .unwrap();
    }
    wait_for("a forced reconnect", || connects() >= 2).await;
    wait_for("the connection to be back", || {
        client.connection_state() == State::Connected
    })
    .await;
    client.flush().await.unwrap();
    nats.health_check().check().await.unwrap();
    nats.close().await;
}

#[tokio::test]
async fn half_a_rotation_leaves_the_connection_as_it_is() {
    let Some((url, dir)) = server() else { return };
    let own = own_copy(&dir);
    let nats = Nats::new(
        config(&url, &dir, Some((own.path(), "client-a"))),
        "tls-half",
    );
    let client = nats.client().await.unwrap();

    // Only the certificate: its key is still the old one, and a handshake would fail.
    std::fs::copy(dir.join("client-b.crt"), own.path().join("client-a.crt")).unwrap();
    // Three looks at a one-second interval.
    tokio::time::sleep(Duration::from_millis(3500)).await;
    assert_eq!(
        client
            .statistics()
            .connects
            .load(std::sync::atomic::Ordering::Relaxed),
        1,
        "no reconnect with a certificate and a key that do not belong together"
    );
    assert_eq!(client.connection_state(), State::Connected);
    client.flush().await.unwrap();
    nats.close().await;
}
