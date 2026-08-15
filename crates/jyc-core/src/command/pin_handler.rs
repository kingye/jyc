use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use tracing::instrument;

use super::handler::{CommandContext, CommandHandler, CommandResult};
use super::pin_common;
use crate::topic_manager::TopicManager;

/// `/pin` command — persist an ad-hoc websocket topic to config.toml.
pub struct PinCommandHandler {
    topic_manager: Arc<TopicManager>,
}

impl PinCommandHandler {
    pub fn new(topic_manager: Arc<TopicManager>) -> Self {
        Self { topic_manager }
    }
}

#[async_trait]
impl CommandHandler for PinCommandHandler {
    fn name(&self) -> &str {
        "/pin"
    }

    fn description(&self) -> &str {
        "Pin this ad-hoc websocket topic to config.toml"
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

        let channel_name: String;

        if ctx.ws_channels.is_empty() {
            // No websocket channel — add a new channel section + pattern to file
            channel_name = ctx.topic_name.clone();

            // First add channel to file
            let channel_section = format!(
                "\n# Pinned by /pin command\n[channels.{}]\ntype = \"websocket\"\n",
                channel_name
            );
            let raw = tokio::fs::read_to_string(&ctx.config_path)
                .await
                .unwrap_or_default();
            let mut new_raw = raw;
            new_raw.push_str(&channel_section);
            tokio::fs::write(&ctx.config_path, &new_raw).await?;

            // Then append pattern
            pin_common::append_pattern_to_config(
                &ctx.config_path,
                &channel_name,
                &ctx.topic_name,
                &ctx.adhoc_path,
            )
            .await?;
        } else {
            channel_name = ctx.ws_channels[0].clone();

            // Check if already pinned by reading file
            let raw = tokio::fs::read_to_string(&ctx.config_path)
                .await
                .unwrap_or_default();
            let escaped = ctx.adhoc_path.to_string_lossy().replace('\\', "\\\\");
            if raw.contains(&escaped) {
                return Ok(CommandResult {
                    success: true,
                    message: format!(
                        "Topic '{}' is already pinned to channel '{}'.",
                        ctx.topic_name, channel_name
                    ),
                    error: None,
                    append_body: None,
                });
            }

            pin_common::append_pattern_to_config(
                &ctx.config_path,
                &channel_name,
                &ctx.topic_name,
                &ctx.adhoc_path,
            )
            .await?;
        }

        let display_path = ctx.adhoc_path.to_string_lossy();
        Ok(CommandResult {
            success: true,
            message: format!(
                "✅ Pinned topic '{}' to channel '{}' with topic_path '{}'.\n⚠️ Restart `jyc serve` for the change to take effect.",
                ctx.topic_name, channel_name, display_path
            ),
            error: None,
            append_body: None,
        })
    }
}
