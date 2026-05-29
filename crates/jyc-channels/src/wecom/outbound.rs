//! WeCom (企业微信) outbound adapter implementation.
//!
//! This module handles sending messages via WeCom Bot webhook URLs.
//! WeCom Bot supports text and markdown message types.
//!
//! Reference: https://developer.work.weixin.qq.com/document/path/91770

use anyhow::{Context, Result};
use async_trait::async_trait;
use std::path::Path;
use std::sync::Arc;

use jyc_core::message_storage::MessageStorage;
use jyc_types::{
    InboundMessage, OutboundAdapter, OutboundAttachment, SendResult,
    config::OutboundAttachmentConfig,
};

use crate::wecom::crypto::generate_nonce;

/// WeCom outbound adapter — sends messages via Bot webhook URL.
///
/// WeCom Bot supports two message types:
/// - `text`: plain text messages (default)
/// - `markdown`: markdown formatted messages
///
/// Unlike WeChat, WeCom does not share a connection between inbound and outbound.
/// Each outbound request is a standalone HTTP POST to the Bot webhook URL.
pub struct WecomOutboundAdapter {
    webhook_url: String,
    storage: Arc<MessageStorage>,
    #[allow(dead_code)]
    attachment_config: Option<OutboundAttachmentConfig>,
    #[allow(dead_code)]
    footer_enabled: bool,
    /// Shared HTTP client with connection pool.
    client: reqwest::Client,
}

impl WecomOutboundAdapter {
    /// Create a new WeCom outbound adapter with attachments and footer support.
    pub fn new_with_attachments(
        webhook_url: String,
        storage: Arc<MessageStorage>,
        attachment_config: Option<OutboundAttachmentConfig>,
        footer_enabled: bool,
    ) -> Self {
        Self {
            webhook_url,
            storage,
            attachment_config,
            footer_enabled,
            client: reqwest::Client::new(),
        }
    }

    /// Build the JSON payload for a WeCom Bot message.
    fn build_payload(reply_text: &str, _original: &InboundMessage) -> serde_json::Value {
        // Detect if the content looks like markdown
        let is_markdown = reply_text.contains("```")
            || reply_text.contains("**")
            || reply_text.contains("##")
            || reply_text.contains("|")
            || reply_text.contains("- [")
            || reply_text.contains("![");

        if is_markdown {
            serde_json::json!({
                "msgtype": "markdown",
                "markdown": {
                    "content": reply_text
                }
            })
        } else {
            serde_json::json!({
                "msgtype": "text",
                "text": {
                    "content": reply_text
                }
            })
        }
    }
}

#[async_trait]
impl OutboundAdapter for WecomOutboundAdapter {
    fn channel_type(&self) -> &str {
        "wecom"
    }

    async fn connect(&self) -> Result<()> {
        // WeCom uses stateless HTTP requests, no persistent connection needed
        tracing::debug!("WeCom outbound: no connection needed (stateless HTTP)");
        Ok(())
    }

    async fn disconnect(&self) -> Result<()> {
        // No-op for stateless HTTP
        Ok(())
    }

    fn clean_body(&self, raw_body: &str) -> String {
        // WeCom is a simple channel with no quoting conventions to strip
        raw_body.to_string()
    }

    async fn send_reply(
        &self,
        original: &InboundMessage,
        reply_text: &str,
        thread_path: &Path,
        message_dir: &str,
        _attachments: Option<&[OutboundAttachment]>,
    ) -> Result<SendResult> {
        // Build the message payload
        let payload = Self::build_payload(reply_text, original);

        // Send via HTTP POST to the Bot webhook URL
        let response = self
            .client
            .post(&self.webhook_url)
            .json(&payload)
            .send()
            .await
            .with_context(|| "failed to send WeCom message to webhook URL".to_string())?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            anyhow::bail!("WeCom webhook returned error {}: {}", status, body);
        }

        let message_id = format!("wecom_{}", generate_nonce());
        let result = SendResult {
            message_id: message_id.clone(),
        };

        // Store the reply
        self.storage
            .store_reply(thread_path, message_dir, reply_text)
            .await
            .context("failed to store WeCom reply")?;

        Ok(result)
    }

    async fn send_alert(&self, _recipient: &str, subject: &str, body: &str) -> Result<SendResult> {
        let payload = serde_json::json!({
            "msgtype": "markdown",
            "markdown": {
                "content": format!("## {}\n\n{}", subject, body)
            }
        });

        let response = self
            .client
            .post(&self.webhook_url)
            .json(&payload)
            .send()
            .await
            .with_context(|| "failed to send WeCom alert")?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            anyhow::bail!("WeCom alert webhook returned error {}: {}", status, body);
        }

        let message_id = format!("wecom_{}", generate_nonce());
        Ok(SendResult { message_id })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use jyc_types::MessageContent;

    fn make_test_message(text: &str) -> InboundMessage {
        InboundMessage {
            id: "test-id".to_string(),
            channel: "wecom".to_string(),
            channel_uid: "test-uid".to_string(),
            sender: "test_user".to_string(),
            sender_address: "wecom:test".to_string(),
            recipients: vec![],
            topic: "Test".to_string(),
            content: MessageContent {
                text: Some(text.to_string()),
                html: None,
                markdown: None,
            },
            timestamp: chrono::Utc::now(),
            thread_refs: None,
            reply_to_id: None,
            external_id: None,
            attachments: vec![],
            metadata: std::collections::HashMap::new(),
            matched_pattern: None,
        }
    }

    #[test]
    fn test_build_payload_text() {
        let msg = make_test_message("Hello");
        let payload = WecomOutboundAdapter::build_payload("Hello World", &msg);
        assert_eq!(payload["msgtype"], "text");
        assert_eq!(payload["text"]["content"], "Hello World");
    }

    #[test]
    fn test_build_payload_markdown() {
        let msg = make_test_message("Hello");
        let payload = WecomOutboundAdapter::build_payload("## Title\n\n**bold** text", &msg);
        assert_eq!(payload["msgtype"], "markdown");
        assert_eq!(payload["markdown"]["content"], "## Title\n\n**bold** text");
    }

    #[test]
    fn test_build_payload_markdown_with_code_block() {
        let msg = make_test_message("Hello");
        let payload = WecomOutboundAdapter::build_payload("```rust\nfn main() {}\n```", &msg);
        assert_eq!(payload["msgtype"], "markdown");
    }

    #[test]
    fn test_build_payload_markdown_with_table() {
        let msg = make_test_message("Hello");
        let payload = WecomOutboundAdapter::build_payload("| A | B |\n|---|---|", &msg);
        assert_eq!(payload["msgtype"], "markdown");
    }

    #[test]
    fn test_clean_body() {
        let storage = Arc::new(MessageStorage::new(&std::env::temp_dir()));
        let adapter = WecomOutboundAdapter::new_with_attachments(
            "https://example.com/webhook".to_string(),
            storage,
            None,
            true,
        );
        let cleaned = adapter.clean_body("Hello **world**");
        assert_eq!(cleaned, "Hello **world**");
    }

    #[test]
    fn test_channel_type() {
        let storage = Arc::new(MessageStorage::new(&std::env::temp_dir()));
        let adapter = WecomOutboundAdapter::new_with_attachments(
            "https://example.com/webhook".to_string(),
            storage,
            None,
            true,
        );
        assert_eq!(adapter.channel_type(), "wecom");
    }
}
