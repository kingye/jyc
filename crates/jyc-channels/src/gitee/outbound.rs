use anyhow::{Context, Result};
use async_trait::async_trait;
use std::path::Path;
use std::sync::Arc;

use super::client::GiteeClient;
use jyc_core::message_storage::MessageStorage;
use jyc_types::GiteeConfig;
use jyc_types::{OutboundAdapter, OutboundAttachment, SendResult};

/// Gitee outbound adapter — posts comments on issues/PRs.
pub struct GiteeOutboundAdapter {
    config: GiteeConfig,
    storage: Arc<MessageStorage>,
    client: GiteeClient,
    footer_enabled: bool,
}

impl GiteeOutboundAdapter {
    pub fn new(config: GiteeConfig, storage: Arc<MessageStorage>) -> Result<Self> {
        Self::with_footer_enabled(config, storage, true)
    }

    pub fn with_footer_enabled(
        config: GiteeConfig,
        storage: Arc<MessageStorage>,
        footer_enabled: bool,
    ) -> Result<Self> {
        let client =
            GiteeClient::new(&config).context("Failed to create Gitee client for outbound")?;
        Ok(Self {
            config,
            storage,
            client,
            footer_enabled,
        })
    }
}

#[async_trait]
impl OutboundAdapter for GiteeOutboundAdapter {
    fn channel_type(&self) -> &str {
        "gitee"
    }

    async fn connect(&self) -> Result<()> {
        tracing::info!(
            owner = %self.config.owner,
            repo = %self.config.repo,
            "Gitee outbound adapter connected"
        );
        Ok(())
    }

    async fn disconnect(&self) -> Result<()> {
        tracing::info!("Gitee outbound adapter disconnected");
        Ok(())
    }

    fn clean_body(&self, raw_body: &str) -> String {
        raw_body.to_string()
    }

    async fn send_reply(
        &self,
        original: &jyc_types::InboundMessage,
        reply_text: &str,
        thread_path: &Path,
        message_dir: &str,
        _attachments: Option<&[OutboundAttachment]>,
    ) -> Result<SendResult> {
        let number = original
            .metadata
            .get("gitee_number")
            .and_then(|v| v.as_str())
            .unwrap_or("");

        if number.is_empty() {
            anyhow::bail!("gitee_number not found in message metadata");
        }

        let gitee_type = original
            .metadata
            .get("gitee_type")
            .and_then(|v| v.as_str())
            .unwrap_or("issue");
        let is_pr = gitee_type == "pull_request";

        let role = original
            .metadata
            .get("role")
            .and_then(|v| v.as_str())
            .unwrap_or("");

        let reply_ctx = jyc_mcp::context::load_reply_context(thread_path).await.ok();
        let model = reply_ctx.as_ref().and_then(|c| c.model.as_deref());
        let mode = reply_ctx.as_ref().and_then(|c| c.mode.as_deref());

        let (input_tokens, max_tokens) =
            jyc_core::session_state::read_input_tokens(thread_path).await;

        let footer = jyc_core::email_parser::build_footer(
            model,
            mode,
            input_tokens,
            max_tokens,
            self.footer_enabled,
        );

        let clean_reply = jyc_core::email_parser::strip_trailing_separators(reply_text);

        let reply_with_footer = if footer.is_empty() {
            clean_reply
        } else {
            format!("{}\n\n{}", clean_reply, footer)
        };

        let comment_body = if role.is_empty()
            || reply_with_footer
                .trim_start()
                .starts_with(&format!("[{}]", role))
        {
            reply_with_footer
        } else {
            format!("[{}] {}", role, reply_with_footer)
        };

        let comment_id = self
            .client
            .create_comment(number, &comment_body, is_pr)
            .await
            .with_context(|| format!("Failed to post comment on #{}", number))?;

        tracing::info!(
            number = number,
            comment_id = comment_id,
            role = role,
            reply_len = reply_text.len(),
            "Gitee comment posted"
        );

        self.storage
            .store_reply(thread_path, reply_text, message_dir)
            .await?;

        Ok(SendResult {
            message_id: format!("gitee-comment-{}", comment_id),
        })
    }

    async fn send_message(
        &self,
        _recipient: &str,
        _subject: &str,
        _body: &str,
    ) -> Result<SendResult> {
        tracing::debug!("Gitee send_message: not implemented");
        Ok(SendResult {
            message_id: "gitee-alert-noop".to_string(),
        })
    }
}
