//! A [`Store`] over a NATS JetStream key-value bucket.

use std::fmt;
use std::marker::PhantomData;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_nats::ConnectOptions;
use async_nats::jetstream::{self, kv};
use async_trait::async_trait;
use bytes::Bytes;
use futures::TryStreamExt;
use jenny_core::domain::{Aggregate, Criteria, DomainError, DomainResult, ValueKind};
use jenny_core::memory::Queryable;
use jenny_core::port::outbound::HealthCheck;
use jenny_core::port::store::{Loaded, Store, Version, encode, expect_version};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::{OnceCell, watch};
use tokio::task::JoinHandle;

use crate::connection::NatsConnectionConfig;
use crate::rotation::Watcher;

/// The service's NATS connection, shared by its key-value stores and its gateway, made
/// on first use.
///
/// Cheap to clone. Wiring is synchronous and runs before the server is up, so this does
/// not connect when it is built: [`prepare`](Self::prepare) connects and creates the
/// buckets of every store handed out, and the runtime calls it before binding its
/// socket, so a NATS that is down, or refuses the service's credentials, stops the
/// service at startup. A store used before that connects and opens its bucket on its
/// first call.
///
/// The connection is configured by a [`NatsConnectionConfig`] (ADR-031): TLS, client
/// certificates and credentials. Once connected, a task looks at the files it names
/// every `refresh_interval` seconds, and forces a reconnect when they rotated and are
/// complete, so a renewed certificate is used without a restart.
/// [`close`](Self::close) stops it.
///
/// ```no_run
/// # use jenny_messaging::connection::NatsConnectionConfig;
/// # use jenny_messaging::kv::Nats;
/// # #[derive(serde::Serialize, serde::Deserialize)] struct Session { id: String }
/// # impl jenny_core::domain::Aggregate for Session { type Id = String; const KIND: &'static str = "session"; fn id(&self) -> String { self.id.clone() } }
/// # async fn wire() -> anyhow::Result<()> {
/// let nats = Nats::new(NatsConnectionConfig::new("nats://localhost:4222"), "my-notes");
/// let sessions = nats.kv::<Session>();          // bucket `my-notes-session`
/// let events = nats.gateway("my-notes", "my-notes.>"); // the same connection
/// nats.prepare().await?;                        // the runtime does this before serving
/// # Ok(()) }
/// ```
#[derive(Clone)]
pub struct Nats {
    pub(crate) inner: Arc<Inner>,
}

/// Adjusts the options built from the configuration, or replaces them.
pub type OptionsHook = Arc<dyn Fn(ConnectOptions) -> ConnectOptions + Send + Sync>;

pub(crate) struct Inner {
    config: NatsConnectionConfig,
    hook: Option<OptionsHook>,
    prefix: String,
    client: OnceCell<async_nats::Client>,
    buckets: Mutex<Vec<Arc<Bucket>>>,
    /// Tells the file watcher to stop; dropped with the last handle, which stops it too.
    stop: watch::Sender<bool>,
    watcher: Mutex<Option<JoinHandle<()>>>,
}

/// One bucket, opened — or created — once.
struct Bucket {
    name: String,
    store: OnceCell<kv::Store>,
}

impl Nats {
    /// A connection configured by `config`, which is validated when it connects.
    /// `prefix` namespaces the buckets, so services sharing a NATS do not share data by
    /// accident.
    pub fn new(config: NatsConnectionConfig, prefix: impl Into<String>) -> Self {
        Self::build(config, None, prefix.into())
    }

    /// [`new`](Self::new), with `options` adjusting the [`ConnectOptions`] built from
    /// `config` — for credentials kept somewhere the configuration cannot name. It runs
    /// before each attempt at the first connection; reconnects reuse what it returned.
    /// It may extend or replace them; the files `config` names are still the only ones
    /// watched.
    pub fn with_options(
        config: NatsConnectionConfig,
        prefix: impl Into<String>,
        options: impl Fn(ConnectOptions) -> ConnectOptions + Send + Sync + 'static,
    ) -> Self {
        Self::build(config, Some(Arc::new(options)), prefix.into())
    }

    pub(crate) fn build(
        config: NatsConnectionConfig,
        hook: Option<OptionsHook>,
        prefix: String,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                config,
                hook,
                prefix,
                client: OnceCell::new(),
                buckets: Mutex::new(Vec::new()),
                stop: watch::Sender::new(false),
                watcher: Mutex::new(None),
            }),
        }
    }

    /// A store for `A` in the bucket `<prefix>-<kind>`.
    ///
    /// Panics when that is not a valid bucket name (letters, digits, `-` and `_`): the
    /// prefix and the kind are configuration and code, so it is a mistake to fix there.
    pub fn kv<A: Aggregate>(&self) -> NatsKv<A> {
        self.kv_in(&format!("{}-{}", self.inner.prefix, A::KIND))
    }

    /// [`kv`](Self::kv) in a bucket of your choosing.
    pub fn kv_in<A: Aggregate>(&self, bucket: &str) -> NatsKv<A> {
        assert!(
            !bucket.is_empty()
                && bucket
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'),
            "`{bucket}` is not a valid NATS bucket name: use letters, digits, `-` and `_`"
        );
        let bucket = Arc::new(Bucket {
            name: bucket.to_string(),
            store: OnceCell::new(),
        });
        self.inner
            .buckets
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(bucket.clone());
        NatsKv {
            nats: self.inner.clone(),
            bucket,
            queryable: Queryable::default(),
            kind: PhantomData,
        }
    }

    /// The connected client, connecting the first time.
    ///
    /// Fails with the server's reason when it refuses the connection: an invalid
    /// configuration, a certificate it does not trust, credentials it does not accept.
    pub async fn client(&self) -> anyhow::Result<async_nats::Client> {
        self.inner.client().await.cloned()
    }

    /// Whether the server required this connection to authenticate, which it then
    /// requires of every client, publishers included. Connects the first time.
    ///
    /// This is what decides whether inbound caller headers are trusted: it holds however
    /// the credentials were given (`messaging.nats.auth`, a client certificate the server
    /// maps to a user, a `nats_options` hook). A server with `no_auth_user` reports it too,
    /// yet admits anonymous clients as that user: give that user no publish permission
    /// where caller headers are trusted.
    pub async fn is_authenticated(&self) -> anyhow::Result<bool> {
        Ok(self.inner.client().await?.server_info().auth_required)
    }

    /// Connect, and open or create the bucket of every store handed out.
    pub async fn prepare(&self) -> anyhow::Result<()> {
        self.inner.client().await?;
        let buckets = self
            .inner
            .buckets
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        for bucket in buckets {
            self.inner
                .store(&bucket)
                .await
                .map_err(|e| anyhow::anyhow!("opening NATS bucket `{}`: {e}", bucket.name))?;
        }
        Ok(())
    }

    /// Stop watching the credential files, flush what was published, and start draining
    /// the connection, which closes the client for every handle.
    pub async fn close(&self) {
        let _ = self.inner.stop.send(true);
        let watcher = self
            .inner
            .watcher
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take();
        if let Some(watcher) = watcher {
            let _ = watcher.await;
        }
        if let Some(client) = self.inner.client.get() {
            // `drain` only asks; flushing first is what waits for buffered publishes.
            if let Err(e) = client.flush().await {
                tracing::debug!(error = %e, "flushing the NATS connection");
            }
            if let Err(e) = client.drain().await {
                tracing::debug!(error = %e, "draining the NATS connection");
            }
        }
    }

    /// Readiness for this connection.
    pub fn health_check(&self) -> NatsKvCheck {
        NatsKvCheck {
            inner: self.inner.clone(),
        }
    }
}

impl Inner {
    pub(crate) async fn client(&self) -> anyhow::Result<&async_nats::Client> {
        self.client
            .get_or_try_init(|| async {
                let config = &self.config;
                let mut options = config
                    .connect_options()
                    .map_err(|e| anyhow::anyhow!("invalid NATS configuration: {e}"))?;
                if let Some(hook) = &self.hook {
                    options = hook(options);
                }
                let servers = config.server_addrs()?;
                // Taken before the client reads the files, so a rotation while it
                // connects is still seen.
                let files = config.files();
                let fingerprint = files.fingerprint().await;
                let client = options.connect(servers).await.map_err(|e| {
                    anyhow::anyhow!("connecting to NATS at `{}`: {}", url(config), reason(&e))
                })?;
                if !files.is_empty() {
                    let handle = Watcher::new(files, fingerprint).spawn(
                        client.clone(),
                        Duration::from_secs(config.refresh_interval),
                        self.stop.subscribe(),
                    );
                    *self
                        .watcher
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(handle);
                }
                Ok(client)
            })
            .await
    }

    async fn store<'a>(&self, bucket: &'a Bucket) -> DomainResult<&'a kv::Store> {
        bucket
            .store
            .get_or_try_init(|| async {
                let client = self
                    .client()
                    .await
                    .map_err(|e| DomainError::internal(e.to_string()))?;
                let jetstream = jetstream::new(client.clone());
                match jetstream.get_key_value(&bucket.name).await {
                    Ok(store) => Ok(store),
                    Err(_) => jetstream
                        .create_key_value(kv::Config {
                            bucket: bucket.name.clone(),
                            // Versions are this store's own; the bucket only needs the
                            // latest value of each key.
                            history: 1,
                            ..Default::default()
                        })
                        .await
                        .map_err(|e| {
                            DomainError::internal(format!(
                                "creating NATS bucket `{}`: {e}",
                                bucket.name
                            ))
                        }),
                }
            })
            .await
    }
}

/// The configured URL, for messages.
fn url(config: &NatsConnectionConfig) -> &str {
    config.url.as_deref().unwrap_or_default()
}

/// A connect error with every cause it carries: the server's own words are usually in
/// the innermost one.
fn reason(error: &(dyn std::error::Error + 'static)) -> String {
    let mut reason = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        let text = cause.to_string();
        if !reason.contains(&text) {
            reason = format!("{reason}: {text}");
        }
        source = cause.source();
    }
    reason
}

impl fmt::Debug for Nats {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Nats")
            .field("prefix", &self.inner.prefix)
            .field("connected", &self.inner.client.initialized())
            .finish_non_exhaustive()
    }
}

/// Aggregates of one kind in a JetStream key-value bucket, one key each.
///
/// A good fit for what is read and written by id and rarely listed — sessions,
/// preferences, the state of a long-running process — and for services that already run
/// NATS and want no database. Each value is the aggregate's document beside its version,
/// `{ "version": 3, "document": { … } }`, so versions count writes from 1 as in every
/// other store, whatever revision the bucket assigns.
///
/// Every save reads the entry, checks its version, and writes with the bucket's
/// compare-and-swap on the revision it read, so a write between the two is refused
/// rather than overwritten.
///
/// Listing reads every key in the bucket and filters, sorts and pages in memory with the
/// same [`Queryable`] allowlist as [`Memory`](jenny_core::memory::Memory). That is fine
/// for hundreds of aggregates and wrong for millions; use Postgres for those.
///
/// Ids become keys, which NATS restricts to letters, digits and `-/_=.`, not starting
/// or ending with `.`. UUIDs and numbers are fine; an id that is not is an internal
/// error, since it means the aggregate does not suit this store.
pub struct NatsKv<A> {
    nats: Arc<Inner>,
    bucket: Arc<Bucket>,
    queryable: Queryable,
    kind: PhantomData<fn() -> A>,
}

/// What a value in the bucket holds.
#[derive(Serialize, Deserialize)]
struct Stored {
    version: u64,
    document: Value,
}

impl<A: Aggregate> NatsKv<A> {
    /// The attribute paths a listing may sort and filter on, and what each holds.
    pub fn queryable<I, P>(mut self, paths: I) -> Self
    where
        I: IntoIterator<Item = (P, ValueKind)>,
        P: Into<String>,
    {
        self.queryable = Queryable::new(paths);
        self
    }

    async fn store(&self) -> DomainResult<&kv::Store> {
        self.nats.store(&self.bucket).await
    }

    /// The entry under `key` and what it holds, unless there is none or it was deleted.
    async fn read(&self, key: &str) -> DomainResult<Option<(u64, Stored)>> {
        let entry =
            self.store().await?.entry(key).await.map_err(|e| {
                DomainError::internal(format!("reading `{key}` from NATS bucket: {e}"))
            })?;
        match entry {
            Some(entry) if entry.operation == kv::Operation::Put => {
                let stored = serde_json::from_slice::<Stored>(&entry.value).map_err(|e| {
                    DomainError::internal(format!(
                        "a stored {} under `{key}` is not a jenny value: {e}",
                        A::KIND
                    ))
                })?;
                Ok(Some((entry.revision, stored)))
            }
            _ => Ok(None),
        }
    }

    fn value(document: Value, version: Version) -> DomainResult<Bytes> {
        serde_json::to_vec(&Stored {
            version: version.get(),
            document,
        })
        .map(Bytes::from)
        .map_err(|e| DomainError::internal(format!("could not encode a {}: {e}", A::KIND)))
    }
}

#[async_trait]
impl<A: Aggregate> Store<A> for NatsKv<A> {
    async fn find(&self, id: &A::Id) -> DomainResult<Option<Loaded<A>>> {
        let key = key::<A>(id)?;
        match self.read(&key).await? {
            Some((_, stored)) => Ok(Some(Loaded::decode(
                stored.document,
                Version::new(stored.version),
            )?)),
            None => Ok(None),
        }
    }

    async fn insert(&self, aggregate: A) -> DomainResult<Loaded<A>> {
        let key = key::<A>(&aggregate.id())?;
        let value = Self::value(encode(&aggregate)?, Version::FIRST)?;
        match self.store().await?.create(&key, value).await {
            Ok(_) => Loaded::written(aggregate, Version::FIRST),
            Err(e) if e.kind() == kv::CreateErrorKind::AlreadyExists => Err(DomainError::conflict(
                format!("{} {key} already exists", A::KIND),
            )),
            Err(e) => Err(DomainError::internal(format!(
                "writing `{key}` to NATS bucket: {e}"
            ))),
        }
    }

    async fn save(&self, aggregate: Loaded<A>) -> DomainResult<Loaded<A>> {
        let key = key::<A>(&aggregate.id())?;
        let read_at = aggregate.version();
        let Some((revision, stored)) = self.read(&key).await? else {
            return Err(DomainError::not_found());
        };
        let current = Version::new(stored.version);
        if current != read_at {
            return Err(stale_write::<A>(&key, read_at, current));
        }
        let next = read_at.next();
        let value = Self::value(aggregate.document()?, next)?;
        match self.store().await?.update(&key, value, revision).await {
            Ok(_) => Loaded::written(aggregate.into_inner(), next),
            // Someone wrote between the read and this update.
            Err(e) if e.kind() == kv::UpdateErrorKind::WrongLastRevision => {
                Err(stale_write::<A>(&key, read_at, next))
            }
            Err(e) => Err(DomainError::internal(format!(
                "writing `{key}` to NATS bucket: {e}"
            ))),
        }
    }

    async fn remove(&self, id: &A::Id, expected: Option<Version>) -> DomainResult<A> {
        let key = key::<A>(id)?;
        let Some((revision, stored)) = self.read(&key).await? else {
            return Err(DomainError::not_found());
        };
        let current = Version::new(stored.version);
        expect_version::<A>(id, expected, current)?;
        let aggregate = Loaded::<A>::decode(stored.document, current)?.into_inner();
        match self
            .store()
            .await?
            .delete_expect_revision(&key, Some(revision))
            .await
        {
            Ok(()) => Ok(aggregate),
            Err(e) if e.kind() == kv::UpdateErrorKind::WrongLastRevision => {
                Err(DomainError::conflict(format!(
                    "{} {key} changed while it was being removed",
                    A::KIND
                )))
            }
            Err(e) => Err(DomainError::internal(format!(
                "removing `{key}` from NATS bucket: {e}"
            ))),
        }
    }

    async fn list(&self, criteria: &Criteria) -> DomainResult<Vec<A>> {
        let store = self.store().await?;
        let mut keys: Vec<String> = store
            .keys()
            .await
            .map_err(|e| DomainError::internal(format!("listing NATS bucket keys: {e}")))?
            .try_collect()
            .await
            .map_err(|e| DomainError::internal(format!("listing NATS bucket keys: {e}")))?;
        // Key order, so ties in a sort come back the same way on every page.
        keys.sort();
        let mut documents = Vec::with_capacity(keys.len());
        for key in keys {
            // A key removed since it was listed is simply not there any more.
            if let Some((_, stored)) = self.read(&key).await? {
                documents.push((Version::new(stored.version), stored.document));
            }
        }
        self.queryable
            .select(criteria, documents)?
            .into_iter()
            .map(|(version, document)| Ok(Loaded::<A>::decode(document, version)?.into_inner()))
            .collect()
    }
}

impl<A> fmt::Debug for NatsKv<A> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("NatsKv")
            .field("bucket", &self.bucket.name)
            .finish_non_exhaustive()
    }
}

/// Readiness for a [`Nats`] connection: connected, and JetStream answering.
///
/// Anything but [`Connected`](async_nats::connection::State::Connected) is not ready,
/// which includes a client that is reconnecting because the server no longer accepts
/// its credentials.
pub struct NatsKvCheck {
    inner: Arc<Inner>,
}

#[async_trait]
impl HealthCheck for NatsKvCheck {
    fn name(&self) -> &str {
        "nats"
    }

    fn kind(&self) -> &str {
        "store"
    }

    async fn check(&self) -> Result<(), String> {
        let client = self.inner.client().await.map_err(|e| e.to_string())?;
        match client.connection_state() {
            async_nats::connection::State::Connected => {}
            state => return Err(format!("the NATS connection is {state:?}")),
        }
        jetstream::new(client.clone())
            .query_account()
            .await
            .map(drop)
            .map_err(|e| format!("JetStream is not answering: {e}"))
    }
}

/// An id as a NATS key.
fn key<A: Aggregate>(id: &A::Id) -> DomainResult<String> {
    let key = id.to_string();
    let valid = !key.is_empty()
        && !key.starts_with('.')
        && !key.ends_with('.')
        && key
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '/' | '_' | '=' | '.'));
    if valid {
        Ok(key)
    } else {
        Err(DomainError::internal(format!(
            "the {} id `{key}` cannot be a NATS key: use letters, digits and `-/_=.`",
            A::KIND
        )))
    }
}

fn stale_write<A: Aggregate>(key: &str, read_at: Version, now: Version) -> DomainError {
    DomainError::conflict(format!(
        "{} {key} was changed by someone else since it was read \
         (read at version {read_at}, now at {now})",
        A::KIND
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Serialize, Deserialize)]
    struct Note {
        id: String,
    }

    impl Aggregate for Note {
        type Id = String;
        const KIND: &'static str = "note";
        fn id(&self) -> String {
            self.id.clone()
        }
    }

    #[test]
    fn ids_become_keys_only_when_nats_accepts_them() {
        for good in ["1", "4b0c3f6e-7a1d-4d6f-9f2e-1c2b3a4d5e6f", "a/b_c=d.e"] {
            assert_eq!(key::<Note>(&good.to_string()).unwrap(), good);
        }
        for bad in ["", ".a", "a.", "with space", "ümlaut", "a*b", "a>b"] {
            assert!(key::<Note>(&bad.to_string()).is_err(), "{bad:?}");
        }
    }

    #[test]
    #[should_panic(expected = "not a valid NATS bucket name")]
    fn a_bucket_name_nats_would_refuse_is_caught_at_wiring() {
        let _ = Nats::new(
            NatsConnectionConfig::new("nats://localhost:4222"),
            "my notes",
        )
        .kv::<Note>();
    }
}
