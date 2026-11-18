//! `PgDocuments` against a real Postgres: the whole `Store` contract, the version
//! checks, listings inside the database, and table creation.
//!
//! Needs `JENNY_TEST_POSTGRES_URL`. `just test-postgres` starts a disposable database
//! and sets it; CI runs a Postgres service. Without it the tests say so and pass,
//! except under `CI`, where a missing database is a failure rather than a skip.

use jenny_core::domain::{Aggregate, Criteria, DomainError, DomainResult, Page, ValueKind};
use jenny_core::port::outbound::HealthCheck as _;
use jenny_core::port::{Store, StoreExt, Version};
use jenny_persistence::{PgDocuments, Postgres, PostgresConfig};
use serde::{Deserialize, Serialize};
use serde_json::json;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct Note {
    id: u32,
    title: Title,
    priority: i64,
    archived: bool,
    author: Author,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct Author {
    name: String,
}

/// Refuses blanks, including on the way back from the database.
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
    fn new(id: u32, title: &str, priority: i64, author: &str) -> Self {
        Self {
            id,
            title: Title(title.into()),
            priority,
            archived: false,
            author: Author {
                name: author.into(),
            },
        }
    }

    fn archive(&mut self) -> DomainResult<()> {
        if self.archived {
            return Err(DomainError::conflict("already archived"));
        }
        self.archived = true;
        Ok(())
    }
}

/// The database URL, or `None` to skip — never `None` under CI.
fn database_url() -> Option<String> {
    match std::env::var("JENNY_TEST_POSTGRES_URL") {
        Ok(url) => Some(url),
        Err(_) if std::env::var_os("CI").is_some() => {
            panic!(
                "JENNY_TEST_POSTGRES_URL must be set in CI; these tests would otherwise pass without running"
            )
        }
        Err(_) => {
            eprintln!("skipping: JENNY_TEST_POSTGRES_URL is not set (try `just test-postgres`)");
            None
        }
    }
}

/// A store in a table of its own, so tests run in parallel and leave nothing behind
/// that the next run trips over.
struct Fixture {
    postgres: Postgres,
    notes: PgDocuments<Note>,
    table: String,
}

impl Fixture {
    async fn new() -> Option<Self> {
        let url = database_url()?;
        let postgres = Postgres::connect_lazy(&PostgresConfig {
            url: Some(url),
            ..Default::default()
        })
        .unwrap();
        let table = format!("note_test_{}", uuid::Uuid::new_v4().simple());
        let notes = postgres.documents_in::<Note>(&table).queryable([
            ("title", ValueKind::Text),
            ("priority", ValueKind::Integer),
            ("archived", ValueKind::Bool),
            ("author.name", ValueKind::Text),
        ]);
        postgres.prepare().await.unwrap();
        Some(Self {
            postgres,
            notes,
            table,
        })
    }

    async fn seeded() -> Option<Self> {
        let fixture = Self::new().await?;
        for (id, title, priority, author) in [
            (1, "gamma", 10, "Ada"),
            (2, "Alpha", 9, "Grace"),
            (3, "beta", 10, "Ada"),
        ] {
            fixture
                .notes
                .insert(Note::new(id, title, priority, author))
                .await
                .unwrap();
        }
        Some(fixture)
    }

    async fn titles(&self, criteria: Criteria) -> Vec<String> {
        self.notes
            .list(&criteria)
            .await
            .unwrap()
            .into_iter()
            .map(|note| note.title.0)
            .collect()
    }

    async fn drop_table(self) {
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "DROP TABLE \"{}\"",
            self.table
        )))
        .execute(self.postgres.pool())
        .await
        .unwrap();
    }
}

#[tokio::test]
async fn an_aggregate_round_trips_through_jsonb() {
    let Some(db) = Fixture::new().await else {
        return;
    };

    let inserted = db
        .notes
        .insert(Note::new(1, "Shopping", 2, "Ada"))
        .await
        .unwrap();
    assert_eq!(inserted.version(), Version::FIRST);

    let found = db.notes.load(&1).await.unwrap();
    assert_eq!(*found, Note::new(1, "Shopping", 2, "Ada"));
    assert_eq!(found.version(), Version::FIRST);
    assert!(db.notes.find(&2).await.unwrap().is_none());
    assert!(matches!(
        db.notes
            .insert(Note::new(1, "again", 1, "Ada"))
            .await
            .unwrap_err(),
        DomainError::Conflict { .. }
    ));
    db.drop_table().await;
}

#[tokio::test]
async fn a_change_is_saved_at_the_next_version_and_a_stale_one_is_refused() {
    let Some(db) = Fixture::new().await else {
        return;
    };
    db.notes
        .insert(Note::new(1, "Shopping", 2, "Ada"))
        .await
        .unwrap();

    let first = db.notes.load(&1).await.unwrap();
    let second = db.notes.load(&1).await.unwrap();

    let changed = db.notes.change(&1, Note::archive).await.unwrap();
    assert_eq!(changed.aggregate.version().get(), 2);
    assert_eq!(changed.changes.paths().collect::<Vec<_>>(), ["/archived"]);

    // Both earlier reads are now out of date.
    assert!(matches!(
        db.notes.save(first).await.unwrap_err(),
        DomainError::Conflict { .. }
    ));
    let err = db
        .notes
        .change(&1, |note| {
            note.priority = 9;
            Ok(())
        })
        .expecting(Some(second.version()))
        .await
        .unwrap_err();
    assert!(matches!(err, DomainError::Stale { .. }), "{err}");

    let stored = db.notes.load(&1).await.unwrap();
    assert!(stored.archived);
    assert_eq!(stored.priority, 2);
    db.drop_table().await;
}

#[tokio::test]
async fn concurrent_writers_cannot_both_win() {
    let Some(db) = Fixture::new().await else {
        return;
    };
    db.notes
        .insert(Note::new(1, "Shopping", 0, "Ada"))
        .await
        .unwrap();

    // Eight read-modify-writes in flight at once, each on its own pooled connection.
    let writers = (0..8).map(|n| {
        let notes = &db.notes;
        async move {
            let mut note = notes.load(&1).await?;
            note.priority = n;
            notes.save(note).await
        }
    });
    let outcomes = futures::future::join_all(writers).await;
    let won = outcomes.iter().filter(|o| o.is_ok()).count();
    let lost = outcomes
        .iter()
        .filter(|o| matches!(o, Err(DomainError::Conflict { .. })))
        .count();
    assert!(won >= 1, "someone must win");
    assert_eq!(won + lost, 8, "every loser is told so: {outcomes:?}");
    assert_eq!(
        db.notes.load(&1).await.unwrap().version().get(),
        1 + won as u64,
        "one version per successful save, none lost"
    );
    db.drop_table().await;
}

#[tokio::test]
async fn removal_can_be_conditional_and_reports_what_was_there() {
    let Some(db) = Fixture::new().await else {
        return;
    };
    db.notes
        .insert(Note::new(1, "Shopping", 2, "Ada"))
        .await
        .unwrap();
    db.notes.change(&1, Note::archive).await.unwrap();

    assert!(matches!(
        db.notes.remove(&1, Some(Version::FIRST)).await.unwrap_err(),
        DomainError::Stale { .. }
    ));
    let removed = db.notes.remove(&1, Some(Version::new(2))).await.unwrap();
    assert_eq!(removed.title.0, "Shopping");
    assert!(matches!(
        db.notes.remove(&1, None).await.unwrap_err(),
        DomainError::NotFound
    ));
    db.drop_table().await;
}

#[tokio::test]
async fn listings_filter_sort_and_page_inside_the_database() {
    let Some(db) = Fixture::seeded().await else {
        return;
    };
    let sorted = |sort: &str| Criteria::new().with_sort(sort.parse().unwrap());
    let filtered = |filter: &str| Criteria::new().with_filter(filter.parse().unwrap());

    // Numbers sort as numbers, ties fall back to the id.
    assert_eq!(
        db.titles(sorted("priority")).await,
        ["Alpha", "gamma", "beta"]
    );
    assert_eq!(
        db.titles(sorted("-priority")).await,
        ["gamma", "beta", "Alpha"]
    );
    assert_eq!(
        db.titles(filtered("priority:gt:9")).await,
        ["gamma", "beta"]
    );
    assert_eq!(db.titles(filtered("title:begins:a")).await, ["Alpha"]);
    assert_eq!(
        db.titles(filtered("author.name:eq:Ada")).await,
        ["gamma", "beta"]
    );
    assert_eq!(db.titles(filtered("archived:eq:false")).await.len(), 3);

    let page = Criteria::new()
        .with_sort("priority".parse().unwrap())
        .with_page(Page::new(Some(2), Some(2)));
    assert_eq!(db.titles(page).await, ["beta"]);

    // A wildcard in the value is a literal, not "everything".
    assert!(db.titles(filtered("title:contains:%")).await.is_empty());
    db.drop_table().await;
}

#[tokio::test]
async fn criteria_outside_the_allowlist_or_the_kind_are_refused() {
    let Some(db) = Fixture::seeded().await else {
        return;
    };
    for criteria in [
        Criteria::new().with_sort("secret".parse().unwrap()),
        Criteria::new().with_filter("priority:gt:banana".parse().unwrap()),
        Criteria::new().with_filter("archived:gt:true".parse().unwrap()),
    ] {
        assert!(matches!(
            db.notes.list(&criteria).await.unwrap_err(),
            DomainError::Invalid { .. }
        ));
    }
    db.drop_table().await;
}

#[tokio::test]
async fn a_stored_document_that_breaks_an_invariant_is_an_internal_error() {
    let Some(db) = Fixture::new().await else {
        return;
    };
    // As if an older release, or a hand-edited row, had written this.
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "INSERT INTO \"{}\" (id, version, document) VALUES ('1', 1, $1)",
        db.table
    )))
    .bind(json!({
        "id": 1, "title": "  ", "priority": 1, "archived": false, "author": { "name": "x" }
    }))
    .execute(db.postgres.pool())
    .await
    .unwrap();

    let err = db.notes.load(&1).await.unwrap_err();
    assert!(matches!(err, DomainError::Internal { .. }), "{err}");
    db.drop_table().await;
}

#[tokio::test]
async fn preparing_twice_and_from_several_replicas_is_harmless() {
    let Some(db) = Fixture::new().await else {
        return;
    };
    let again = futures::future::join_all((0..4).map(|_| db.postgres.prepare())).await;
    assert!(again.iter().all(Result::is_ok), "{again:?}");
    assert!(db.postgres.health_check().check().await.is_ok());
    db.drop_table().await;
}

// --- PgTable: a table of your own, a column per field -------------------------------

/// A table store over a freshly created table with a column per field of `Note`, plus
/// one the aggregate does not have, as a real schema would.
struct TableFixture {
    postgres: Postgres,
    notes: jenny_persistence::PgTable<Note>,
    table: String,
}

impl TableFixture {
    async fn new() -> Option<Self> {
        let url = database_url()?;
        let postgres = Postgres::connect_lazy(&PostgresConfig {
            url: Some(url),
            ..Default::default()
        })
        .unwrap();
        let table = format!("note_table_{}", uuid::Uuid::new_v4().simple());
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "CREATE TABLE \"{table}\" (
                id         integer     PRIMARY KEY,
                version    bigint      NOT NULL,
                title      text        NOT NULL,
                priority   bigint      NOT NULL,
                archived   boolean     NOT NULL,
                author     jsonb       NOT NULL,
                created_at timestamptz NOT NULL DEFAULT now()
            )"
        )))
        .execute(postgres.pool())
        .await
        .unwrap();
        let notes = postgres.table::<Note>(&table).queryable([
            ("title", ValueKind::Text),
            ("priority", ValueKind::Integer),
            ("archived", ValueKind::Bool),
            ("author.name", ValueKind::Text),
        ]);
        postgres.prepare().await.unwrap();
        Some(Self {
            postgres,
            notes,
            table,
        })
    }

    async fn drop_table(self) {
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "DROP TABLE \"{}\"",
            self.table
        )))
        .execute(self.postgres.pool())
        .await
        .unwrap();
    }
}

#[tokio::test]
async fn a_table_store_round_trips_through_real_columns() {
    let Some(db) = TableFixture::new().await else {
        return;
    };
    db.notes
        .insert(Note::new(1, "Shopping", 2, "Ada"))
        .await
        .unwrap();

    let found = db.notes.load(&1).await.unwrap();
    assert_eq!(*found, Note::new(1, "Shopping", 2, "Ada"));
    assert_eq!(found.version(), Version::FIRST);

    // The columns are the fields: other systems can query them relationally.
    let (title, author): (String, String) = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT title, author ->> 'name' FROM \"{}\" WHERE id = 1",
        db.table
    )))
    .fetch_one(db.postgres.pool())
    .await
    .unwrap();
    assert_eq!((title.as_str(), author.as_str()), ("Shopping", "Ada"));

    // A column the aggregate does not have is not a change.
    let unchanged = db.notes.change(&1, |_| Ok(())).await.unwrap();
    assert!(unchanged.changes.is_empty(), "{:?}", unchanged.changes);
    assert_eq!(db.notes.load(&1).await.unwrap().version(), Version::FIRST);
    db.drop_table().await;
}

#[tokio::test]
async fn a_table_store_refuses_stale_writes_and_conditional_removals() {
    let Some(db) = TableFixture::new().await else {
        return;
    };
    db.notes
        .insert(Note::new(1, "Shopping", 2, "Ada"))
        .await
        .unwrap();
    let stale = db.notes.load(&1).await.unwrap();

    let archived = db.notes.change(&1, Note::archive).await.unwrap();
    assert_eq!(archived.aggregate.version().get(), 2);
    assert!(db.notes.load(&1).await.unwrap().archived);

    assert!(matches!(
        db.notes.save(stale).await.unwrap_err(),
        DomainError::Conflict { .. }
    ));
    assert!(matches!(
        db.notes.remove(&1, Some(Version::FIRST)).await.unwrap_err(),
        DomainError::Stale { .. }
    ));
    assert!(matches!(
        db.notes
            .insert(Note::new(1, "again", 1, "Ada"))
            .await
            .unwrap_err(),
        DomainError::Conflict { .. }
    ));
    assert_eq!(
        db.notes
            .remove(&1, Some(Version::new(2)))
            .await
            .unwrap()
            .title
            .0,
        "Shopping"
    );
    assert!(matches!(
        db.notes.load(&1).await.unwrap_err(),
        DomainError::NotFound
    ));
    db.drop_table().await;
}

#[tokio::test]
async fn a_table_store_lists_over_columns_and_into_jsonb() {
    let Some(db) = TableFixture::new().await else {
        return;
    };
    for (id, title, priority, author) in [
        (1, "gamma", 10, "Ada"),
        (2, "Alpha", 9, "Grace"),
        (3, "beta", 10, "Ada"),
    ] {
        db.notes
            .insert(Note::new(id, title, priority, author))
            .await
            .unwrap();
    }
    let titles = |criteria: Criteria| {
        let notes = &db.notes;
        async move {
            notes
                .list(&criteria)
                .await
                .unwrap()
                .into_iter()
                .map(|note| note.title.0)
                .collect::<Vec<_>>()
        }
    };
    let sorted = |sort: &str| Criteria::new().with_sort(sort.parse().unwrap());
    let filtered = |filter: &str| Criteria::new().with_filter(filter.parse().unwrap());

    assert_eq!(
        titles(sorted("-priority")).await,
        ["gamma", "beta", "Alpha"]
    );
    assert_eq!(titles(filtered("priority:gt:9")).await, ["gamma", "beta"]);
    assert_eq!(titles(filtered("title:begins:a")).await, ["Alpha"]);
    assert_eq!(titles(filtered("author.name:eq:Grace")).await, ["Alpha"]);
    let page = sorted("priority").with_page(Page::new(Some(2), Some(2)));
    assert_eq!(titles(page).await, ["beta"]);
    assert!(matches!(
        db.notes
            .list(&filtered("created_at:gt:2020-01-01"))
            .await
            .unwrap_err(),
        DomainError::Invalid { .. }
    ));
    db.drop_table().await;
}

#[tokio::test]
async fn a_table_without_what_the_store_needs_stops_the_service_at_startup() {
    let Some(url) = database_url() else { return };
    let postgres = Postgres::connect_lazy(&PostgresConfig {
        url: Some(url),
        ..Default::default()
    })
    .unwrap();

    let missing = format!("note_missing_{}", uuid::Uuid::new_v4().simple());
    let _ = postgres.table::<Note>(&missing);
    let err = postgres.prepare().await.unwrap_err();
    assert!(err.to_string().contains("does not exist"), "{err}");

    let unversioned = format!("note_unversioned_{}", uuid::Uuid::new_v4().simple());
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "CREATE TABLE \"{unversioned}\" (id integer PRIMARY KEY, title text)"
    )))
    .execute(postgres.pool())
    .await
    .unwrap();
    let only_unversioned = Postgres::from_pool(postgres.pool().clone());
    let _ = only_unversioned.table::<Note>(&unversioned);
    let err = only_unversioned.prepare().await.unwrap_err();
    assert!(err.to_string().contains("no `version` column"), "{err}");

    // A table with a version but without a column for every field: refused on the
    // first write, and nothing is written.
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "ALTER TABLE \"{unversioned}\" ADD COLUMN version bigint NOT NULL DEFAULT 1"
    )))
    .execute(postgres.pool())
    .await
    .unwrap();
    let partial = Postgres::from_pool(postgres.pool().clone()).table::<Note>(&unversioned);
    let err = partial
        .insert(Note::new(1, "Shopping", 2, "Ada"))
        .await
        .unwrap_err();
    assert!(
        matches!(err, DomainError::Internal { .. }) && err.to_string().contains("no column"),
        "{err}"
    );
    assert!(partial.find(&1).await.unwrap().is_none());

    sqlx::query(sqlx::AssertSqlSafe(format!("DROP TABLE \"{unversioned}\"")))
        .execute(postgres.pool())
        .await
        .unwrap();
}
