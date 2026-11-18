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

use crate::domain::{Aggregate, Check, Criteria, DomainError, DomainResult, Filter};
use crate::port::outbound::HealthCheck;
use crate::port::store::{Loaded, Store, Version, encode};

/// Aggregates kept in a map, as the JSON documents a database-backed store would write.
///
/// Holding documents rather than aggregates is deliberate: every save goes through the
/// same encoding and every read through the same decoding as a real store, so a value
/// object that cannot round-trip fails here, in a unit test, rather than against
/// Postgres in staging.
///
/// ```
/// # use jenny_core::domain::{Aggregate, Criteria};
/// # use jenny_core::port::Store;
/// use jenny_core::memory::Memory;
/// # #[derive(serde::Serialize, serde::Deserialize)] pub struct Note { id: u32, title: String }
/// # impl Aggregate for Note { type Id = u32; const KIND: &'static str = "note"; fn id(&self) -> u32 { self.id } }
///
/// # #[tokio::main(flavor = "current_thread")] async fn main() -> jenny_core::domain::DomainResult<()> {
/// let notes = Memory::<Note>::new().queryable(["title"]);
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
/// Text compares case-insensitively and numbers numerically. Sorting puts missing and
/// `null` values last, as Postgres does, and ties keep key order, so paging is stable.
pub struct Memory<A> {
    documents: Mutex<BTreeMap<String, Entry>>,
    queryable: Vec<String>,
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
            queryable: Vec::new(),
            name: format!("{}-store", A::KIND),
            kind: PhantomData,
        }
    }

    /// The attribute paths a listing may sort and filter on.
    pub fn queryable<I, P>(mut self, paths: I) -> Self
    where
        I: IntoIterator<Item = P>,
        P: Into<String>,
    {
        self.queryable = paths.into_iter().map(Into::into).collect();
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

    async fn remove(&self, id: &A::Id) -> DomainResult<A> {
        let entry = self
            .lock()?
            .remove(&id.to_string())
            .ok_or_else(DomainError::not_found)?;
        Ok(Loaded::<A>::decode(entry.document, entry.version)?.into_inner())
    }

    async fn list(&self, criteria: &Criteria) -> DomainResult<Vec<A>> {
        let allowed: Vec<&str> = self.queryable.iter().map(String::as_str).collect();
        criteria.ensure_allowed(&allowed)?;

        let mut found: Vec<Entry> = self
            .lock()?
            .values()
            .filter(|entry| criteria.filter.iter().all(|f| matches(&entry.document, f)))
            .cloned()
            .collect();

        // Later keys first, so earlier ones win under a stable sort — what a client
        // writing `sort=a&sort=b` means. Ties keep key order, which makes it total.
        for key in criteria.sort.iter().rev() {
            found.sort_by(|left, right| {
                let ordering = compare(
                    lookup(&left.document, key.path()),
                    lookup(&right.document, key.path()),
                );
                match key.ordering() {
                    crate::domain::Ordering::Asc => ordering,
                    crate::domain::Ordering::Desc => ordering.reverse(),
                }
            });
        }

        found
            .into_iter()
            .skip(criteria.page.offset() as usize)
            .take(criteria.page.limit() as usize)
            .map(|entry| Ok(Loaded::<A>::decode(entry.document, entry.version)?.into_inner()))
            .collect()
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
        Memory::new().queryable(["title", "priority", "archived"])
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
        notes.remove(&1).await.unwrap();

        assert!(matches!(
            notes.save(loaded).await.unwrap_err(),
            DomainError::NotFound
        ));
    }

    #[tokio::test]
    async fn removing_returns_the_aggregate_then_reports_not_found() {
        let notes = store();
        notes.insert(Note::new(1, "Shopping", 2)).await.unwrap();

        assert_eq!(notes.remove(&1).await.unwrap().title.0, "Shopping");
        assert!(matches!(
            notes.remove(&1).await.unwrap_err(),
            DomainError::NotFound
        ));
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
        // A number compared against text matches nothing rather than guessing.
        assert!(
            titles(&notes, filtered("priority:eq:banana"))
                .await
                .is_empty()
        );

        let page = Criteria::new()
            .with_sort("title".parse().unwrap())
            .with_page(crate::domain::Page::new(Some(2), Some(2)));
        assert_eq!(titles(&notes, page).await, ["gamma"]);
    }

    #[tokio::test]
    async fn a_path_outside_the_allowlist_is_refused() {
        let notes = Memory::<Note>::new().queryable(["title"]);
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
