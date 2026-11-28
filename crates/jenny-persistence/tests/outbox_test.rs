//! The transactional outbox against a real Postgres: events committed with the write
//! that raised them or not at all, and a relay that forwards each exactly once per
//! successful pass.
//!
//! Needs `JENNY_TEST_POSTGRES_URL`, like `postgres_test`. Each test runs in a schema of
//! its own, because every outbox is called `jenny_outbox` and a relay in one test
//! would otherwise drain another's.

use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use jenny_core::domain::{Aggregate, DomainError, DomainEvent, DomainResult};
use jenny_core::port::outbound::{EventEnvelope, EventPublisher, PublishError};
use jenny_core::port::{Context, Events, StoreExt, Version};
use jenny_core::testing::RecordedEvents;
use jenny_persistence::{PgDocuments, Postgres};
use serde::{Deserialize, Serialize};
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Note {
    id: u32,
    archived: bool,
}

impl Aggregate for Note {
    type Id = u32;
    const KIND: &'static str = "note";
    fn id(&self) -> u32 {
        self.id
    }
}

#[derive(Debug, Serialize)]
struct NoteCreated {
    id: u32,
}

impl DomainEvent for NoteCreated {
    fn name(&self) -> &str {
        "note.created"
    }
}

#[derive(Debug, Serialize)]
struct NoteArchived {
    id: u32,
}

impl DomainEvent for NoteArchived {
    fn name(&self) -> &str {
        "note.archived"
    }
}

impl Note {
    fn archive(&mut self) -> DomainResult<NoteArchived> {
        if self.archived {
            return Err(DomainError::conflict("already archived"));
        }
        self.archived = true;
        Ok(NoteArchived { id: self.id })
    }
}

fn database_url() -> Option<String> {
    match std::env::var("JENNY_TEST_POSTGRES_URL") {
        Ok(url) => Some(url),
        Err(_) if std::env::var_os("CI").is_some() => {
            panic!("JENNY_TEST_POSTGRES_URL must be set in CI")
        }
        Err(_) => {
            eprintln!("skipping: JENNY_TEST_POSTGRES_URL is not set (try `just test-postgres`)");
            None
        }
    }
}

/// A Postgres handle whose every table, the outbox included, lives in a fresh schema.
struct Fixture {
    postgres: Postgres,
    notes: PgDocuments<Note>,
    schema: String,
    /// What the relay forwards to.
    relayed: Arc<dyn EventPublisher>,
    /// What the use case's `Events` would publish to directly — with an outbox, nothing.
    direct: Arc<RecordedEvents>,
}

impl Fixture {
    async fn with(relayed: Arc<dyn EventPublisher>) -> Option<Self> {
        let url = database_url()?;
        let schema = format!("outbox_test_{}", uuid::Uuid::new_v4().simple());
        let admin = PgPoolOptions::new().connect(&url).await.unwrap();
        sqlx::query(sqlx::AssertSqlSafe(format!("CREATE SCHEMA \"{schema}\"")))
            .execute(&admin)
            .await
            .unwrap();
        let options: PgConnectOptions = url.parse::<PgConnectOptions>().unwrap();
        let pool = PgPoolOptions::new()
            .connect_lazy_with(options.options([("search_path", schema.as_str())]));

        let postgres = Postgres::from_pool(pool);
        postgres.with_outbox(relayed.clone());
        let notes = postgres.documents::<Note>();
        postgres.prepare().await.unwrap();
        Some(Self {
            postgres,
            notes,
            schema,
            relayed,
            direct: Arc::default(),
        })
    }

    async fn new() -> Option<(Self, Arc<RecordedEvents>)> {
        let recorded = Arc::new(RecordedEvents::default());
        let fixture = Self::with(recorded.clone()).await?;
        Some((fixture, recorded))
    }

    fn events(&self) -> Events {
        Events::new(self.direct.clone())
    }

    async fn waiting(&self) -> i64 {
        sqlx::query_scalar("SELECT count(*) FROM jenny_outbox")
            .fetch_one(self.postgres.pool())
            .await
            .unwrap()
    }

    async fn drop_schema(self) {
        let _ = &self.relayed;
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "DROP SCHEMA \"{}\" CASCADE",
            self.schema
        )))
        .execute(self.postgres.pool())
        .await
        .unwrap();
    }
}

#[tokio::test]
async fn raised_events_are_committed_with_the_change_and_relayed_in_order() {
    let Some((db, relayed)) = Fixture::new().await else {
        return;
    };
    let events = db.events();
    let cx = Context::new();

    db.notes
        .add(
            Note {
                id: 1,
                archived: false,
            },
            NoteCreated { id: 1 },
        )
        .raising(&events, &cx)
        .await
        .unwrap();
    db.notes
        .change(&1, Note::archive)
        .raising(&events, &cx)
        .await
        .unwrap();

    // Recorded with the writes, not published yet, and not published around the outbox.
    assert_eq!(db.waiting().await, 2);
    assert!(db.direct.names().is_empty());
    assert!(relayed.names().is_empty());

    let relay = db.postgres.relay().unwrap();
    assert_eq!(relay.drain().await, 2);
    assert_eq!(relayed.names(), ["note.created", "note.archived"]);
    assert!(
        relayed
            .envelopes()
            .iter()
            .all(|e| e.correlation_id() == Some(cx.request_id()))
    );
    assert_eq!(
        relayed.payloads("note.archived"),
        [serde_json::json!({ "id": 1 })]
    );
    assert_eq!(db.waiting().await, 0, "forwarded rows are deleted");
    assert_eq!(relay.drain().await, 0);
    db.drop_schema().await;
}

#[tokio::test]
async fn a_change_that_fails_records_nothing() {
    let Some((db, _)) = Fixture::new().await else {
        return;
    };
    let events = db.events();
    let cx = Context::new();
    db.notes
        .add(
            Note {
                id: 1,
                archived: false,
            },
            NoteCreated { id: 1 },
        )
        .raising(&events, &cx)
        .await
        .unwrap();
    assert_eq!(db.waiting().await, 1);

    // A stale precondition, a rule the domain refuses, a duplicate insert.
    db.notes.change(&1, Note::archive).await.unwrap();
    let stale = db
        .notes
        .change(&1, |note| {
            note.archived = false;
            Ok(NoteArchived { id: note.id })
        })
        .expecting(Some(Version::FIRST))
        .raising(&events, &cx)
        .await;
    assert!(matches!(stale, Err(DomainError::Stale { .. })), "{stale:?}");
    let refused = db
        .notes
        .change(&1, Note::archive)
        .raising(&events, &cx)
        .await;
    assert!(matches!(refused, Err(DomainError::Conflict { .. })));
    let duplicate = db
        .notes
        .add(
            Note {
                id: 1,
                archived: false,
            },
            NoteCreated { id: 1 },
        )
        .raising(&events, &cx)
        .await;
    assert!(matches!(duplicate, Err(DomainError::Conflict { .. })));

    assert_eq!(db.waiting().await, 1, "only the first insert's event");
    assert!(db.direct.names().is_empty());
    db.drop_schema().await;
}

/// Fails the first `failures` publishes, then records like `RecordedEvents`.
#[derive(Default)]
struct Flaky {
    failures: std::sync::Mutex<usize>,
    recorded: RecordedEvents,
}

#[async_trait]
impl EventPublisher for Flaky {
    async fn publish(&self, event: EventEnvelope) -> Result<(), PublishError> {
        {
            let mut failures = self.failures.lock().unwrap();
            if *failures > 0 {
                *failures -= 1;
                return Err(PublishError::transport("bus is down"));
            }
        }
        self.recorded.publish(event).await
    }
}

#[tokio::test]
async fn an_event_the_bus_refused_waits_for_the_next_pass() {
    let flaky = Arc::new(Flaky {
        failures: std::sync::Mutex::new(1),
        ..Default::default()
    });
    let Some(db) = Fixture::with(flaky.clone()).await else {
        return;
    };
    let events = db.events();
    let cx = Context::new();
    for id in 1..=3 {
        db.notes
            .add(
                Note {
                    id,
                    archived: false,
                },
                NoteCreated { id },
            )
            .raising(&events, &cx)
            .await
            .unwrap();
    }

    let relay = db.postgres.relay().unwrap();
    assert_eq!(relay.drain().await, 0, "the first publish failed");
    assert_eq!(db.waiting().await, 3, "so everything waits");

    assert_eq!(relay.drain().await, 3);
    assert_eq!(
        flaky.recorded.payloads("note.created"),
        [1, 2, 3].map(|id| serde_json::json!({ "id": id })),
        "and goes out in the order it was recorded"
    );
    db.drop_schema().await;
}

/// Refuses the first publish, and keeps every envelope it was offered.
#[derive(Default)]
struct RefusesOnce {
    offered: std::sync::Mutex<Vec<EventEnvelope>>,
}

#[async_trait]
impl EventPublisher for RefusesOnce {
    async fn publish(&self, event: EventEnvelope) -> Result<(), PublishError> {
        let mut offered = self.offered.lock().unwrap();
        offered.push(event);
        if offered.len() == 1 {
            return Err(PublishError::transport("bus is down"));
        }
        Ok(())
    }
}

#[tokio::test]
async fn a_replay_carries_the_headers_enriched_at_the_write_under_the_same_id() {
    let bus = Arc::new(RefusesOnce::default());
    let Some(db) = Fixture::with(bus.clone()).await else {
        return;
    };
    let events = db
        .events()
        .enriched_with(jenny_core::port::Caller)
        .enriched_with(jenny_core::port::Source::new("my-notes", "1.2.3"));
    let cx = Context::new().with_principal(jenny_core::domain::Principal::builder("u-7").build());
    db.notes
        .add(
            Note {
                id: 1,
                archived: false,
            },
            NoteCreated { id: 1 },
        )
        .raising(&events, &cx)
        .await
        .unwrap();

    let relay = db.postgres.relay().unwrap();
    assert_eq!(relay.drain().await, 0, "refused the first time");
    assert_eq!(relay.drain().await, 1, "replayed the second");

    let offered = bus.offered.lock().unwrap().clone();
    assert_eq!(offered.len(), 2);
    assert_eq!(offered[0].id(), offered[1].id(), "the replay keeps the id");
    for envelope in &offered {
        let headers = envelope.headers();
        assert_eq!(headers["caller-subject"], "u-7");
        assert_eq!(headers["source-service"], "my-notes");
        assert_eq!(headers["source-version"], "1.2.3");
    }
    db.drop_schema().await;
}

/// Counts publishes and remembers every envelope id it saw.
#[derive(Default)]
struct Counting {
    publishes: AtomicUsize,
    ids: std::sync::Mutex<HashSet<uuid::Uuid>>,
}

#[async_trait]
impl EventPublisher for Counting {
    async fn publish(&self, event: EventEnvelope) -> Result<(), PublishError> {
        self.publishes.fetch_add(1, Ordering::SeqCst);
        self.ids.lock().unwrap().insert(event.id());
        // Give the other relay a chance to run in between.
        tokio::task::yield_now().await;
        Ok(())
    }
}

#[tokio::test]
async fn two_relays_never_forward_the_same_event() {
    let counting = Arc::new(Counting::default());
    let Some(db) = Fixture::with(counting.clone()).await else {
        return;
    };
    let events = db.events();
    let cx = Context::new();
    for id in 1..=250 {
        db.notes
            .add(
                Note {
                    id,
                    archived: false,
                },
                NoteCreated { id },
            )
            .raising(&events, &cx)
            .await
            .unwrap();
    }

    let (first, second) = (db.postgres.relay().unwrap(), db.postgres.relay().unwrap());
    let (a, b) = tokio::join!(first.drain(), second.drain());
    assert_eq!(a + b, 250, "every event forwarded");
    assert_eq!(
        counting.publishes.load(Ordering::SeqCst),
        250,
        "and only once"
    );
    assert_eq!(counting.ids.lock().unwrap().len(), 250);
    assert_eq!(db.waiting().await, 0);
    db.drop_schema().await;
}

#[tokio::test]
async fn a_running_relay_is_woken_by_a_commit_and_drains_on_stop() {
    let Some((db, relayed)) = Fixture::new().await else {
        return;
    };
    let (stop, stopping) = tokio::sync::oneshot::channel::<()>();
    let relay = tokio::spawn(db.postgres.relay().unwrap().run(async move {
        let _ = stopping.await;
    }));
    let events = db.events();
    let cx = Context::new();

    db.notes
        .add(
            Note {
                id: 1,
                archived: false,
            },
            NoteCreated { id: 1 },
        )
        .raising(&events, &cx)
        .await
        .unwrap();
    // Well inside the poll interval: it was the commit that woke the relay.
    for _ in 0..50 {
        if !relayed.names().is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert_eq!(relayed.names(), ["note.created"]);

    db.notes
        .change(&1, Note::archive)
        .raising(&events, &cx)
        .await
        .unwrap();
    stop.send(()).unwrap();
    relay.await.unwrap();
    assert_eq!(relayed.names(), ["note.created", "note.archived"]);
    db.drop_schema().await;
}
