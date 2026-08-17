use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use tracing::instrument;

use super::handler::{CommandContext, CommandHandler, CommandResult};
use super::pin_common;
use crate::topic_manager::TopicManager;

/// `/unpin` command — remove a pinned topic configuration from config.toml.
pub struct UnpinCommandHandler {
    topic_manager: Arc<TopicManager>,
}

impl UnpinCommandHandler {
    pub fn new(topic_manager: Arc<TopicManager>) -> Self {
        Self { topic_manager }
    }
}

#[async_trait]
impl CommandHandler for UnpinCommandHandler {
    fn name(&self) -> &str {
        "/unpin"
    }

    fn description(&self) -> &str {
        "Remove pinned topic configuration from config.toml"
    }

    #[instrument(skip(self, context))]
    async fn execute(&self, context: CommandContext) -> Result<CommandResult> {
        // Build the shared pin/unpin context (validates websocket channel type, etc.)
        let ctx = match pin_common::build_pin_context(&context, &self.topic_manager).await {
            Ok(c) => c,
            Err(e) => {
                return Ok(CommandResult {
                    success: false,
                    message: e.to_string(),
                    error: Some(e.to_string()),
                    append_body: None,
                });
            }
        };

        let removed =
            pin_common::remove_pinned_from_config(&ctx.config_path, &ctx.adhoc_path).await?;

        if !removed {
            return Ok(CommandResult {
                success: false,
                message: format!(
                    "Topic '{}' is not pinned. No matching section found.",
                    ctx.topic_name
                ),
                error: Some("section not found".into()),
                append_body: None,
            });
        }

        Ok(CommandResult {
            success: true,
            message: format!(
                "✅ Unpinned topic '{}'.\n⚠️ Restart `jyc serve` for the change to take effect.",
                ctx.topic_name
            ),
            error: None,
            append_body: None,
        })
    }
}
