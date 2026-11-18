//! Standalone task scheduling. When your application runs inside the jenny
//! server runtime, prefer `JennyServer::schedule` — the server then drives the
//! scheduler alongside the HTTP host.
use jenny::task::{ScheduledTask, Task, TaskScheduler};

struct PrintTask(String);

#[async_trait::async_trait]
impl Task for PrintTask {
    async fn execute(&self) -> anyhow::Result<()> {
        println!("Executing task: {}", self.0);
        Ok(())
    }
}

#[tokio::main]
async fn main() -> Result<(), anyhow::Error> {
    let scheduler = TaskScheduler::default()
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

    // Stop starting runs on Ctrl-C, and let the ones in progress finish.
    scheduler
        .run_until(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await;

    Ok(())
}
