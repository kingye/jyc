//! Background delivery watcher for MCP tools that need to send messages
//! during an active SSE stream (e.g., the question tool).
//!
//! Channel-agnostic: uses the OutboundAdapter trait for delivery.
//! Watches for `reply-sent.flag` + `reply.md` files and delivers immediately.

use std::path::Path;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

use crate::channels::types::{InboundMessage, OutboundAdapter};

const POLL_INTERVAL: Duration = Duration::from_secs(2);

/// Watch for pending message deliveries during SSE processing.
///
/// MCP tools (like the question tool) write `reply.md` + `reply-sent.flag`
/// during the SSE stream. This watcher detects them and delivers immediately
/// via the outbound adapter, without waiting for the SSE stream to complete.
///
/// The watcher runs until cancelled (when the agent finishes processing).
pub async fn watch_pending_deliveries(
    thread_path: &Path,
    message_dir: &str,
    message: &InboundMessage,
    outbound: &dyn OutboundAdapter,
    cancel: CancellationToken,
) {
    let jyc_dir = thread_path.join(".jyc");
    let signal_path = jyc_dir.join("reply-sent.flag");
    let reply_path = jyc_dir.join("reply.md");

    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,
            _ = tokio::time::sleep(POLL_INTERVAL) => {}
        }

        // Check if a pending delivery exists
        if !signal_path.exists() || !reply_path.exists() {
            continue;
        }

        // Read the reply text
        let reply_text = match tokio::fs::read_to_string(&reply_path).await {
            Ok(text) if !text.trim().is_empty() => text,
            _ => continue,
        };

        tracing::info!(
            text_len = reply_text.len(),
            "Delivering pending message from MCP tool (background watcher)"
        );

        // Deliver via outbound adapter (channel-agnostic)
        if let Err(e) = outbound
            .send_reply(
                message,
                &reply_text,
                thread_path,
                message_dir,
                None,
            )
            .await
        {
            tracing::error!(error = %e, "Failed to deliver pending message");
        } else {
            tracing::info!("Pending message delivered successfully");
        }

        // Clean up signal file (reply.md stays for chat log)
        tokio::fs::remove_file(&signal_path).await.ok();
        // Remove reply.md to prevent re-delivery
        tokio::fs::remove_file(&reply_path).await.ok();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::channels::types::{InboundMessage, MessageContent, OutboundAdapter, OutboundAttachment, SendResult};
    use async_trait::async_trait;
    use std::sync::{Arc, Mutex};
    use tempfile::tempdir;

    /// Mock outbound adapter that records delivered messages.
    struct MockOutbound {
        delivered: Arc<Mutex<Vec<String>>>,
    }

    impl MockOutbound {
        fn new() -> (Self, Arc<Mutex<Vec<String>>>) {
            let delivered = Arc::new(Mutex::new(Vec::new()));
            (Self { delivered: delivered.clone() }, delivered)
        }
    }

    #[async_trait]
    impl OutboundAdapter for MockOutbound {
        fn channel_type(&self) -> &str { "mock" }

        async fn connect(&self) -> anyhow::Result<()> { Ok(()) }

        async fn disconnect(&self) -> anyhow::Result<()> { Ok(()) }

        fn clean_body(&self, body: &str) -> String { body.to_string() }

        async fn send_reply(
            &self,
            _original: &InboundMessage,
            reply_text: &str,
            _thread_path: &Path,
            _message_dir: &str,
            _attachments: Option<&[OutboundAttachment]>,
        ) -> anyhow::Result<SendResult> {
            self.delivered.lock().unwrap().push(reply_text.to_string());
            Ok(SendResult { message_id: "mock-id".to_string() })
        }

        async fn send_alert(
            &self,
            _recipient: &str,
            _subject: &str,
            _body: &str,
        ) -> anyhow::Result<SendResult> {
            Ok(SendResult { message_id: "mock-id".to_string() })
        }

        async fn send_heartbeat(
            &self,
            _original: &InboundMessage,
            _text: &str,
        ) -> anyhow::Result<SendResult> {
            Ok(SendResult { message_id: "mock-id".to_string() })
        }
    }

    fn test_message() -> InboundMessage {
        InboundMessage {
            id: "test".to_string(),
            channel: "test".to_string(),
            channel_uid: "1".to_string(),
            sender: "user".to_string(),
            sender_address: "user@test".to_string(),
            recipients: vec![],
            topic: "Test".to_string(),
            content: MessageContent::default(),
            timestamp: chrono::Utc::now(),
            thread_refs: None,
            reply_to_id: None,
            external_id: None,
            attachments: vec![],
            metadata: std::collections::HashMap::new(),
            matched_pattern: None,
        }
    }

    #[tokio::test]
    async fn test_delivers_when_signal_and_reply_exist() {
        let tmp = tempdir().unwrap();
        let thread_path = tmp.path().to_path_buf();
        let message_dir = "2026-01-01_00-00-00";

        // Create directories
        let jyc_dir = thread_path.join(".jyc");
        tokio::fs::create_dir_all(&jyc_dir).await.unwrap();

        // Write signal and reply files to .jyc/
        tokio::fs::write(jyc_dir.join("reply-sent.flag"), "{}").await.unwrap();
        tokio::fs::write(jyc_dir.join("reply.md"), "❓ What color?").await.unwrap();

        let (outbound, delivered) = MockOutbound::new();
        let cancel = CancellationToken::new();
        let cancel_clone = cancel.clone();

        let tp = thread_path.clone();
        let handle = tokio::spawn(async move {
            watch_pending_deliveries(
                &tp,
                message_dir,
                &test_message(),
                &outbound,
                cancel_clone,
            ).await;
        });

        // Wait for watcher to pick up the files
        tokio::time::sleep(Duration::from_secs(3)).await;
        cancel.cancel();
        let _ = handle.await;

        // Verify delivery
        let msgs = delivered.lock().unwrap();
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0], "❓ What color?");

        // Verify cleanup
        assert!(!jyc_dir.join("reply-sent.flag").exists());
        assert!(!jyc_dir.join("reply.md").exists());
    }

    #[tokio::test]
    async fn test_no_delivery_without_signal() {
        let tmp = tempdir().unwrap();
        let thread_path = tmp.path().to_path_buf();
        let message_dir = "2026-01-01_00-00-00";

        let jyc_dir = thread_path.join(".jyc");
        tokio::fs::create_dir_all(&jyc_dir).await.unwrap();
        // reply.md exists but no signal file
        tokio::fs::write(jyc_dir.join("reply.md"), "test").await.unwrap();

        let (outbound, delivered) = MockOutbound::new();
        let cancel = CancellationToken::new();
        let cancel_clone = cancel.clone();

        let tp = thread_path.clone();
        let handle = tokio::spawn(async move {
            watch_pending_deliveries(
                &tp,
                message_dir,
                &test_message(),
                &outbound,
                cancel_clone,
            ).await;
        });

        tokio::time::sleep(Duration::from_secs(3)).await;
        cancel.cancel();
        let _ = handle.await;

        assert!(delivered.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn test_no_delivery_with_empty_reply() {
        let tmp = tempdir().unwrap();
        let thread_path = tmp.path().to_path_buf();
        let message_dir = "2026-01-01_00-00-00";

        let jyc_dir = thread_path.join(".jyc");
        tokio::fs::create_dir_all(&jyc_dir).await.unwrap();

        tokio::fs::write(jyc_dir.join("reply-sent.flag"), "{}").await.unwrap();
        tokio::fs::write(jyc_dir.join("reply.md"), "   ").await.unwrap();

        let (outbound, delivered) = MockOutbound::new();
        let cancel = CancellationToken::new();
        let cancel_clone = cancel.clone();

        let tp = thread_path.clone();
        let handle = tokio::spawn(async move {
            watch_pending_deliveries(
                &tp,
                message_dir,
                &test_message(),
                &outbound,
                cancel_clone,
            ).await;
        });

        tokio::time::sleep(Duration::from_secs(3)).await;
        cancel.cancel();
        let _ = handle.await;

        assert!(delivered.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn test_cancellation_stops_watcher() {
        let tmp = tempdir().unwrap();
        let thread_path = tmp.path().to_path_buf();
        let message_dir = "2026-01-01_00-00-00";

        tokio::fs::create_dir_all(thread_path.join(".jyc")).await.unwrap();

        let (outbound, _delivered) = MockOutbound::new();
        let cancel = CancellationToken::new();
        let cancel_clone = cancel.clone();

        let handle = tokio::spawn(async move {
            watch_pending_deliveries(
                &thread_path,
                message_dir,
                &test_message(),
                &outbound,
                cancel_clone,
            ).await;
        });

        cancel.cancel();
        tokio::time::timeout(Duration::from_secs(5), handle)
            .await
            .expect("Watcher should stop within 5 seconds")
            .unwrap();
    }
}
