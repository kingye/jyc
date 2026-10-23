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
use jyc_types::{
    InboundMessage, OutboundAdapter, OutboundAttachment, OutboundMessage,
    PROACTIVE_CAPABLE_CHANNEL_TYPES, SendResult,
};

use crate::server::pipe_proxy::PipeRoute;

/// WebSocket outbound adapter.
///
/// Holds a `tokio::sync::broadcast::Sender` that is shared with the inbound
/// adapter. All connected dashboard clients receive broadcast replies.
pub struct WebsocketOutboundAdapter {
    /// Broadcast sender — cloned for each new WebSocket connection.
    broadcast_tx: broadcast::Sender<String>,
    /// Message storage for persisting replies to chat log.
    storage: Arc<MessageStorage>,
    /// Pipe-owned channels whose patterns route into this hub channel. Empty
    /// for a hub channel the pipe does not feed, which then behaves exactly as
    /// it did before the split.
    route: Arc<PipeRoute>,
}

impl WebsocketOutboundAdapter {
    /// Create a new WebSocket outbound adapter with the given broadcast sender.
    pub fn new(broadcast_tx: broadcast::Sender<String>, storage: Arc<MessageStorage>) -> Self {
        Self {
            broadcast_tx,
            storage,
            route: Arc::new(PipeRoute::default()),
        }
    }

    /// Route proactive messages addressed to pipe-owned channels.
    ///
    /// Without this the adapter refuses attachments and broadcasts proactive
    /// text to dashboard clients only: the pipe never learns about either.
    pub fn with_pipe_route(mut self, route: PipeRoute) -> Self {
        self.route = Arc::new(route);
        self
    }

    /// Get the broadcast sender for the inbound adapter to clone.
    pub fn broadcast_tx(&self) -> broadcast::Sender<String> {
        self.broadcast_tx.clone()
    }

    /// Broadcast a reply to all connected clients.
    ///
    /// `attachments` is a pre-built JSON array (see `attachment_entry`) —
    /// absent from the payload when `None`.
    async fn broadcast_reply(
        &self,
        topic: &str,
        text: &str,
        attachments: Option<serde_json::Value>,
        chat_seq: u64,
    ) -> Result<()> {
        let mut payload = serde_json::json!({
            "type": "reply",
            "topic": topic,
            "text": text,
            "id": chat_seq,
        });
        if let Some(atts) = attachments {
            payload["attachments"] = atts;
        }
        let payload = payload.to_string();
        // broadcast::Sender::send is non-blocking; it returns an error only when
        // there are no active receivers. We ignore that error since it's fine
        // to have no connected clients.
        let _ = self.broadcast_tx.send(payload);
        Ok(())
    }

    /// Broadcast a proactive (non-reply) message.
    ///
    /// The frame is a reply frame with `topic` = recipient, so dashboard
    /// clients keep rendering it as they always have, plus `proactive: true`
    /// and — when the addressed channel is known — its name, which is what
    /// lets the pipe pick the forwarder that owns the recipient instead of
    /// looking the topic up in its reply state (where it would miss).
    async fn broadcast_proactive(
        &self,
        recipient: &str,
        text: &str,
        subject: &str,
        channel: Option<&str>,
        attachments: Option<serde_json::Value>,
    ) -> Result<()> {
        let mut payload = serde_json::json!({
            "type": "reply",
            "topic": recipient,
            "text": text,
            "proactive": true,
        });
        if !subject.is_empty() {
            payload["subject"] = subject.into();
        }
        if let Some(channel) = channel {
            payload["channel"] = channel.into();
        }
        if let Some(atts) = attachments {
            payload["attachments"] = atts;
        }
        let _ = self.broadcast_tx.send(payload.to_string());
        Ok(())
    }
}

/// Build one entry of the broadcast `attachments` array.
///
/// `path` is the relative URL of the inspect server's topic-file endpoint
/// (`GET /api/topics/{channel}/{topic}/files/{*file_path}`); consumers
/// (dashboard, pipe forwarders) prepend their known inspect base URL.
fn attachment_entry(channel: &str, topic: &str, att: &OutboundAttachment) -> serde_json::Value {
    let encoded_name = att
        .filename
        .split('/')
        .map(jyc_core::url_encode_segment)
        .collect::<Vec<_>>()
        .join("/");
    serde_json::json!({
        "filename": att.filename,
        "path": format!(
            "/api/topics/{}/{}/files/{}",
            jyc_core::url_encode_segment(channel),
            jyc_core::url_encode_segment(topic),
            encoded_name
        ),
        "content_type": att.content_type,
    })
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
        topic_path: &Path,
        _message_dir: &str,
        attachments: Option<&[OutboundAttachment]>,
    ) -> Result<SendResult> {
        // Use the original message topic as the broadcast key for normal
        // messages (topic = topic name from the WebSocket protocol).
        // Fall back to the topic directory name for scheduled jobs, where
        // `topic` is a descriptive string like "Scheduled job: ...".
        let topic = if original.topic.starts_with("Scheduled job:") {
            topic_path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or(original.topic.as_str())
        } else {
            original.topic.as_str()
        };
        let attachments_json = attachments.filter(|a| !a.is_empty()).map(|atts| {
            atts.iter()
                .map(|a| attachment_entry(&original.channel, topic, a))
                .collect::<Vec<_>>()
                .into()
        });
        // Allocate the reply's chat sequence number before anything reads
        // the row: the broadcast frame, the chat log row and the `ReplySent`
        // event the caller publishes all carry this one number.
        let chat_seq = jyc_core::chat_seq::next(topic);
        self.broadcast_reply(topic, reply_text, attachments_json, chat_seq)
            .await?;
        let message_id = uuid::Uuid::new_v4().to_string();

        // Persist reply to chat log for history loading
        if let Err(e) = self
            .storage
            .store_reply(topic, topic_path, reply_text, _message_dir, chat_seq)
            .await
        {
            tracing::warn!(error = %e, "Failed to store WebSocket reply to chat log");
        }

        tracing::info!(text_len = reply_text.len(), message_id = %message_id, "WebSocket reply broadcast");
        Ok(SendResult {
            message_id,
            chat_seq,
        })
    }

    async fn send_message(
        &self,
        _recipient: &str,
        _subject: &str,
        body: &str,
    ) -> Result<SendResult> {
        // A proactive send logs no chat row, so it carries no sequence.
        self.broadcast_reply(_recipient, body, None, 0).await?;
        let message_id = uuid::Uuid::new_v4().to_string();
        tracing::info!(text_len = body.len(), message_id = %message_id, "WebSocket message broadcast");
        Ok(SendResult {
            message_id,
            chat_seq: 0,
        })
    }

    /// Proactive send with the context a pipe-owned channel needs.
    ///
    /// A message for a pipe channel leaves the hub as a broadcast frame on this
    /// channel: the pipe is subscribed to it, and no other path exists — the
    /// hub has neither the channel's credentials nor a shared filesystem for
    /// the attachment bytes. Anything else keeps the pre-split behaviour:
    /// proactive text goes to whoever is listening, attachments are refused.
    async fn send_proactive(&self, message: &OutboundMessage<'_>) -> Result<SendResult> {
        let attachments = message.attachments.filter(|a| !a.is_empty());
        let Some((channel_name, channel)) = self.route.resolve(message.channel)? else {
            if attachments.is_some() {
                anyhow::bail!("Attachments not supported for this channel type");
            }
            self.broadcast_proactive(message.recipient, message.body, message.subject, None, None)
                .await?;
            tracing::info!(
                recipient = %message.recipient,
                text_len = message.body.len(),
                "WebSocket proactive message broadcast"
            );
            return Ok(SendResult {
                message_id: uuid::Uuid::new_v4().to_string(),
                chat_seq: 0,
            });
        };

        if !PROACTIVE_CAPABLE_CHANNEL_TYPES.contains(&channel.channel_type.as_str()) {
            // The file case keeps the error the tool has always reported;
            // a channel that takes no proactive message at all says so.
            if attachments.is_some() {
                anyhow::bail!("Attachments not supported for this channel type");
            }
            anyhow::bail!(
                "proactive messages are not supported for channel type '{}'",
                channel.channel_type
            );
        }
        // The pipe downloads every attachment from the inspect server's
        // topic-file endpoint, which is addressed by channel and topic.
        if attachments.is_some()
            && (message.origin_channel.is_empty() || message.origin_topic.is_empty())
        {
            anyhow::bail!("attachments need the channel and topic holding them (origin is empty)");
        }
        let attachments_json = attachments.map(|atts| {
            atts.iter()
                .map(|a| attachment_entry(message.origin_channel, message.origin_topic, a))
                .collect::<Vec<_>>()
                .into()
        });

        self.broadcast_proactive(
            message.recipient,
            message.body,
            message.subject,
            Some(channel_name),
            attachments_json,
        )
        .await?;
        tracing::info!(
            channel = %channel_name,
            recipient = %message.recipient,
            text_len = message.body.len(),
            attachments = attachments.map_or(0, |a| a.len()),
            "proactive message relayed to the pipe"
        );
        Ok(SendResult {
            message_id: uuid::Uuid::new_v4().to_string(),
            chat_seq: 0,
        })
    }

    async fn send_question(&self, request: &jyc_types::channel::QuestionRequest) -> Result<()> {
        let payload = serde_json::json!({
            "type": "question",
            "topic": request.topic,
            "context": request.context,
            "question": request.question,
            "options": request.options,
            "allow_multiple": request.allow_multiple,
        });
        // Same no-receiver tolerance as `broadcast_reply`.
        let _ = self.broadcast_tx.send(payload.to_string());
        tracing::info!(topic = %request.topic, "WebSocket question broadcast");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The question frame is assembled field by field, so anything the UI
    /// needs has to be listed here. `allow_multiple` is the one the question
    /// box switches its whole input mode on — dropping it fails silently.
    /// `context` is the one it shows above the question; dropping it leaves
    /// the user answering blind.
    #[tokio::test]
    async fn test_send_question_broadcasts_render_fields() {
        let (tx, mut rx) = broadcast::channel(16);
        let tmp = tempfile::TempDir::new().unwrap();
        let storage = Arc::new(MessageStorage::new(tmp.path()));
        let adapter = WebsocketOutboundAdapter::new(tx, storage);
        let request = jyc_types::channel::QuestionRequest {
            topic: "general".to_string(),
            context: Some("I compared the two options".to_string()),
            question: "Which?".to_string(),
            options: vec!["a".to_string(), "b".to_string()],
            allow_multiple: true,
        };

        adapter.send_question(&request).await.unwrap();

        let frame = rx.recv().await.unwrap();
        assert!(
            frame.contains("\"allow_multiple\":true"),
            "the question box cannot guess the mode: {frame}"
        );
        assert!(
            frame.contains("\"context\":\"I compared the two options\""),
            "the question box cannot show what the question is about: {frame}"
        );
    }

    #[tokio::test]
    async fn test_send_reply_broadcasts() {
        let (tx, mut rx) = broadcast::channel(16);
        let tmp = tempfile::TempDir::new().unwrap();
        let topic_path = tmp.path().join("general");
        jyc_types::state_dir::register("general", &topic_path.join(".jyc"));
        tokio::fs::create_dir_all(&topic_path).await.unwrap();
        let storage = Arc::new(MessageStorage::new(&topic_path));
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
            references: None,
            reply_to_id: None,
            external_id: None,
            attachments: vec![],
            metadata: std::collections::HashMap::new(),
            matched_pattern: None,
        };

        let result = adapter
            .send_reply(&message, "AI reply", &topic_path, "msg_001", None)
            .await;
        assert!(result.is_ok());

        let sent = rx.recv().await.expect("should receive broadcast");
        let parsed: serde_json::Value = serde_json::from_str(&sent).unwrap();
        assert_eq!(parsed["type"], "reply");
        assert_eq!(parsed["topic"], "general");
        assert_eq!(parsed["text"], "AI reply");
        // No attachments → key absent from the payload
        assert!(parsed.get("attachments").is_none());
    }

    /// Replies carrying attachments broadcast download paths pointing at the
    /// inspect server's topic-file endpoint; consumers prepend the base URL.
    #[tokio::test]
    async fn test_send_reply_includes_attachment_download_paths() {
        let (tx, mut rx) = broadcast::channel(16);
        let tmp = tempfile::TempDir::new().unwrap();
        let topic_path = tmp.path().join("general");
        jyc_types::state_dir::register("general", &topic_path.join(".jyc"));
        tokio::fs::create_dir_all(&topic_path).await.unwrap();
        let storage = Arc::new(MessageStorage::new(&topic_path));
        let adapter = WebsocketOutboundAdapter::new(tx, storage);

        let message = InboundMessage {
            id: "test".to_string(),
            channel: "local_dev".to_string(),
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
            references: None,
            reply_to_id: None,
            external_id: None,
            attachments: vec![],
            metadata: std::collections::HashMap::new(),
            matched_pattern: None,
        };
        let atts = vec![OutboundAttachment {
            filename: "报表 v2.pdf".to_string(),
            path: topic_path.join("报表 v2.pdf"),
            content_type: "application/pdf".to_string(),
        }];

        adapter
            .send_reply(&message, "done", &topic_path, "msg_001", Some(&atts))
            .await
            .unwrap();

        let sent = rx.recv().await.expect("should receive broadcast");
        let parsed: serde_json::Value = serde_json::from_str(&sent).unwrap();
        let atts = parsed["attachments"].as_array().unwrap();
        assert_eq!(atts.len(), 1);
        assert_eq!(atts[0]["filename"], "报表 v2.pdf");
        assert_eq!(atts[0]["content_type"], "application/pdf");
        // Non-ASCII and spaces are percent-encoded per URL segment.
        assert_eq!(
            atts[0]["path"],
            "/api/topics/local_dev/general/files/%E6%8A%A5%E8%A1%A8%20v2.pdf"
        );
    }

    /// Regression test: scheduled jobs set `topic` to a descriptive string
    /// ("Scheduled job: ...") rather than the topic name. The broadcast
    /// must use the topic directory name so WebSocket clients subscribed
    /// to the topic actually receive the reply.
    #[tokio::test]
    async fn test_send_reply_uses_topic_path_not_topic_for_job() {
        let (tx, mut rx) = broadcast::channel(16);
        let tmp = tempfile::TempDir::new().unwrap();
        let topic_path = tmp.path().join("dev");
        jyc_types::state_dir::register("dev", &topic_path.join(".jyc"));
        tokio::fs::create_dir_all(&topic_path).await.unwrap();
        let storage = Arc::new(MessageStorage::new(&topic_path));
        let adapter = WebsocketOutboundAdapter::new(tx, storage);

        // Simulate a scheduled job message: topic is descriptive, not the topic name
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
            references: None,
            reply_to_id: None,
            external_id: None,
            attachments: vec![],
            metadata: std::collections::HashMap::new(),
            matched_pattern: None,
        };

        let result = adapter
            .send_reply(&message, "CI passed!", &topic_path, "msg_001", None)
            .await;
        assert!(result.is_ok());

        let sent = rx.recv().await.expect("should receive broadcast");
        let parsed: serde_json::Value = serde_json::from_str(&sent).unwrap();
        assert_eq!(parsed["type"], "reply");
        // Topic must be "dev" (from topic_path), NOT "Scheduled job: ..."
        assert_eq!(parsed["topic"], "dev");
        assert_eq!(parsed["text"], "CI passed!");
    }

    #[tokio::test]
    async fn test_send_without_receiver_ok() {
        // broadcast with no receivers should still succeed
        let tx = broadcast::channel(16).0;
        let tmp = tempfile::TempDir::new().unwrap();
        let topic_path = tmp.path().join("general");
        jyc_types::state_dir::register("general", &topic_path.join(".jyc"));
        tokio::fs::create_dir_all(&topic_path).await.unwrap();
        let storage = Arc::new(MessageStorage::new(&topic_path));
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
            references: None,
            reply_to_id: None,
            external_id: None,
            attachments: vec![],
            metadata: std::collections::HashMap::new(),
            matched_pattern: None,
        };

        let result = adapter
            .send_reply(&message, "AI reply", &topic_path, "msg_001", None)
            .await;
        assert!(result.is_ok());
    }

    #[test]
    fn test_clean_body_passthrough() {
        let tmp = tempfile::TempDir::new().unwrap();
        let topic_path = tmp.path().join("general");
        jyc_types::state_dir::register("general", &topic_path.join(".jyc"));
        std::fs::create_dir_all(&topic_path).unwrap();
        let storage = Arc::new(MessageStorage::new(&topic_path));
        let adapter = WebsocketOutboundAdapter::new(broadcast::channel(16).0, storage);
        assert_eq!(adapter.clean_body("hello\nworld"), "hello\nworld");
    }

    /// Adapter for a hub channel with the given pipe channels routing into it.
    fn pipe_adapter(
        broadcast_tx: broadcast::Sender<String>,
        storage: Arc<MessageStorage>,
        hub_channel: &str,
        pipe_channels: &[(&str, &str)],
    ) -> WebsocketOutboundAdapter {
        use crate::server::pipe_proxy::PipeChannel;
        WebsocketOutboundAdapter::new(broadcast_tx, storage).with_pipe_route(PipeRoute::new(
            hub_channel,
            pipe_channels.iter().map(|(name, channel_type)| {
                (
                    name.to_string(),
                    PipeChannel {
                        channel_type: channel_type.to_string(),
                        targets: vec![hub_channel.to_string()],
                    },
                )
            }),
        ))
    }

    fn storage_at(topic_path: &Path) -> Arc<MessageStorage> {
        jyc_types::state_dir::register("general", &topic_path.join(".jyc"));
        std::fs::create_dir_all(topic_path).unwrap();
        Arc::new(MessageStorage::new(topic_path))
    }

    /// A proactive message for a pipe-owned channel must name the channel and
    /// carry attachment *paths*, not files: the pipe holds the credentials and
    /// has no shared filesystem, so a frame is the only thing that can reach it.
    #[tokio::test]
    async fn test_send_proactive_relays_attachments_for_pipe_channel() {
        let (tx, mut rx) = broadcast::channel(16);
        let tmp = tempfile::TempDir::new().unwrap();
        let topic_path = tmp.path().join("general");
        let storage = storage_at(&topic_path);
        let adapter = pipe_adapter(tx, storage, "agents", &[("wecom_bot", "wecom_bot")]);
        let atts = vec![OutboundAttachment {
            filename: "报表 v2.pdf".to_string(),
            path: topic_path.join("报表 v2.pdf"),
            content_type: "application/pdf".to_string(),
        }];

        adapter
            .send_proactive(&OutboundMessage {
                recipient: "chat-42",
                channel: "wecom_bot",
                subject: "",
                body: "报表好了",
                attachments: Some(&atts),
                origin_channel: "agents",
                origin_topic: "general",
            })
            .await
            .unwrap();

        let sent = rx.recv().await.expect("should receive broadcast");
        let parsed: serde_json::Value = serde_json::from_str(&sent).unwrap();
        assert_eq!(parsed["type"], "reply");
        assert_eq!(parsed["proactive"].as_bool(), Some(true));
        assert_eq!(parsed["channel"], "wecom_bot");
        // `topic` carries the recipient: the pipe has no topic state for a
        // proactive message, so this frame must not look like a reply to one.
        assert_eq!(parsed["topic"], "chat-42");
        assert_eq!(parsed["text"], "报表好了");
        assert_eq!(
            parsed["attachments"][0]["path"],
            "/api/topics/agents/general/files/%E6%8A%A5%E8%A1%A8%20v2.pdf"
        );
    }

    /// A channel the pipe owns but whose adapter cannot send files keeps the
    /// error the tool has always reported — reporting success would send the
    /// agent looking for a file that never arrived.
    #[tokio::test]
    async fn test_send_proactive_refuses_attachments_on_incapable_pipe_channel() {
        let (tx, mut rx) = broadcast::channel(16);
        let tmp = tempfile::TempDir::new().unwrap();
        let storage = storage_at(&tmp.path().join("general"));
        let adapter = pipe_adapter(tx, storage, "agents", &[("github", "github")]);
        let atts = vec![OutboundAttachment {
            filename: "log.txt".to_string(),
            path: tmp.path().join("general").join("log.txt"),
            content_type: "text/plain".to_string(),
        }];

        let err = adapter
            .send_proactive(&OutboundMessage {
                recipient: "owner/repo#1",
                channel: "github",
                subject: "",
                body: "log attached",
                attachments: Some(&atts),
                origin_channel: "agents",
                origin_topic: "general",
            })
            .await
            .unwrap_err()
            .to_string();

        assert_eq!(err, "Attachments not supported for this channel type");
        assert!(
            rx.try_recv().is_err(),
            "nothing may be broadcast when the send is refused"
        );

        // Text alone is refused too: this channel's forwarder has no proactive
        // path, so broadcasting would report a success that sends nothing.
        let err = adapter
            .send_proactive(&OutboundMessage {
                recipient: "owner/repo#1",
                channel: "github",
                subject: "",
                body: "deploy finished",
                attachments: None,
                origin_channel: "agents",
                origin_topic: "general",
            })
            .await
            .unwrap_err()
            .to_string();
        assert_eq!(
            err,
            "proactive messages are not supported for channel type 'github'"
        );
        assert!(rx.try_recv().is_err());
    }

    /// Two pipe channels on one hub channel: without a name the message could
    /// go to either, so it is refused rather than guessed.
    #[tokio::test]
    async fn test_send_proactive_refuses_ambiguous_channel() {
        let (tx, _rx) = broadcast::channel(16);
        let tmp = tempfile::TempDir::new().unwrap();
        let storage = storage_at(&tmp.path().join("general"));
        let adapter = pipe_adapter(
            tx,
            storage,
            "agents",
            &[("wecom_bot", "wecom_bot"), ("feishu", "feishu")],
        );

        let err = adapter
            .send_proactive(&OutboundMessage {
                recipient: "someone",
                channel: "",
                subject: "",
                body: "alert",
                attachments: None,
                origin_channel: "agents",
                origin_topic: "general",
            })
            .await
            .unwrap_err()
            .to_string();

        assert!(err.contains("several pipe channels"), "{err}");
    }

    /// A hub channel the pipe does not feed keeps the pre-split behaviour:
    /// proactive text goes to the dashboard clients, attachments are refused.
    #[tokio::test]
    async fn test_send_proactive_without_pipe_route_keeps_old_behaviour() {
        let (tx, mut rx) = broadcast::channel(16);
        let tmp = tempfile::TempDir::new().unwrap();
        let storage = storage_at(&tmp.path().join("general"));
        let adapter = WebsocketOutboundAdapter::new(tx, storage);
        let atts = vec![OutboundAttachment {
            filename: "x.txt".to_string(),
            path: tmp.path().join("general").join("x.txt"),
            content_type: "text/plain".to_string(),
        }];

        let err = adapter
            .send_proactive(&OutboundMessage {
                recipient: "user",
                channel: "",
                subject: "",
                body: "hello",
                attachments: Some(&atts),
                origin_channel: "agents",
                origin_topic: "general",
            })
            .await
            .unwrap_err()
            .to_string();
        assert_eq!(err, "Attachments not supported for this channel type");

        adapter
            .send_proactive(&OutboundMessage {
                recipient: "user",
                channel: "",
                subject: "",
                body: "hello",
                attachments: None,
                origin_channel: "agents",
                origin_topic: "general",
            })
            .await
            .unwrap();
        let sent = rx.recv().await.expect("should receive broadcast");
        let parsed: serde_json::Value = serde_json::from_str(&sent).unwrap();
        assert_eq!(parsed["topic"], "user");
        assert_eq!(parsed["text"], "hello");
        assert!(parsed.get("channel").is_none());
    }

    /// A reply must never look like a proactive message: the pipe's reply path
    /// keys on topic state and would otherwise take the proactive branch.
    #[tokio::test]
    async fn test_send_reply_is_not_marked_proactive() {
        let (tx, mut rx) = broadcast::channel(16);
        let tmp = tempfile::TempDir::new().unwrap();
        let topic_path = tmp.path().join("general");
        let storage = storage_at(&topic_path);
        let adapter = WebsocketOutboundAdapter::new(tx, storage);
        let message = InboundMessage {
            id: "test".to_string(),
            channel: "agents".to_string(),
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
            references: None,
            reply_to_id: None,
            external_id: None,
            attachments: vec![],
            metadata: std::collections::HashMap::new(),
            matched_pattern: None,
        };

        adapter
            .send_reply(&message, "AI reply", &topic_path, "msg_001", None)
            .await
            .unwrap();

        let sent = rx.recv().await.expect("should receive broadcast");
        let parsed: serde_json::Value = serde_json::from_str(&sent).unwrap();
        assert!(parsed.get("proactive").is_none());
    }
}
