//! OpeniLink outbound adapter implementation.
//!
//! This module handles sending WeChat messages via the OpeniLink Hub HTTP API.

use std::path::Path;

use anyhow::{Context, Result};
use async_trait::async_trait;

use jyc_types::{InboundMessage, OutboundAttachment, SendResult};
use jyc_types::OpenilinkConfig;

use super::client::OpenilinkClient;

/// OpeniLink outbound adapter for sending messages via HTTP API.
pub struct OpenilinkOutboundAdapter {
    client: OpenilinkClient,
    footer_enabled: bool,
}

impl OpenilinkOutboundAdapter {
    /// Create a new OpeniLink outbound adapter.
    pub fn new(config: OpenilinkConfig, footer_enabled: bool) -> Self {
        Self {
            client: OpenilinkClient::new(config),
            footer_enabled,
        }
    }
}

#[async_trait]
impl jyc_types::OutboundAdapter for OpenilinkOutboundAdapter {
    fn channel_type(&self) -> &str {
        "openilink"
    }

    async fn connect(&self) -> Result<()> {
        // Verify API key is valid by fetching hub config
        let config = self
            .client
            .get_config()
            .await
            .context("Failed to connect to OpeniLink Hub: invalid API key or hub unreachable")?;

        tracing::info!(
            version = ?config.version,
            connected = config.connected,
            wechat_user = ?config.wechat_user.as_ref().and_then(|u| u.nickname.as_deref()),
            "OpeniLink outbound adapter connected"
        );
        Ok(())
    }

    async fn disconnect(&self) -> Result<()> {
        tracing::info!("OpeniLink outbound adapter disconnected");
        Ok(())
    }

    fn clean_body(&self, raw_body: &str) -> String {
        // WeChat messages don't have quoted reply history.
        // Just trim whitespace.
        raw_body.trim().to_string()
    }

    async fn send_reply(
        &self,
        original: &InboundMessage,
        reply_text: &str,
        thread_path: &Path,
        _message_dir: &str,
        _attachments: Option<&[OutboundAttachment]>,
    ) -> Result<SendResult> {
        // Extract the target user ID from the original message
        let to_user_id = original.sender_address.as_str();

        // Extract context_token from metadata for reply mode
        let context_token = original
            .metadata
            .get("context_token")
            .and_then(|v| v.as_str());

        // Build footer with model/mode/tokens information
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

        // Clean reply text
        let clean_reply = jyc_core::email_parser::strip_trailing_separators(reply_text);

        // Combine with footer
        let full_reply = if footer.is_empty() {
            clean_reply
        } else {
            format!("{}\n\n{}", clean_reply, footer)
        };

        // Send the message
        let result = if let Some(token) = context_token {
            // Reply mode: use context token to maintain conversation context
            self.client
                .send_message(to_user_id, &full_reply, Some(token))
                .await
                .context("Failed to send OpeniLink reply")?
        } else {
            // Active push mode: no context token (fallback)
            tracing::warn!(
                to_user_id = %to_user_id,
                "No context_token in message metadata, sending as active push"
            );
            self.client
                .send_message(to_user_id, &full_reply, None)
                .await
                .context("Failed to send OpeniLink reply")?
        };

        tracing::info!(
            to_user_id = %to_user_id,
            message_id = ?result.message_id,
            text_len = full_reply.len(),
            "OpeniLink reply sent"
        );

        Ok(SendResult {
            message_id: result.message_id.unwrap_or_default(),
        })
    }

    async fn send_alert(
        &self,
        _recipient: &str,
        _subject: &str,
        _body: &str,
    ) -> Result<SendResult> {
        // OpeniLink does not support alert sending in the initial version
        anyhow::bail!("OpeniLink channel does not support send_alert in this version");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use jyc_types::OutboundAdapter;

    #[test]
    fn test_channel_type() {
        let config = OpenilinkConfig::default();
        let adapter = OpenilinkOutboundAdapter::new(config, true);
        assert_eq!(adapter.channel_type(), "openilink");
    }

    #[test]
    fn test_clean_body() {
        let config = OpenilinkConfig::default();
        let adapter = OpenilinkOutboundAdapter::new(config, true);
        assert_eq!(adapter.clean_body("  Hello, World!  "), "Hello, World!");
        assert_eq!(adapter.clean_body("No change"), "No change");
    }
}
