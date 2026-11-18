//! The storage port: find an aggregate by id, change it, save it back — versioned.
//!
//! [`Store<A>`] is the one storage port the framework ships, and it is deliberately
//! narrow: identity in, aggregate out, and a version on every write so two callers
//! cannot overwrite each other. Queries phrased in your domain's language do not belong
//! here; declare them on a port of your own that extends this one (ADR-012):
//!
//! ```
//! use async_trait::async_trait;
//! use jenny_core::domain::{Aggregate, DomainResult};
//! use jenny_core::port::Store;
//! # #[derive(serde::Serialize, serde::Deserialize)] pub struct Note { id: u32 }
//! # impl Aggregate for Note { type Id = u32; const KIND: &'static str = "note"; fn id(&self) -> u32 { self.id } }
//!
//! #[async_trait]
//! pub trait NoteRepository: Store<Note> {
//!     async fn archived_before(&self, cutoff: chrono::DateTime<chrono::Utc>) -> DomainResult<Vec<Note>>;
//! }
//! ```
//!
//! A use case then never spells out load, change, save by hand — [`StoreExt::change`]
//! does it, and refuses to overwrite a concurrent write:
//!
//! ```
//! # use jenny_core::domain::{Aggregate, DomainResult, DomainError};
//! # use jenny_core::port::{Store, StoreExt};
//! # use jenny_core::memory::Memory;
//! # #[derive(serde::Serialize, serde::Deserialize)] pub struct Note { id: u32, archived: bool }
//! # impl Aggregate for Note { type Id = u32; const KIND: &'static str = "note"; fn id(&self) -> u32 { self.id } }
//! # impl Note { fn archive(&mut self) -> DomainResult<()> { self.archived = true; Ok(()) } }
//! # #[tokio::main(flavor = "current_thread")] async fn main() -> DomainResult<()> {
//! let notes = Memory::<Note>::new();
//! notes.insert(Note { id: 7, archived: false }).await?;
//!
//! let archived = notes.change(&7, Note::archive).await?;
//! assert_eq!(archived.changes.paths().collect::<Vec<_>>(), ["/archived"]);
//! assert_eq!(archived.aggregate.version().get(), 2);
//! # Ok(()) }
//! ```

use std::fmt;
use std::future::{Future, IntoFuture};
use std::ops::{Deref, DerefMut};

use async_trait::async_trait;
use futures::future::BoxFuture;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::domain::{Aggregate, Changes, Criteria, DomainError, DomainEvent, DomainResult};

use super::context::Context;
use super::events::{Events, Raised};
use super::outbound::{EventEnvelope, PublishError};

/// How many times an aggregate has been written. The first write is version 1.
///
/// A store increments it on every save and refuses a save whose aggregate was loaded at
/// an older one — optimistic concurrency, so a slow writer cannot silently undo a fast
/// one. It is also what an HTTP adapter turns into an `ETag`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Version(u64);

impl Version {
    /// The version of a freshly inserted aggregate.
    pub const FIRST: Version = Version(1);

    pub fn new(version: u64) -> Self {
        Self(version)
    }

    pub fn get(self) -> u64 {
        self.0
    }

    /// The version the next successful save produces.
    pub fn next(self) -> Self {
        Self(self.0 + 1)
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// An aggregate as a store handed it out: the value, the version it was read at, and
/// the document it was read from.
///
/// It dereferences to the aggregate, so domain methods are called on it directly.
/// Handing it back to [`Store::save`] is what makes the save conditional on nobody
/// else having written since. The version never enters the domain model.
#[derive(Clone, Debug)]
pub struct Loaded<A> {
    aggregate: A,
    version: Version,
    stored: Value,
}

impl<A: Aggregate> Loaded<A> {
    /// Decode a stored document. For store adapters.
    ///
    /// A document that no longer decodes — an older release wrote a blank title, or
    /// someone edited the table by hand — is an internal failure, not the caller's:
    /// the value objects' constructors refused it, which is exactly their job.
    pub fn decode(document: Value, version: Version) -> DomainResult<Self> {
        let aggregate = A::deserialize(&document)
            .map_err(|e| DomainError::internal(format!("a stored {} is unusable: {e}", A::KIND)))?;
        Ok(Self {
            aggregate,
            version,
            stored: document,
        })
    }

    /// Wrap an aggregate a store has just written at `version`. For store adapters.
    pub fn written(aggregate: A, version: Version) -> DomainResult<Self> {
        let stored = encode(&aggregate)?;
        Ok(Self {
            aggregate,
            version,
            stored,
        })
    }

    /// The version this was read at.
    pub fn version(&self) -> Version {
        self.version
    }

    /// The document as it was read, before any change made since.
    pub fn stored(&self) -> &Value {
        &self.stored
    }

    /// The aggregate's current state, encoded the way a store writes it.
    pub fn document(&self) -> DomainResult<Value> {
        encode(&self.aggregate)
    }

    /// What has changed since this was read.
    pub fn changes(&self) -> DomainResult<Changes> {
        Ok(Changes::between(&self.stored, &self.document()?))
    }

    pub fn into_inner(self) -> A {
        self.aggregate
    }
}

impl<A> Deref for Loaded<A> {
    type Target = A;

    fn deref(&self) -> &A {
        &self.aggregate
    }
}

impl<A> DerefMut for Loaded<A> {
    fn deref_mut(&mut self) -> &mut A {
        &mut self.aggregate
    }
}

/// Check a caller's expected version against the current one. For store adapters and
/// [`StoreExt`]: `Ok` when there was no expectation or it holds,
/// [`Stale`](DomainError::Stale) naming both versions otherwise.
pub fn expect_version<A: Aggregate>(
    id: &A::Id,
    expected: Option<Version>,
    current: Version,
) -> DomainResult<()> {
    match expected {
        Some(expected) if expected != current => Err(DomainError::stale(format!(
            "{} {id} is at version {current}, not {expected}",
            A::KIND
        ))),
        _ => Ok(()),
    }
}

/// Encode an aggregate as a store writes it. For store adapters.
pub fn encode<A: Aggregate>(aggregate: &A) -> DomainResult<Value> {
    serde_json::to_value(aggregate)
        .map_err(|e| DomainError::internal(format!("could not encode a {}: {e}", A::KIND)))
}

/// Where aggregates of one kind live.
///
/// Implemented by store adapters — [`Memory`](crate::memory::Memory) in this crate,
/// database-backed ones in the persistence crates — and object-safe, so use cases hold
/// an `Arc<dyn Store<Note>>`, or an `Arc<dyn NoteRepository>` when the domain needs
/// queries of its own.
///
/// Every write is versioned. That is not optional, because the failure it prevents —
/// two requests read version 3, both write, the first write is lost without anyone
/// being told — is silent, and silent data loss is the worst kind.
#[async_trait]
pub trait Store<A: Aggregate>: Send + Sync + 'static {
    /// The aggregate with this id, or `None` when there is none.
    async fn find(&self, id: &A::Id) -> DomainResult<Option<Loaded<A>>>;

    /// Store a new aggregate at [`Version::FIRST`].
    ///
    /// [`Conflict`](DomainError::Conflict) when one with the same id already exists.
    async fn insert(&self, aggregate: A) -> DomainResult<Loaded<A>>;

    /// Write back an aggregate that was read at `aggregate.version()`, producing the
    /// next version.
    ///
    /// [`Conflict`](DomainError::Conflict) when someone else saved it in between,
    /// [`NotFound`](DomainError::NotFound) when it was removed.
    async fn save(&self, aggregate: Loaded<A>) -> DomainResult<Loaded<A>>;

    /// Remove an aggregate and return it.
    ///
    /// With an `expected` version, only if that is still the current one:
    /// [`Stale`](DomainError::Stale) otherwise, decided atomically with the removal.
    /// [`NotFound`](DomainError::NotFound) when there was nothing to remove.
    async fn remove(&self, id: &A::Id, expected: Option<Version>) -> DomainResult<A>;

    /// One page of aggregates matching `criteria`.
    ///
    /// Criteria name attributes with client-supplied paths, so an adapter must refuse
    /// any path outside its allowlist with [`Invalid`](DomainError::Invalid) — the same
    /// rule [`Criteria::ensure_allowed`] encodes (ADR-010).
    async fn list(&self, criteria: &Criteria) -> DomainResult<Vec<A>>;

    /// [`insert`](Self::insert), keeping `events` with the write when this store can
    /// commit them together — an outbox in the same transaction (ADR-016).
    ///
    /// Returns the events it did not keep. The caller publishes those once the write
    /// has succeeded, which is all a store without an outbox can offer; the default
    /// keeps none.
    async fn insert_raising(
        &self,
        aggregate: A,
        events: Vec<EventEnvelope>,
    ) -> DomainResult<(Loaded<A>, Vec<EventEnvelope>)> {
        Ok((self.insert(aggregate).await?, events))
    }

    /// [`save`](Self::save), keeping `events` with the write when this store can. See
    /// [`insert_raising`](Self::insert_raising).
    async fn save_raising(
        &self,
        aggregate: Loaded<A>,
        events: Vec<EventEnvelope>,
    ) -> DomainResult<(Loaded<A>, Vec<EventEnvelope>)> {
        Ok((self.save(aggregate).await?, events))
    }
}

/// The load-change-save sequence every command repeats, written once.
///
/// Implemented for every [`Store`], including `dyn Store<A>` and any domain port that
/// extends it.
pub trait StoreExt<A: Aggregate>: Store<A> {
    /// [`find`](Store::find), failing with [`NotFound`](DomainError::NotFound) when
    /// there is nothing there.
    fn load(&self, id: &A::Id) -> impl Future<Output = DomainResult<Loaded<A>>> + Send;

    /// Load an aggregate, apply `change` to it, and save it if anything changed.
    ///
    /// `change` is usually a domain method — `Note::archive` — or a closure calling
    /// one. Whatever it returns, typically the event it raised, comes back as
    /// [`Changed::outcome`]. When it fails, nothing is saved. When it succeeds without
    /// changing anything, nothing is written either and the version stays put, so
    /// repeating a request is harmless.
    ///
    /// Await it as it is, or say more first:
    ///
    /// - [`expecting`](Change::expecting) — only if the aggregate is still at the
    ///   version the caller read (an HTTP `If-Match`); [`Stale`](DomainError::Stale)
    ///   otherwise.
    /// - [`raising`](Change::raising) — publish what `change` returned, with the write:
    ///   in the same transaction when the store has an outbox, right after it otherwise.
    ///
    /// A save that loses the race against another writer fails with
    /// [`Conflict`](DomainError::Conflict) rather than overwriting it.
    fn change<'a, T, F>(&'a self, id: &'a A::Id, change: F) -> Change<'a, A, Self, F>
    where
        F: FnOnce(&mut A) -> DomainResult<T> + Send + 'a,
        T: Send + 'a;

    /// Insert `aggregate` together with what creating it raised. Await it after
    /// [`raising`](Add::raising) — an `add` that forgot its events would lose them, so it
    /// is not awaitable on its own; [`insert`](Store::insert) is for aggregates that
    /// raise nothing.
    fn add<'a, T>(&'a self, aggregate: A, raised: T) -> Add<'a, A, Self, T>
    where
        T: Raised + 'a;
}

impl<A, S> StoreExt<A> for S
where
    A: Aggregate,
    S: Store<A> + ?Sized,
{
    // `async fn` here, `impl Future + Send` in the trait: the trait's signature is the
    // contract, and it is what makes the futures usable from a multi-threaded handler.
    async fn load(&self, id: &A::Id) -> DomainResult<Loaded<A>> {
        self.find(id).await?.ok_or_else(DomainError::not_found)
    }

    fn change<'a, T, F>(&'a self, id: &'a A::Id, change: F) -> Change<'a, A, Self, F>
    where
        F: FnOnce(&mut A) -> DomainResult<T> + Send + 'a,
        T: Send + 'a,
    {
        Change {
            store: self,
            id,
            change,
            expected: None,
        }
    }

    fn add<'a, T>(&'a self, aggregate: A, raised: T) -> Add<'a, A, Self, T>
    where
        T: Raised + 'a,
    {
        Add {
            store: self,
            aggregate,
            raised,
        }
    }
}

/// A change about to be made. See [`StoreExt::change`].
#[must_use = "a change does nothing until it is awaited"]
pub struct Change<'a, A: Aggregate, S: ?Sized, F> {
    store: &'a S,
    id: &'a A::Id,
    change: F,
    expected: Option<Version>,
}

impl<'a, A, S, F, T> Change<'a, A, S, F>
where
    A: Aggregate,
    S: Store<A> + ?Sized,
    F: FnOnce(&mut A) -> DomainResult<T> + Send + 'a,
    T: Send + 'a,
{
    /// Only if the aggregate is still at `expected`; `None` means no expectation.
    ///
    /// Fails with [`Stale`](DomainError::Stale) when the version read is not the one
    /// expected, and also when another writer saves between that read and this save:
    /// either way the caller's view is out of date, and it asked to be told.
    pub fn expecting(mut self, expected: Option<Version>) -> Self {
        self.expected = expected;
        self
    }

    /// Publish what the change returned — an event, an `Option` or `Vec` of them — with
    /// the write.
    ///
    /// A store with an outbox commits the events in the same transaction as the
    /// aggregate, so either both happen or neither does. Any other store saves first
    /// and publishes right after; nothing is published when the change or the save
    /// fails. When the change turns out to change nothing, there is no write, and the
    /// events are published as they are.
    pub fn raising(self, events: &'a Events, cx: &'a Context) -> RaisingChange<'a, A, S, F> {
        RaisingChange {
            change: self,
            events,
            cx,
            announce: false,
        }
    }
}

impl<'a, A, S, F, T> IntoFuture for Change<'a, A, S, F>
where
    A: Aggregate,
    S: Store<A> + ?Sized,
    F: FnOnce(&mut A) -> DomainResult<T> + Send + 'a,
    T: Send + 'a,
{
    type Output = DomainResult<Changed<A, T>>;
    type IntoFuture = BoxFuture<'a, Self::Output>;

    fn into_future(self) -> Self::IntoFuture {
        Box::pin(apply(self, None, |_, _| Ok(Vec::new())))
    }
}

/// A change whose events are published with it. See [`Change::raising`].
#[must_use = "a change does nothing until it is awaited"]
pub struct RaisingChange<'a, A: Aggregate, S: ?Sized, F> {
    change: Change<'a, A, S, F>,
    events: &'a Events,
    cx: &'a Context,
    announce: bool,
}

impl<'a, A, S, F, T> RaisingChange<'a, A, S, F>
where
    A: Aggregate,
    S: Store<A> + ?Sized,
    F: FnOnce(&mut A) -> DomainResult<T> + Send + 'a,
    T: Raised + 'a,
{
    /// See [`Change::expecting`].
    pub fn expecting(mut self, expected: Option<Version>) -> Self {
        self.change.expected = expected;
        self
    }

    /// Also publish an `<kind>.updated` event carrying what changed, as a JSON Patch
    /// ([`Updated`]), when anything did — for listeners that mirror state rather than
    /// react to what the domain said happened.
    pub fn announcing_changes(mut self) -> Self {
        self.announce = true;
        self
    }
}

impl<'a, A, S, F, T> IntoFuture for RaisingChange<'a, A, S, F>
where
    A: Aggregate,
    S: Store<A> + ?Sized,
    F: FnOnce(&mut A) -> DomainResult<T> + Send + 'a,
    T: Raised + 'a,
{
    type Output = DomainResult<Changed<A, T>>;
    type IntoFuture = BoxFuture<'a, Self::Output>;

    fn into_future(self) -> Self::IntoFuture {
        let Self {
            change,
            events,
            cx,
            announce,
        } = self;
        Box::pin(apply(
            change,
            Some((events, cx)),
            move |outcome: &T, updated| {
                let mut envelopes = outcome.envelopes(cx).map_err(encoding)?;
                if announce && let Some(updated) = updated {
                    envelopes.extend(updated.envelopes(cx).map_err(encoding)?);
                }
                Ok(envelopes)
            },
        ))
    }
}

/// Load, check, change, and save with whatever events `encode` makes of the outcome.
async fn apply<'a, A, S, F, T, E>(
    change: Change<'a, A, S, F>,
    deliver: Option<(&'a Events, &'a Context)>,
    encode: E,
) -> DomainResult<Changed<A, T>>
where
    A: Aggregate,
    S: Store<A> + ?Sized,
    F: FnOnce(&mut A) -> DomainResult<T> + Send + 'a,
    T: Send + 'a,
    E: FnOnce(&T, Option<&Updated>) -> DomainResult<Vec<EventEnvelope>> + Send,
{
    let Change {
        store,
        id,
        change,
        expected,
    } = change;
    let mut aggregate = store.load(id).await?;
    expect_version::<A>(id, expected, aggregate.version())?;
    let outcome = change(&mut aggregate)?;
    let changes = aggregate.changes()?;

    if changes.is_empty() {
        // Nothing to write, so nothing to commit the events with; what the domain
        // raised still happened.
        let envelopes = encode(&outcome, None)?;
        if let Some((events, cx)) = deliver {
            events.deliver(envelopes, cx).await;
        }
        return Ok(Changed {
            outcome,
            changes,
            aggregate,
        });
    }

    // Encoded before the write: an event that cannot be encoded is a bug, and the
    // change must not be saved without it.
    let updated = Updated::new::<A>(&aggregate.id(), aggregate.version().next(), changes.clone());
    let envelopes = encode(&outcome, Some(&updated))?;
    let (aggregate, unsent) = match store.save_raising(aggregate, envelopes).await {
        // Read at the expected version, but someone saved since: still stale.
        Err(DomainError::Conflict { detail }) if expected.is_some() => {
            return Err(DomainError::stale(detail));
        }
        saved => saved?,
    };
    if let Some((events, cx)) = deliver {
        events.deliver(unsent, cx).await;
    }
    Ok(Changed {
        outcome,
        changes,
        aggregate,
    })
}

/// An aggregate about to be inserted with what creating it raised. See
/// [`StoreExt::add`].
#[must_use = "an add does nothing until it is raised and awaited"]
pub struct Add<'a, A, S: ?Sized, T> {
    store: &'a S,
    aggregate: A,
    raised: T,
}

impl<'a, A, S, T> Add<'a, A, S, T>
where
    A: Aggregate,
    S: Store<A> + ?Sized,
    T: Raised + 'a,
{
    /// Publish what creating the aggregate raised, with the insert. See
    /// [`Change::raising`].
    pub fn raising(self, events: &'a Events, cx: &'a Context) -> RaisingAdd<'a, A, S, T> {
        RaisingAdd {
            add: self,
            events,
            cx,
        }
    }
}

/// An insert whose events are published with it. See [`Add::raising`].
#[must_use = "an add does nothing until it is awaited"]
pub struct RaisingAdd<'a, A, S: ?Sized, T> {
    add: Add<'a, A, S, T>,
    events: &'a Events,
    cx: &'a Context,
}

impl<'a, A, S, T> IntoFuture for RaisingAdd<'a, A, S, T>
where
    A: Aggregate,
    S: Store<A> + ?Sized,
    T: Raised + 'a,
{
    type Output = DomainResult<Loaded<A>>;
    type IntoFuture = BoxFuture<'a, Self::Output>;

    fn into_future(self) -> Self::IntoFuture {
        let Self { add, events, cx } = self;
        Box::pin(async move {
            let envelopes = add.raised.envelopes(cx).map_err(encoding)?;
            let (inserted, unsent) = add.store.insert_raising(add.aggregate, envelopes).await?;
            events.deliver(unsent, cx).await;
            Ok(inserted)
        })
    }
}

/// An event that cannot be encoded is a bug in its `Serialize`, not the caller's fault.
fn encoding(error: PublishError) -> DomainError {
    DomainError::internal(format!("could not encode an event: {error}"))
}

/// What [`StoreExt::change`] did.
#[derive(Debug)]
pub struct Changed<A, T> {
    /// What the change function returned — usually the event it raised.
    pub outcome: T,
    /// What changed in the stored form. Empty when the change was a no-op.
    pub changes: Changes,
    /// The aggregate as saved, at its new version.
    pub aggregate: Loaded<A>,
}

impl<A: Aggregate, T> Changed<A, T> {
    /// An `<kind>.updated` event carrying the id, the new version and the changes, or
    /// `None` when nothing changed.
    ///
    /// For listeners that want to mirror state — an audit log, a search index, a
    /// read model — rather than react to what the domain said happened.
    pub fn updated(&self) -> Option<Updated> {
        if self.changes.is_empty() {
            return None;
        }
        Some(Updated::new::<A>(
            &self.aggregate.id(),
            self.aggregate.version(),
            self.changes.clone(),
        ))
    }
}

/// An aggregate changed; here is the patch. See [`Changed::updated`].
///
/// ```json
/// { "id": "4b0c…", "version": 3,
///   "changes": [{ "op": "replace", "path": "/title", "value": "Groceries" }] }
/// ```
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Updated {
    #[serde(skip)]
    name: String,
    pub id: String,
    pub version: Version,
    pub changes: Changes,
}

impl Updated {
    fn new<A: Aggregate>(id: &A::Id, version: Version, changes: Changes) -> Self {
        Self {
            name: format!("{}.updated", A::KIND),
            id: id.to_string(),
            version,
            changes,
        }
    }
}

impl DomainEvent for Updated {
    fn name(&self) -> &str {
        &self.name
    }
}
