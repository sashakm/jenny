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
//! use jenny_core::port::{Context, UseCase};
//! use jenny_core::task::Task;
//!
//! # use jenny_core::domain::DomainError;
//! # struct ExpireStaleNotes;
//! # #[async_trait]
//! # impl UseCase for ExpireStaleNotes {
//! #     type Input = (); type Output = usize; type Error = DomainError;
//! #     async fn invoke(&self, _: (), _: &Context) -> Result<usize, DomainError> { Ok(0) }
//! # }
//! /// Driving adapter: clock fires -> invoke the use case.
//! struct ExpiryTask(Arc<ExpireStaleNotes>);
//!
//! #[async_trait]
//! impl Task for ExpiryTask {
//!     async fn execute(&self) -> anyhow::Result<()> {
//!         let expired = self.0.invoke((), &Context::new()).await?;
//!         tracing::info!(expired, "expired stale notes");
//!         Ok(())
//!     }
//! }
//! ```

use std::{fmt::Debug, sync::Arc, time::Duration};

use anyhow::Context as _;
use async_trait::async_trait;
use cron::Schedule;
use futures::StreamExt;
use tokio_util::time::DelayQueue;
use tracing::{error, info};

/// Something to run when its schedule fires.
#[async_trait]
pub trait Task: Send + Sync + 'static {
    async fn execute(&self) -> anyhow::Result<()>;
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
///     async fn execute(&self) -> anyhow::Result<()> {
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
/// Each firing is spawned onto its own tokio task, so a slow run never delays the
/// next one or blocks a different task's schedule. A run that returns an error is
/// logged and the schedule continues — a failed run is not a reason to stop
/// scheduling.
///
/// The [`JennyServer`] runtime drives this for you; construct one directly only when
/// running tasks outside a server.
///
/// [`JennyServer`]: https://docs.rs/jenny-server
#[derive(Clone, Debug, Default)]
pub struct TaskScheduler {
    tasks: Vec<ScheduledTask>,
}

impl TaskScheduler {
    pub fn with_task(mut self, task: ScheduledTask) -> Self {
        self.tasks.push(task);
        self
    }

    pub fn is_empty(&self) -> bool {
        self.tasks.is_empty()
    }

    pub fn len(&self) -> usize {
        self.tasks.len()
    }

    /// Run until every schedule is exhausted, which for ordinary cron expressions
    /// means forever. Spawn this rather than awaiting it inline.
    pub async fn run(self) -> anyhow::Result<()> {
        let mut queue =
            self.tasks
                .iter()
                .enumerate()
                .try_fold(DelayQueue::new(), |mut acc, (idx, task)| {
                    if let Some(delay) = Self::duration_until_next(&task.schedule)? {
                        acc.insert(idx, delay);
                    }
                    Ok::<_, anyhow::Error>(acc)
                })?;

        while let Some(expired) = queue.next().await {
            if let Some(scheduled) = self.tasks.get(*expired.get_ref()) {
                let name = scheduled.name.clone();
                info!(task = %name, "executing scheduled task");
                let task = scheduled.task.clone();
                tokio::task::spawn(async move {
                    if let Err(e) = task.execute().await {
                        error!(task = %name, "scheduled task failed: {e}");
                    }
                });

                if let Some(delay) = Self::duration_until_next(&scheduled.schedule)? {
                    queue.insert(*expired.get_ref(), delay);
                }
            }
        }

        Ok(())
    }

    fn duration_until_next(schedule: &Schedule) -> anyhow::Result<Option<Duration>> {
        let Some(next) = schedule.upcoming(chrono::Local).next() else {
            return Ok(None);
        };
        let delay = next
            .signed_duration_since(chrono::Local::now())
            .to_std()
            .context("next scheduled time is in the past")?;
        Ok(Some(delay))
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
        async fn execute(&self) -> anyhow::Result<()> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    struct Failing;

    #[async_trait]
    impl Task for Failing {
        async fn execute(&self) -> anyhow::Result<()> {
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

    #[tokio::test(start_paused = true)]
    async fn tasks_fire_on_their_schedule() {
        let counter = Arc::new(AtomicU32::new(0));
        let scheduler = TaskScheduler::default().with_task(
            ScheduledTask::new("tick", Counting(counter.clone()), "*/1 * * * * * *").unwrap(),
        );
        let running = tokio::spawn(scheduler.run());

        tokio::time::sleep(Duration::from_secs(5)).await;
        // Yield so the spawned per-firing tasks get to run under the paused clock.
        tokio::task::yield_now().await;
        running.abort();

        assert!(
            counter.load(Ordering::SeqCst) >= 4,
            "expected at least 4 firings in 5 simulated seconds, got {}",
            counter.load(Ordering::SeqCst)
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
}
