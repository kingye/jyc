//! WeChat outbound adapter implementation.
//!
//! This module handles sending messages to WeChat via the OpenILink WebSocket Bridge.
//! Unlike Feishu which uses HTTP API calls, WeChat sends messages through the same
//! WebSocket connection used for receiving messages. The outbound adapter holds a
//! `mpsc::UnboundedSender<String>` to push JSON-formatted messages into the WebSocket.

use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};
use async_trait::async_trait;
use tokio::sync::{mpsc, Mutex};

use jyc_core::email_parser;
use jyc_core::message_storage::MessageStorage;
use jyc_types::{InboundMessage, OutboundAdapter, OutboundAttachment, SendResult};
use jyc_types::OutboundAttachmentConfig;

/// WeChat outbound adapter for sending messages via WebSocket.
///
/// Uses an `mpsc::UnboundedSender<String>` to push messages into the shared
/// WebSocket connection established by the inbound adapter. The sender is
/// stored behind `Arc<Mutex<Option<...>>>` so it can be set after construction
/// (the outbound adapter is created before the WebSocket is initialized).
pub struct WechatOutboundAdapter {
    /// Sender to push outbound messages through the WebSocket
    sender: Arc<Mutex<Option<mpsc::UnboundedSender<String>>>>,
    /// Message storage for logging replies
    storage: Arc<MessageStorage>,
    /// Attachment configuration
    #[allow(dead_code)]
    attachment_config: Option<OutboundAttachmentConfig>,
    /// Whether footer is enabled
    footer_enabled: bool,
}

impl WechatOutboundAdapter {
    /// Create a new WeChat outbound adapter.
    ///
    /// The `sender` is not available until the inbound adapter creates the
    /// WebSocket connection. Use `set_sender()` to set it before sending.
    pub fn new(storage: Arc<MessageStorage>) -> Self {
        Self {
            sender: Arc::new(Mutex::new(None)),
            storage,
            attachment_config: None,
            footer_enabled: true,
        }
    }

    /// Create a new WeChat outbound adapter with attachment config.
    pub fn new_with_attachments(
        storage: Arc<MessageStorage>,
        attachment_config: Option<OutboundAttachmentConfig>,
        footer_enabled: bool,
    ) -> Self {
        Self {
            sender: Arc::new(Mutex::new(None)),
            storage,
            attachment_config,
            footer_enabled,
        }
    }

    /// Get the shared sender Arc so the monitor can set it after WebSocket creation.
    pub fn sender_arc(&self) -> Arc<Mutex<Option<mpsc::UnboundedSender<String>>>> {
        self.sender.clone()
    }

    /// Set the WebSocket sender after the WebSocket connection is established.
    ///
    /// This is called by the monitor after creating the `WechatWebSocket` instance,
    /// allowing the inbound and outbound adapters to share the same connection.
    pub async fn set_sender(&self, sender: mpsc::UnboundedSender<String>) {
        let mut guard = self.sender.lock().await;
        *guard = Some(sender);
    }

    /// Send a JSON-formatted message through the WebSocket.
    async fn send_internal(&self, json_msg: &str) -> Result<()> {
        let guard = self.sender.lock().await;
        match guard.as_ref() {
            Some(sender) => sender
                .send(json_msg.to_string())
                .map_err(|e| anyhow::anyhow!("Failed to send WeChat outbound message: {}", e)),
            None => Err(anyhow::anyhow!(
                "WeChat outbound sender not set (WebSocket not initialized)"
            )),
        }
    }
}

#[async_trait]
impl OutboundAdapter for WechatOutboundAdapter {
    fn channel_type(&self) -> &str {
        "wechat"
    }

    async fn connect(&self) -> Result<()> {
        // WebSocket connection is managed by the inbound adapter.
        // The sender must be set via `set_sender()` before sending.
        let guard = self.sender.lock().await;
        if guard.is_some() {
            tracing::info!("WeChat outbound adapter connected (sender available)");
        } else {
            tracing::warn!("WeChat outbound adapter: no sender set yet (WebSocket may not be connected)");
        }
        Ok(())
    }

    async fn disconnect(&self) -> Result<()> {
        tracing::info!("WeChat outbound adapter disconnected");
        Ok(())
    }

    fn clean_body(&self, raw_body: &str) -> String {
        // WeChat messages don't have quoted reply history like email.
        // Just trim whitespace for now.
        raw_body.trim().to_string()
    }

    async fn send_reply(
        &self,
        _original: &InboundMessage,
        reply_text: &str,
        thread_path: &Path,
        message_dir: &str,
        attachments: Option<&[OutboundAttachment]>,
    ) -> Result<SendResult> {
        // 1. Read model/mode from reply context file (if available)
        let reply_ctx = jyc_mcp::context::load_reply_context(thread_path).await.ok();
        let model = reply_ctx.as_ref().and_then(|c| c.model.as_deref());
        let mode = reply_ctx.as_ref().and_then(|c| c.mode.as_deref());

        // Read current input tokens from session state
        let (input_tokens, max_tokens) =
            jyc_core::session_state::read_input_tokens(thread_path).await;

        // 2. Build footer with model/mode/tokens information
        let footer = email_parser::build_footer(
            model,
            mode,
            input_tokens,
            max_tokens,
            self.footer_enabled,
        );

        // 3. Clean reply text to remove any trailing `---` separators
        let clean_reply = email_parser::strip_trailing_separators(reply_text);

        // 4. Combine cleaned reply text with footer
        let full_reply = if footer.is_empty() {
            clean_reply
        } else {
            format!("{}\n\n{}", clean_reply, footer)
        };

        // 5. Skip attachment validation for v1 (text-only)
        // Attachments will be supported in future versions.

        // 6. Send the reply through WebSocket
        // Format: {"type":"send","content":"..."}
        let json_msg = serde_json::json!({
            "type": "send",
            "content": full_reply,
        })
        .to_string();

        let message_id = uuid::Uuid::new_v4().to_string();

        self.send_internal(&json_msg).await
            .context("Failed to send WeChat reply through WebSocket")?;

        tracing::info!(
            text_len = full_reply.len(),
            message_id = %message_id,
            "WeChat reply sent"
        );

        // 7. Handle attachments (WeChat v1: text-only, log a warning)
        if let Some(atts) = attachments {
            if !atts.is_empty() {
                tracing::warn!(
                    count = atts.len(),
                    "WeChat v1 does not support attachments, skipping"
                );
            }
        }

        // 8. Store reply to chat log
        self.storage
            .store_reply(thread_path, &full_reply, message_dir)
            .await?;

        tracing::debug!(
            message_dir = %message_dir,
            "Reply stored"
        );

        Ok(SendResult { message_id })
    }

    async fn send_alert(
        &self,
        recipient: &str,
        subject: &str,
        body: &str,
    ) -> Result<SendResult> {
        // Format alert message
        let alert_text = format!("{}\n\n{}", subject, body);

        let message_id = uuid::Uuid::new_v4().to_string();

        let json_msg = serde_json::json!({
            "type": "send",
            "content": alert_text,
        })
        .to_string();

        self.send_internal(&json_msg).await
            .context("Failed to send WeChat alert through WebSocket")?;

        tracing::info!("WeChat alert sent to {}: {}", recipient, subject);

        Ok(SendResult { message_id })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_send_reply_json_format() {
        let json_msg = serde_json::json!({
            "type": "send",
            "content": "Hello, world!",
        })
        .to_string();

        let parsed: serde_json::Value = serde_json::from_str(&json_msg).unwrap();
        assert_eq!(parsed["type"], "send");
        assert_eq!(parsed["content"], "Hello, world!");
    }

    #[test]
    fn test_send_alert_json_format() {
        let alert_text = "Alert: System down\n\nPlease check the server.";
        let json_msg = serde_json::json!({
            "type": "send",
            "content": alert_text,
        })
        .to_string();

        let parsed: serde_json::Value = serde_json::from_str(&json_msg).unwrap();
        assert_eq!(parsed["type"], "send");
        assert_eq!(parsed["content"], alert_text);
    }

    #[test]
    fn test_outbound_adapter_creation() {
        let storage = Arc::new(MessageStorage::new(
            &std::path::PathBuf::from("/tmp/test_wechat"),
        ));
        let adapter = WechatOutboundAdapter::new_with_attachments(
            storage,
            None,
            true,
        );
        assert_eq!(adapter.channel_type(), "wechat");
    }

    #[test]
    fn test_sender_set_and_connect() {
        let storage = Arc::new(MessageStorage::new(
            &std::path::PathBuf::from("/tmp/test_wechat"),
        ));
        let adapter = WechatOutboundAdapter::new_with_attachments(
            storage,
            None,
            true,
        );

        // Initially no sender
        let guard = adapter.sender.blocking_lock();
        assert!(guard.is_none());
        drop(guard);

        // Create a channel and set it
        let (tx, _rx) = mpsc::unbounded_channel();
        adapter.sender.blocking_lock().replace(tx);

        let guard = adapter.sender.blocking_lock();
        assert!(guard.is_some());
    }

    #[test]
    fn test_clean_body() {
        let storage = Arc::new(MessageStorage::new(
            &std::path::PathBuf::from("/tmp/test_wechat"),
        ));
        let adapter = WechatOutboundAdapter::new(storage);
        assert_eq!(adapter.clean_body("  hello  "), "hello");
        assert_eq!(adapter.clean_body("hello\n\nworld"), "hello\n\nworld");
        assert_eq!(adapter.clean_body("trimmed  "), "trimmed");
    }
}
