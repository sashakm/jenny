//! An in-memory [`Store`] — for tests, prototypes and services that start without a
//! database.
//!
//! A driven adapter, living in the core for the same reason [`task`](crate::task) does:
//! it needs nothing beyond what the core already depends on, and every service wants
//! it in its tests.

use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::marker::PhantomData;
use std::sync::{Mutex, MutexGuard};

use async_trait::async_trait;
use serde_json::Value;

use crate::domain::{Aggregate, Check, Criteria, DomainError, DomainResult, Filter, ValueKind};
use crate::port::outbound::HealthCheck;
use crate::port::store::{Loaded, Store, Version, encode, expect_version};

/// Aggregates kept in a map, as the JSON documents a database-backed store would write.
///
/// Holding documents rather than aggregates is deliberate: every save goes through the
/// same encoding and every read through the same decoding as a real store, so a value
/// object that cannot round-trip fails here, in a unit test, rather than against
/// Postgres in staging.
///
/// ```
/// # use jenny_core::domain::{Aggregate, Criteria, ValueKind};
/// # use jenny_core::port::Store;
/// use jenny_core::memory::Memory;
/// # #[derive(serde::Serialize, serde::Deserialize)] pub struct Note { id: u32, title: String }
/// # impl Aggregate for Note { type Id = u32; const KIND: &'static str = "note"; fn id(&self) -> u32 { self.id } }
///
/// # #[tokio::main(flavor = "current_thread")] async fn main() -> jenny_core::domain::DomainResult<()> {
/// let notes = Memory::<Note>::new().queryable([("title", ValueKind::Text)]);
/// notes.insert(Note { id: 1, title: "Shopping".into() }).await?;
/// notes.insert(Note { id: 2, title: "Gardening".into() }).await?;
///
/// let criteria = Criteria::new().with_filter("title:begins:shop".parse()?);
/// assert_eq!(notes.list(&criteria).await?.len(), 1);
/// # Ok(()) }
/// ```
///
/// ### Listing
///
/// Criteria paths are dotted paths into the document (`author.name`), and only those
/// given to [`queryable`](Self::queryable) may be used — none by default, because a
/// filter is also a question about the data. `filter=password_hash:begins:a` answered
/// truthfully, one character at a time, discloses a field the API never returns.
///
/// Each path comes with its [`ValueKind`], so a filter value is checked the way a
/// database store checks it: `priority:gt:banana` is a 400, not an empty page. The same
/// allowlist serves every store, so moving to Postgres does not mean rewriting it.
///
/// Text compares case-insensitively and numbers numerically. Sorting puts missing and
/// `null` values last, as Postgres does, and ties keep key order, so paging is stable.
pub struct Memory<A> {
    documents: Mutex<BTreeMap<String, Entry>>,
    queryable: Queryable,
    name: String,
    kind: PhantomData<fn() -> A>,
}

#[derive(Clone)]
struct Entry {
    version: Version,
    document: Value,
}

impl<A: Aggregate> Memory<A> {
    pub fn new() -> Self {
        Self {
            documents: Mutex::new(BTreeMap::new()),
            queryable: Queryable::default(),
            name: format!("{}-store", A::KIND),
            kind: PhantomData,
        }
    }

    /// The attribute paths a listing may sort and filter on, and what each holds.
    pub fn queryable<I, P>(mut self, paths: I) -> Self
    where
        I: IntoIterator<Item = (P, ValueKind)>,
        P: Into<String>,
    {
        self.queryable = Queryable::new(paths);
        self
    }

    /// Store a raw document under `key` at [`Version::FIRST`], bypassing encoding.
    ///
    /// For fixtures, and for testing how a service copes with data it would not have
    /// written itself — an older release's, or a hand-edited one.
    pub fn seed(&self, key: impl Into<String>, document: Value) {
        self.lock_unpoisoned().insert(
            key.into(),
            Entry {
                version: Version::FIRST,
                document,
            },
        );
    }

    pub fn len(&self) -> usize {
        self.lock_unpoisoned().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn lock(&self) -> DomainResult<MutexGuard<'_, BTreeMap<String, Entry>>> {
        // A poisoned lock means a thread panicked mid-write: internal, not the caller's.
        self.documents
            .lock()
            .map_err(|e| DomainError::internal(format!("{} is poisoned: {e}", self.name)))
    }

    fn lock_unpoisoned(&self) -> MutexGuard<'_, BTreeMap<String, Entry>> {
        self.documents
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl<A: Aggregate> Default for Memory<A> {
    fn default() -> Self {
        Self::new()
    }
}

impl<A> std::fmt::Debug for Memory<A> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Memory")
            .field("name", &self.name)
            .field("queryable", &self.queryable)
            .finish_non_exhaustive()
    }
}

#[async_trait]
impl<A: Aggregate> Store<A> for Memory<A> {
    async fn find(&self, id: &A::Id) -> DomainResult<Option<Loaded<A>>> {
        let entry = self.lock()?.get(&id.to_string()).cloned();
        entry
            .map(|entry| Loaded::decode(entry.document, entry.version))
            .transpose()
    }

    async fn insert(&self, aggregate: A) -> DomainResult<Loaded<A>> {
        let key = aggregate.id().to_string();
        let document = encode(&aggregate)?;
        {
            let mut documents = self.lock()?;
            if documents.contains_key(&key) {
                return Err(DomainError::conflict(format!(
                    "{} {key} already exists",
                    A::KIND
                )));
            }
            documents.insert(
                key,
                Entry {
                    version: Version::FIRST,
                    document,
                },
            );
        }
        Loaded::written(aggregate, Version::FIRST)
    }

    async fn save(&self, aggregate: Loaded<A>) -> DomainResult<Loaded<A>> {
        let key = aggregate.id().to_string();
        let document = aggregate.document()?;
        let next = {
            let mut documents = self.lock()?;
            let entry = documents.get_mut(&key).ok_or_else(DomainError::not_found)?;
            if entry.version != aggregate.version() {
                return Err(DomainError::conflict(format!(
                    "{} {key} was changed by someone else since it was read \
                     (read at version {}, now at {})",
                    A::KIND,
                    aggregate.version(),
                    entry.version
                )));
            }
            entry.version = entry.version.next();
            entry.document = document;
            entry.version
        };
        Loaded::written(aggregate.into_inner(), next)
    }

    async fn remove(&self, id: &A::Id, expected: Option<Version>) -> DomainResult<A> {
        let key = id.to_string();
        let entry = {
            let mut documents = self.lock()?;
            let current = documents.get(&key).ok_or_else(DomainError::not_found)?;
            // Under the same lock as the removal, so nobody can save in between.
            expect_version::<A>(id, expected, current.version)?;
            documents.remove(&key).expect("checked above")
        };
        Ok(Loaded::<A>::decode(entry.document, entry.version)?.into_inner())
    }

    async fn list(&self, criteria: &Criteria) -> DomainResult<Vec<A>> {
        // Snapshot first, so the lock is not held while documents are compared.
        let documents: Vec<(Version, Value)> = self
            .lock()?
            .values()
            .map(|entry| (entry.version, entry.document.clone()))
            .collect();
        self.queryable
            .select(criteria, documents)?
            .into_iter()
            .map(|(version, document)| Ok(Loaded::<A>::decode(document, version)?.into_inner()))
            .collect()
    }
}

/// An allowlist of attribute paths, and listing by [`Criteria`] over JSON documents
/// held in memory.
///
/// What [`Memory`] lists with, public for stores that cannot query either — a key-value
/// bucket — so they filter, sort and page exactly like it. Nothing is queryable until
/// a path is allowed, because a filter is also a question about the data.
///
/// ```
/// use jenny_core::domain::{Criteria, ValueKind};
/// use jenny_core::memory::Queryable;
/// use jenny_core::port::Version;
/// use serde_json::json;
///
/// let queryable = Queryable::new([("title", ValueKind::Text)]);
/// let documents = vec![
///     (Version::FIRST, json!({ "title": "Shopping" })),
///     (Version::FIRST, json!({ "title": "Gardening" })),
/// ];
/// let criteria = Criteria::new().with_filter("title:begins:shop".parse()?);
/// assert_eq!(queryable.select(&criteria, documents)?.len(), 1);
/// # Ok::<(), jenny_core::domain::DomainError>(())
/// ```
#[derive(Clone, Debug, Default)]
pub struct Queryable(BTreeMap<String, ValueKind>);

impl Queryable {
    pub fn new<I, P>(paths: I) -> Self
    where
        I: IntoIterator<Item = (P, ValueKind)>,
        P: Into<String>,
    {
        Self(
            paths
                .into_iter()
                .map(|(path, kind)| (path.into(), kind))
                .collect(),
        )
    }

    /// The documents matching `criteria`, sorted and paged.
    ///
    /// `documents` must come in a stable order — key order, say — because ties keep
    /// it, and that is what makes paging over a sort with duplicates consistent. Fails
    /// with [`Invalid`](DomainError::Invalid) for a path outside the allowlist or a
    /// value its kind cannot hold.
    pub fn select(
        &self,
        criteria: &Criteria,
        documents: Vec<(Version, Value)>,
    ) -> DomainResult<Vec<(Version, Value)>> {
        let allowed: Vec<&str> = self.0.keys().map(String::as_str).collect();
        criteria.ensure_allowed(&allowed)?;
        for filter in &criteria.filter {
            self.0[filter.path()].accept(filter)?;
        }

        let mut found: Vec<(Version, Value)> = documents
            .into_iter()
            .filter(|(_, document)| criteria.filter.iter().all(|f| matches(document, f)))
            .collect();

        // Later keys first, so earlier ones win under a stable sort — what a client
        // writing `sort=a&sort=b` means. Ties keep the order they came in.
        for key in criteria.sort.iter().rev() {
            found.sort_by(|(_, left), (_, right)| {
                let ordering = compare(lookup(left, key.path()), lookup(right, key.path()));
                match key.ordering() {
                    crate::domain::Ordering::Asc => ordering,
                    crate::domain::Ordering::Desc => ordering.reverse(),
                }
            });
        }

        Ok(found
            .into_iter()
            .skip(criteria.page.offset() as usize)
            .take(criteria.page.limit() as usize)
            .collect())
    }
}

#[async_trait]
impl<A: Aggregate> HealthCheck for Memory<A> {
    fn name(&self) -> &str {
        &self.name
    }

    fn kind(&self) -> &str {
        "store"
    }

    async fn check(&self) -> Result<(), String> {
        self.lock().map(drop).map_err(|e| e.to_string())
    }
}

/// The value at a dotted path, if the document has one there.
fn lookup<'a>(document: &'a Value, path: &str) -> Option<&'a Value> {
    path.split('.')
        .try_fold(document, |value, segment| value.get(segment))
}

fn matches(document: &Value, filter: &Filter) -> bool {
    let Some(value) = lookup(document, filter.path()) else {
        return false;
    };
    let wanted = filter.value();
    match filter.check() {
        Check::Eq => compare_to(value, wanted) == Some(Ordering::Equal),
        Check::Gt => compare_to(value, wanted) == Some(Ordering::Greater),
        Check::St => compare_to(value, wanted) == Some(Ordering::Less),
        Check::Begins | Check::Ends | Check::Contains => {
            let Value::String(text) = value else {
                return false;
            };
            let (text, wanted) = (text.to_lowercase(), wanted.to_lowercase());
            match filter.check() {
                Check::Begins => text.starts_with(&wanted),
                Check::Ends => text.ends_with(&wanted),
                _ => text.contains(&wanted),
            }
        }
    }
}

/// How a stored value compares to a filter's text. `None` when they cannot be compared
/// — a number against `banana` — which matches nothing rather than guessing.
fn compare_to(value: &Value, wanted: &str) -> Option<Ordering> {
    match value {
        Value::String(text) => Some(text.to_lowercase().cmp(&wanted.to_lowercase())),
        Value::Number(number) => number.as_f64()?.partial_cmp(&wanted.parse::<f64>().ok()?),
        Value::Bool(flag) => Some(flag.cmp(&wanted.to_ascii_lowercase().parse().ok()?)),
        _ => None,
    }
}

/// Order two values for sorting: booleans, then numbers, then text, then anything
/// else, with missing and `null` last.
fn compare(left: Option<&Value>, right: Option<&Value>) -> Ordering {
    fn rank(value: Option<&Value>) -> u8 {
        match value {
            Some(Value::Bool(_)) => 0,
            Some(Value::Number(_)) => 1,
            Some(Value::String(_)) => 2,
            Some(Value::Array(_) | Value::Object(_)) => 3,
            Some(Value::Null) | None => 4,
        }
    }
    match (left, right) {
        (Some(Value::Bool(a)), Some(Value::Bool(b))) => a.cmp(b),
        (Some(Value::Number(a)), Some(Value::Number(b))) => a
            .as_f64()
            .partial_cmp(&b.as_f64())
            .unwrap_or(Ordering::Equal),
        (Some(Value::String(a)), Some(Value::String(b))) => a
            .to_lowercase()
            .cmp(&b.to_lowercase())
            .then_with(|| a.cmp(b)),
        _ => rank(left).cmp(&rank(right)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::port::StoreExt;
    use serde::{Deserialize, Serialize};
    use serde_json::json;

    #[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
    struct Note {
        id: u32,
        title: Title,
        priority: u8,
        archived: bool,
    }

    /// A value object that refuses blanks, including when it is loaded.
    #[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
    #[serde(try_from = "String", into = "String")]
    struct Title(String);

    impl TryFrom<String> for Title {
        type Error = String;
        fn try_from(raw: String) -> Result<Self, String> {
            if raw.trim().is_empty() {
                Err("title must not be empty".into())
            } else {
                Ok(Self(raw))
            }
        }
    }

    impl From<Title> for String {
        fn from(title: Title) -> String {
            title.0
        }
    }

    impl Aggregate for Note {
        type Id = u32;
        const KIND: &'static str = "note";
        fn id(&self) -> u32 {
            self.id
        }
    }

    impl Note {
        fn new(id: u32, title: &str, priority: u8) -> Self {
            Self {
                id,
                title: Title(title.into()),
                priority,
                archived: false,
            }
        }

        fn archive(&mut self) -> DomainResult<&'static str> {
            if self.archived {
                return Err(DomainError::conflict("already archived"));
            }
            self.archived = true;
            Ok("archived")
        }
    }

    fn store() -> Memory<Note> {
        Memory::new().queryable([
            ("title", ValueKind::Text),
            ("priority", ValueKind::Integer),
            ("archived", ValueKind::Bool),
        ])
    }

    #[tokio::test]
    async fn an_inserted_aggregate_comes_back_at_the_first_version() {
        let notes = store();
        notes.insert(Note::new(1, "Shopping", 2)).await.unwrap();

        let found = notes.load(&1).await.unwrap();
        assert_eq!(found.title.0, "Shopping");
        assert_eq!(found.version(), Version::FIRST);
        assert!(notes.find(&2).await.unwrap().is_none());
        assert!(matches!(
            notes.load(&2).await.unwrap_err(),
            DomainError::NotFound
        ));
    }

    #[tokio::test]
    async fn inserting_an_existing_id_conflicts() {
        let notes = store();
        notes.insert(Note::new(1, "a", 1)).await.unwrap();
        assert!(matches!(
            notes.insert(Note::new(1, "b", 1)).await.unwrap_err(),
            DomainError::Conflict { .. }
        ));
    }

    #[tokio::test]
    async fn a_change_is_saved_at_the_next_version_with_its_patch() {
        let notes = store();
        notes.insert(Note::new(1, "Shopping", 2)).await.unwrap();

        let changed = notes.change(&1, Note::archive).await.unwrap();
        assert_eq!(changed.outcome, "archived");
        assert_eq!(changed.changes.paths().collect::<Vec<_>>(), ["/archived"]);
        assert_eq!(changed.aggregate.version().get(), 2);
        assert!(notes.load(&1).await.unwrap().archived);

        let updated = changed.updated().unwrap();
        assert_eq!(crate::domain::DomainEvent::name(&updated), "note.updated");
        assert_eq!(
            serde_json::to_value(&updated).unwrap(),
            json!({
                "id": "1",
                "version": 2,
                "changes": [{ "op": "replace", "path": "/archived", "value": true }]
            })
        );
    }

    #[tokio::test]
    async fn a_failed_change_saves_nothing() {
        let notes = store();
        notes.insert(Note::new(1, "Shopping", 2)).await.unwrap();
        notes.change(&1, Note::archive).await.unwrap();

        assert!(matches!(
            notes.change(&1, Note::archive).await.unwrap_err(),
            DomainError::Conflict { .. }
        ));
        assert_eq!(notes.load(&1).await.unwrap().version().get(), 2);
    }

    #[tokio::test]
    async fn a_change_that_changes_nothing_writes_nothing() {
        let notes = store();
        notes.insert(Note::new(1, "Shopping", 2)).await.unwrap();

        let changed = notes.change(&1, |note| {
            note.priority = 2;
            Ok(())
        });
        let changed = changed.await.unwrap();
        assert!(changed.changes.is_empty());
        assert!(changed.updated().is_none());
        assert_eq!(notes.load(&1).await.unwrap().version(), Version::FIRST);
    }

    #[tokio::test]
    async fn saving_a_stale_read_conflicts_instead_of_overwriting() {
        let notes = store();
        notes.insert(Note::new(1, "Shopping", 2)).await.unwrap();

        let mut first = notes.load(&1).await.unwrap();
        let mut second = notes.load(&1).await.unwrap();
        first.priority = 5;
        notes.save(first).await.unwrap();

        second.priority = 1;
        let err = notes.save(second).await.unwrap_err();
        assert!(matches!(err, DomainError::Conflict { .. }), "{err}");
        assert_eq!(notes.load(&1).await.unwrap().priority, 5);
    }

    #[tokio::test]
    async fn saving_something_removed_meanwhile_is_not_found() {
        let notes = store();
        notes.insert(Note::new(1, "Shopping", 2)).await.unwrap();
        let loaded = notes.load(&1).await.unwrap();
        notes.remove(&1, None).await.unwrap();

        assert!(matches!(
            notes.save(loaded).await.unwrap_err(),
            DomainError::NotFound
        ));
    }

    #[tokio::test]
    async fn removing_returns_the_aggregate_then_reports_not_found() {
        let notes = store();
        notes.insert(Note::new(1, "Shopping", 2)).await.unwrap();

        assert_eq!(notes.remove(&1, None).await.unwrap().title.0, "Shopping");
        assert!(matches!(
            notes.remove(&1, None).await.unwrap_err(),
            DomainError::NotFound
        ));
        assert!(notes.is_empty());
    }

    #[tokio::test]
    async fn a_change_expecting_a_version_that_has_moved_on_is_stale() {
        let notes = store();
        notes.insert(Note::new(1, "Shopping", 2)).await.unwrap();
        notes.change(&1, Note::archive).await.unwrap();

        let err = notes
            .change(&1, |note| {
                note.priority = 9;
                Ok(())
            })
            .expecting(Some(Version::FIRST))
            .await
            .unwrap_err();
        assert!(matches!(err, DomainError::Stale { .. }), "{err}");
        assert!(err.to_string().contains("at version 2, not 1"), "{err}");
        assert_eq!(notes.load(&1).await.unwrap().priority, 2);

        let current = notes.load(&1).await.unwrap().version();
        notes
            .change(&1, |note| {
                note.priority = 9;
                Ok(())
            })
            .expecting(Some(current))
            .await
            .unwrap();
        assert_eq!(notes.load(&1).await.unwrap().priority, 9);
    }

    #[derive(Debug, Serialize)]
    struct Archived {
        id: u32,
    }

    impl crate::domain::DomainEvent for Archived {
        fn name(&self) -> &str {
            "note.archived"
        }
    }

    #[derive(Serialize)]
    struct Created {
        id: u32,
    }

    impl crate::domain::DomainEvent for Created {
        fn name(&self) -> &str {
            "note.created"
        }
    }

    fn archive_raising(note: &mut Note) -> DomainResult<Archived> {
        note.archive()?;
        Ok(Archived { id: note.id })
    }

    fn recorder() -> (
        std::sync::Arc<crate::testing::RecordedEvents>,
        crate::port::Events,
    ) {
        let recorded = std::sync::Arc::new(crate::testing::RecordedEvents::default());
        let events = crate::port::Events::new(recorded.clone());
        (recorded, events)
    }

    #[tokio::test]
    async fn a_raised_event_is_published_after_the_save_and_not_without_it() {
        let notes = store();
        let (recorded, events) = recorder();
        let cx = crate::port::Context::new();
        notes.insert(Note::new(1, "Shopping", 2)).await.unwrap();

        let archived = notes
            .change(&1, archive_raising)
            .raising(&events, &cx)
            .await
            .unwrap();
        assert_eq!(archived.outcome.id, 1);
        assert_eq!(recorded.names(), ["note.archived"]);
        assert_eq!(
            recorded.envelopes()[0].correlation_id(),
            Some(cx.request_id())
        );

        // The second archive fails in the domain: nothing saved, nothing published.
        assert!(
            notes
                .change(&1, archive_raising)
                .raising(&events, &cx)
                .await
                .is_err()
        );
        assert_eq!(recorded.names().len(), 1);
    }

    #[tokio::test]
    async fn a_stale_expectation_publishes_nothing() {
        let notes = store();
        let (recorded, events) = recorder();
        let cx = crate::port::Context::new();
        notes.insert(Note::new(1, "Shopping", 2)).await.unwrap();
        notes.change(&1, Note::archive).await.unwrap();

        let err = notes
            .change(&1, |note| {
                note.priority = 1;
                Ok(Archived { id: note.id })
            })
            .expecting(Some(Version::FIRST))
            .raising(&events, &cx)
            .await
            .unwrap_err();
        assert!(matches!(err, DomainError::Stale { .. }), "{err}");
        assert!(recorded.names().is_empty());
    }

    #[tokio::test]
    async fn announcing_changes_publishes_the_patch_when_there_is_one() {
        let notes = store();
        let (recorded, events) = recorder();
        let cx = crate::port::Context::new();
        notes.insert(Note::new(1, "Shopping", 2)).await.unwrap();

        notes
            .change(&1, |note| {
                note.priority = 7;
                Ok(())
            })
            .raising(&events, &cx)
            .announcing_changes()
            .await
            .unwrap();
        assert_eq!(
            recorded.payloads("note.updated"),
            [json!({
                "id": "1",
                "version": 2,
                "changes": [{ "op": "replace", "path": "/priority", "value": 7 }],
            })]
        );

        // Nothing changed, so there is no patch to announce.
        notes
            .change(&1, |_| Ok(()))
            .raising(&events, &cx)
            .announcing_changes()
            .await
            .unwrap();
        assert_eq!(recorded.names().len(), 1);
    }

    #[tokio::test]
    async fn an_added_aggregate_is_published_with_what_created_it() {
        let notes = store();
        let (recorded, events) = recorder();
        let cx = crate::port::Context::new();

        let added = notes
            .add(Note::new(1, "Shopping", 2), Created { id: 1 })
            .raising(&events, &cx)
            .await
            .unwrap();
        assert_eq!(added.version(), Version::FIRST);
        assert_eq!(recorded.names(), ["note.created"]);

        assert!(matches!(
            notes
                .add(Note::new(1, "Again", 1), Created { id: 1 })
                .raising(&events, &cx)
                .await
                .unwrap_err(),
            DomainError::Conflict { .. }
        ));
        assert_eq!(recorded.names().len(), 1, "a failed insert raises nothing");
    }

    #[tokio::test]
    async fn a_conditional_remove_only_removes_the_expected_version() {
        let notes = store();
        notes.insert(Note::new(1, "Shopping", 2)).await.unwrap();
        notes.change(&1, Note::archive).await.unwrap();

        assert!(matches!(
            notes.remove(&1, Some(Version::FIRST)).await.unwrap_err(),
            DomainError::Stale { .. }
        ));
        assert_eq!(notes.len(), 1);
        notes.remove(&1, Some(Version::new(2))).await.unwrap();
        assert!(notes.is_empty());
    }

    #[tokio::test]
    async fn a_stored_document_that_breaks_an_invariant_is_an_internal_error() {
        let notes = store();
        // As if an older release, or a hand-edited table, had written this.
        notes.seed(
            "1",
            json!({ "id": 1, "title": "  ", "priority": 1, "archived": false }),
        );

        let err = notes.load(&1).await.unwrap_err();
        assert!(
            matches!(err, DomainError::Internal { .. }),
            "corrupt storage is not the caller's fault: {err}"
        );
        assert!(err.to_string().contains("title must not be empty"), "{err}");
    }

    async fn titles(notes: &Memory<Note>, criteria: Criteria) -> Vec<String> {
        notes
            .list(&criteria)
            .await
            .unwrap()
            .into_iter()
            .map(|note| note.title.0)
            .collect()
    }

    async fn seeded() -> Memory<Note> {
        let notes = store();
        for (id, title, priority) in [(1, "gamma", 10), (2, "Alpha", 9), (3, "beta", 10)] {
            notes.insert(Note::new(id, title, priority)).await.unwrap();
        }
        notes
    }

    #[tokio::test]
    async fn listings_sort_text_case_insensitively_and_numbers_numerically() {
        let notes = seeded().await;
        let by = |sort: &str| Criteria::new().with_sort(sort.parse().unwrap());

        assert_eq!(
            titles(&notes, by("title")).await,
            ["Alpha", "beta", "gamma"]
        );
        // 9 before 10: compared as numbers, not as text.
        assert_eq!(
            titles(&notes, by("priority")).await,
            ["Alpha", "gamma", "beta"]
        );
        // Ties on priority keep key order, so the order is total.
        assert_eq!(
            titles(&notes, by("-priority")).await,
            ["gamma", "beta", "Alpha"]
        );
    }

    #[tokio::test]
    async fn listings_filter_and_page() {
        let notes = seeded().await;
        let filtered = |filter: &str| Criteria::new().with_filter(filter.parse().unwrap());

        assert_eq!(titles(&notes, filtered("title:begins:a")).await, ["Alpha"]);
        assert_eq!(
            titles(&notes, filtered("priority:gt:9")).await,
            ["gamma", "beta"]
        );
        assert_eq!(titles(&notes, filtered("archived:eq:false")).await.len(), 3);

        let page = Criteria::new()
            .with_sort("title".parse().unwrap())
            .with_page(crate::domain::Page::new(Some(2), Some(2)));
        assert_eq!(titles(&notes, page).await, ["gamma"]);
    }

    #[tokio::test]
    async fn a_value_the_kind_cannot_hold_is_refused_not_matched() {
        let notes = seeded().await;
        for filter in [
            "priority:eq:banana",
            "archived:gt:true",
            "priority:contains:1",
        ] {
            let criteria = Criteria::new().with_filter(filter.parse().unwrap());
            assert!(
                matches!(
                    notes.list(&criteria).await.unwrap_err(),
                    DomainError::Invalid { .. }
                ),
                "{filter}"
            );
        }
    }

    #[tokio::test]
    async fn a_path_outside_the_allowlist_is_refused() {
        let notes = Memory::<Note>::new().queryable([("title", ValueKind::Text)]);
        let criteria = Criteria::new().with_filter("priority:eq:1".parse().unwrap());
        assert!(matches!(
            notes.list(&criteria).await.unwrap_err(),
            DomainError::Invalid { .. }
        ));
    }

    #[tokio::test]
    async fn nothing_is_queryable_unless_allowed() {
        let notes = Memory::<Note>::new();
        let criteria = Criteria::new().with_sort("title".parse().unwrap());
        assert!(notes.list(&criteria).await.is_err());
        assert!(notes.list(&Criteria::new()).await.is_ok());
    }

    #[test]
    fn dotted_paths_reach_into_nested_documents() {
        let doc = json!({ "author": { "name": "Ada" } });
        assert_eq!(lookup(&doc, "author.name"), Some(&json!("Ada")));
        assert_eq!(lookup(&doc, "author.age"), None);
    }

    #[tokio::test]
    async fn it_reports_itself_healthy() {
        let notes = store();
        assert_eq!(HealthCheck::name(&notes), "note-store");
        assert!(notes.check().await.is_ok());
    }
}
