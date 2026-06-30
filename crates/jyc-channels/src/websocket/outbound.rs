//! WebSocket channel outbound adapter.
//!
//! Broadcasts AI replies to all connected dashboard clients via
//! `tokio::sync::broadcast`.

use std::path::Path;
use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use tokio::sync::broadcast;

use jyc_core::message_storage::MessageStorage;
use jyc_types::{InboundMessage, OutboundAdapter, OutboundAttachment, SendResult};

/// WebSocket outbound adapter.
///
/// Holds a `tokio::sync::broadcast::Sender` that is shared with the inbound
/// adapter. All connected dashboard clients receive broadcast replies.
pub struct WebsocketOutboundAdapter {
    /// Broadcast sender — cloned for each new WebSocket connection.
    broadcast_tx: broadcast::Sender<String>,
    /// Message storage for persisting replies to chat log.
    storage: Arc<MessageStorage>,
}

impl WebsocketOutboundAdapter {
    /// Create a new WebSocket outbound adapter with the given broadcast sender.
    pub fn new(broadcast_tx: broadcast::Sender<String>, storage: Arc<MessageStorage>) -> Self {
        Self {
            broadcast_tx,
            storage,
        }
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
        original: &InboundMessage,
        reply_text: &str,
        thread_path: &Path,
        _message_dir: &str,
        _attachments: Option<&[OutboundAttachment]>,
    ) -> Result<SendResult> {
        // Use the original message topic as the broadcast key for normal
        // messages (topic = thread name from the WebSocket protocol).
        // Fall back to the thread directory name for scheduled jobs, where
        // `topic` is a descriptive string like "Scheduled job: ...".
        let thread = if original.topic.starts_with("Scheduled job:") {
            thread_path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or(original.topic.as_str())
        } else {
            original.topic.as_str()
        };
        self.broadcast_reply(thread, reply_text).await?;
        let message_id = uuid::Uuid::new_v4().to_string();

        // Persist reply to chat log for history loading
        if let Err(e) = self
            .storage
            .store_reply(thread_path, reply_text, _message_dir)
            .await
        {
            tracing::warn!(error = %e, "Failed to store WebSocket reply to chat log");
        }

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
        let tmp = tempfile::TempDir::new().unwrap();
        let thread_path = tmp.path().join("general");
        tokio::fs::create_dir_all(&thread_path).await.unwrap();
        let storage = Arc::new(MessageStorage::new(&thread_path));
        let adapter = WebsocketOutboundAdapter::new(tx, storage);

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
            .send_reply(&message, "AI reply", &thread_path, "msg_001", None)
            .await;
        assert!(result.is_ok());

        let sent = rx.recv().await.expect("should receive broadcast");
        let parsed: serde_json::Value = serde_json::from_str(&sent).unwrap();
        assert_eq!(parsed["type"], "reply");
        assert_eq!(parsed["thread"], "general");
        assert_eq!(parsed["text"], "AI reply");
    }

    /// Regression test: scheduled jobs set `topic` to a descriptive string
    /// ("Scheduled job: ...") rather than the thread name. The broadcast
    /// must use the thread directory name so WebSocket clients subscribed
    /// to the thread actually receive the reply.
    #[tokio::test]
    async fn test_send_reply_uses_thread_path_not_topic_for_job() {
        let (tx, mut rx) = broadcast::channel(16);
        let tmp = tempfile::TempDir::new().unwrap();
        let thread_path = tmp.path().join("dev");
        tokio::fs::create_dir_all(&thread_path).await.unwrap();
        let storage = Arc::new(MessageStorage::new(&thread_path));
        let adapter = WebsocketOutboundAdapter::new(tx, storage);

        // Simulate a scheduled job message: topic is descriptive, not the thread name
        let message = InboundMessage {
            id: "job-123".to_string(),
            channel: "websocket".to_string(),
            channel_uid: "job-123".to_string(),
            sender: "scheduler".to_string(),
            sender_address: "scheduler@jyc".to_string(),
            recipients: vec![],
            topic: "Scheduled job: check CI status".to_string(),
            content: jyc_types::MessageContent {
                text: Some("check CI".to_string()),
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
            .send_reply(&message, "CI passed!", &thread_path, "msg_001", None)
            .await;
        assert!(result.is_ok());

        let sent = rx.recv().await.expect("should receive broadcast");
        let parsed: serde_json::Value = serde_json::from_str(&sent).unwrap();
        assert_eq!(parsed["type"], "reply");
        // Thread must be "dev" (from thread_path), NOT "Scheduled job: ..."
        assert_eq!(parsed["thread"], "dev");
        assert_eq!(parsed["text"], "CI passed!");
    }

    #[tokio::test]
    async fn test_send_without_receiver_ok() {
        // broadcast with no receivers should still succeed
        let tx = broadcast::channel(16).0;
        let tmp = tempfile::TempDir::new().unwrap();
        let thread_path = tmp.path().join("general");
        tokio::fs::create_dir_all(&thread_path).await.unwrap();
        let storage = Arc::new(MessageStorage::new(&thread_path));
        let adapter = WebsocketOutboundAdapter::new(tx, storage);

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
            .send_reply(&message, "AI reply", &thread_path, "msg_001", None)
            .await;
        assert!(result.is_ok());
    }

    #[test]
    fn test_clean_body_passthrough() {
        let tmp = tempfile::TempDir::new().unwrap();
        let thread_path = tmp.path().join("general");
        std::fs::create_dir_all(&thread_path).unwrap();
        let storage = Arc::new(MessageStorage::new(&thread_path));
        let adapter = WebsocketOutboundAdapter::new(broadcast::channel(16).0, storage);
        assert_eq!(adapter.clean_body("hello\nworld"), "hello\nworld");
    }
}
