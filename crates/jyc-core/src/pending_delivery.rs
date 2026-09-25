//! Background delivery watcher for tools that need to send messages
//! during an active SSE stream (e.g., synchronously-delivered replies).
//!
//! Channel-agnostic: uses the OutboundAdapter trait for delivery.
//! Watches for `reply-sent.flag` + `reply.md` files and delivers immediately.

use chrono::Utc;
use jyc_types::state_dir::jyc_dir;
use std::path::Path;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

use jyc_types::config::HookEvent;
use jyc_utils::hooks::{HookCtx, HookOutcome, HookSet};

use crate::topic_event::TopicEvent;
use crate::topic_event_bus::TopicEventBusRef;
use jyc_types::{InboundMessage, OutboundAdapter};

const POLL_INTERVAL: Duration = Duration::from_secs(2);

/// Evaluate `reply_send` hooks for an AI reply about to be delivered.
/// Returns the hook's stderr when a hook exited 2 (send must be
/// suppressed); `None` includes the fast empty-set path.
pub(crate) async fn reply_blocked_by_hook(
    hooks: &HookSet,
    topic_name: &str,
    topic_path: &Path,
    message: &InboundMessage,
    reply_text: &str,
) -> Option<String> {
    if hooks.is_empty() {
        return None;
    }
    let ctx = HookCtx {
        topic: topic_name.to_string(),
        cwd: topic_path.display().to_string(),
        channel: Some(message.channel.clone()),
        message_content: message.content.text.clone(),
        sender: Some(message.sender.clone()),
        sender_address: Some(message.sender_address.clone()),
        reply_text: Some(reply_text.to_string()),
        metadata: json_metadata(&message.metadata),
        ..Default::default()
    };
    match hooks
        .run(HookEvent::ReplySend, Some(topic_name), &ctx)
        .await
    {
        HookOutcome::Block(reason) => Some(reason),
        HookOutcome::Proceed => None,
    }
}

/// Inbound message metadata as an optional JSON object for hook payloads.
pub(crate) fn json_metadata(
    md: &std::collections::HashMap<String, serde_json::Value>,
) -> Option<serde_json::Value> {
    (!md.is_empty())
        .then(|| serde_json::to_value(md).ok())
        .flatten()
}

/// Read attachment filenames from the reply-sent.flag signal file.
/// Returns OutboundAttachment list, or None if no attachments.
pub(crate) async fn read_signal_attachments(
    signal_path: &Path,
    topic_path: &Path,
) -> Option<Vec<jyc_types::OutboundAttachment>> {
    let content = tokio::fs::read_to_string(signal_path).await.ok()?;
    let signal: serde_json::Value = serde_json::from_str(&content).ok()?;

    let filenames = signal.get("attachments")?.as_array()?;
    if filenames.is_empty() {
        return None;
    }

    let attachments: Vec<jyc_types::OutboundAttachment> = filenames
        .iter()
        .filter_map(|v| v.as_str())
        .map(|filename| {
            let path = topic_path.join(filename);
            let ext = path
                .extension()
                .map(|e| e.to_string_lossy().to_lowercase())
                .unwrap_or_default();
            let content_type = match ext.as_str() {
                "pdf" => "application/pdf",
                "pptx" => {
                    "application/vnd.openxmlformats-officedocument.presentationml.presentation"
                }
                "docx" => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
                "xlsx" => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
                "png" => "image/png",
                "jpg" | "jpeg" => "image/jpeg",
                "txt" | "md" => "text/plain",
                _ => "application/octet-stream",
            };
            jyc_types::OutboundAttachment {
                filename: filename.to_string(),
                path,
                content_type: content_type.to_string(),
            }
        })
        .collect();

    Some(attachments)
}

/// Watch for pending message deliveries during SSE processing.
///
/// Tools delivering mid-stream write `reply.md` + `reply-sent.flag` during
/// the SSE stream. This watcher detects them and delivers immediately via
/// the outbound adapter, without waiting for the SSE stream to complete.
///
/// When delivery succeeds, the watcher also publishes a `ReplySent` event on
/// the provided topic event bus so dashboard clients can display the reply
/// live. If the bus is omitted, the reply is still delivered but not fanned
/// out to the dashboard.
///
/// The watcher runs until cancelled (when the agent finishes processing).
// The `hooks` parameter pushed this over clippy's 7-arg heuristic;
// grouping the delivery targets into a struct would obscure more than
// it helps (same call as `#[allow(clippy::too_many_arguments)]` in
// `jyc-services::smtp` and `jyc-agent::session`).
#[allow(clippy::too_many_arguments)]
pub async fn watch_pending_deliveries(
    topic_path: &Path,
    message_dir: &str,
    message: &InboundMessage,
    outbound: &dyn OutboundAdapter,
    hooks: std::sync::Arc<HookSet>,
    cancel: CancellationToken,
    event_bus: Option<TopicEventBusRef>,
    topic_name: &str,
) {
    let jyc_dir = jyc_dir(topic_name, topic_path);
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

        // reply_send hook: exit-2 suppresses this delivery. The signal
        // files are cleaned up identically to a delivered reply, so a
        // suppressed message never re-fires on the next poll.
        if let Some(reason) =
            reply_blocked_by_hook(&hooks, topic_name, topic_path, message, &reply_text).await
        {
            tracing::warn!(
                topic = %topic_name,
                reason = %reason,
                "reply_send hook suppressed pending delivery"
            );
            tokio::fs::remove_file(&signal_path).await.ok();
            tokio::fs::remove_file(&reply_path).await.ok();
            continue;
        }

        // Deliver via outbound adapter (channel-agnostic), carrying any
        // attachments the reply tool recorded in the signal file.
        let attachments = read_signal_attachments(&signal_path, topic_path).await;
        if let Err(e) = outbound
            .send_reply(
                message,
                &reply_text,
                topic_path,
                message_dir,
                attachments.as_deref(),
            )
            .await
        {
            tracing::error!(error = %e, "Failed to deliver pending message");
        } else {
            tracing::info!("Pending message delivered successfully");
            // Fan out a ReplySent event so the dashboard can display the
            // reply live. The main post-SSE delivery path does this too;
            // when the watcher wins the race we must still emit it.
            if let Some(bus) = &event_bus {
                let event = TopicEvent::ReplySent {
                    topic_name: topic_name.to_string(),
                    text: reply_text.clone(),
                    timestamp: Utc::now(),
                };
                if let Err(e) = bus.publish(event).await {
                    tracing::warn!(
                        error = %e,
                        topic = %topic_name,
                        "Failed to publish ReplySent event from pending delivery watcher"
                    );
                }
            }
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
    use crate::topic_event::TopicEvent;
    use crate::topic_event_bus::{SimpleThreadEventBus, TopicEventBusRef};
    use async_trait::async_trait;
    use jyc_types::{
        InboundMessage, MessageContent, OutboundAdapter, OutboundAttachment, SendResult,
    };
    use std::sync::{Arc, Mutex};
    use tempfile::tempdir;

    /// Per-delivery attachment filename lists, as recorded by `MockOutbound`.
    type AttachmentLog = Arc<Mutex<Vec<Vec<String>>>>;

    /// Mock outbound adapter that records delivered messages.
    struct MockOutbound {
        delivered: Arc<Mutex<Vec<String>>>,
        delivered_attachments: AttachmentLog,
    }

    impl MockOutbound {
        fn new() -> (Self, Arc<Mutex<Vec<String>>>) {
            let (mock, delivered, _) = Self::new_with_attachments();
            (mock, delivered)
        }

        fn new_with_attachments() -> (Self, Arc<Mutex<Vec<String>>>, AttachmentLog) {
            let delivered = Arc::new(Mutex::new(Vec::new()));
            let delivered_attachments = Arc::new(Mutex::new(Vec::new()));
            (
                Self {
                    delivered: delivered.clone(),
                    delivered_attachments: delivered_attachments.clone(),
                },
                delivered,
                delivered_attachments,
            )
        }
    }

    #[async_trait]
    impl OutboundAdapter for MockOutbound {
        fn channel_type(&self) -> &str {
            "mock"
        }

        async fn connect(&self) -> anyhow::Result<()> {
            Ok(())
        }

        async fn disconnect(&self) -> anyhow::Result<()> {
            Ok(())
        }

        fn clean_body(&self, body: &str) -> String {
            body.to_string()
        }

        async fn send_reply(
            &self,
            _original: &InboundMessage,
            reply_text: &str,
            _topic_path: &Path,
            _message_dir: &str,
            attachments: Option<&[OutboundAttachment]>,
        ) -> anyhow::Result<SendResult> {
            self.delivered.lock().unwrap().push(reply_text.to_string());
            self.delivered_attachments.lock().unwrap().push(
                attachments
                    .unwrap_or(&[])
                    .iter()
                    .map(|a| a.filename.clone())
                    .collect(),
            );
            Ok(SendResult {
                message_id: "mock-id".to_string(),
            })
        }

        async fn send_message(
            &self,
            _recipient: &str,
            _subject: &str,
            _body: &str,
        ) -> anyhow::Result<SendResult> {
            Ok(SendResult {
                message_id: "mock-id".to_string(),
            })
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
            references: None,
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
        let topic_path = tmp.path().to_path_buf();
        let message_dir = "2026-01-01_00-00-00";

        // Create directories
        let jyc_dir = topic_path.join(".jyc");
        tokio::fs::create_dir_all(&jyc_dir).await.unwrap();

        // Write signal and reply files to .jyc/
        tokio::fs::write(jyc_dir.join("reply-sent.flag"), "{}")
            .await
            .unwrap();
        tokio::fs::write(jyc_dir.join("reply.md"), "❓ What color?")
            .await
            .unwrap();

        let (outbound, delivered) = MockOutbound::new();
        let cancel = CancellationToken::new();
        let cancel_clone = cancel.clone();

        let topic = "%s";
        jyc_types::state_dir::register(topic, &topic_path.join(".jyc"));
        let tp = topic_path.clone();
        let handle = tokio::spawn(async move {
            watch_pending_deliveries(
                &tp,
                message_dir,
                &test_message(),
                &outbound,
                std::sync::Arc::new(HookSet::default()),
                cancel_clone,
                None,
                topic,
            )
            .await;
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

    /// Attachments recorded in the signal file must reach the outbound
    /// adapter — previously the watcher hardcoded `None` and dropped them.
    #[tokio::test]
    async fn test_delivers_attachments_from_signal() {
        let tmp = tempdir().unwrap();
        let topic_path = tmp.path().to_path_buf();
        let message_dir = "2026-01-01_00-00-00";

        let jyc_dir = topic_path.join(".jyc");
        tokio::fs::create_dir_all(&jyc_dir).await.unwrap();
        tokio::fs::write(topic_path.join("report.pdf"), b"%PDF")
            .await
            .unwrap();
        tokio::fs::write(
            jyc_dir.join("reply-sent.flag"),
            r#"{"attachments": ["report.pdf"]}"#,
        )
        .await
        .unwrap();
        tokio::fs::write(jyc_dir.join("reply.md"), "here you go")
            .await
            .unwrap();

        let (outbound, delivered, delivered_atts) = MockOutbound::new_with_attachments();
        let cancel = CancellationToken::new();
        let cancel_clone = cancel.clone();

        let topic = "%s";
        jyc_types::state_dir::register(topic, &topic_path.join(".jyc"));
        let tp = topic_path.clone();
        let handle = tokio::spawn(async move {
            watch_pending_deliveries(
                &tp,
                message_dir,
                &test_message(),
                &outbound,
                std::sync::Arc::new(HookSet::default()),
                cancel_clone,
                None,
                topic,
            )
            .await;
        });

        tokio::time::sleep(Duration::from_secs(3)).await;
        cancel.cancel();
        let _ = handle.await;

        assert_eq!(delivered.lock().unwrap().len(), 1);
        let atts = delivered_atts.lock().unwrap();
        assert_eq!(atts.len(), 1);
        assert_eq!(atts[0], vec!["report.pdf".to_string()]);
    }

    #[tokio::test]
    async fn test_no_delivery_without_signal() {
        let tmp = tempdir().unwrap();
        let topic_path = tmp.path().to_path_buf();
        let message_dir = "2026-01-01_00-00-00";

        let jyc_dir = topic_path.join(".jyc");
        tokio::fs::create_dir_all(&jyc_dir).await.unwrap();
        // reply.md exists but no signal file
        tokio::fs::write(jyc_dir.join("reply.md"), "test")
            .await
            .unwrap();

        let (outbound, delivered) = MockOutbound::new();
        let cancel = CancellationToken::new();
        let cancel_clone = cancel.clone();

        let topic = "%s";
        jyc_types::state_dir::register(topic, &topic_path.join(".jyc"));
        let tp = topic_path.clone();
        let handle = tokio::spawn(async move {
            watch_pending_deliveries(
                &tp,
                message_dir,
                &test_message(),
                &outbound,
                std::sync::Arc::new(HookSet::default()),
                cancel_clone,
                None,
                topic,
            )
            .await;
        });

        tokio::time::sleep(Duration::from_secs(3)).await;
        cancel.cancel();
        let _ = handle.await;

        assert!(delivered.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn test_no_delivery_with_empty_reply() {
        let tmp = tempdir().unwrap();
        let topic_path = tmp.path().to_path_buf();
        let message_dir = "2026-01-01_00-00-00";

        let jyc_dir = topic_path.join(".jyc");
        tokio::fs::create_dir_all(&jyc_dir).await.unwrap();

        tokio::fs::write(jyc_dir.join("reply-sent.flag"), "{}")
            .await
            .unwrap();
        tokio::fs::write(jyc_dir.join("reply.md"), "   ")
            .await
            .unwrap();

        let (outbound, delivered) = MockOutbound::new();
        let cancel = CancellationToken::new();
        let cancel_clone = cancel.clone();

        let topic = "%s";
        jyc_types::state_dir::register(topic, &topic_path.join(".jyc"));
        let tp = topic_path.clone();
        let handle = tokio::spawn(async move {
            watch_pending_deliveries(
                &tp,
                message_dir,
                &test_message(),
                &outbound,
                std::sync::Arc::new(HookSet::default()),
                cancel_clone,
                None,
                topic,
            )
            .await;
        });

        tokio::time::sleep(Duration::from_secs(3)).await;
        cancel.cancel();
        let _ = handle.await;

        assert!(delivered.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn test_cancellation_stops_watcher() {
        let tmp = tempdir().unwrap();
        let topic_path = tmp.path().to_path_buf();
        let message_dir = "2026-01-01_00-00-00";
        let topic = "test_cancellation_stops_watcher";
        jyc_types::state_dir::register(topic, &topic_path.join(".jyc"));

        tokio::fs::create_dir_all(topic_path.join(".jyc"))
            .await
            .unwrap();

        let (outbound, _delivered) = MockOutbound::new();
        let cancel = CancellationToken::new();
        let cancel_clone = cancel.clone();

        let handle = tokio::spawn(async move {
            watch_pending_deliveries(
                &topic_path,
                message_dir,
                &test_message(),
                &outbound,
                std::sync::Arc::new(HookSet::default()),
                cancel_clone,
                None,
                topic,
            )
            .await;
        });

        cancel.cancel();
        tokio::time::timeout(Duration::from_secs(5), handle)
            .await
            .expect("Watcher should stop within 5 seconds")
            .unwrap();
    }

    #[tokio::test]
    async fn test_publishes_reply_sent_event_on_delivery() {
        let tmp = tempdir().unwrap();
        let topic_path = tmp.path().to_path_buf();
        let message_dir = "2026-01-01_00-00-00";
        let jyc_dir = topic_path.join(".jyc");
        tokio::fs::create_dir_all(&jyc_dir).await.unwrap();
        tokio::fs::write(jyc_dir.join("reply-sent.flag"), "{}")
            .await
            .unwrap();
        tokio::fs::write(jyc_dir.join("reply.md"), "Hi from watcher")
            .await
            .unwrap();

        let (outbound, delivered) = MockOutbound::new();
        let bus: TopicEventBusRef = Arc::new(SimpleThreadEventBus::new(10));
        let mut rx = bus.subscribe().await.unwrap();
        let cancel = CancellationToken::new();
        let cancel_clone = cancel.clone();

        let topic = "%s";
        jyc_types::state_dir::register(topic, &topic_path.join(".jyc"));
        let tp = topic_path.clone();
        let bus_for_watcher = bus.clone();
        let handle = tokio::spawn(async move {
            watch_pending_deliveries(
                &tp,
                message_dir,
                &test_message(),
                &outbound,
                std::sync::Arc::new(HookSet::default()),
                cancel_clone,
                Some(bus_for_watcher),
                topic,
            )
            .await;
        });

        let event = tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("Should receive ReplySent event")
            .expect("Event channel closed unexpectedly");

        cancel.cancel();
        let _ = handle.await;

        assert!(
            matches!(event, TopicEvent::ReplySent { ref text, .. } if text == "Hi from watcher"),
            "Expected ReplySent event, got {:?}",
            event
        );
        assert_eq!(delivered.lock().unwrap().len(), 1);
        assert!(!jyc_dir.join("reply-sent.flag").exists());
        assert!(!jyc_dir.join("reply.md").exists());
    }

    #[tokio::test]
    async fn reply_blocked_by_hook_fast_paths_and_reason() {
        let msg = test_message();
        let dir = std::path::Path::new(".");
        // Empty set → immediate None without spawning.
        assert_eq!(
            reply_blocked_by_hook(&HookSet::default(), "t", dir, &msg, "hi").await,
            None
        );
        // exit-2 → the hook's stderr.
        let blocker = HookSet::for_agent(
            &[jyc_types::config::HookConfig {
                event: "reply_send".into(),
                matcher: None,
                shell: vec!["sh".into(), "-c".into(), "echo censor >&2; exit 2".into()],
                timeout: None,
            }],
            "",
        );
        assert_eq!(
            reply_blocked_by_hook(&blocker, "t", dir, &msg, "hi")
                .await
                .as_deref(),
            Some("censor")
        );
        // exit-0 → None (proceed).
        let pass = HookSet::for_agent(
            &[jyc_types::config::HookConfig {
                event: "reply_send".into(),
                matcher: None,
                shell: vec!["true".into()],
                timeout: None,
            }],
            "",
        );
        assert_eq!(
            reply_blocked_by_hook(&pass, "t", dir, &msg, "hi").await,
            None
        );
    }
}
