//! The clock in: a scheduled task, as thin as a REST handler.

use async_trait::async_trait;
use jenny::task::Task;
use jenny::{Context, Criteria};

use crate::application::Notes;

/// Reports how many notes there are, every hour. It runs as
/// `System[basic-example/tasks]`, which is who Cedar sees when a use case it calls asks
/// `cx.authorize(…)`.
pub struct NoteCount(pub Notes);

#[async_trait]
impl Task for NoteCount {
    async fn execute(&self, cx: &Context) -> anyhow::Result<()> {
        let notes = self.0.list(&Criteria::default(), cx).await?;
        tracing::info!(
            notes = notes.len(),
            by = %cx.require_principal()?,
            "note count"
        );
        Ok(())
    }
}
