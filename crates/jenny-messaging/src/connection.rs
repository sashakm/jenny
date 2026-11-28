//! The NATS connection: where it goes, how it is secured, and who it claims to be
//! (ADR-031).
//!
//! [`NatsConnectionConfig`] is the `messaging.nats` section of a jenny service. It turns
//! into [`async_nats::ConnectOptions`] through
//! [`connect_options`](NatsConnectionConfig::connect_options), which
//! [`Nats`](crate::kv::Nats) connects with:
//!
//! ```yaml
//! messaging:
//!   nats:
//!     url: tls://nats-1:4222,tls://nats-2:4222
//!     refresh_interval: 30              # seconds between looks at the files below
//!     tls:                              # present, even empty: TLS on every server
//!       ca: /etc/nats-tls/ca.crt        # trust anchors; default: the system's roots
//!       cert: /etc/nats-tls/tls.crt     # client certificate, for mTLS
//!       key: /etc/nats-tls/tls.key
//!       handshake_first: false          # for a server with `handshake_first`
//!       server_name: nats.internal      # optional, when the URL names an IP address
//!     auth:                             # at most one of:
//!       creds: /etc/nats/service.creds  # user JWT + NKey seed
//!       # nkey: …                       # seed; JENNY__MESSAGING__NATS__AUTH__NKEY
//!       # user: notes
//!       # password: …                   # JENNY__MESSAGING__NATS__AUTH__PASSWORD
//!       # token: …                      # JENNY__MESSAGING__NATS__AUTH__TOKEN
//! ```
//!
//! Certificates, keys and `.creds` are paths, read by the client at each connect, so a
//! rotated file is used from the next reconnect on, and [`Nats`](crate::kv::Nats) forces
//! one when the files change.

use std::fmt;
use std::path::{Path, PathBuf};

use async_nats::{Auth, AuthError, ConnectOptions, ServerAddr};
use serde::Deserialize;

/// Seconds between two looks at the files a connection names, unless
/// `refresh_interval` says otherwise.
pub const DEFAULT_REFRESH_INTERVAL_SECONDS: u64 = 30;

/// The `messaging.nats` section: one NATS connection, shared by the key-value stores and
/// the gateway.
///
/// Every field has a default, so a configuration with only `url` keeps working. Without
/// a `url` there is no NATS.
///
/// ```
/// # use serde_json::{from_value, json};
/// use jenny_messaging::connection::NatsConnectionConfig;
///
/// let config: NatsConnectionConfig = from_value(json!({
///     "url": "tls://nats-1:4222,tls://nats-2:4222",
///     "tls": { "cert": "/etc/nats-tls/tls.crt", "key": "/etc/nats-tls/tls.key" },
/// }))?;
/// config.validate()?;
/// let options = config.connect_options()?;
/// # Ok::<(), anyhow::Error>(())
/// ```
#[derive(Clone, Debug, Deserialize)]
#[serde(default)]
pub struct NatsConnectionConfig {
    /// The servers, comma-separated (`nats://`, `tls://`, `ws://`, `wss://`).
    pub url: Option<String>,
    /// Seconds between two looks at the files `tls` and `auth.creds` name. Defaults to
    /// [`DEFAULT_REFRESH_INTERVAL_SECONDS`].
    pub refresh_interval: u64,
    /// How the connection is secured. Present, even empty (a bare `tls:`, which YAML
    /// reads as null), it asks for TLS on every server, whatever a URL's scheme says.
    #[serde(deserialize_with = "present")]
    pub tls: Option<NatsTlsConfig>,
    /// Who the service claims to be, on top of `tls`. At most one entry.
    pub auth: NatsAuthConfig,
}

impl Default for NatsConnectionConfig {
    fn default() -> Self {
        Self {
            url: None,
            refresh_interval: DEFAULT_REFRESH_INTERVAL_SECONDS,
            tls: None,
            auth: NatsAuthConfig::default(),
        }
    }
}

/// A key that is there, even with nothing under it, is `Some`: an operator who writes
/// `tls:` and comments out every line below it still asked for TLS.
fn present<'de, D>(deserializer: D) -> Result<Option<NatsTlsConfig>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Option::<NatsTlsConfig>::deserialize(deserializer).map(|tls| Some(tls.unwrap_or_default()))
}

/// `messaging.nats.tls`. Paths, never loaded by jenny: the client reads them again at
/// each TLS handshake.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct NatsTlsConfig {
    /// PEM trust anchors for the server's certificate. Without it, the system's roots.
    pub ca: Option<PathBuf>,
    /// PEM client certificate, for mTLS. Needs `key`.
    pub cert: Option<PathBuf>,
    /// PEM private key of `cert`.
    pub key: Option<PathBuf>,
    /// Start with the TLS handshake, for a server configured with `handshake_first`.
    pub handshake_first: bool,
    /// The name to verify every server's certificate against, when the URLs name IP
    /// addresses.
    pub server_name: Option<String>,
}

/// `messaging.nats.auth`: at most one of `creds`, `nkey`, `user` with `password`, or
/// `token`, because a server uses only one, and a silently ignored secret is a bug that
/// is hard to find.
///
/// `Debug` shows `nkey`, `password` and `token` as `<redacted>`, because the
/// configuration is logged at startup. Set them through `JENNY__MESSAGING__NATS__AUTH__…`.
#[derive(Clone, Default, Deserialize)]
#[serde(default)]
pub struct NatsAuthConfig {
    /// A `.creds` file: a user JWT and its NKey seed, read again at each connect.
    pub creds: Option<PathBuf>,
    /// An NKey seed (`SU…`); the server challenges it with a nonce.
    pub nkey: Option<String>,
    /// A user name. Needs `password`.
    pub user: Option<String>,
    /// The password of `user`.
    pub password: Option<String>,
    /// A shared token.
    pub token: Option<String>,
}

impl fmt::Debug for NatsAuthConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let redacted = |secret: &Option<String>| secret.as_ref().map(|_| "<redacted>");
        f.debug_struct("NatsAuthConfig")
            .field("creds", &self.creds)
            .field("nkey", &redacted(&self.nkey))
            .field("user", &self.user)
            .field("password", &redacted(&self.password))
            .field("token", &redacted(&self.token))
            .finish()
    }
}

impl NatsConnectionConfig {
    /// A connection to `url` with every other setting at its default.
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            url: Some(url.into()),
            ..Self::default()
        }
    }

    /// Find every mistake at once, naming the keys at fault relative to this section.
    ///
    /// The URL is only checked when there is one: without it there is no connection.
    pub fn validate(&self) -> anyhow::Result<()> {
        let mut problems = Vec::new();
        if self.url.is_some()
            && let Err(e) = self.server_addrs()
        {
            problems.push(e.to_string());
        }
        if self.refresh_interval == 0 {
            problems.push("`refresh_interval` must be at least 1 (seconds)".to_string());
        }
        // An empty variable (`JENNY__MESSAGING__NATS__AUTH__TOKEN=`) is a mistake, not a
        // secret: say so rather than sending nothing as a credential.
        for (key, value) in [
            ("`auth.nkey`", &self.auth.nkey),
            ("`auth.user`", &self.auth.user),
            ("`auth.password`", &self.auth.password),
            ("`auth.token`", &self.auth.token),
        ] {
            if value.as_deref().is_some_and(str::is_empty) {
                problems.push(format!("{key} is set but empty"));
            }
        }
        if let Some(tls) = &self.tls {
            match (&tls.cert, &tls.key) {
                (Some(_), None) => problems.push("`tls.cert` is set without `tls.key`".into()),
                (None, Some(_)) => problems.push("`tls.key` is set without `tls.cert`".into()),
                _ => {}
            }
        }
        let auth = &self.auth;
        match (&auth.user, &auth.password) {
            (Some(_), None) => problems.push("`auth.user` is set without `auth.password`".into()),
            (None, Some(_)) => problems.push("`auth.password` is set without `auth.user`".into()),
            _ => {}
        }
        let entries: Vec<&str> = [
            ("`auth.creds`", auth.creds.is_some()),
            ("`auth.nkey`", auth.nkey.is_some()),
            (
                "`auth.user`/`auth.password`",
                auth.user.is_some() || auth.password.is_some(),
            ),
            ("`auth.token`", auth.token.is_some()),
        ]
        .into_iter()
        .filter_map(|(key, set)| set.then_some(key))
        .collect();
        if entries.len() > 1 {
            problems.push(format!(
                "`auth` takes at most one entry, but {} are set",
                entries.join(", ")
            ));
        }
        if problems.is_empty() {
            Ok(())
        } else {
            anyhow::bail!("{}", problems.join("; "))
        }
    }

    /// The servers `url` lists.
    ///
    /// Fails on any URL that does not parse, rather than connecting to the rest: a typo
    /// in one of three cluster addresses should be found at startup, not during the
    /// outage of the other two.
    pub fn server_addrs(&self) -> anyhow::Result<Vec<ServerAddr>> {
        let addrs = self
            .url
            .as_deref()
            .unwrap_or_default()
            .split(',')
            .map(str::trim)
            .filter(|url| !url.is_empty())
            .map(|url| {
                url.parse::<ServerAddr>()
                    .map_err(|e| anyhow::anyhow!("invalid NATS server url `{url}` in `url`: {e}"))
            })
            .collect::<anyhow::Result<Vec<_>>>()?;
        if addrs.is_empty() {
            anyhow::bail!("no NATS server in `url`");
        }
        Ok(addrs)
    }

    /// The options to connect with, after [`validate`](Self::validate).
    ///
    /// Certificates are passed as paths, so the client reads them again at each
    /// handshake, and any `tls` asks for TLS on every server and every reconnect. A
    /// `.creds` file is read through an auth callback at each connect, rather than once
    /// through `credentials_file`. `handshake_first` and `server_name` are passed through
    /// when set.
    pub fn connect_options(&self) -> anyhow::Result<ConnectOptions> {
        self.validate()?;
        let auth = &self.auth;
        let mut options = match &auth.creds {
            Some(path) => {
                let path = path.clone();
                ConnectOptions::with_auth_callback(move |nonce| {
                    let path = path.clone();
                    async move { creds_auth(&path, &nonce).await }
                })
            }
            None => ConnectOptions::new(),
        };
        if let Some(seed) = &auth.nkey {
            options = options.nkey(seed.clone());
        }
        if let (Some(user), Some(password)) = (&auth.user, &auth.password) {
            options = options.user_and_password(user.clone(), password.clone());
        }
        if let Some(token) = &auth.token {
            options = options.token(token.clone());
        }
        if let Some(tls) = &self.tls {
            ensure_crypto_provider();
            options = options.require_tls(true);
            if let Some(ca) = &tls.ca {
                options = options.add_root_certificates(ca.clone());
            }
            if let (Some(cert), Some(key)) = (&tls.cert, &tls.key) {
                options = options.add_client_certificate(cert.clone(), key.clone());
            }
            if tls.handshake_first {
                options = options.tls_first();
            }
            if let Some(name) = &tls.server_name {
                options = options.tls_server_name(name);
            }
        }
        Ok(options)
    }

    /// The files the connection reads at each connect, which the watcher looks at.
    pub(crate) fn files(&self) -> crate::rotation::Files {
        let tls = self.tls.as_ref();
        crate::rotation::Files {
            ca: tls.and_then(|tls| tls.ca.clone()),
            cert: tls.and_then(|tls| tls.cert.clone().zip(tls.key.clone())),
            creds: self.auth.creds.clone(),
        }
    }
}

/// Make sure rustls has a process-level crypto provider before async-nats builds its
/// TLS configuration with `ClientConfig::builder()`.
///
/// rustls only picks one by itself when exactly one of its `ring` and `aws-lc-rs`
/// features is on. A service whose other dependencies enable `aws-lc-rs` next to the
/// `ring` that async-nats asks for has both, and the first TLS handshake would panic.
/// A provider the application installed itself is kept; otherwise ring, the one
/// async-nats is built with, becomes the default.
fn ensure_crypto_provider() {
    use async_nats::rustls::crypto::{CryptoProvider, ring};
    if CryptoProvider::get_default().is_none() {
        // Losing a race to another installer is fine: either provider works.
        let _ = ring::default_provider().install_default();
    }
}

/// The user JWT and the NKey seed of a `.creds` file.
pub(crate) struct Creds {
    pub(crate) jwt: String,
    pub(crate) seed: nkeys::KeyPair,
}

/// Parse a `.creds` file: the first block is the user JWT, the second the NKey seed.
///
/// ```text
/// -----BEGIN NATS USER JWT-----
/// eyJ0eXAiOiJKV1QiLCJhbGciOiJlZDI1NTE5LW5rZXkifQ…
/// ------END NATS USER JWT------
///
/// -----BEGIN USER NKEY SEED-----
/// SUAIO3FHUX5PNV2LQIIP7TZ3N4L7TX3W53MQGEIVYFIGA635OZCKEYHFLM
/// ------END USER NKEY SEED------
/// ```
pub(crate) fn parse_creds(contents: &str) -> Result<Creds, String> {
    let block = |marker: &str| {
        let mut lines = contents.lines().map(str::trim);
        lines.find(|line| line.starts_with("---") && line.contains(marker))?;
        lines
            .next()
            .filter(|line| !line.is_empty() && !line.starts_with("---"))
            .map(str::to_string)
    };
    let jwt = block("BEGIN NATS USER JWT").ok_or("it holds no user JWT")?;
    let seed = block("BEGIN USER NKEY SEED").ok_or("it holds no NKey seed")?;
    let seed = nkeys::KeyPair::from_seed(&seed).map_err(|e| format!("its NKey seed: {e}"))?;
    Ok(Creds { jwt, seed })
}

/// Read `path` now and answer the server's nonce with it.
pub(crate) async fn creds_auth(path: &Path, nonce: &[u8]) -> Result<Auth, AuthError> {
    let contents = tokio::fs::read_to_string(path).await.map_err(|e| {
        AuthError::new(format!(
            "reading the NATS credentials `{}`: {e}",
            path.display()
        ))
    })?;
    let creds = parse_creds(&contents).map_err(|e| {
        AuthError::new(format!(
            "the NATS credentials `{}` are not usable: {e}",
            path.display()
        ))
    })?;
    let signature = creds
        .seed
        .sign(nonce)
        .map_err(|e| AuthError::new(format!("signing the server's nonce: {e}")))?;
    let mut auth = Auth::new();
    auth.jwt = Some(creds.jwt);
    auth.signature = Some(signature);
    Ok(auth)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use serde_json::{from_value, json};

    fn config(value: serde_json::Value) -> NatsConnectionConfig {
        from_value(value).unwrap()
    }

    fn problems(value: serde_json::Value) -> String {
        config(value).validate().unwrap_err().to_string()
    }

    /// A `.creds` file for a fresh user, with `jwt` standing in for the JWT.
    pub(crate) fn creds_file(jwt: &str) -> (String, nkeys::KeyPair) {
        let user = nkeys::KeyPair::new_user();
        let contents = format!(
            "-----BEGIN NATS USER JWT-----\n{jwt}\n------END NATS USER JWT------\n\n\
             ************************* IMPORTANT *************************\n\
             NKEY Seed printed below can be used to sign and prove identity.\n\n\
             -----BEGIN USER NKEY SEED-----\n{}\n------END USER NKEY SEED------\n",
            user.seed().unwrap()
        );
        (contents, user)
    }

    #[test]
    fn a_config_with_only_a_url_keeps_working() {
        let config = config(json!({ "url": "nats://localhost:4222" }));
        config.validate().unwrap();
        assert_eq!(config.refresh_interval, DEFAULT_REFRESH_INTERVAL_SECONDS);
        assert!(config.tls.is_none());
        let options = format!("{:?}", config.connect_options().unwrap());
        assert!(options.contains("\"tls_required\": false"), "{options}");
    }

    #[test]
    fn every_server_url_must_parse() {
        let addrs = |url: &str| NatsConnectionConfig::new(url).server_addrs();
        assert_eq!(addrs("nats://a:4222, nats://b:4222").unwrap().len(), 2);
        let err = addrs("nats://a:4222,nats://b:not-a-port").unwrap_err();
        assert!(err.to_string().contains("nats://b:not-a-port"), "{err}");
        assert!(addrs(" , ").is_err());
        let err = NatsConnectionConfig::new(" , ").validate().unwrap_err();
        assert!(err.to_string().contains("`url`"), "{err}");
    }

    #[test]
    fn a_certificate_needs_its_key_and_the_key_its_certificate() {
        let err = problems(json!({ "url": "tls://a:4222", "tls": { "cert": "/c.crt" } }));
        assert!(err.contains("`tls.cert` is set without `tls.key`"), "{err}");
        let err = problems(json!({ "url": "tls://a:4222", "tls": { "key": "/c.key" } }));
        assert!(err.contains("`tls.key` is set without `tls.cert`"), "{err}");
    }

    #[test]
    fn auth_takes_one_entry_and_a_user_needs_a_password() {
        let err = problems(json!({
            "url": "nats://a:4222",
            "auth": { "creds": "/s.creds", "token": "t" },
        }));
        assert!(err.contains("at most one entry"), "{err}");
        assert!(
            err.contains("`auth.creds`") && err.contains("`auth.token`"),
            "{err}"
        );

        // A user and a password are one entry.
        config(json!({ "url": "nats://a:4222", "auth": { "user": "u", "password": "p" } }))
            .validate()
            .unwrap();
        let err = problems(json!({
            "url": "nats://a:4222",
            "auth": { "user": "u", "password": "p", "nkey": "SU…" },
        }));
        assert!(
            err.contains("`auth.nkey`, `auth.user`/`auth.password`"),
            "{err}"
        );

        let err = problems(json!({ "url": "nats://a:4222", "auth": { "user": "u" } }));
        assert!(
            err.contains("`auth.user` is set without `auth.password`"),
            "{err}"
        );
        let err = problems(json!({ "url": "nats://a:4222", "auth": { "password": "p" } }));
        assert!(
            err.contains("`auth.password` is set without `auth.user`"),
            "{err}"
        );
    }

    #[test]
    fn a_tls_key_with_nothing_under_it_still_asks_for_tls() {
        let config = config(json!({ "url": "nats://a:4222", "tls": null }));
        assert!(config.tls.is_some());
        let shown = format!("{:?}", config.connect_options().unwrap());
        assert!(shown.contains("\"tls_required\": true"), "{shown}");
        assert!(
            super::tests::config(json!({ "url": "nats://a:4222" }))
                .tls
                .is_none()
        );
    }

    #[test]
    fn asking_for_tls_installs_a_crypto_provider() {
        let config = config(json!({ "url": "tls://a:4222", "tls": {} }));
        config.connect_options().unwrap();
        assert!(async_nats::rustls::crypto::CryptoProvider::get_default().is_some());
    }

    #[test]
    fn an_empty_secret_is_a_mistake() {
        let err = problems(json!({ "url": "nats://a:4222", "auth": { "token": "" } }));
        assert!(err.contains("`auth.token` is set but empty"), "{err}");
    }

    #[test]
    fn every_mistake_is_reported_at_once() {
        let err = problems(json!({
            "url": "nats://a:bad",
            "refresh_interval": 0,
            "tls": { "cert": "/c.crt" },
            "auth": { "user": "u" },
        }));
        for key in ["`url`", "`refresh_interval`", "`tls.cert`", "`auth.user`"] {
            assert!(err.contains(key), "{key} in {err}");
        }
    }

    #[test]
    fn debug_redacts_the_secrets_and_shows_the_paths() {
        let shown = format!(
            "{:?}",
            config(json!({
                "url": "nats://a:4222",
                "tls": { "cert": "/etc/tls/tls.crt", "key": "/etc/tls/tls.key" },
                "auth": {
                    "creds": "/etc/nats/s.creds",
                    "nkey": "SUSECRETSEED",
                    "user": "notes",
                    "password": "hunter2",
                    "token": "s3cr3t-token",
                },
            }))
        );
        for secret in ["SUSECRETSEED", "hunter2", "s3cr3t-token"] {
            assert!(!shown.contains(secret), "{secret} in {shown}");
        }
        assert_eq!(shown.matches("<redacted>").count(), 3, "{shown}");
        for path in [
            "/etc/tls/tls.crt",
            "/etc/tls/tls.key",
            "/etc/nats/s.creds",
            "notes",
        ] {
            assert!(shown.contains(path), "{path} in {shown}");
        }
        // Unset secrets say so, rather than pretending to hold one.
        let unset = format!("{:?}", NatsAuthConfig::default());
        assert!(!unset.contains("<redacted>"), "{unset}");
    }

    #[test]
    fn any_tls_setting_requires_tls_and_paths_are_passed_through() {
        for tls in [
            json!({}),
            json!({ "ca": "/ca.crt" }),
            json!({ "cert": "/c.crt", "key": "/c.key" }),
            json!({ "handshake_first": true }),
            json!({ "server_name": "nats.internal" }),
        ] {
            let options = config(json!({ "url": "nats://a:4222", "tls": tls }))
                .connect_options()
                .unwrap();
            let shown = format!("{options:?}");
            assert!(shown.contains("\"tls_required\": true"), "{tls}: {shown}");
        }
        let shown = format!(
            "{:?}",
            config(json!({
                "url": "nats://a:4222",
                "tls": {
                    "ca": "/ca.crt",
                    "cert": "/c.crt",
                    "key": "/c.key",
                    "handshake_first": true,
                    "server_name": "nats.internal",
                },
            }))
            .connect_options()
            .unwrap()
        );
        for expected in [
            "\"certificates\": [\"/ca.crt\"]",
            "\"client_cert\": Some(\"/c.crt\")",
            "\"client_key\": Some(\"/c.key\")",
            "\"tls_first\": true",
            "\"tls_server_name\": Some(\"nats.internal\")",
        ] {
            assert!(shown.contains(expected), "{expected} in {shown}");
        }
    }

    #[test]
    fn invalid_settings_build_no_options() {
        let err = config(json!({ "url": "nats://a:4222", "tls": { "cert": "/c.crt" } }))
            .connect_options()
            .unwrap_err();
        assert!(err.to_string().contains("`tls.key`"), "{err}");
    }

    #[tokio::test]
    async fn a_creds_file_is_read_again_at_each_connect() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("service.creds");
        let nonce = b"a server nonce";

        let (first, first_user) = creds_file("first.jwt");
        std::fs::write(&path, first).unwrap();
        let auth = creds_auth(&path, nonce).await.unwrap();
        assert_eq!(auth.jwt.as_deref(), Some("first.jwt"));
        let signature = auth.signature.unwrap();
        assert!(first_user.verify(nonce, &signature).is_ok());

        // Rotated: the next connect answers with the new file, not the one read before.
        let (second, second_user) = creds_file("second.jwt");
        std::fs::write(&path, second).unwrap();
        let auth = creds_auth(&path, nonce).await.unwrap();
        assert_eq!(auth.jwt.as_deref(), Some("second.jwt"));
        assert!(second_user.verify(nonce, &auth.signature.unwrap()).is_ok());

        // Half-written: the connect fails, saying why.
        std::fs::write(
            &path,
            "-----BEGIN NATS USER JWT-----\nthird.jwt\n------END NATS USER JWT------\n",
        )
        .unwrap();
        let err = creds_auth(&path, nonce)
            .await
            .err()
            .expect("no seed, no auth");
        assert!(err.to_string().contains("no NKey seed"), "{err}");
    }

    #[test]
    fn a_creds_file_needs_a_jwt_and_a_seed() {
        let (contents, _) = creds_file("the.jwt");
        assert_eq!(parse_creds(&contents).unwrap().jwt, "the.jwt");
        assert!(parse_creds("").is_err());
        let without_jwt = contents.replace("the.jwt", "");
        assert!(
            parse_creds(&without_jwt)
                .err()
                .unwrap()
                .contains("no user JWT")
        );
        let bad_seed = "-----BEGIN NATS USER JWT-----\nthe.jwt\n------END NATS USER JWT------\n\
                        -----BEGIN USER NKEY SEED-----\nSUNOTASEED\n------END USER NKEY SEED------\n";
        assert!(parse_creds(bad_seed).err().unwrap().contains("seed"));
    }
}
