//! `NatsKv` against a real NATS with JetStream: the whole `Store` contract, versions
//! that count writes whatever revision the bucket assigns, and listings.
//!
//! Needs `JENNY_TEST_NATS_URL`. `just test-nats` starts a disposable server and sets
//! it; CI runs one. Without it the tests say so and pass, except under `CI`, where a
//! missing server is a failure rather than a skip.

use jenny_core::domain::{Aggregate, Criteria, DomainError, DomainResult, Page, ValueKind};
use jenny_core::port::{Context, Events, Store, StoreExt, Version};
use jenny_core::testing::RecordedEvents;
use jenny_messaging::NatsConnectionConfig;
use jenny_messaging::kv::{Nats, NatsKv};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct Session {
    id: String,
    user: String,
    visits: i64,
    closed: bool,
}

impl Aggregate for Session {
    type Id = String;
    const KIND: &'static str = "session";
    fn id(&self) -> String {
        self.id.clone()
    }
}

#[derive(Debug, Serialize)]
struct SessionClosed {
    id: String,
}

impl jenny_core::domain::DomainEvent for SessionClosed {
    fn name(&self) -> &str {
        "session.closed"
    }
}

impl Session {
    fn new(id: &str, user: &str, visits: i64) -> Self {
        Self {
            id: id.into(),
            user: user.into(),
            visits,
            closed: false,
        }
    }

    fn close(&mut self) -> DomainResult<SessionClosed> {
        if self.closed {
            return Err(DomainError::conflict("already closed"));
        }
        self.closed = true;
        Ok(SessionClosed {
            id: self.id.clone(),
        })
    }
}

fn nats_url() -> Option<String> {
    match std::env::var("JENNY_TEST_NATS_URL") {
        Ok(url) => Some(url),
        Err(_) if std::env::var_os("CI").is_some() => {
            panic!(
                "JENNY_TEST_NATS_URL must be set in CI; these tests would otherwise pass without running"
            )
        }
        Err(_) => {
            eprintln!("skipping: JENNY_TEST_NATS_URL is not set (try `just test-nats`)");
            None
        }
    }
}

/// A store in a bucket of its own, deleted afterwards.
struct Fixture {
    url: String,
    bucket: String,
    sessions: NatsKv<Session>,
}

impl Fixture {
    async fn new() -> Option<Self> {
        let url = nats_url()?;
        let prefix = format!("kvtest-{}", uuid::Uuid::new_v4().simple());
        let nats = Nats::new(NatsConnectionConfig::new(url.clone()), prefix.clone());
        let sessions = nats.kv::<Session>().queryable([
            ("user", ValueKind::Text),
            ("visits", ValueKind::Integer),
            ("closed", ValueKind::Bool),
        ]);
        nats.prepare().await.unwrap();
        assert!(
            jenny_core::port::outbound::HealthCheck::check(&nats.health_check())
                .await
                .is_ok()
        );
        Some(Self {
            url,
            bucket: format!("{prefix}-session"),
            sessions,
        })
    }

    async fn delete_bucket(self) {
        let client = async_nats::connect(&self.url).await.unwrap();
        async_nats::jetstream::new(client)
            .delete_key_value(&self.bucket)
            .await
            .unwrap();
    }
}

#[tokio::test]
async fn an_aggregate_round_trips_with_versions_that_count_writes() {
    let Some(kv) = Fixture::new().await else {
        return;
    };
    let inserted = kv
        .sessions
        .insert(Session::new("s1", "ada", 1))
        .await
        .unwrap();
    assert_eq!(inserted.version(), Version::FIRST);
    // Another key's write moves the bucket's revision on, not this key's version.
    kv.sessions
        .insert(Session::new("s2", "grace", 1))
        .await
        .unwrap();

    let found = kv.sessions.load(&"s1".to_string()).await.unwrap();
    assert_eq!(*found, Session::new("s1", "ada", 1));
    assert_eq!(found.version(), Version::FIRST);

    let changed = kv
        .sessions
        .change(&"s1".to_string(), |s| {
            s.visits += 1;
            Ok(())
        })
        .await
        .unwrap();
    assert_eq!(changed.aggregate.version().get(), 2);
    assert_eq!(changed.changes.paths().collect::<Vec<_>>(), ["/visits"]);

    assert!(matches!(
        kv.sessions
            .insert(Session::new("s1", "again", 1))
            .await
            .unwrap_err(),
        DomainError::Conflict { .. }
    ));
    assert!(
        kv.sessions
            .find(&"missing".to_string())
            .await
            .unwrap()
            .is_none()
    );
    kv.delete_bucket().await;
}

#[tokio::test]
async fn stale_writes_are_refused_and_concurrent_ones_cannot_both_win() {
    let Some(kv) = Fixture::new().await else {
        return;
    };
    let id = "s1".to_string();
    kv.sessions
        .insert(Session::new("s1", "ada", 0))
        .await
        .unwrap();

    let stale = kv.sessions.load(&id).await.unwrap();
    kv.sessions.change(&id, Session::close).await.unwrap();
    assert!(matches!(
        kv.sessions.save(stale).await.unwrap_err(),
        DomainError::Conflict { .. }
    ));
    let err = kv
        .sessions
        .change(&id, |s| {
            s.visits = 9;
            Ok(())
        })
        .expecting(Some(Version::FIRST))
        .await
        .unwrap_err();
    assert!(matches!(err, DomainError::Stale { .. }), "{err}");

    let writers = (0..8).map(|n| {
        let sessions = &kv.sessions;
        let id = id.clone();
        async move {
            let mut session = sessions.load(&id).await?;
            session.visits = n;
            sessions.save(session).await
        }
    });
    let outcomes = futures::future::join_all(writers).await;
    let won = outcomes.iter().filter(|o| o.is_ok()).count();
    assert!(won >= 1, "someone must win");
    assert!(
        outcomes
            .iter()
            .all(|o| o.is_ok() || matches!(o, Err(DomainError::Conflict { .. }))),
        "every loser is told so: {outcomes:?}"
    );
    assert_eq!(
        kv.sessions.load(&id).await.unwrap().version().get(),
        2 + won as u64,
        "one version per successful save, none lost"
    );
    kv.delete_bucket().await;
}

#[tokio::test]
async fn removal_is_conditional_and_a_removed_key_is_gone() {
    let Some(kv) = Fixture::new().await else {
        return;
    };
    let id = "s1".to_string();
    kv.sessions
        .insert(Session::new("s1", "ada", 0))
        .await
        .unwrap();
    kv.sessions.change(&id, Session::close).await.unwrap();

    assert!(matches!(
        kv.sessions
            .remove(&id, Some(Version::FIRST))
            .await
            .unwrap_err(),
        DomainError::Stale { .. }
    ));
    let removed = kv
        .sessions
        .remove(&id, Some(Version::new(2)))
        .await
        .unwrap();
    assert!(removed.closed);
    assert!(kv.sessions.find(&id).await.unwrap().is_none());
    assert!(matches!(
        kv.sessions.remove(&id, None).await.unwrap_err(),
        DomainError::NotFound
    ));
    // The key can be used again: a removed aggregate leaves nothing behind.
    kv.sessions
        .insert(Session::new("s1", "ada", 0))
        .await
        .unwrap();
    kv.delete_bucket().await;
}

#[tokio::test]
async fn listings_filter_sort_and_page_like_the_in_memory_store() {
    let Some(kv) = Fixture::new().await else {
        return;
    };
    for (id, user, visits) in [("c", "gamma", 10), ("a", "Alpha", 9), ("b", "beta", 10)] {
        kv.sessions
            .insert(Session::new(id, user, visits))
            .await
            .unwrap();
    }
    let users = |criteria: Criteria| {
        let sessions = &kv.sessions;
        async move {
            sessions
                .list(&criteria)
                .await
                .unwrap()
                .into_iter()
                .map(|s| s.user)
                .collect::<Vec<_>>()
        }
    };
    let sorted = |sort: &str| Criteria::new().with_sort(sort.parse().unwrap());
    let filtered = |filter: &str| Criteria::new().with_filter(filter.parse().unwrap());

    // Ties on visits keep key order: b before c.
    assert_eq!(users(sorted("-visits")).await, ["beta", "gamma", "Alpha"]);
    assert_eq!(users(filtered("user:begins:a")).await, ["Alpha"]);
    assert_eq!(users(filtered("visits:gt:9")).await, ["beta", "gamma"]);
    let page = sorted("user").with_page(Page::new(Some(2), Some(2)));
    assert_eq!(users(page).await, ["gamma"]);
    assert!(matches!(
        kv.sessions
            .list(&filtered("visits:gt:banana"))
            .await
            .unwrap_err(),
        DomainError::Invalid { .. }
    ));
    kv.delete_bucket().await;
}

#[tokio::test]
async fn raised_events_are_published_after_the_write() {
    let Some(kv) = Fixture::new().await else {
        return;
    };
    let recorded = std::sync::Arc::new(RecordedEvents::default());
    let events = Events::new(recorded.clone());
    let cx = Context::new();
    kv.sessions
        .insert(Session::new("s1", "ada", 0))
        .await
        .unwrap();

    kv.sessions
        .change(&"s1".to_string(), Session::close)
        .raising(&events, &cx)
        .await
        .unwrap();
    assert_eq!(recorded.names(), ["session.closed"]);
    kv.delete_bucket().await;
}
