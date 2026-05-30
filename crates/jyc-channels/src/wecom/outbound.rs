//! WeCom (企业微信) outbound adapter implementation.
//!
//! This module handles sending messages via the WeCom External Contact API
//! (`/cgi-bin/externalcontact/message/send`). Authentication uses
//! `corpid` + `corpsecret` to obtain an access_token.
//!
//! Reference: https://developer.work.weixin.qq.com/document/path/92135

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
use crate::wecom::token_cache::AccessTokenCache;

/// The external contact message send API base URL.
const EXTERNAL_CONTACT_API: &str =
    "https://qyapi.weixin.qq.com/cgi-bin/externalcontact/message/send";

/// WeCom outbound adapter — sends messages via external contact API.
///
/// Uses `corp_id` + `corp_secret` to obtain an access_token for authentication,
/// then sends messages to the WeCom External Contact message API.
///
/// Supports two message types:
/// - `text`: plain text messages (default)
/// - `markdown`: markdown formatted messages
pub struct WecomOutboundAdapter {
    access_token_cache: AccessTokenCache,
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
        corp_id: String,
        corp_secret: String,
        storage: Arc<MessageStorage>,
        attachment_config: Option<OutboundAttachmentConfig>,
        footer_enabled: bool,
    ) -> Self {
        Self {
            access_token_cache: AccessTokenCache::new(corp_id, corp_secret),
            storage,
            attachment_config,
            footer_enabled,
            client: reqwest::Client::new(),
        }
    }

    /// Get a valid access token from the cache.
    async fn get_token(&self) -> Result<String> {
        self.access_token_cache.get_token().await
    }

    /// Build the JSON payload for a WeCom external contact message.
    ///
    /// The payload includes `chat_id` and the message content (text or markdown).
    fn build_payload(reply_text: &str, original: &InboundMessage) -> serde_json::Value {
        // Get the chat_id from the original message metadata
        let chat_id = original
            .metadata
            .get("chat_id")
            .and_then(|v| v.as_str())
            .unwrap_or_default();

        // Detect if the content looks like markdown
        let is_markdown = reply_text.contains("```")
            || reply_text.contains("**")
            || reply_text.contains("##")
            || reply_text.contains("|")
            || reply_text.contains("- [")
            || reply_text.contains("![");

        if is_markdown {
            serde_json::json!({
                "chat_id": chat_id,
                "msgtype": "markdown",
                "markdown": {
                    "content": reply_text
                }
            })
        } else {
            serde_json::json!({
                "chat_id": chat_id,
                "msgtype": "text",
                "text": {
                    "content": reply_text
                }
            })
        }
    }

    /// Build the JSON payload for an alert message (always markdown).
    fn build_alert_payload(chat_id: &str, subject: &str, body: &str) -> serde_json::Value {
        serde_json::json!({
            "chat_id": chat_id,
            "msgtype": "markdown",
            "markdown": {
                "content": format!("## {}\n\n{}", subject, body)
            }
        })
    }
}

#[async_trait]
impl OutboundAdapter for WecomOutboundAdapter {
    fn channel_type(&self) -> &str {
        "wecom"
    }

    async fn connect(&self) -> Result<()> {
        // WeCom uses stateless HTTP requests, no persistent connection needed.
        // We verify connectivity by fetching an access token.
        self.get_token().await?;
        tracing::debug!("WeCom outbound: connected (access_token obtained)");
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
        // Get the access token
        let token = self.get_token().await?;

        // Build the message payload with chat_id from the original message
        let payload = Self::build_payload(reply_text, original);

        // Send via HTTP POST to the external contact API
        let url = format!("{}?access_token={}", EXTERNAL_CONTACT_API, token);
        let response = self
            .client
            .post(&url)
            .json(&payload)
            .send()
            .await
            .with_context(|| "failed to send WeCom external contact message".to_string())?;

        let status = response.status();
        let body: serde_json::Value = response.json().await.unwrap_or(serde_json::Value::Null);

        if !status.is_success() {
            let errmsg = body["errmsg"].as_str().unwrap_or("unknown error");
            anyhow::bail!(
                "WeCom external contact API returned error {}: {} (status: {})",
                body["errcode"],
                errmsg,
                status
            );
        }

        let errcode = body["errcode"].as_i64().unwrap_or(-1);
        if errcode != 0 {
            let errmsg = body["errmsg"].as_str().unwrap_or("unknown error");
            anyhow::bail!(
                "WeCom external contact API returned error {}: {}",
                errcode,
                errmsg
            );
        }

        let message_id = format!("wecom_{}", generate_nonce());
        let result = SendResult {
            message_id: message_id.clone(),
        };

        // Store the reply
        self.storage
            .store_reply(thread_path, reply_text, message_dir)
            .await
            .context("failed to store WeCom reply")?;

        Ok(result)
    }

    async fn send_alert(&self, recipient: &str, subject: &str, body: &str) -> Result<SendResult> {
        // Get the access token
        let token = self.get_token().await?;

        // The recipient is in format "wecom:{chat_id}" — extract the chat_id
        let chat_id = recipient.strip_prefix("wecom:").unwrap_or(recipient);

        let payload = Self::build_alert_payload(chat_id, subject, body);

        let url = format!("{}?access_token={}", EXTERNAL_CONTACT_API, token);
        let response = self
            .client
            .post(&url)
            .json(&payload)
            .send()
            .await
            .with_context(|| "failed to send WeCom alert")?;

        let status = response.status();
        let body: serde_json::Value = response.json().await.unwrap_or(serde_json::Value::Null);

        if !status.is_success() {
            let errmsg = body["errmsg"].as_str().unwrap_or("unknown error");
            anyhow::bail!(
                "WeCom alert API returned error {}: {} (status: {})",
                body["errcode"],
                errmsg,
                status,
            );
        }

        let errcode = body["errcode"].as_i64().unwrap_or(-1);
        if errcode != 0 {
            let errmsg = body["errmsg"].as_str().unwrap_or("unknown error");
            anyhow::bail!("WeCom alert API returned error {}: {}", errcode, errmsg);
        }

        let message_id = format!("wecom_{}", generate_nonce());
        Ok(SendResult { message_id })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use jyc_types::MessageContent;

    fn make_test_message(text: &str, chat_id: &str) -> InboundMessage {
        let mut metadata = std::collections::HashMap::new();
        metadata.insert(
            "chat_id".to_string(),
            serde_json::Value::String(chat_id.to_string()),
        );
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
            metadata,
            matched_pattern: None,
        }
    }

    #[test]
    fn test_build_payload_text() {
        let msg = make_test_message("Hello", "wr12345");
        let payload = WecomOutboundAdapter::build_payload("Hello World", &msg);
        assert_eq!(payload["chat_id"], "wr12345");
        assert_eq!(payload["msgtype"], "text");
        assert_eq!(payload["text"]["content"], "Hello World");
    }

    #[test]
    fn test_build_payload_markdown() {
        let msg = make_test_message("Hello", "wr12345");
        let payload = WecomOutboundAdapter::build_payload("## Title\n\n**bold** text", &msg);
        assert_eq!(payload["chat_id"], "wr12345");
        assert_eq!(payload["msgtype"], "markdown");
        assert_eq!(payload["markdown"]["content"], "## Title\n\n**bold** text");
    }

    #[test]
    fn test_build_payload_markdown_with_code_block() {
        let msg = make_test_message("Hello", "wr12345");
        let payload = WecomOutboundAdapter::build_payload("```rust\nfn main() {}\n```", &msg);
        assert_eq!(payload["chat_id"], "wr12345");
        assert_eq!(payload["msgtype"], "markdown");
    }

    #[test]
    fn test_build_payload_markdown_with_table() {
        let msg = make_test_message("Hello", "wr12345");
        let payload = WecomOutboundAdapter::build_payload("| A | B |\n|---|---|", &msg);
        assert_eq!(payload["chat_id"], "wr12345");
        assert_eq!(payload["msgtype"], "markdown");
    }

    #[test]
    fn test_build_payload_empty_chat_id() {
        let msg = make_test_message("Hello", "");
        let payload = WecomOutboundAdapter::build_payload("Hello World", &msg);
        assert_eq!(payload["chat_id"], "");
        assert_eq!(payload["msgtype"], "text");
    }

    #[test]
    fn test_clean_body() {
        let storage = Arc::new(MessageStorage::new(&std::env::temp_dir()));
        let adapter = WecomOutboundAdapter::new_with_attachments(
            "corp_id".to_string(),
            "corp_secret".to_string(),
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
            "corp_id".to_string(),
            "corp_secret".to_string(),
            storage,
            None,
            true,
        );
        assert_eq!(adapter.channel_type(), "wecom");
    }

    #[test]
    fn test_access_token_cache_creation() {
        let cache = AccessTokenCache::new("corp_id".to_string(), "corp_secret".to_string());
        let inner = cache.inner_clone();
        let guard = inner.lock().unwrap();
        assert!(guard.is_none());
    }

    #[test]
    fn test_build_alert_payload() {
        let payload = WecomOutboundAdapter::build_alert_payload(
            "wr12345",
            "Alert Title",
            "Alert body content",
        );
        assert_eq!(payload["chat_id"], "wr12345");
        assert_eq!(payload["msgtype"], "markdown");
        let content = payload["markdown"]["content"].as_str().unwrap();
        assert!(content.contains("Alert Title"));
        assert!(content.contains("Alert body content"));
    }

    #[test]
    fn test_build_alert_payload_empty_chat_id() {
        let payload = WecomOutboundAdapter::build_alert_payload("", "Subject", "Body");
        assert_eq!(payload["chat_id"], "");
    }
}
