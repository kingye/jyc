use anyhow::Result;
use async_trait::async_trait;
use std::path::Path;
use std::sync::Arc;

use crate::channels::types::{OutboundAdapter, OutboundAttachment, SendResult};
use crate::core::message_storage::MessageStorage;
use super::config::GithubConfig;

/// GitHub outbound adapter — posts comments on issues/PRs.
pub struct GithubOutboundAdapter {
    config: GithubConfig,
    storage: Arc<MessageStorage>,
}

impl GithubOutboundAdapter {
    pub fn new(config: GithubConfig, storage: Arc<MessageStorage>) -> Self {
        Self { config, storage }
    }
}

#[async_trait]
impl OutboundAdapter for GithubOutboundAdapter {
    fn channel_type(&self) -> &str {
        "github"
    }

    async fn connect(&self) -> Result<()> {
        tracing::info!(
            owner = %self.config.owner,
            repo = %self.config.repo,
            "GitHub outbound adapter connected (stub)"
        );
        Ok(())
    }

    async fn disconnect(&self) -> Result<()> {
        tracing::info!("GitHub outbound adapter disconnected");
        Ok(())
    }

    fn clean_body(&self, raw_body: &str) -> String {
        raw_body.to_string()
    }

    async fn send_reply(
        &self,
        original: &crate::channels::types::InboundMessage,
        reply_text: &str,
        thread_path: &Path,
        message_dir: &str,
        _attachments: Option<&[OutboundAttachment]>,
    ) -> Result<SendResult> {
        let number = original
            .metadata
            .get("github_number")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);

        tracing::info!(
            number = %number,
            reply_len = %reply_text.len(),
            "GitHub outbound: would post comment (stub — not implemented yet)"
        );

        // Store reply to chat log
        self.storage
            .store_reply(thread_path, reply_text, message_dir)
            .await?;

        Ok(SendResult {
            message_id: format!("github-stub-{}", uuid::Uuid::new_v4()),
        })
    }

    async fn send_alert(
        &self,
        _recipient: &str,
        _subject: &str,
        _body: &str,
    ) -> Result<SendResult> {
        tracing::debug!("GitHub send_alert: not implemented");
        Ok(SendResult {
            message_id: "github-alert-stub".to_string(),
        })
    }

    async fn send_heartbeat(
        &self,
        _original: &crate::channels::types::InboundMessage,
        _message: &str,
    ) -> Result<SendResult> {
        // GitHub doesn't need heartbeats — comments are discrete
        Ok(SendResult {
            message_id: "github-heartbeat-noop".to_string(),
        })
    }
}
