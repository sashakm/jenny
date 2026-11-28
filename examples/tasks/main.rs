//! Standalone task scheduling. When your application runs inside the jenny
//! server runtime, prefer `JennyServer::schedule` — the server then drives the
//! scheduler alongside the HTTP host, and runs every task as `System[<service>/tasks]`.
use jenny::Principal;
use jenny::port::Context;
use jenny::task::{ScheduledTask, Task, TaskScheduler};

struct PrintTask(String);

#[async_trait::async_trait]
impl Task for PrintTask {
    async fn execute(&self, cx: &Context) -> anyhow::Result<()> {
        // A fresh context per run: its own request id, and the principal below.
        let who = cx.principal().map(ToString::to_string).unwrap_or_default();
        println!("Executing task: {} as {who} ({})", self.0, cx.request_id());
        Ok(())
    }
}

#[tokio::main]
async fn main() -> Result<(), anyhow::Error> {
    let scheduler = TaskScheduler::default()
        .running_as(Principal::system("tasks-example", "tasks"))
        .with_task(ScheduledTask::new(
            "Every minute task",
            PrintTask("Every Minute Task".into()),
            "0 */1 * * * *",
        )?)
        .with_task(ScheduledTask::new(
            "ten second task",
            PrintTask("Ten second task".into()),
            "*/10 * * * * *",
        )?)
        .with_task(ScheduledTask::new(
            "three second task",
            PrintTask("Three second task".into()),
            "*/3 * * * * *",
        )?);
    // With several replicas, give the scheduler a lease so each slot runs on one of them
    // (`tasks.lease` in `JennyServer`), and keep per-replica work on every replica:
    //
    //     let scheduler = scheduler.with_lease(lease, Duration::from_secs(30), "tasks-example");
    //     ScheduledTask::new("clear local cache", ClearCache, "0 * * * * *")?.per_replica()

    // Stop starting runs on Ctrl-C, and let the ones in progress finish.
    scheduler
        .run_until(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await;

    Ok(())
}
