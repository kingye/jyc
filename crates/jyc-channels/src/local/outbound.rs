//! Local TUI channel outbound adapter.
//!
//! Sends AI replies from the async system back to the TUI for display.

use std::path::Path;
use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use tokio::sync::Mutex;

use jyc_types::{InboundMessage, OutboundAdapter, OutboundAttachment, SendResult};

/// Local TUI outbound adapter.
///
/// Holds an optional mpsc sender that is injected after construction
/// (same pattern as WeCom Bot's `handle_arc`). Replies are sent to
/// the TUI via this channel.
pub struct LocalOutboundAdapter {
    /// Shared output sender — injected after construction.
    output_tx: Arc<Mutex<Option<tokio::sync::mpsc::UnboundedSender<String>>>>,
}

impl LocalOutboundAdapter {
    /// Create a new local outbound adapter.
    pub fn new() -> Self {
        Self {
            output_tx: Arc::new(Mutex::new(None)),
        }
    }

    /// Get the shared output sender Arc so the inbound adapter can set it.
    pub fn output_tx_arc(&self) -> Arc<Mutex<Option<tokio::sync::mpsc::UnboundedSender<String>>>> {
        self.output_tx.clone()
    }

    /// Set the output sender.
    pub async fn set_output_tx(&self, tx: tokio::sync::mpsc::UnboundedSender<String>) {
        let mut guard = self.output_tx.lock().await;
        *guard = Some(tx);
    }

    /// Send text through the output channel.
    async fn send_text(&self, text: &str) -> Result<()> {
        let guard = self.output_tx.lock().await;
        match guard.as_ref() {
            Some(tx) => tx
                .send(text.to_string())
                .map_err(|e| anyhow::anyhow!("Local outbound send failed: {e}")),
            None => Err(anyhow::anyhow!(
                "Local outbound output_tx not set (TUI not initialized)"
            )),
        }
    }
}

impl Default for LocalOutboundAdapter {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl OutboundAdapter for LocalOutboundAdapter {
    fn channel_type(&self) -> &str {
        "local"
    }

    async fn connect(&self) -> Result<()> {
        tracing::info!("Local outbound adapter connected");
        Ok(())
    }

    async fn disconnect(&self) -> Result<()> {
        tracing::info!("Local outbound adapter disconnected");
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
        self.send_text(reply_text).await?;
        let message_id = uuid::Uuid::new_v4().to_string();
        tracing::info!(text_len = reply_text.len(), message_id = %message_id, "Local reply sent");
        Ok(SendResult { message_id })
    }

    async fn send_message(
        &self,
        _recipient: &str,
        _subject: &str,
        body: &str,
    ) -> Result<SendResult> {
        self.send_text(body).await?;
        let message_id = uuid::Uuid::new_v4().to_string();
        tracing::info!(text_len = body.len(), message_id = %message_id, "Local message sent");
        Ok(SendResult { message_id })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_send_reply() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        let adapter = LocalOutboundAdapter::new();
        adapter.set_output_tx(tx).await;

        let message = InboundMessage {
            id: "test".to_string(),
            channel: "local".to_string(),
            channel_uid: "user".to_string(),
            sender: "user".to_string(),
            sender_address: "user".to_string(),
            recipients: vec![],
            topic: "Test".to_string(),
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

        let sent = rx.recv().await.expect("should receive reply");
        assert_eq!(sent, "AI reply");
    }

    #[tokio::test]
    async fn test_send_message() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        let adapter = LocalOutboundAdapter::new();
        adapter.set_output_tx(tx).await;

        let result = adapter.send_message("user", "Subject", "Hello").await;
        assert!(result.is_ok());

        let sent = rx.recv().await.expect("should receive message");
        assert_eq!(sent, "Hello");
    }

    #[test]
    fn test_clean_body_passthrough() {
        let adapter = LocalOutboundAdapter::new();
        assert_eq!(adapter.clean_body("hello\nworld"), "hello\nworld");
    }

    #[tokio::test]
    async fn test_send_without_tx_fails() {
        let adapter = LocalOutboundAdapter::new();
        let message = InboundMessage {
            id: "test".to_string(),
            channel: "local".to_string(),
            channel_uid: "user".to_string(),
            sender: "user".to_string(),
            sender_address: "user".to_string(),
            recipients: vec![],
            topic: "Test".to_string(),
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
        assert!(result.is_err());
        let msg = format!("{:#}", result.unwrap_err());
        assert!(msg.contains("not set"), "error: {msg}");
    }
}
