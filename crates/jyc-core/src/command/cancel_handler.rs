use anyhow::Result;
use async_trait::async_trait;
use std::sync::Arc;

use super::handler::{CommandContext, CommandHandler, CommandResult};
use crate::thread_manager::ThreadManager;

/// /cancel command — cancel the current AI processing for this thread.
///
/// Triggers the per-thread cancellation token, causing the agent loop to
/// break at the next iteration check. The thread directory and queue are
/// preserved.
pub struct CancelCommandHandler {
    thread_manager: Arc<ThreadManager>,
}

impl CancelCommandHandler {
    pub fn new(thread_manager: Arc<ThreadManager>) -> Self {
        Self { thread_manager }
    }
}

#[async_trait]
impl CommandHandler for CancelCommandHandler {
    fn name(&self) -> &str {
        "/cancel"
    }

    fn description(&self) -> &str {
        "Cancel the current AI processing for this thread"
    }

    async fn execute(&self, context: CommandContext) -> Result<CommandResult> {
        let thread_name = context
            .thread_path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("");

        if thread_name.is_empty() {
            return Ok(CommandResult {
                success: false,
                message: format!(
                    "Failed to determine thread name from path: {:?}",
                    context.thread_path
                ),
                error: Some("Thread directory name could not be extracted".into()),
            });
        }

        self.thread_manager.cancel_thread(thread_name).await;

        Ok(CommandResult {
            success: true,
            message: format!("AI processing cancelled for thread '{}'.", thread_name),
            error: None,
        })
    }
}
