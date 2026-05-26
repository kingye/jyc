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
        original: &InboundMessage,
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

        // 6. Resolve the outbound `to` address from the original inbound
        // message. The OpenILink Bridge multiplexes many WeChat
        // conversations through one bot connection, so every send must
        // declare which conversation it's targeting. Without `to` the
        // server returns `{"error":"to is required","type":"error"}`
        // and the reply is dropped.
        //
        // For 1:1 messages: use sender_address (event.data.sender.id),
        //                   which is the WeChat user ID like
        //                   `o9cq8082...@im.wechat`.
        // For group messages (event.data.group non-null in the inbound
        // payload, surfaced as `is_group=true` metadata): prefer
        // event.data.group.id so the reply lands in the same group.
        let to = resolve_outbound_to(original).ok_or_else(|| {
            anyhow::anyhow!(
                "Cannot send WeChat reply: original message has no usable `to` \
                 address (no sender_address, no group.id metadata)"
            )
        })?;

        // 7. Send the reply through WebSocket
        // Format: {"type":"send","to":"<conversation>","content":"..."}
        let json_msg = serde_json::json!({
            "type": "send",
            "to": &to,
            "content": full_reply,
        })
        .to_string();

        let message_id = uuid::Uuid::new_v4().to_string();

        self.send_internal(&json_msg).await
            .context("Failed to send WeChat reply through WebSocket")?;

        tracing::info!(
            text_len = full_reply.len(),
            to = %to,
            message_id = %message_id,
            "WeChat reply sent"
        );

        // 8. Handle attachments (WeChat v1: text-only, log a warning)
        if let Some(atts) = attachments {
            if !atts.is_empty() {
                tracing::warn!(
                    count = atts.len(),
                    "WeChat v1 does not support attachments, skipping"
                );
            }
        }

        // 9. Store reply to chat log
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
        // The OpenILink Bridge requires `to` on every send. For alerts
        // (proactive, no inbound message to reply to) we use the
        // configured `recipient` directly. The recipient is whatever
        // string the operator put in the alerting config — it must be
        // a valid OpenILink address (e.g. a WeChat user/group ID like
        // `oXXX@im.wechat`).
        if recipient.is_empty() {
            anyhow::bail!(
                "Cannot send WeChat alert: recipient is empty. Configure a \
                 valid WeChat conversation ID in the alerting recipient field."
            );
        }

        // Format alert message
        let alert_text = format!("{}\n\n{}", subject, body);

        let message_id = uuid::Uuid::new_v4().to_string();

        let json_msg = serde_json::json!({
            "type": "send",
            "to": recipient,
            "content": alert_text,
        })
        .to_string();

        self.send_internal(&json_msg).await
            .context("Failed to send WeChat alert through WebSocket")?;

        tracing::info!("WeChat alert sent to {}: {}", recipient, subject);

        Ok(SendResult { message_id })
    }
}

/// Resolve the outbound `to` address from an inbound message.
///
/// Picks, in order:
/// 1. `metadata.group.id` if the message came from a group chat
///    (`metadata.is_group == true`). Group replies must address the
///    group, not the individual sender.
/// 2. `sender_address` (the WeChat user ID extracted from
///    `event.data.sender.id` in the inbound parser).
/// 3. `None` — the inbound message can't be replied to. Caller decides
///    whether that's an error or a no-op.
fn resolve_outbound_to(original: &InboundMessage) -> Option<String> {
    let is_group = original
        .metadata
        .get("is_group")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    if is_group {
        if let Some(group_id) = original
            .metadata
            .get("group")
            .and_then(|g| g.get("id"))
            .and_then(|v| v.as_str())
        {
            if !group_id.is_empty() {
                return Some(group_id.to_string());
            }
        }
        // Group flag set but no group.id available — fall through to
        // sender_address rather than failing outright.
    }

    if !original.sender_address.is_empty() {
        return Some(original.sender_address.clone());
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use jyc_types::MessageContent;

    fn make_inbound_with_sender(sender: &str) -> InboundMessage {
        InboundMessage {
            id: "id".to_string(),
            channel: "wechat_me".to_string(),
            channel_uid: "msg_1".to_string(),
            sender: sender.to_string(),
            sender_address: sender.to_string(),
            recipients: vec![],
            topic: String::new(),
            content: MessageContent {
                text: Some("hi".to_string()),
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

    /// 1:1 message: `to` resolves to the sender's WeChat ID. Mirrors the
    /// production case (May 26 incident: `o9cq8082...@im.wechat`).
    #[test]
    fn resolve_to_uses_sender_address_for_one_to_one() {
        let msg = make_inbound_with_sender("o9cq8082DBb8Fd8p8DTRmzBFN7AM@im.wechat");
        let to = resolve_outbound_to(&msg);
        assert_eq!(
            to.as_deref(),
            Some("o9cq8082DBb8Fd8p8DTRmzBFN7AM@im.wechat"),
        );
    }

    /// Group message: `to` resolves to the group ID, not the sender.
    /// `is_group=true` and `group.id` are both set in metadata by the
    /// inbound parser when `event.data.group` is non-null.
    #[test]
    fn resolve_to_uses_group_id_when_is_group_true() {
        let mut msg = make_inbound_with_sender("u1@im.wechat");
        msg.metadata.insert(
            "is_group".to_string(),
            serde_json::Value::Bool(true),
        );
        msg.metadata.insert(
            "group".to_string(),
            serde_json::json!({"id": "grp_abc", "name": "Group X"}),
        );

        let to = resolve_outbound_to(&msg);
        assert_eq!(to.as_deref(), Some("grp_abc"));
    }

    /// Defensive fallback: if `is_group=true` is set but `group.id` is
    /// missing or empty, fall back to sender_address rather than failing
    /// outright. The reply lands on the sender instead of the group; not
    /// ideal but better than dropping the reply.
    #[test]
    fn resolve_to_falls_back_to_sender_when_group_id_missing() {
        let mut msg = make_inbound_with_sender("u1@im.wechat");
        msg.metadata.insert(
            "is_group".to_string(),
            serde_json::Value::Bool(true),
        );
        // group object exists but has no `id` field.
        msg.metadata.insert(
            "group".to_string(),
            serde_json::json!({"name": "Group X"}),
        );

        let to = resolve_outbound_to(&msg);
        assert_eq!(to.as_deref(), Some("u1@im.wechat"));
    }

    /// No usable address at all — the message has no sender_address and
    /// no group metadata. Caller (send_reply) is expected to translate
    /// this into an explicit error.
    #[test]
    fn resolve_to_returns_none_when_no_address_available() {
        let msg = make_inbound_with_sender("");
        let to = resolve_outbound_to(&msg);
        assert!(to.is_none());
    }

    /// Documents the wire format the OpenILink Bridge accepts for a 1:1
    /// reply. Required fields: `type`, `to`, `content`. Without `to` the
    /// server returns `{"error":"to is required","type":"error"}` (the
    /// May 26 production failure that prompted this fix).
    #[test]
    fn outbound_send_frame_includes_to_field() {
        let json_msg = serde_json::json!({
            "type": "send",
            "to": "o9cq8082DBb8Fd8p8DTRmzBFN7AM@im.wechat",
            "content": "Hello, world!",
        });

        assert_eq!(json_msg["type"], "send");
        assert_eq!(json_msg["to"], "o9cq8082DBb8Fd8p8DTRmzBFN7AM@im.wechat");
        assert_eq!(json_msg["content"], "Hello, world!");
    }

    #[test]
    fn outbound_alert_frame_includes_to_field() {
        let json_msg = serde_json::json!({
            "type": "send",
            "to": "kingye@im.wechat",
            "content": "Alert: System down\n\nPlease check the server.",
        });

        assert_eq!(json_msg["type"], "send");
        assert_eq!(json_msg["to"], "kingye@im.wechat");
        assert!(json_msg["content"].as_str().unwrap().contains("Alert"));
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
