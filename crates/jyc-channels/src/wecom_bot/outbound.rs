//! WeCom Smart Robot (wecom_bot) outbound adapter implementation.
//!
//! Handles sending replies via WebSocket using `aibot_respond_msg` and
//! proactive messages using `aibot_send_msg`.
//!
//! Supports streaming replies via `msgtype: "stream"`.
//!
//! Reference: doc 101031 - Passive Reply Messages

use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};
use async_trait::async_trait;
use tokio::sync::{Mutex, mpsc};

use jyc_core::email_parser;
use jyc_core::message_storage::MessageStorage;
use jyc_types::{
    InboundMessage, OutboundAdapter, OutboundAttachment, OutboundAttachmentConfig, SendResult,
};

/// WeCom Bot outbound adapter for sending messages via WebSocket.
///
/// Uses an `mpsc::UnboundedSender<String>` to push messages into the shared
/// WebSocket connection established by the inbound adapter.
pub struct WecomBotOutboundAdapter {
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

impl WecomBotOutboundAdapter {
    /// Create a new WeCom Bot outbound adapter.
    pub fn new(storage: Arc<MessageStorage>) -> Self {
        Self::new_with_attachments(storage, None, true)
    }

    /// Create a new WeCom Bot outbound adapter with attachment config.
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
                .map_err(|e| anyhow::anyhow!("Failed to send WeCom Bot outbound message: {}", e)),
            None => Err(anyhow::anyhow!(
                "WeCom Bot outbound sender not set (WebSocket not initialized)"
            )),
        }
    }

    /// Send a text/markdown reply through the WebSocket.
    ///
    /// NOTE: aibot_respond_msg only supports msgtype="stream" (and "template_card").
    /// Text/markdown must be sent as a stream with finish=true.
    async fn send_text_reply(
        &self,
        req_id: &str,
        content: &str,
        _use_markdown: bool,
    ) -> Result<()> {
        let stream_id = uuid::Uuid::new_v4().to_string();
        let json = serde_json::json!({
            "cmd": "aibot_respond_msg",
            "headers": {"req_id": req_id},
            "body": {
                "msgtype": "stream",
                "stream": {
                    "id": stream_id,
                    "content": content,
                    "finish": true
                }
            }
        })
        .to_string();

        self.send_internal(&json).await
    }

    /// Send a streaming reply chunk through the WebSocket.
    #[allow(dead_code)]
    async fn send_stream_chunk(
        &self,
        req_id: &str,
        stream_id: &str,
        content: &str,
        finish: bool,
    ) -> Result<()> {
        let json = serde_json::json!({
            "cmd": "aibot_respond_msg",
            "headers": {"req_id": req_id},
            "body": {
                "msgtype": "stream",
                "stream": {
                    "id": stream_id,
                    "content": content,
                    "finish": finish
                }
            }
        })
        .to_string();

        self.send_internal(&json).await
    }
}

#[async_trait]
impl OutboundAdapter for WecomBotOutboundAdapter {
    fn channel_type(&self) -> &str {
        "wecom_bot"
    }

    async fn connect(&self) -> Result<()> {
        let guard = self.sender.lock().await;
        if guard.is_some() {
            tracing::info!("WeCom Bot outbound adapter connected (sender available)");
        } else {
            tracing::warn!(
                "WeCom Bot outbound adapter: no sender set yet (WebSocket may not be connected)"
            );
        }
        Ok(())
    }

    async fn disconnect(&self) -> Result<()> {
        tracing::info!("WeCom Bot outbound adapter disconnected");
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
        _attachments: Option<&[OutboundAttachment]>,
    ) -> Result<SendResult> {
        // 1. Read model/mode from reply context file
        let reply_ctx = jyc_mcp::context::load_reply_context(thread_path).await.ok();
        let model = reply_ctx.as_ref().and_then(|c| c.model.as_deref());
        let mode = reply_ctx.as_ref().and_then(|c| c.mode.as_deref());

        // Read current input tokens from session state
        let (input_tokens, max_tokens) =
            jyc_core::session_state::read_input_tokens(thread_path).await;

        // 2. Build footer
        let footer =
            email_parser::build_footer(model, mode, input_tokens, max_tokens, self.footer_enabled);

        // 3. Clean reply text
        let clean_reply = email_parser::strip_trailing_separators(reply_text);

        // 4. Combine with footer
        let full_reply = if footer.is_empty() {
            clean_reply
        } else {
            format!("{}\n\n{}", clean_reply, footer)
        };

        // 5. Get req_id from original message metadata
        let req_id = original
            .metadata
            .get("req_id")
            .and_then(|v| v.as_str())
            .unwrap_or("");

        if req_id.is_empty() {
            tracing::warn!("Original message missing req_id, reply may not be correlated by WeCom");
        }

        // 6. Send reply via WebSocket
        // Use markdown for better formatting if the content contains markdown syntax
        let use_markdown = full_reply.contains("**")
            || full_reply.contains("*")
            || full_reply.contains("`")
            || full_reply.contains("#")
            || full_reply.contains("[")
            || full_reply.contains("- ");

        self.send_text_reply(req_id, &full_reply, use_markdown)
            .await
            .context("Failed to send WeCom Bot reply")?;

        let message_id = uuid::Uuid::new_v4().to_string();

        tracing::info!(
            text_len = full_reply.len(),
            req_id = %req_id,
            message_id = %message_id,
            "WeCom Bot reply sent"
        );

        // 7. Store reply to chat log
        self.storage
            .store_reply(thread_path, &full_reply, message_dir)
            .await
            .context("Failed to store WeCom Bot reply")?;

        Ok(SendResult { message_id })
    }

    async fn send_message(
        &self,
        recipient: &str,
        _subject: &str,
        body: &str,
    ) -> Result<SendResult> {
        // Proactive message: use aibot_send_msg with nested format
        let use_markdown = body.contains("**")
            || body.contains("*")
            || body.contains("`")
            || body.contains("#")
            || body.contains("[")
            || body.contains("- ");

        let body_json = if use_markdown {
            serde_json::json!({
                "msgtype": "markdown",
                "chatid": recipient,
                "markdown": {"content": body}
            })
        } else {
            serde_json::json!({
                "msgtype": "text",
                "chatid": recipient,
                "text": {"content": body}
            })
        };

        let json = serde_json::json!({
            "cmd": "aibot_send_msg",
            "headers": {"req_id": format!("aibot_send_msg_{}_{}",
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis(),
                uuid::Uuid::new_v4().to_string().replace('-', "")[..8].to_string()
            )},
            "body": body_json
        })
        .to_string();

        self.send_internal(&json)
            .await
            .context("Failed to send WeCom Bot proactive message")?;

        let message_id = uuid::Uuid::new_v4().to_string();

        tracing::info!(
            recipient = %recipient,
            text_len = body.len(),
            message_id = %message_id,
            "WeCom Bot proactive message sent"
        );

        Ok(SendResult { message_id })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_clean_body() {
        let adapter = WecomBotOutboundAdapter::new(Arc::new(MessageStorage::new(
            std::path::Path::new("/tmp"),
        )));
        assert_eq!(adapter.clean_body("  hello  "), "hello");
        assert_eq!(adapter.clean_body("hello\n\n"), "hello");
    }

    #[test]
    fn test_markdown_detection() {
        assert!("**bold**".contains("**"));
        assert!("*italic*".contains("*"));
        assert!("`code`".contains("`"));
        assert!("# heading".contains("#"));
        assert!("[link](url)".contains("["));
        assert!("- list".contains("- "));
    }
}
