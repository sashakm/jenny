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
use std::future::Future;
use std::ops::{Deref, DerefMut};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::domain::{Aggregate, Changes, Criteria, DomainError, DomainEvent, DomainResult};

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

    /// Remove an aggregate and return it. [`NotFound`](DomainError::NotFound) when
    /// there was nothing to remove.
    async fn remove(&self, id: &A::Id) -> DomainResult<A>;

    /// One page of aggregates matching `criteria`.
    ///
    /// Criteria name attributes with client-supplied paths, so an adapter must refuse
    /// any path outside its allowlist with [`Invalid`](DomainError::Invalid) — the same
    /// rule [`Criteria::ensure_allowed`] encodes (ADR-010).
    async fn list(&self, criteria: &Criteria) -> DomainResult<Vec<A>>;
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
    /// A save that loses the race against another writer fails with
    /// [`Conflict`](DomainError::Conflict) rather than overwriting it.
    fn change<T, F>(
        &self,
        id: &A::Id,
        change: F,
    ) -> impl Future<Output = DomainResult<Changed<A, T>>> + Send
    where
        F: FnOnce(&mut A) -> DomainResult<T> + Send,
        T: Send;
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

    async fn change<T, F>(&self, id: &A::Id, change: F) -> DomainResult<Changed<A, T>>
    where
        F: FnOnce(&mut A) -> DomainResult<T> + Send,
        T: Send,
    {
        let mut aggregate = self.load(id).await?;
        let outcome = change(&mut aggregate)?;
        let changes = aggregate.changes()?;
        let aggregate = if changes.is_empty() {
            aggregate
        } else {
            self.save(aggregate).await?
        };
        Ok(Changed {
            outcome,
            changes,
            aggregate,
        })
    }
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
        Some(Updated {
            name: format!("{}.updated", A::KIND),
            id: self.aggregate.id().to_string(),
            version: self.aggregate.version(),
            changes: self.changes.clone(),
        })
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

impl DomainEvent for Updated {
    fn name(&self) -> &str {
        &self.name
    }
}
