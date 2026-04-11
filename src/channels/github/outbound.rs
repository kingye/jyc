//! GitHub outbound adapter implementation.
//!
//! This module handles sending messages to GitHub via HTTP API.

use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};
use async_trait::async_trait;

use crate::channels::github::client::GitHubClient;
use crate::channels::github::config::GitHubConfig;
use crate::channels::types::{InboundMessage, OutboundAttachment, SendResult};
use crate::config::types::OutboundAttachmentConfig;
use crate::core::email_parser;
use crate::core::message_storage::MessageStorage;
use crate::utils::attachment_validator;

pub struct GitHubOutboundAdapter {
    client: GitHubClient,
    storage: Arc<MessageStorage>,
    attachment_config: Option<OutboundAttachmentConfig>,
}

impl GitHubOutboundAdapter {
    #[allow(dead_code)]
    pub fn new(config: GitHubConfig, storage: Arc<MessageStorage>) -> Self {
        Self::new_with_attachments(config, storage, None)
    }

    pub fn new_with_attachments(
        config: GitHubConfig,
        storage: Arc<MessageStorage>,
        attachment_config: Option<OutboundAttachmentConfig>,
    ) -> Self {
        Self {
            client: GitHubClient::new(config),
            storage,
            attachment_config,
        }
    }
}

#[async_trait]
impl crate::channels::types::OutboundAdapter for GitHubOutboundAdapter {
    fn channel_type(&self) -> &str {
        "github"
    }

    async fn connect(&self) -> Result<()> {
        tracing::info!("GitHub outbound adapter connected");
        Ok(())
    }

    async fn disconnect(&self) -> Result<()> {
        tracing::info!("GitHub outbound adapter disconnected");
        Ok(())
    }

    fn clean_body(&self, raw_body: &str) -> String {
        raw_body.trim().to_string()
    }

    async fn send_reply(
        &self,
        original: &InboundMessage,
        reply_text: &str,
        thread_path: &Path,
        message_dir: &str,
        attachments: Option<&[OutboundAttachment]>,
    ) -> Result<SendResult> {
        let reply_ctx = crate::mcp::context::load_reply_context(thread_path).await.ok();
        let model = reply_ctx.as_ref().and_then(|c| c.model.as_deref());
        let mode = reply_ctx.as_ref().and_then(|c| c.mode.as_deref());

        let (input_tokens, max_tokens) = crate::services::opencode::session::read_input_tokens(thread_path).await;

        let footer = email_parser::build_footer(model, mode, input_tokens, max_tokens);

        let clean_reply = email_parser::strip_trailing_separators(reply_text);

        let full_reply = if footer.is_empty() {
            clean_reply
        } else {
            format!("{}\n\n{}", clean_reply, footer)
        };

        if let Some(attachments) = attachments {
            if let Some(ref config) = self.attachment_config {
                attachment_validator::validate_outbound_attachments(attachments, config)
                    .await
                    .context("Failed to validate outbound attachments")?;
                tracing::debug!("Outbound attachments validated successfully for GitHub");
            }
        }

        let issue_number = original.channel_uid.parse::<i64>()
            .context("Failed to parse issue number from channel_uid")?;

        let result = self.client.create_issue_comment(issue_number, &full_reply).await
            .context("Failed to create GitHub issue comment")?;

        tracing::info!(
            issue_number = %issue_number,
            comment_id = result.id,
            "GitHub issue comment created"
        );

        self.storage
            .store_reply(thread_path, &full_reply, message_dir)
            .await?;

        tracing::debug!(
            message_dir = %message_dir,
            "Reply stored"
        );

        Ok(SendResult {
            message_id: result.id.to_string(),
        })
    }

    async fn send_alert(
        &self,
        recipient: &str,
        subject: &str,
        body: &str,
    ) -> Result<SendResult> {
        let issue_number = recipient.parse::<i64>()
            .context("Failed to parse issue number from recipient")?;

        let alert_text = format!("**{}**\n\n{}", subject, body);

        let result = self.client.create_issue_comment(issue_number, &alert_text).await
            .context("Failed to create GitHub issue comment for alert")?;

        tracing::info!("GitHub alert sent to issue #{}: {}", issue_number, subject);

        Ok(SendResult {
            message_id: result.id.to_string(),
        })
    }

    async fn send_heartbeat(
        &self,
        original: &InboundMessage,
        message: &str,
    ) -> Result<SendResult> {
        let issue_number = original.channel_uid.parse::<i64>()
            .context("Failed to parse issue number from channel_uid")?;

        let result = self.client.create_issue_comment(issue_number, message).await
            .context("Failed to create GitHub issue comment for heartbeat")?;

        tracing::debug!("GitHub heartbeat sent to issue #{}", issue_number);

        Ok(SendResult {
            message_id: result.id.to_string(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::channels::types::OutboundAdapter;

    #[test]
    fn test_clean_body() {
        let adapter = GitHubOutboundAdapter::new_with_attachments(
            GitHubConfig::default(),
            Arc::new(MessageStorage::new(&std::path::PathBuf::new())),
            None,
        );
        assert_eq!(adapter.clean_body("  hello  "), "hello");
    }
}