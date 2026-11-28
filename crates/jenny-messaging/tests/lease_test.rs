//! `NatsLease` against a real NATS with JetStream: the `Lease` contract, renewals that
//! restart the bucket's `max_age`, the bucket's settings, and two schedulers sharing one
//! bucket running each slot once.
//!
//! Needs `JENNY_TEST_NATS_URL`, like `kv_test`. Each test uses a bucket of its own and
//! deletes it.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use jenny_core::port::{Context, Lease, TaskLease};
use jenny_core::task::{ScheduledTask, Task, TaskScheduler};
use jenny_messaging::NatsConnectionConfig;
use jenny_messaging::kv::Nats;
use jenny_messaging::lease::NatsLease;

fn nats_url() -> Option<String> {
    match std::env::var("JENNY_TEST_NATS_URL") {
        Ok(url) => Some(url),
        Err(_) if std::env::var_os("CI").is_some() => {
            panic!("JENNY_TEST_NATS_URL must be set in CI")
        }
        Err(_) => {
            eprintln!("skipping: JENNY_TEST_NATS_URL is not set (try `just test-nats`)");
            None
        }
    }
}

/// A lease in a bucket of its own, deleted by `delete_bucket`.
struct Fixture {
    nats: Nats,
    lease: NatsLease,
}

impl Fixture {
    async fn new(ttl: Duration) -> Option<Self> {
        let url = nats_url()?;
        let bucket = format!("leasetest_{}", uuid::Uuid::new_v4().simple());
        Some(Self::in_bucket(&url, &bucket, ttl).await)
    }

    async fn in_bucket(url: &str, bucket: &str, ttl: Duration) -> Self {
        let nats = Nats::new(NatsConnectionConfig::new(url), "lease-test");
        let lease = nats.lease_in(bucket, ttl);
        nats.prepare().await.unwrap();
        Self { nats, lease }
    }

    async fn max_age(&self) -> Duration {
        let jetstream = async_nats::jetstream::new(self.nats.client().await.unwrap());
        let store = jetstream.get_key_value(self.lease.bucket()).await.unwrap();
        store.status().await.unwrap().info.config.max_age
    }

    async fn delete_bucket(self) {
        let jetstream = async_nats::jetstream::new(self.nats.client().await.unwrap());
        jetstream
            .delete_key_value(self.lease.bucket())
            .await
            .unwrap();
        self.nats.close().await;
    }
}

const TTL: Duration = Duration::from_secs(2);

#[tokio::test]
async fn nats_leases_keep_the_contract() {
    let Some(fixture) = Fixture::new(TTL).await else {
        return;
    };
    jenny_core::testing::lease::conformance(
        &fixture.lease,
        TTL,
        Duration::from_millis(1_500),
        "contract",
    )
    .await;
    fixture.delete_bucket().await;
}

#[tokio::test]
async fn nats_a_renewal_restarts_the_age() {
    let Some(fixture) = Fixture::new(TTL).await else {
        return;
    };
    let lease = &fixture.lease;
    let held = lease.acquire("k", "one", TTL).await.unwrap().unwrap();
    for _ in 0..4 {
        // Four renewals 1.2s apart: 4.8s in all, well past one max_age.
        tokio::time::sleep(Duration::from_millis(1_200)).await;
        assert!(lease.renew(&held, TTL).await.unwrap(), "still held");
    }
    assert!(lease.acquire("k", "two", TTL).await.unwrap().is_none());
    assert!(
        held.revision() > held.token(),
        "renewals move the revision on"
    );
    lease.release(held).await.unwrap();
    assert!(lease.acquire("k", "two", TTL).await.unwrap().is_some());
    fixture.delete_bucket().await;
}

#[tokio::test]
async fn nats_the_bucket_follows_the_ttl_and_refuses_longer_leases() {
    let Some(url) = nats_url() else {
        return;
    };
    let bucket = format!("leasetest_{}", uuid::Uuid::new_v4().simple());
    // Made by an operator, with a setting of their own the lease must keep.
    let jetstream = async_nats::jetstream::new(async_nats::connect(&url).await.unwrap());
    jetstream
        .create_key_value(async_nats::jetstream::kv::Config {
            bucket: bucket.clone(),
            history: 1,
            max_age: Duration::from_secs(30),
            storage: async_nats::jetstream::stream::StorageType::Memory,
            ..Default::default()
        })
        .await
        .unwrap();

    let second = Fixture::in_bucket(&url, &bucket, TTL).await;
    assert_eq!(second.max_age().await, TTL, "max_age is updated to the ttl");
    let stream = jetstream.get_stream(format!("KV_{bucket}")).await.unwrap();
    assert_eq!(
        stream.cached_info().config.storage,
        async_nats::jetstream::stream::StorageType::Memory,
        "nothing but max_age changes"
    );
    let longer = second
        .lease
        .acquire("k", "one", Duration::from_secs(3))
        .await;
    assert!(
        longer.is_err(),
        "a lease would expire before its holder expects"
    );
    assert!(
        second
            .lease
            .acquire("k", "one", TTL)
            .await
            .unwrap()
            .is_some()
    );
    second.delete_bucket().await;
}

/// The lease each run held, `None` when it had none.
#[derive(Default)]
struct Slots(Mutex<Vec<Option<TaskLease>>>);

struct Recording(Arc<Slots>);

#[async_trait]
impl Task for Recording {
    async fn execute(&self, cx: &Context) -> anyhow::Result<()> {
        self.0
            .0
            .lock()
            .unwrap()
            .push(cx.get::<TaskLease>().cloned());
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
            .with_lease(lease.clone(), TTL, "lease-test")
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

#[tokio::test]
async fn nats_two_schedulers_run_each_slot_once() {
    let Some(fixture) = Fixture::new(TTL).await else {
        return;
    };
    let (one, two) = two_replicas(Arc::new(fixture.lease.clone()), false, 4).await;
    let mut keys: Vec<String> = one
        .iter()
        .chain(&two)
        .map(|run| run.clone().expect("a leased run carries its lease").key)
        .collect();
    let total = keys.len();
    keys.sort();
    keys.dedup();
    assert_eq!(keys.len(), total, "a slot ran twice: {one:?} {two:?}");
    assert!(
        (3..=5).contains(&total),
        "one run per slot in 4s, got {total}"
    );
    fixture.delete_bucket().await;
}

#[tokio::test]
async fn nats_per_replica_tasks_run_on_both() {
    let Some(fixture) = Fixture::new(TTL).await else {
        return;
    };
    let (one, two) = two_replicas(Arc::new(fixture.lease.clone()), true, 4).await;
    assert!(
        one.len() >= 3 && two.len() >= 3,
        "{} and {}",
        one.len(),
        two.len()
    );
    assert!(one.iter().chain(&two).all(Option::is_none));
    fixture.delete_bucket().await;
}
