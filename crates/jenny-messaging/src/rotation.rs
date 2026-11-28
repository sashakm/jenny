//! Watching the files a NATS connection names, and forcing a reconnect once they have
//! rotated (ADR-031).
//!
//! The client reads its certificates and `.creds` again at each connect, but an open
//! connection keeps what it was opened with. So a task looks at the files' modification
//! time and length every `refresh_interval` seconds, the way the API-key store does, and
//! calls [`Client::force_reconnect`] when they changed and are complete again. The
//! metadata follows symlinks, so a Kubernetes secret mount swapping its `..data` link is
//! seen too.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use async_nats::Client;
use async_nats::rustls::crypto::CryptoProvider;
use async_nats::rustls::pki_types::pem::PemObject;
use async_nats::rustls::pki_types::{CertificateDer, PrivateKeyDer};
use async_nats::rustls::sign::CertifiedKey;
use async_nats::rustls::{RootCertStore, crypto};
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};

/// The files a connection reads at each connect.
#[derive(Clone, Debug, Default)]
pub(crate) struct Files {
    pub(crate) ca: Option<PathBuf>,
    /// A client certificate and its key.
    pub(crate) cert: Option<(PathBuf, PathBuf)>,
    pub(crate) creds: Option<PathBuf>,
}

/// What a file looked like: its modification time and length, or nothing when it could
/// not be read.
type Fingerprint = Vec<Option<(SystemTime, u64)>>;

impl Files {
    fn paths(&self) -> Vec<&Path> {
        let mut paths = Vec::new();
        paths.extend(self.ca.as_deref());
        if let Some((cert, key)) = &self.cert {
            paths.extend([cert.as_path(), key.as_path()]);
        }
        paths.extend(self.creds.as_deref());
        paths
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.paths().is_empty()
    }

    pub(crate) async fn fingerprint(&self) -> Fingerprint {
        let mut fingerprint = Vec::new();
        for path in self.paths() {
            // `metadata` follows symlinks: a swapped `..data` link shows up here.
            let metadata = tokio::fs::metadata(path).await.ok();
            fingerprint.push(
                metadata.and_then(|metadata| Some((metadata.modified().ok()?, metadata.len()))),
            );
        }
        fingerprint
    }

    /// Whether the files are complete enough to connect with: the certificate and the key
    /// parse and belong together, the CA parses, and the `.creds` file holds a user JWT
    /// and an NKey seed.
    pub(crate) async fn check(&self) -> Result<(), String> {
        if let Some(ca) = &self.ca {
            let certificates = certificates(ca).await?;
            let mut roots = RootCertStore::empty();
            for certificate in certificates {
                roots
                    .add(certificate)
                    .map_err(|e| format!("`{}` is not a CA certificate: {e}", ca.display()))?;
            }
        }
        if let Some((cert, key)) = &self.cert {
            let chain = certificates(cert).await?;
            let bytes = read(key).await?;
            let private = PrivateKeyDer::from_pem_slice(&bytes)
                .map_err(|e| format!("`{}` holds no private key: {e}", key.display()))?;
            CertifiedKey::from_der(chain, private, &provider()).map_err(|e| {
                format!(
                    "the key `{}` does not belong to the certificate `{}`: {e}",
                    key.display(),
                    cert.display()
                )
            })?;
        }
        if let Some(creds) = &self.creds {
            let bytes = read(creds).await?;
            let contents = String::from_utf8_lossy(&bytes);
            crate::connection::parse_creds(&contents)
                .map_err(|e| format!("the credentials `{}`: {e}", creds.display()))?;
        }
        Ok(())
    }
}

/// The provider the client's TLS uses, for loading the key the way the handshake will.
fn provider() -> Arc<CryptoProvider> {
    CryptoProvider::get_default()
        .cloned()
        .unwrap_or_else(|| Arc::new(crypto::ring::default_provider()))
}

async fn read(path: &Path) -> Result<Vec<u8>, String> {
    tokio::fs::read(path)
        .await
        .map_err(|e| format!("reading `{}`: {e}", path.display()))
}

/// Every certificate in a PEM file; at least one, and no section cut short.
async fn certificates(path: &Path) -> Result<Vec<CertificateDer<'static>>, String> {
    let bytes = read(path).await?;
    let certificates = CertificateDer::pem_slice_iter(&bytes)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("`{}` is not a PEM certificate: {e}", path.display()))?;
    if certificates.is_empty() {
        return Err(format!("`{}` holds no certificate", path.display()));
    }
    Ok(certificates)
}

/// What a look at the files found.
#[derive(Debug, PartialEq)]
pub(crate) enum Look {
    /// The same as when the connection last used them.
    Unchanged,
    /// Changed, but not complete: keep the connection as it is and look again.
    Incomplete(String),
    /// Changed and complete: reconnect.
    Rotated,
}

/// The files, and what they looked like when the connection last used them.
pub(crate) struct Watcher {
    files: Files,
    applied: Fingerprint,
}

impl Watcher {
    /// `applied` is the fingerprint taken before the connection read the files.
    pub(crate) fn new(files: Files, applied: Fingerprint) -> Self {
        Self { files, applied }
    }

    pub(crate) async fn look(&mut self) -> Look {
        let now = self.files.fingerprint().await;
        if now == self.applied {
            return Look::Unchanged;
        }
        match self.files.check().await {
            Ok(()) => {
                // Taken again: a file written between the first look and the check was
                // checked as it is now, and should not count as a second rotation.
                self.applied = self.files.fingerprint().await;
                Look::Rotated
            }
            Err(why) => Look::Incomplete(why),
        }
    }

    /// Look every `interval` until `stop` says so (`Nats::close`, or the last handle
    /// dropped), or a forced reconnect finds the client closed. async-nats reports no
    /// closed state of its own, so a client drained by other means is only noticed at
    /// the next rotation.
    pub(crate) fn spawn(
        mut self,
        client: Client,
        interval: Duration,
        mut stop: watch::Receiver<bool>,
    ) -> JoinHandle<()> {
        tokio::spawn(async move {
            let mut ticks = tokio::time::interval(interval);
            ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            // The first tick is immediate, and the files were just read.
            ticks.tick().await;
            loop {
                tokio::select! {
                    _ = ticks.tick() => {}
                    // Stopped, or every handle to the connection is gone.
                    _ = stop.wait_for(|stop| *stop) => break,
                }
                match self.look().await {
                    Look::Unchanged => {}
                    Look::Incomplete(why) => warn!(
                        reason = why,
                        "NATS credential files changed but are not complete; \
                         keeping the connection and looking again"
                    ),
                    Look::Rotated => {
                        info!("NATS credential files rotated; reconnecting");
                        if let Err(e) = client.force_reconnect().await {
                            debug!(error = %e, "NATS connection closed; no longer watching");
                            break;
                        }
                    }
                }
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The certificates `scripts/nats-test-certs.sh` writes are for the backend tests;
    /// these are fixtures of the same shape, made here with `openssl` so no key is
    /// checked in.
    fn fixtures() -> Option<tempfile::TempDir> {
        let dir = tempfile::tempdir().unwrap();
        let script = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../scripts/nats-test-certs.sh"
        );
        let status = std::process::Command::new("bash")
            .arg(script)
            .arg(dir.path())
            .status();
        match status {
            Ok(status) if status.success() => Some(dir),
            _ if std::env::var_os("CI").is_some() => {
                panic!("scripts/nats-test-certs.sh must run in CI (is openssl installed?)")
            }
            _ => {
                eprintln!("skipping: scripts/nats-test-certs.sh did not run (no openssl?)");
                None
            }
        }
    }

    fn pair(dir: &Path, name: &str) -> Files {
        Files {
            ca: Some(dir.join("ca.crt")),
            cert: Some((
                dir.join(format!("{name}.crt")),
                dir.join(format!("{name}.key")),
            )),
            creds: None,
        }
    }

    #[tokio::test]
    async fn a_certificate_with_its_own_key_is_complete() {
        let Some(dir) = fixtures() else { return };
        pair(dir.path(), "client-a").check().await.unwrap();
        pair(dir.path(), "client-b").check().await.unwrap();
    }

    #[tokio::test]
    async fn a_key_of_another_certificate_is_not() {
        let Some(dir) = fixtures() else { return };
        let files = Files {
            cert: Some((
                dir.path().join("client-a.crt"),
                dir.path().join("client-b.key"),
            )),
            ..Files::default()
        };
        let err = files.check().await.unwrap_err();
        assert!(err.contains("does not belong"), "{err}");
    }

    #[tokio::test]
    async fn a_half_written_certificate_is_not() {
        let Some(dir) = fixtures() else { return };
        let files = pair(dir.path(), "client-a");
        let cert = dir.path().join("client-a.crt");
        let pem = std::fs::read_to_string(&cert).unwrap();
        std::fs::write(&cert, &pem[..pem.len() / 2]).unwrap();
        let err = files.check().await.unwrap_err();
        assert!(err.contains("client-a.crt"), "{err}");

        std::fs::write(&cert, "").unwrap();
        assert!(files.check().await.unwrap_err().contains("no certificate"));
    }

    #[tokio::test]
    async fn a_ca_must_parse() {
        let Some(dir) = fixtures() else { return };
        let files = Files {
            ca: Some(dir.path().join("ca.key")),
            ..Files::default()
        };
        assert!(files.check().await.is_err());
    }

    #[tokio::test]
    async fn a_creds_file_without_a_seed_is_not_complete() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("service.creds");
        let files = Files {
            creds: Some(path.clone()),
            ..Files::default()
        };
        let (contents, _) = crate::connection::tests::creds_file("the.jwt");
        std::fs::write(&path, &contents).unwrap();
        files.check().await.unwrap();

        let without_seed = &contents[..contents.find("-----BEGIN USER NKEY SEED").unwrap()];
        std::fs::write(&path, without_seed).unwrap();
        let err = files.check().await.unwrap_err();
        assert!(err.contains("no NKey seed"), "{err}");
    }

    #[tokio::test]
    async fn nothing_changed_means_no_reconnect_and_a_completed_change_means_one() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("service.creds");
        let (contents, _) = crate::connection::tests::creds_file("first.jwt");
        std::fs::write(&path, &contents).unwrap();
        let files = Files {
            creds: Some(path.clone()),
            ..Files::default()
        };
        let mut watcher = Watcher::new(files.clone(), files.fingerprint().await);
        assert_eq!(watcher.look().await, Look::Unchanged);
        assert_eq!(watcher.look().await, Look::Unchanged);

        // Half-written: looked at again on every look, until it is complete.
        std::fs::write(&path, "-----BEGIN NATS USER JWT-----\n").unwrap();
        assert!(matches!(watcher.look().await, Look::Incomplete(_)));
        assert!(matches!(watcher.look().await, Look::Incomplete(_)));

        let (rotated, _) = crate::connection::tests::creds_file("second.jwt");
        std::fs::write(&path, rotated).unwrap();
        assert_eq!(watcher.look().await, Look::Rotated);
        // Once, not on every look after.
        assert_eq!(watcher.look().await, Look::Unchanged);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_swapped_symlink_is_a_change() {
        let dir = tempfile::tempdir().unwrap();
        let (first, _) = crate::connection::tests::creds_file("first.jwt");
        let (second, _) = crate::connection::tests::creds_file("second-and-longer.jwt");
        std::fs::write(dir.path().join("one"), first).unwrap();
        std::fs::write(dir.path().join("two"), second).unwrap();
        let link = dir.path().join("service.creds");
        std::os::unix::fs::symlink(dir.path().join("one"), &link).unwrap();
        let files = Files {
            creds: Some(link.clone()),
            ..Files::default()
        };
        let mut watcher = Watcher::new(files.clone(), files.fingerprint().await);

        // The way a Kubernetes secret mount rotates: a new target, then the link swapped.
        let swapped = dir.path().join("swapped");
        std::os::unix::fs::symlink(dir.path().join("two"), &swapped).unwrap();
        std::fs::rename(&swapped, &link).unwrap();
        assert_eq!(watcher.look().await, Look::Rotated);
    }
}
