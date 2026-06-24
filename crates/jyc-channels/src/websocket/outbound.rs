//! WebSocket channel outbound adapter.
//!
//! Broadcasts AI replies to all connected dashboard clients via
//! `tokio::sync::broadcast`.

use std::path::Path;

use anyhow::Result;
use async_trait::async_trait;
use tokio::sync::broadcast;

use jyc_types::{InboundMessage, OutboundAdapter, OutboundAttachment, SendResult};

/// WebSocket outbound adapter.
///
/// Holds a `tokio::sync::broadcast::Sender` that is shared with the inbound
/// adapter. All connected dashboard clients receive broadcast replies.
pub struct WebsocketOutboundAdapter {
    /// Broadcast sender — cloned for each new WebSocket connection.
    broadcast_tx: broadcast::Sender<String>,
}

impl WebsocketOutboundAdapter {
    /// Create a new WebSocket outbound adapter with the given broadcast sender.
    pub fn new(broadcast_tx: broadcast::Sender<String>) -> Self {
        Self { broadcast_tx }
    }

    /// Get the broadcast sender for the inbound adapter to clone.
    pub fn broadcast_tx(&self) -> broadcast::Sender<String> {
        self.broadcast_tx.clone()
    }

    /// Broadcast a reply to all connected clients.
    async fn broadcast_reply(&self, thread: &str, text: &str) -> Result<()> {
        let payload = serde_json::json!({
            "type": "reply",
            "thread": thread,
            "text": text,
        })
        .to_string();
        // broadcast::Sender::send is non-blocking; it returns an error only when
        // there are no active receivers. We ignore that error since it's fine
        // to have no connected clients.
        let _ = self.broadcast_tx.send(payload);
        Ok(())
    }
}

#[async_trait]
impl OutboundAdapter for WebsocketOutboundAdapter {
    fn channel_type(&self) -> &str {
        "websocket"
    }

    async fn connect(&self) -> Result<()> {
        tracing::info!("WebSocket outbound adapter connected");
        Ok(())
    }

    async fn disconnect(&self) -> Result<()> {
        tracing::info!("WebSocket outbound adapter disconnected");
        Ok(())
    }

    fn clean_body(&self, raw_body: &str) -> String {
        raw_body.to_string()
    }

    async fn send_reply(
        &self,
        _original: &InboundMessage,
        reply_text: &str,
        _thread_path: &Path,
        _message_dir: &str,
        _attachments: Option<&[OutboundAttachment]>,
    ) -> Result<SendResult> {
        let thread = _original.topic.as_str();
        self.broadcast_reply(thread, reply_text).await?;
        let message_id = uuid::Uuid::new_v4().to_string();
        tracing::info!(text_len = reply_text.len(), message_id = %message_id, "WebSocket reply broadcast");
        Ok(SendResult { message_id })
    }

    async fn send_message(
        &self,
        _recipient: &str,
        _subject: &str,
        body: &str,
    ) -> Result<SendResult> {
        self.broadcast_reply(_recipient, body).await?;
        let message_id = uuid::Uuid::new_v4().to_string();
        tracing::info!(text_len = body.len(), message_id = %message_id, "WebSocket message broadcast");
        Ok(SendResult { message_id })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_send_reply_broadcasts() {
        let (tx, mut rx) = broadcast::channel(16);
        let adapter = WebsocketOutboundAdapter::new(tx);

        let message = InboundMessage {
            id: "test".to_string(),
            channel: "websocket".to_string(),
            channel_uid: "user".to_string(),
            sender: "user".to_string(),
            sender_address: "user".to_string(),
            recipients: vec![],
            topic: "general".to_string(),
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
            metadata: std::collections::HashMap::new(),
            matched_pattern: None,
        };

        let result = adapter
            .send_reply(&message, "AI reply", Path::new("/tmp"), "msg_001", None)
            .await;
        assert!(result.is_ok());

        let sent = rx.recv().await.expect("should receive broadcast");
        let parsed: serde_json::Value = serde_json::from_str(&sent).unwrap();
        assert_eq!(parsed["type"], "reply");
        assert_eq!(parsed["thread"], "general");
        assert_eq!(parsed["text"], "AI reply");
    }

    #[tokio::test]
    async fn test_send_without_receiver_ok() {
        // broadcast with no receivers should still succeed
        let tx = broadcast::channel(16).0;
        let adapter = WebsocketOutboundAdapter::new(tx);

        let message = InboundMessage {
            id: "test".to_string(),
            channel: "websocket".to_string(),
            channel_uid: "user".to_string(),
            sender: "user".to_string(),
            sender_address: "user".to_string(),
            recipients: vec![],
            topic: "general".to_string(),
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
            metadata: std::collections::HashMap::new(),
            matched_pattern: None,
        };

        let result = adapter
            .send_reply(&message, "AI reply", Path::new("/tmp"), "msg_001", None)
            .await;
        assert!(result.is_ok());
    }

    #[test]
    fn test_clean_body_passthrough() {
        let adapter = WebsocketOutboundAdapter::new(broadcast::channel(16).0);
        assert_eq!(adapter.clean_body("hello\nworld"), "hello\nworld");
    }
}
