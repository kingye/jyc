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

/// Tracks an active streaming message so the final reply can reuse the same
/// `stream.id` and update the message in-place instead of posting a second one.
#[derive(Debug, Clone)]
struct ActiveStream {
    req_id: String,
    stream_id: String,
}

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
    /// Currently active stream message (if any). Used to correlate the final
    /// `finish=true` reply with an earlier `finish=false` processing indicator.
    active_stream: Arc<Mutex<Option<ActiveStream>>>,
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
            active_stream: Arc::new(Mutex::new(None)),
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
        stream_id: &str,
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

    /// Update an existing processing indicator with new content.
    ///
    /// Unlike `send_reply`, this does NOT clear the `active_stream` state,
    /// allowing subsequent updates to reuse the same `stream_id`.
    pub async fn update_processing_indicator(
        &self,
        req_id: &str,
        stream_id: &str,
        content: &str,
    ) -> Result<()> {
        self.send_text_reply(req_id, content, stream_id, false)
            .await
            .context("Failed to update WeCom Bot processing indicator")
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

        // 6. Check if there's an active stream for this req_id
        let active = self.active_stream.lock().await.take();
        let stream_id = if let Some(ref stream) = active
            && stream.req_id == req_id
        {
            tracing::debug!(
                req_id = %req_id,
                stream_id = %stream.stream_id,
                "Reusing active stream for final reply"
            );
            stream.stream_id.clone()
        } else {
            if active.is_some() {
                tracing::debug!(
                    "Active stream req_id mismatch (expected a different message), creating new stream"
                );
            }
            uuid::Uuid::new_v4().to_string()
        };

        // 7. Send reply via WebSocket with finish=true
        self.send_text_reply(req_id, &full_reply, &stream_id, true)
            .await
            .context("Failed to send WeCom Bot reply")?;

        let message_id = uuid::Uuid::new_v4().to_string();

        tracing::info!(
            text_len = full_reply.len(),
            req_id = %req_id,
            stream_id = %stream_id,
            message_id = %message_id,
            "WeCom Bot reply sent"
        );

        // 8. Store reply to chat log
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

    /// Send a processing indicator (`finish=false`) so the user sees
    /// "正在思考中..." while AI is working.
    ///
    /// The returned `stream_id` is also stored internally so that a
    /// subsequent `send_reply` can reuse it and set `finish=true`.
    async fn send_processing_indicator(&self, original: &InboundMessage) -> Result<Option<String>> {
        let req_id = original
            .metadata
            .get("req_id")
            .and_then(|v| v.as_str())
            .unwrap_or("");

        if req_id.is_empty() {
            tracing::warn!("Cannot send processing indicator: original message missing req_id");
            return Ok(None);
        }

        let stream_id = uuid::Uuid::new_v4().to_string();
        let content = "正在思考中...";

        let json = serde_json::json!({
            "cmd": "aibot_respond_msg",
            "headers": {"req_id": req_id},
            "body": {
                "msgtype": "stream",
                "stream": {
                    "id": &stream_id,
                    "content": content,
                    "finish": false
                }
            }
        })
        .to_string();

        self.send_internal(&json)
            .await
            .context("Failed to send WeCom Bot processing indicator")?;

        // Store the active stream so send_reply can reuse the stream_id
        let mut guard = self.active_stream.lock().await;
        *guard = Some(ActiveStream {
            req_id: req_id.to_string(),
            stream_id: stream_id.clone(),
        });

        tracing::info!(
            req_id = %req_id,
            stream_id = %stream_id,
            "WeCom Bot processing indicator sent"
        );

        Ok(Some(stream_id))
    }

    /// Clear a previously sent processing indicator.
    ///
    /// Called when AI processing fails or produces no reply. Sends a
    /// `finish=true` message using the same stream_id so the indicator
    /// does not remain stuck in an intermediate state.
    async fn clear_processing_indicator(&self, _handle: Option<String>) -> Result<()> {
        let active = self.active_stream.lock().await.take();

        let Some(stream) = active else {
            tracing::debug!("No active stream to clear");
            return Ok(());
        };

        let content = "处理失败，请稍后重试";

        let json = serde_json::json!({
            "cmd": "aibot_respond_msg",
            "headers": {"req_id": &stream.req_id},
            "body": {
                "msgtype": "stream",
                "stream": {
                    "id": &stream.stream_id,
                    "content": content,
                    "finish": true
                }
            }
        })
        .to_string();

        self.send_internal(&json)
            .await
            .context("Failed to clear WeCom Bot processing indicator")?;

        tracing::info!(
            req_id = %stream.req_id,
            stream_id = %stream.stream_id,
            "WeCom Bot processing indicator cleared"
        );

        Ok(())
    }

    /// Update an existing processing indicator with new content.
    ///
    /// Sends `finish=false` with the same `stream_id` so the message
    /// is updated in-place rather than creating a new one.
    async fn update_processing_indicator(
        &self,
        original: &InboundMessage,
        handle: &str,
        content: &str,
    ) -> Result<()> {
        let req_id = original
            .metadata
            .get("req_id")
            .and_then(|v| v.as_str())
            .unwrap_or("");

        if req_id.is_empty() {
            tracing::warn!("Cannot update processing indicator: original message missing req_id");
            return Ok(());
        }

        self.send_text_reply(req_id, content, handle, false)
            .await
            .context("Failed to update WeCom Bot processing indicator")?;

        tracing::debug!(
            req_id = %req_id,
            stream_id = %handle,
            content = %content,
            "WeCom Bot processing indicator updated"
        );

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

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

    /// Documents the wire format for a processing indicator.
    #[test]
    fn test_processing_indicator_wire_format() {
        let json = serde_json::json!({
            "cmd": "aibot_respond_msg",
            "headers": {"req_id": "req_123"},
            "body": {
                "msgtype": "stream",
                "stream": {
                    "id": "stream_abc",
                    "content": "正在思考中...",
                    "finish": false
                }
            }
        });

        assert_eq!(json["cmd"], "aibot_respond_msg");
        assert_eq!(json["headers"]["req_id"], "req_123");
        assert_eq!(json["body"]["msgtype"], "stream");
        assert_eq!(json["body"]["stream"]["id"], "stream_abc");
        assert_eq!(json["body"]["stream"]["content"], "正在思考中...");
        assert_eq!(json["body"]["stream"]["finish"], false);
    }

    /// Documents the wire format for clearing a processing indicator.
    #[test]
    fn test_clear_indicator_wire_format() {
        let json = serde_json::json!({
            "cmd": "aibot_respond_msg",
            "headers": {"req_id": "req_123"},
            "body": {
                "msgtype": "stream",
                "stream": {
                    "id": "stream_abc",
                    "content": "处理失败，请稍后重试",
                    "finish": true
                }
            }
        });

        assert_eq!(json["body"]["stream"]["finish"], true);
        assert_eq!(json["body"]["stream"]["content"], "处理失败，请稍后重试");
    }

    /// When send_reply is called after send_processing_indicator, it must
    /// reuse the same stream_id and set finish=true.
    #[tokio::test]
    async fn test_send_reply_reuses_active_stream() {
        let (tx, mut rx) = mpsc::unbounded_channel::<String>();
        let storage = Arc::new(MessageStorage::new(std::path::Path::new("/tmp")));
        let adapter = WecomBotOutboundAdapter::new_with_attachments(storage, None, false);

        // Set the sender so messages can be captured
        adapter.set_sender(tx).await;

        // Build a message with req_id
        let message = InboundMessage {
            id: "test".to_string(),
            channel: "wecom_bot".to_string(),
            channel_uid: "msg_1".to_string(),
            sender: "user".to_string(),
            sender_address: "user".to_string(),
            recipients: vec![],
            topic: String::new(),
            content: jyc_types::MessageContent {
                text: Some("hello".to_string()),
                html: None,
                markdown: None,
            },
            timestamp: chrono::Utc::now(),
            thread_refs: None,
            reply_to_id: None,
            external_id: None,
            attachments: vec![],
            metadata: {
                let mut m = HashMap::new();
                m.insert(
                    "req_id".to_string(),
                    serde_json::Value::String("req_123".to_string()),
                );
                m
            },
            matched_pattern: None,
        };

        // Send processing indicator
        let handle = adapter
            .send_processing_indicator(&message)
            .await
            .expect("indicator should send");
        assert!(handle.is_some(), "should return stream_id");

        // Capture the indicator frame
        let indicator_json = rx.recv().await.expect("indicator frame should be sent");
        let indicator: serde_json::Value = serde_json::from_str(&indicator_json).unwrap();
        let stream_id = indicator["body"]["stream"]["id"]
            .as_str()
            .expect("stream id should be present")
            .to_string();
        assert_eq!(indicator["body"]["stream"]["finish"], false);
        assert_eq!(indicator["body"]["stream"]["content"], "正在思考中...");

        // Now send reply — it should reuse the same stream_id
        let thread_path = std::path::PathBuf::from("/tmp/test_thread");
        tokio::fs::create_dir_all(&thread_path).await.ok();
        let result = adapter
            .send_reply(&message, "AI reply", &thread_path, "msg_001", None)
            .await
            .expect("reply should send");
        assert!(!result.message_id.is_empty());

        // Capture the reply frame
        let reply_json = rx.recv().await.expect("reply frame should be sent");
        let reply: serde_json::Value = serde_json::from_str(&reply_json).unwrap();
        assert_eq!(reply["body"]["stream"]["finish"], true);
        assert_eq!(
            reply["body"]["stream"]["id"], stream_id,
            "reply must reuse the same stream_id"
        );
        assert_eq!(reply["body"]["stream"]["content"], "AI reply");

        // Active stream should be cleared after send_reply
        let guard = adapter.active_stream.lock().await;
        assert!(
            guard.is_none(),
            "active stream should be cleared after reply"
        );
    }

    /// When send_reply is called without a prior processing indicator,
    /// it should create a new stream with finish=true.
    #[tokio::test]
    async fn test_send_reply_without_indicator() {
        let (tx, mut rx) = mpsc::unbounded_channel::<String>();
        let storage = Arc::new(MessageStorage::new(std::path::Path::new("/tmp")));
        let adapter = WecomBotOutboundAdapter::new_with_attachments(storage, None, false);

        adapter.set_sender(tx).await;

        let message = InboundMessage {
            id: "test".to_string(),
            channel: "wecom_bot".to_string(),
            channel_uid: "msg_1".to_string(),
            sender: "user".to_string(),
            sender_address: "user".to_string(),
            recipients: vec![],
            topic: String::new(),
            content: jyc_types::MessageContent {
                text: Some("hello".to_string()),
                html: None,
                markdown: None,
            },
            timestamp: chrono::Utc::now(),
            thread_refs: None,
            reply_to_id: None,
            external_id: None,
            attachments: vec![],
            metadata: {
                let mut m = HashMap::new();
                m.insert(
                    "req_id".to_string(),
                    serde_json::Value::String("req_456".to_string()),
                );
                m
            },
            matched_pattern: None,
        };

        let thread_path = std::path::PathBuf::from("/tmp/test_thread2");
        tokio::fs::create_dir_all(&thread_path).await.ok();
        adapter
            .send_reply(&message, "Direct reply", &thread_path, "msg_002", None)
            .await
            .expect("reply should send");

        let reply_json = rx.recv().await.expect("reply frame should be sent");
        let reply: serde_json::Value = serde_json::from_str(&reply_json).unwrap();
        assert_eq!(reply["body"]["stream"]["finish"], true);
        assert!(
            !reply["body"]["stream"]["id"].as_str().unwrap().is_empty(),
            "should have a new stream_id"
        );
    }

    /// clear_processing_indicator should send finish=true with an error message
    /// and clear the active stream.
    #[tokio::test]
    async fn test_clear_processing_indicator() {
        let (tx, mut rx) = mpsc::unbounded_channel::<String>();
        let storage = Arc::new(MessageStorage::new(std::path::Path::new("/tmp")));
        let adapter = WecomBotOutboundAdapter::new_with_attachments(storage, None, false);

        adapter.set_sender(tx).await;

        let message = InboundMessage {
            id: "test".to_string(),
            channel: "wecom_bot".to_string(),
            channel_uid: "msg_1".to_string(),
            sender: "user".to_string(),
            sender_address: "user".to_string(),
            recipients: vec![],
            topic: String::new(),
            content: jyc_types::MessageContent {
                text: Some("hello".to_string()),
                html: None,
                markdown: None,
            },
            timestamp: chrono::Utc::now(),
            thread_refs: None,
            reply_to_id: None,
            external_id: None,
            attachments: vec![],
            metadata: {
                let mut m = HashMap::new();
                m.insert(
                    "req_id".to_string(),
                    serde_json::Value::String("req_789".to_string()),
                );
                m
            },
            matched_pattern: None,
        };

        // Send indicator first
        adapter
            .send_processing_indicator(&message)
            .await
            .expect("indicator should send");

        // Consume the indicator frame
        let indicator_json = rx.recv().await.expect("indicator frame");
        let indicator: serde_json::Value = serde_json::from_str(&indicator_json).unwrap();
        let stream_id = indicator["body"]["stream"]["id"]
            .as_str()
            .unwrap()
            .to_string();

        // Clear it
        adapter
            .clear_processing_indicator(None)
            .await
            .expect("clear should succeed");

        let clear_json = rx.recv().await.expect("clear frame");
        let clear: serde_json::Value = serde_json::from_str(&clear_json).unwrap();
        assert_eq!(clear["body"]["stream"]["finish"], true);
        assert_eq!(
            clear["body"]["stream"]["id"], stream_id,
            "clear must use the same stream_id"
        );
        assert_eq!(clear["body"]["stream"]["content"], "处理失败，请稍后重试");

        // Active stream should be cleared
        let guard = adapter.active_stream.lock().await;
        assert!(guard.is_none());
    }
}
