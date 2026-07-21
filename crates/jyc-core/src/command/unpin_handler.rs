use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use tracing::instrument;

use super::handler::{CommandContext, CommandHandler, CommandResult};
use super::pin_common;
use crate::thread_manager::ThreadManager;

/// `/unpin` command — remove a pinned thread configuration from config.toml.
pub struct UnpinCommandHandler {
    thread_manager: Arc<ThreadManager>,
}

impl UnpinCommandHandler {
    pub fn new(thread_manager: Arc<ThreadManager>) -> Self {
        Self { thread_manager }
    }
}

#[async_trait]
impl CommandHandler for UnpinCommandHandler {
    fn name(&self) -> &str {
        "/unpin"
    }

    fn description(&self) -> &str {
        "Remove pinned thread configuration from config.toml"
    }

    #[instrument(skip(self, context))]
    async fn execute(&self, context: CommandContext) -> Result<CommandResult> {
        // Build the shared pin/unpin context (validates websocket channel type, etc.)
        let ctx = match pin_common::build_pin_context(&context, &self.thread_manager).await {
            Ok(c) => c,
            Err(e) => {
                return Ok(CommandResult {
                    success: false,
                    message: e.to_string(),
                    error: Some(e.to_string()),
                });
            }
        };

        if ctx.ws_channels.is_empty() {
            return Ok(CommandResult {
                success: false,
                message: "No websocket channels in config. Nothing to unpin.".into(),
                error: Some("no websocket channels".into()),
            });
        }

        let removed =
            pin_common::remove_pattern_from_config(&ctx.config_path, &ctx.adhoc_path).await?;

        if !removed {
            return Ok(CommandResult {
                success: false,
                message: format!(
                    "Thread '{}' is not pinned. No matching pattern found.",
                    ctx.thread_name
                ),
                error: Some("pattern not found".into()),
            });
        }

        Ok(CommandResult {
            success: true,
            message: format!(
                "✅ Unpinned thread '{}'.\n⚠️ Restart `jyc serve` for the change to take effect.",
                ctx.thread_name
            ),
            error: None,
        })
    }
}
