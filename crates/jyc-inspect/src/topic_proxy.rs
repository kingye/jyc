//! `TopicProxyHandler` — dashboard-side WebSocket handler for **any**
//! (channel, topic) pair.
//!
//! Used by the inspect server to expose a unified WebSocket endpoint for the
//! dashboard to chat with topics regardless of channel type. The handler is
//! constructed per-connection with the (channel, topic) pair bound from the
//! URL path (`/ws/<channel>/<topic>`), so the WebSocket protocol no longer
//! needs to carry these fields in the payload.
//!
//! Inbound messages are routed to the channel's `TopicManager::enqueue` after
//! loading routing metadata (channel_uid, external_id, references) from
//! `.jyc/topic-meta.json`. Outbound events come from the per-channel
//! `InspectContext.broadcast` bus populated by the `ActivityTracker`.

use std::path::PathBuf;
use std::sync::Arc;

use arc_swap::ArcSwap;
use async_trait::async_trait;
use futures_util::{SinkExt, StreamExt};
use jyc_core::topic_manager::TopicManager;
use jyc_types::{InboundMessage, MessageContent, PatternMatch};
use serde::Deserialize;
use std::collections::HashMap;
use tokio::sync::broadcast;

use crate::server::WebsocketHandler;

/// WebSocket messages accepted by `TopicProxyHandler`.
///
/// `channel` and `topic` are bound at handler construction time from
/// the URL path. The payload carries only message-specific fields like
/// `text`.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ClientMessage {
    /// Inject a message into the topic for AI processing.
    #[serde(rename = "message")]
    Message { text: String },
    /// Close the WebSocket connection cleanly.
    #[serde(rename = "disconnect")]
    Disconnect,
    /// Ping for keep-alive (no-op; tokio-tungstenite handles WS-level pings).
    #[serde(rename = "ping")]
    Ping,
}

/// Per-channel routing metadata persisted in `.jyc/topic-meta.json` on the
/// first inbound message. Loaded by `TopicProxyHandler` to restore
/// channel-specific fields (github_number, chat_id, etc.) when injecting
/// messages from the dashboard.
#[derive(Debug, Default, Clone, serde::Deserialize)]
#[serde(default)]
struct TopicMeta {
    channel_uid: String,
    external_id: Option<String>,
    references: Option<Vec<String>>,
    metadata: HashMap<String, serde_json::Value>,
}

/// WebSocket handler that proxies between the dashboard and a specific
/// `(channel, topic)` pair via the channel's `TopicManager` and the
/// `InspectContext.broadcast` bus.
pub struct TopicProxyHandler {
    channel: String,
    topic: String,
    topic_managers: Arc<ArcSwap<Vec<Arc<TopicManager>>>>,
    inspect_broadcast: Arc<broadcast::Sender<String>>,
}

impl TopicProxyHandler {
    pub fn new(
        channel: String,
        topic: String,
        topic_managers: Arc<ArcSwap<Vec<Arc<TopicManager>>>>,
        inspect_broadcast: Arc<broadcast::Sender<String>>,
    ) -> Self {
        Self {
            channel,
            topic,
            topic_managers,
            inspect_broadcast,
        }
    }

    /// Find the `TopicManager` for `channel`, returning a descriptive error
    /// if no such channel is registered.
    fn find_topic_manager(&self) -> anyhow::Result<Arc<TopicManager>> {
        let tms = self.topic_managers.load();
        tms.iter()
            .find(|tm| tm.channel_name() == self.channel)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("no topic manager found for channel '{}'", self.channel))
    }

    /// Load routing metadata for this topic from `.jyc/topic-meta.json`.
    /// Returns sensible defaults if the file doesn't exist (e.g., for a
    /// never-before-seen topic).
    async fn load_topic_meta(&self, tm: &Arc<TopicManager>) -> TopicMeta {
        let Some(topic_path) = tm.topic_path(&self.topic).await else {
            return TopicMeta::default();
        };
        let meta_path: PathBuf = topic_path.join(".jyc").join("topic-meta.json");
        let Ok(content) = tokio::fs::read_to_string(&meta_path).await else {
            return TopicMeta::default();
        };
        serde_json::from_str(&content).unwrap_or_default()
    }

    /// Handle an inbound `Message { text }` by constructing a synthetic
    /// `InboundMessage` and enqueueing it via `TopicManager::enqueue`.
    async fn handle_inbound_message(
        &self,
        tm: &Arc<TopicManager>,
        text: String,
    ) -> anyhow::Result<()> {
        let meta = self.load_topic_meta(tm).await;

        let now = chrono::Utc::now();
        let message = InboundMessage {
            id: format!("inspect-{}", now.timestamp_millis()),
            channel: self.channel.clone(),
            channel_uid: if meta.channel_uid.is_empty() {
                "dashboard".to_string()
            } else {
                meta.channel_uid
            },
            sender: "dashboard".to_string(),
            sender_address: "dashboard@inspect".to_string(),
            recipients: vec![],
            topic: self.topic.clone(),
            content: MessageContent {
                text: Some(text),
                html: None,
                markdown: None,
            },
            timestamp: now,
            references: meta.references,
            reply_to_id: None,
            external_id: meta.external_id,
            attachments: vec![],
            metadata: meta.metadata,
            matched_pattern: None,
        };

        let pattern_match = PatternMatch {
            pattern_name: String::new(),
            channel: self.channel.clone(),
            matches: HashMap::new(),
        };

        tm.enqueue(message, self.topic.clone(), pattern_match, None, true, None)
            .await;
        Ok(())
    }
}

#[async_trait]
impl WebsocketHandler for TopicProxyHandler {
    async fn handle(
        &self,
        ws: axum::extract::ws::WebSocket,
        addr: std::net::SocketAddr,
        // This handler binds `channel` + `topic` at construction time from
        // the URL (`/ws/<channel>/<topic>`), so the `scoped_topic` passed
        // by the inspect server is intentionally ignored.
        _scoped_topic: Option<&str>,
    ) -> anyhow::Result<()> {
        let tm = self.find_topic_manager()?;
        let mut broadcast_rx = self.inspect_broadcast.subscribe();
        let channel = self.channel.clone();
        let topic = self.topic.clone();

        tracing::info!(
            addr = %addr,
            channel = %channel,
            topic = %topic,
            "TopicProxyHandler: dashboard client connected"
        );

        // Split the WebSocket into independent read/write halves so the
        // select! below can drive both concurrently. axum's WebSocket
        // has the same `split()` shape as tokio_tungstenite's stream:
        // a `Stream<Item = Result<Message, Error>>` for the read half
        // and a `Sink<Message>` for the write half.
        let (mut write, mut read) = ws.split();

        loop {
            tokio::select! {
                msg = read.next() => {
                    use axum::extract::ws::Message;
                    match msg {
                        Some(Ok(Message::Text(text))) => {
                            let parsed: Result<ClientMessage, _> = serde_json::from_str(&text);
                            match parsed {
                                Ok(ClientMessage::Message { text }) => {
                                    if let Err(e) = self.handle_inbound_message(&tm, text).await {
                                        tracing::warn!(error = %e, "handle_inbound_message failed");
                                    }
                                }
                                Ok(ClientMessage::Disconnect) => {
                                    tracing::info!(
                                        channel = %channel,
                                        topic = %topic,
                                        "Dashboard sent disconnect"
                                    );
                                    break;
                                }
                                Ok(ClientMessage::Ping) => {
                                    // No-op; axum auto-replies to WS pings at
                                    // the protocol layer.
                                }
                                Err(e) => {
                                    tracing::debug!(
                                        error = %e,
                                        text = %text,
                                        "Invalid TopicProxy message (ignored)"
                                    );
                                }
                            }
                        }
                        Some(Ok(Message::Close(_))) => break,
                        // Ignore Binary/Ping/Pong at this layer.
                        Some(Ok(_)) => {}
                        Some(Err(e)) => {
                            tracing::debug!(error = %e, "WebSocket read error");
                            break;
                        }
                        None => break,
                    }
                }
                broadcast = broadcast_rx.recv() => {
                    match broadcast {
                        Ok(payload) => {
                            // Filter to events for this (channel, topic) only.
                            // Payload format: {"type":..., "channel":..., "topic":..., ...}
                            if let Some(filtered) = filter_for_topic(&payload, &channel, &topic)
                                && let Err(e) = write
                                    .send(axum::extract::ws::Message::Text(filtered.into()))
                                    .await
                            {
                                tracing::debug!(error = %e, "Failed to forward event");
                                break;
                            }
                        }
                        Err(broadcast::error::RecvError::Lagged(n)) => {
                            // Send a resync event so the client re-hydrates via REST.
                            let resync = serde_json::json!({
                                "type": "resync",
                                "channel": channel,
                                "topic": topic,
                                "dropped": n,
                            });
                            if let Err(e) = write
                                .send(axum::extract::ws::Message::Text(resync.to_string().into()))
                                .await
                            {
                                tracing::debug!(error = %e, "Failed to send resync");
                                break;
                            }
                        }
                        Err(broadcast::error::RecvError::Closed) => break,
                    }
                }
            }
        }

        tracing::info!(
            addr = %addr,
            channel = %channel,
            topic = %topic,
            "TopicProxyHandler: dashboard client disconnected"
        );
        Ok(())
    }
}

/// Filter a broadcast payload to only include events for the given
/// `(channel, topic)`. Returns Some(payload) if it matches, None otherwise.
///
/// All payloads on the bus have the shape:
///   {"type": "...", "channel": "...", "topic": "...", ...}
fn filter_for_topic(payload: &str, channel: &str, topic: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(payload).ok()?;
    let p_channel = v.get("channel").and_then(|c| c.as_str())?;
    let p_topic = v.get("topic").and_then(|c| c.as_str())?;
    if p_channel == channel && p_topic == topic {
        Some(payload.to_string())
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filter_matches_channel_and_topic() {
        let payload = r#"{"type":"activity","channel":"c1","topic":"t1","entry":{}}"#;
        assert_eq!(
            filter_for_topic(payload, "c1", "t1").as_deref(),
            Some(payload)
        );
    }

    #[test]
    fn filter_rejects_other_topic() {
        let payload = r#"{"type":"activity","channel":"c1","topic":"t1","entry":{}}"#;
        assert!(filter_for_topic(payload, "c1", "t2").is_none());
        assert!(filter_for_topic(payload, "c2", "t1").is_none());
        assert!(filter_for_topic(payload, "c2", "t2").is_none());
    }

    #[test]
    fn filter_rejects_malformed_payload() {
        assert!(filter_for_topic("not json", "c", "t").is_none());
        assert!(filter_for_topic(r#"{"type":"x"}"#, "c", "t").is_none());
    }
}
