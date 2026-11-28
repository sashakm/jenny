//! The clock as a driving adapter.
//!
//! A scheduled task is a driving adapter like any other: something outside the
//! application decides it is time, and the adapter's job is to invoke a use case. The
//! trigger happens to be a cron expression instead of an HTTP request, but the shape
//! is identical — which is why a [`Task`] implementation should stay as thin as a REST
//! handler, holding an `Arc` to its use case and little else.
//!
//! ```
//! use std::sync::Arc;
//! use async_trait::async_trait;
//! use jenny_core::port::Context;
//! use jenny_core::task::Task;
//!
//! # use jenny_core::domain::DomainResult;
//! # struct Notes;
//! # impl Notes { async fn expire_stale(&self, _: &Context) -> DomainResult<usize> { Ok(0) } }
//! /// Driving adapter: clock fires -> call the use case.
//! struct ExpiryTask(Arc<Notes>);
//!
//! #[async_trait]
//! impl Task for ExpiryTask {
//!     async fn execute(&self, cx: &Context) -> anyhow::Result<()> {
//!         let expired = self.0.expire_stale(cx).await?;
//!         tracing::info!(expired, "expired stale notes");
//!         Ok(())
//!     }
//! }
//! ```
//!
//! ### Who a task runs as
//!
//! Each run gets a fresh [`Context`]: a new request id, the label `task=<name>`, and
//! whatever the scheduler was told to run as. The `JennyServer` runtime makes that the
//! system principal `System[<service>/tasks]` with the configured `system.roles`, and
//! attaches its authorizer, so a use case's [`Context::authorize`] works the same way it
//! does for a request, and Cedar policies address the task as `System::"<service>/tasks"`.

use std::{
    fmt::Debug,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use async_trait::async_trait;
use chrono::{DateTime, Local, Utc};
use cron::Schedule;
use futures::StreamExt;
use tokio::task::JoinSet;
use tokio_util::time::DelayQueue;
use tracing::{error, info, warn};

use crate::domain::{Label, Principal};
use crate::port::Context;
use crate::port::outbound::Authorizer;

/// Something to run when its schedule fires.
#[async_trait]
pub trait Task: Send + Sync + 'static {
    /// One run. `cx` is new for every run; see [the module docs](self) for what it holds.
    async fn execute(&self, cx: &Context) -> anyhow::Result<()>;
}

/// A [`Task`] bound to a cron schedule and a name.
///
/// The schedule uses seven-field cron (seconds first), so sub-minute intervals like
/// `"*/5 * * * * * *"` are expressible.
///
/// ```
/// use async_trait::async_trait;
/// use jenny_core::task::{ScheduledTask, Task};
///
/// struct Ping;
///
/// #[async_trait]
/// impl Task for Ping {
///     async fn execute(&self, _: &jenny_core::port::Context) -> anyhow::Result<()> {
///         println!("ping");
///         Ok(())
///     }
/// }
///
/// let task = ScheduledTask::new("ping", Ping, "0 * * * * *")?;
/// assert_eq!(task.name(), "ping");
/// # Ok::<(), anyhow::Error>(())
/// ```
#[derive(Clone)]
pub struct ScheduledTask {
    task: Arc<dyn Task>,
    schedule: Schedule,
    name: String,
}

impl Debug for ScheduledTask {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ScheduledTask")
            .field("name", &self.name)
            .field("schedule", &self.schedule)
            .finish()
    }
}

impl ScheduledTask {
    /// Fails when `schedule` is not a valid cron expression.
    pub fn new<S: Into<String>>(name: S, task: impl Task, schedule: S) -> anyhow::Result<Self> {
        Ok(Self {
            name: name.into(),
            task: Arc::new(task),
            schedule: schedule.into().parse()?,
        })
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn schedule(&self) -> &Schedule {
        &self.schedule
    }
}

/// Runs [`ScheduledTask`]s at their appointed times.
///
/// Each firing is spawned onto its own tokio task, so a slow run never delays a
/// different task's schedule. A run that returns an error is logged and the schedule
/// continues: a failed run is not a reason to stop scheduling.
///
/// **One run per task at a time.** A firing that comes due while the previous run of
/// the same task is still going is skipped, with a warning, rather than started beside
/// it. Two copies of a cleanup job racing each other is rarely what anyone wanted.
///
/// **Slots are wall-clock times, delays are monotonic.** The scheduler reads the wall
/// clock once, when it starts, and advances it with tokio's monotonic clock from then
/// on. A slot is therefore never fired twice because a timer woke a moment before the
/// wall clock reached it, slots missed while the process was suspended are skipped
/// rather than fired in a burst, and changing the system clock while running does not
/// move the schedule.
///
/// The [`JennyServer`] runtime drives this for you, and at shutdown stops starting
/// runs and waits for those in progress. Construct one directly only when running
/// tasks outside a server.
///
/// [`JennyServer`]: https://docs.rs/jenny-server
#[derive(Clone, Default)]
pub struct TaskScheduler {
    tasks: Vec<ScheduledTask>,
    principal: Option<Principal>,
    authorizer: Option<Arc<dyn Authorizer>>,
}

impl Debug for TaskScheduler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TaskScheduler")
            .field("tasks", &self.tasks)
            .field("principal", &self.principal)
            .field("authorizer", &self.authorizer.is_some())
            .finish()
    }
}

/// The scheduler's notion of now: the wall clock when it started, plus the runtime's
/// monotonic time since.
struct Clock {
    wall: DateTime<Utc>,
    at: tokio::time::Instant,
}

impl Clock {
    fn start() -> Self {
        Self {
            wall: Utc::now(),
            at: tokio::time::Instant::now(),
        }
    }

    fn now(&self) -> DateTime<Utc> {
        let elapsed =
            chrono::Duration::from_std(self.at.elapsed()).unwrap_or(chrono::TimeDelta::MAX);
        self.wall + elapsed
    }

    /// The first slot of `schedule` after `fired` (or after now, whichever is later),
    /// and how long until it.
    fn next(
        &self,
        schedule: &Schedule,
        fired: Option<DateTime<Utc>>,
    ) -> Option<(DateTime<Utc>, Duration)> {
        let now = self.now();
        let from = fired.map_or(now, |fired| fired.max(now));
        let next = schedule
            .after(&from.with_timezone(&Local))
            .next()?
            .with_timezone(&Utc);
        // A slot can only lie in the past by the time it takes to compute it.
        let delay = (next - now).to_std().unwrap_or(Duration::ZERO);
        Some((next, delay))
    }
}

impl TaskScheduler {
    pub fn with_task(mut self, task: ScheduledTask) -> Self {
        self.tasks.push(task);
        self
    }

    /// Run every task as `principal`, usually [`Principal::system`]`(service, "tasks")`.
    /// Without one, a run's context has no principal.
    pub fn running_as(mut self, principal: Principal) -> Self {
        self.principal = Some(principal);
        self
    }

    /// Attach `authorizer` to every run's context.
    pub fn with_authorizer(mut self, authorizer: Arc<dyn Authorizer>) -> Self {
        self.authorizer = Some(authorizer);
        self
    }

    /// The context one run of the task `name` gets.
    fn context(&self, name: &str) -> Context {
        let mut cx = Context::new().with_labels([Label::new("task", name)]);
        if let Some(principal) = &self.principal {
            cx = cx.with_principal(principal.clone());
        }
        if let Some(authorizer) = &self.authorizer {
            cx = cx.with_authorizer(authorizer.clone());
        }
        cx
    }

    pub fn is_empty(&self) -> bool {
        self.tasks.is_empty()
    }

    pub fn len(&self) -> usize {
        self.tasks.len()
    }

    /// Run until every schedule is exhausted, which for ordinary cron expressions
    /// means forever. Spawn this rather than awaiting it inline.
    pub async fn run(self) {
        self.run_until(std::future::pending()).await;
    }

    /// Run until `shutdown` resolves, then start no more runs and wait for the ones in
    /// progress to finish.
    pub async fn run_until(self, shutdown: impl Future<Output = ()>) {
        let clock = Clock::start();
        let busy: Vec<Arc<AtomicBool>> = self.tasks.iter().map(|_| Arc::default()).collect();
        let mut queue = DelayQueue::new();
        for (index, task) in self.tasks.iter().enumerate() {
            if let Some((slot, delay)) = clock.next(&task.schedule, None) {
                queue.insert((index, slot), delay);
            }
        }

        let mut runs = JoinSet::new();
        tokio::pin!(shutdown);
        loop {
            tokio::select! {
                () = &mut shutdown => break,
                Some(expired) = queue.next() => {
                    let (index, slot) = expired.into_inner();
                    let scheduled = &self.tasks[index];
                    let name = scheduled.name.clone();

                    if busy[index].swap(true, Ordering::AcqRel) {
                        warn!(task = %name, "skipping a scheduled run: the previous one is still going");
                    } else {
                        let cx = self.context(&name);
                        info!(task = %name, request_id = %cx.request_id(), "executing scheduled task");
                        let task = scheduled.task.clone();
                        let busy = busy[index].clone();
                        runs.spawn(async move {
                            if let Err(e) = task.execute(&cx).await {
                                error!(task = %name, request_id = %cx.request_id(), "scheduled task failed: {e}");
                            }
                            busy.store(false, Ordering::Release);
                        });
                    }

                    if let Some((next, delay)) = clock.next(&scheduled.schedule, Some(slot)) {
                        queue.insert((index, next), delay);
                    }
                }
                Some(finished) = runs.join_next(), if !runs.is_empty() => {
                    if let Err(e) = finished
                        && e.is_panic()
                    {
                        error!("a scheduled task panicked: {e}");
                    }
                }
                else => break,
            }
        }

        if !runs.is_empty() {
            info!(
                running = runs.len(),
                "waiting for scheduled runs in progress"
            );
        }
        while runs.join_next().await.is_some() {}
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc,
        atomic::{AtomicU32, Ordering},
    };

    use super::*;

    struct Counting(Arc<AtomicU32>);

    #[async_trait]
    impl Task for Counting {
        async fn execute(&self, _: &Context) -> anyhow::Result<()> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    struct Failing;

    #[async_trait]
    impl Task for Failing {
        async fn execute(&self, _: &Context) -> anyhow::Result<()> {
            anyhow::bail!("nope")
        }
    }

    #[test]
    fn an_invalid_cron_expression_is_rejected() {
        assert!(ScheduledTask::new("bad", Counting(Arc::default()), "not a cron").is_err());
    }

    #[test]
    fn the_scheduler_reports_what_it_holds() {
        let scheduler = TaskScheduler::default();
        assert!(scheduler.is_empty());
        let scheduler = scheduler.with_task(
            ScheduledTask::new("tick", Counting(Arc::default()), "*/1 * * * * * *").unwrap(),
        );
        assert_eq!(scheduler.len(), 1);
        assert!(!scheduler.is_empty());
    }

    /// Counts its runs, each taking `takes`, and the most that were ever in flight.
    #[derive(Default)]
    struct Slow {
        runs: AtomicU32,
        in_flight: AtomicU32,
        most_in_flight: AtomicU32,
        takes: Duration,
    }

    #[async_trait]
    impl Task for Arc<Slow> {
        async fn execute(&self, _: &Context) -> anyhow::Result<()> {
            self.runs.fetch_add(1, Ordering::SeqCst);
            let now = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
            self.most_in_flight.fetch_max(now, Ordering::SeqCst);
            tokio::time::sleep(self.takes).await;
            self.in_flight.fetch_sub(1, Ordering::SeqCst);
            Ok(())
        }
    }

    #[tokio::test(start_paused = true)]
    async fn tasks_fire_once_per_slot() {
        let counter = Arc::new(AtomicU32::new(0));
        let scheduler = TaskScheduler::default().with_task(
            ScheduledTask::new("tick", Counting(counter.clone()), "*/1 * * * * * *").unwrap(),
        );
        let running = tokio::spawn(scheduler.run());

        tokio::time::sleep(Duration::from_millis(5_500)).await;
        // Yield so the spawned per-firing tasks get to run under the paused clock.
        tokio::task::yield_now().await;
        running.abort();

        let fired = counter.load(Ordering::SeqCst);
        assert!(
            (5..=6).contains(&fired),
            "expected one firing per second in 5.5 simulated seconds, got {fired}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_failing_task_does_not_stop_the_schedule() {
        let counter = Arc::new(AtomicU32::new(0));
        let scheduler = TaskScheduler::default()
            .with_task(ScheduledTask::new("boom", Failing, "*/1 * * * * * *").unwrap())
            .with_task(
                ScheduledTask::new("tick", Counting(counter.clone()), "*/1 * * * * * *").unwrap(),
            );
        let running = tokio::spawn(scheduler.run());

        tokio::time::sleep(Duration::from_secs(3)).await;
        tokio::task::yield_now().await;
        running.abort();

        assert!(counter.load(Ordering::SeqCst) >= 2);
    }

    #[tokio::test(start_paused = true)]
    async fn a_run_that_overruns_its_interval_is_not_doubled() {
        let slow = Arc::new(Slow {
            takes: Duration::from_millis(2_500),
            ..Default::default()
        });
        let scheduler = TaskScheduler::default()
            .with_task(ScheduledTask::new("slow", slow.clone(), "*/1 * * * * * *").unwrap());
        let running = tokio::spawn(scheduler.run());

        tokio::time::sleep(Duration::from_secs(10)).await;
        running.abort();

        assert_eq!(slow.most_in_flight.load(Ordering::SeqCst), 1);
        let runs = slow.runs.load(Ordering::SeqCst);
        assert!(
            (3..=4).contains(&runs),
            "one run per 2.5s + skipped slots, got {runs}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn shutdown_waits_for_runs_in_progress_and_starts_no_more() {
        let slow = Arc::new(Slow {
            takes: Duration::from_secs(3),
            ..Default::default()
        });
        let scheduler = TaskScheduler::default()
            .with_task(ScheduledTask::new("slow", slow.clone(), "*/1 * * * * * *").unwrap());

        let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
        let running = tokio::spawn(scheduler.run_until(async {
            let _ = stopped.await;
        }));
        tokio::time::sleep(Duration::from_millis(1_500)).await;
        assert_eq!(slow.in_flight.load(Ordering::SeqCst), 1);

        stop.send(()).unwrap();
        running.await.unwrap();
        assert_eq!(
            slow.in_flight.load(Ordering::SeqCst),
            0,
            "the run was allowed to finish"
        );
        assert_eq!(slow.runs.load(Ordering::SeqCst), 1);
    }

    /// Keeps the context of every run.
    #[derive(Default)]
    struct Witness(std::sync::Mutex<Vec<(uuid::Uuid, Option<Principal>, Vec<Label>, bool)>>);

    #[async_trait]
    impl Task for Arc<Witness> {
        async fn execute(&self, cx: &Context) -> anyhow::Result<()> {
            self.0.lock().unwrap().push((
                cx.request_id(),
                cx.principal().cloned(),
                cx.labels().to_vec(),
                cx.authorizer().is_some(),
            ));
            Ok(())
        }
    }

    struct AllowAll;

    #[async_trait]
    impl Authorizer for AllowAll {
        async fn authorize(
            &self,
            _: &Principal,
            _: &crate::port::AccessRequest,
        ) -> Result<(), crate::domain::AuthorizationError> {
            Ok(())
        }
    }

    #[tokio::test(start_paused = true)]
    async fn every_run_gets_a_fresh_context_as_the_configured_principal() {
        let witness = Arc::new(Witness::default());
        let scheduler = TaskScheduler::default()
            .running_as(Principal::system("my-notes", "tasks"))
            .with_authorizer(Arc::new(AllowAll))
            .with_task(ScheduledTask::new("tidy", witness.clone(), "*/1 * * * * * *").unwrap());
        let running = tokio::spawn(scheduler.run());
        tokio::time::sleep(Duration::from_millis(2_500)).await;
        tokio::task::yield_now().await;
        running.abort();

        let runs = witness.0.lock().unwrap();
        assert!(runs.len() >= 2, "{}", runs.len());
        assert_ne!(runs[0].0, runs[1].0, "a request id per run");
        for (_, principal, labels, authorized) in runs.iter() {
            assert_eq!(
                principal.as_ref().unwrap().subject(),
                "System[my-notes/tasks]"
            );
            assert_eq!(labels, &[Label::new("task", "tidy")]);
            assert!(authorized);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_scheduler_told_nothing_runs_without_a_principal() {
        let witness = Arc::new(Witness::default());
        let scheduler = TaskScheduler::default()
            .with_task(ScheduledTask::new("tidy", witness.clone(), "*/1 * * * * * *").unwrap());
        let running = tokio::spawn(scheduler.run());
        tokio::time::sleep(Duration::from_millis(1_500)).await;
        tokio::task::yield_now().await;
        running.abort();
        let runs = witness.0.lock().unwrap();
        assert!(!runs.is_empty());
        assert!(
            runs.iter()
                .all(|(_, principal, _, authorized)| principal.is_none() && !authorized)
        );
    }

    #[test]
    fn a_slot_is_never_handed_out_twice() {
        let clock = Clock::start();
        let schedule: Schedule = "*/1 * * * * * *".parse().unwrap();
        let (first, _) = clock.next(&schedule, None).unwrap();
        // Even if asked again before the wall clock reaches `first`...
        let (second, _) = clock.next(&schedule, Some(first)).unwrap();
        assert_eq!(second - first, chrono::Duration::seconds(1));
    }

    #[test]
    fn a_slot_in_the_past_is_due_now_rather_than_an_error() {
        let clock = Clock::start();
        let schedule: Schedule = "*/1 * * * * * *".parse().unwrap();
        let long_ago = clock.now() - chrono::Duration::hours(1);
        let (next, delay) = clock.next(&schedule, Some(long_ago)).unwrap();
        assert!(
            next > clock.now() - chrono::Duration::seconds(1),
            "missed slots are skipped"
        );
        assert!(delay <= Duration::from_secs(1));
    }
}
