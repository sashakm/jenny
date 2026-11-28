//! `PgLease` against a real Postgres: the `Lease` contract, the sweep of old rows, and
//! two schedulers sharing one table running each slot once.
//!
//! Needs `JENNY_TEST_POSTGRES_URL`, like `postgres_test`. Each test runs in a schema of
//! its own, because the table is always `jenny_leases`.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use jenny_core::port::{Context, Lease, TaskLease};
use jenny_core::task::{ScheduledTask, Task, TaskScheduler};
use jenny_persistence::Postgres;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};

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

/// A handle whose tables live in a fresh schema, dropped by `drop_schema`.
struct Fixture {
    postgres: Postgres,
    admin: sqlx::PgPool,
    schema: String,
}

impl Fixture {
    async fn new() -> Option<Self> {
        let url = database_url()?;
        let schema = format!("lease_test_{}", uuid::Uuid::new_v4().simple());
        let admin = PgPoolOptions::new().connect(&url).await.unwrap();
        sqlx::query(sqlx::AssertSqlSafe(format!("CREATE SCHEMA \"{schema}\"")))
            .execute(&admin)
            .await
            .unwrap();
        let options: PgConnectOptions = url.parse().unwrap();
        let pool = PgPoolOptions::new()
            .connect_lazy_with(options.options([("search_path", schema.as_str())]));
        Some(Self {
            postgres: Postgres::from_pool(pool),
            admin,
            schema,
        })
    }

    async fn lease(&self) -> jenny_persistence::PgLease {
        let lease = self.postgres.lease();
        self.postgres.prepare().await.unwrap();
        lease
    }

    async fn rows(&self) -> i64 {
        sqlx::query_scalar("SELECT count(*) FROM jenny_leases")
            .fetch_one(self.postgres.pool())
            .await
            .unwrap()
    }

    async fn drop_schema(self) {
        self.postgres.close().await;
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "DROP SCHEMA \"{}\" CASCADE",
            self.schema
        )))
        .execute(&self.admin)
        .await
        .unwrap();
    }
}

#[tokio::test]
async fn postgres_leases_keep_the_contract() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let lease = fixture.lease().await;
    jenny_core::testing::lease::conformance(
        &lease,
        Duration::from_secs(2),
        Duration::from_millis(500),
        "contract",
    )
    .await;
    fixture.drop_schema().await;
}

#[tokio::test]
async fn postgres_sweeps_leases_expired_over_a_minute_ago() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let lease = fixture.lease().await;
    sqlx::query(
        "INSERT INTO jenny_leases (key, holder, token, expires_at) VALUES
            ('old', 'x', 1, now() - interval '2 minutes'),
            ('recent', 'x', 2, now() - interval '10 seconds')",
    )
    .execute(fixture.postgres.pool())
    .await
    .unwrap();
    let held = lease
        .acquire("new", "one", Duration::from_secs(30))
        .await
        .unwrap()
        .unwrap();
    assert!(held.token() >= 1);
    assert_eq!(
        fixture.rows().await,
        2,
        "`old` is gone, `recent` and `new` stay"
    );
    fixture.drop_schema().await;
}

#[tokio::test]
async fn postgres_lease_schema_runs_as_a_migration() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    sqlx::raw_sql(sqlx::AssertSqlSafe(Postgres::lease_schema()))
        .execute(fixture.postgres.pool())
        .await
        .unwrap();
    assert_eq!(fixture.rows().await, 0);
    fixture.drop_schema().await;
}

/// The lease each run held, `None` when it had none.
#[derive(Default)]
struct Slots(Mutex<Vec<Option<TaskLease>>>);

/// One replica's copy of the task, recording into its `Slots`.
struct Recording(Arc<Slots>);

#[async_trait]
impl Task for Recording {
    async fn execute(&self, cx: &Context) -> anyhow::Result<()> {
        self.0.0.lock().unwrap().push(cx.get::<TaskLease>().cloned());
        Ok(())
    }
}

/// Two schedulers on one lease, each with its own copy of the task, for `secs`.
async fn two_replicas(
    lease: Arc<dyn Lease>,
    per_replica: bool,
    secs: u64,
) -> (Vec<Option<TaskLease>>, Vec<Option<TaskLease>>) {
    let (one, two) = (Arc::new(Slots::default()), Arc::new(Slots::default()));
    let replica = |slots: &Arc<Slots>| {
        let task = ScheduledTask::new("tick", Recording(slots.clone()), "*/1 * * * * * *").unwrap();
        let task = if per_replica {
            task.per_replica()
        } else {
            task
        };
        TaskScheduler::default()
            .with_lease(lease.clone(), Duration::from_secs(5), "lease-test")
            .with_task(task)
    };
    let (stop, _) = tokio::sync::watch::channel(false);
    let runs: Vec<_> = [replica(&one), replica(&two)]
        .into_iter()
        .map(|scheduler| {
            let mut stopping = stop.subscribe();
            tokio::spawn(scheduler.run_until(async move {
                let _ = stopping.wait_for(|stop| *stop).await;
            }))
        })
        .collect();
    tokio::time::sleep(Duration::from_secs(secs)).await;
    stop.send(true).unwrap();
    for run in runs {
        run.await.unwrap();
    }
    let take = |slots: Arc<Slots>| std::mem::take(&mut *slots.0.lock().unwrap());
    (take(one), take(two))
}

/// Every slot that ran, once each, between both replicas.
fn assert_once_per_slot(one: &[Option<TaskLease>], two: &[Option<TaskLease>], secs: u64) {
    let mut keys: Vec<String> = one
        .iter()
        .chain(two)
        .map(|run| run.clone().expect("a leased run carries its lease").key)
        .collect();
    let total = keys.len();
    keys.sort();
    keys.dedup();
    assert_eq!(keys.len(), total, "a slot ran twice: {one:?} {two:?}");
    assert!(
        (secs - 1..=secs + 1).contains(&(total as u64)),
        "one run per slot in {secs}s, got {total}"
    );
}

#[tokio::test]
async fn postgres_two_schedulers_run_each_slot_once() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let lease: Arc<dyn Lease> = Arc::new(fixture.lease().await);
    let (one, two) = two_replicas(lease, false, 4).await;
    assert_once_per_slot(&one, &two, 4);
    fixture.drop_schema().await;
}

#[tokio::test]
async fn postgres_per_replica_tasks_run_on_both() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let lease: Arc<dyn Lease> = Arc::new(fixture.lease().await);
    let (one, two) = two_replicas(lease, true, 4).await;
    assert!(
        one.len() >= 3 && two.len() >= 3,
        "{} and {}",
        one.len(),
        two.len()
    );
    assert!(one.iter().chain(&two).all(Option::is_none));
    assert_eq!(fixture.rows().await, 0, "nothing was leased");
    fixture.drop_schema().await;
}
