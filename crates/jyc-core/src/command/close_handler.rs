use anyhow::Result;
use async_trait::async_trait;
use std::sync::Arc;

use super::handler::{CommandContext, CommandHandler, CommandResult};
use crate::thread_manager::ThreadManager;

/// /close command — close and delete thread directory.
pub struct CloseCommandHandler {
    thread_manager: Arc<ThreadManager>,
}

impl CloseCommandHandler {
    pub fn new(thread_manager: Arc<ThreadManager>) -> Self {
        Self { thread_manager }
    }
}

#[async_trait]
impl CommandHandler for CloseCommandHandler {
    fn name(&self) -> &str {
        "/close"
    }

    fn description(&self) -> &str {
        "Close thread and delete its directory"
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
                requires_restart: false,
            });
        }

        match self.thread_manager.close_thread(thread_name).await {
            Ok(()) => {
                tracing::info!(thread = %thread_name, "Thread closed successfully via /close command");
                Ok(CommandResult {
                    success: true,
                    message: format!(
                        "Thread '{}' closed and directory deleted.",
                        thread_name
                    ),
                    error: None,
                    requires_restart: false,
                })
            }
            Err(e) => Ok(CommandResult {
                success: false,
                message: format!("Failed to close thread '{}'", thread_name),
                error: Some(e.context("close_thread failed").to_string()),
                requires_restart: false,
            }),
        }
    }
}