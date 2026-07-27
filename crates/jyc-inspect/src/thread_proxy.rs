//! `ThreadProxyHandler` — dashboard-side WebSocket handler for **any**
//! (channel, thread) pair.
//!
//! Used by the inspect server to expose a unified WebSocket endpoint for the
//! dashboard to chat with threads regardless of channel type. The handler is
//! constructed per-connection with the (channel, thread) pair bound from the
//! URL path (`/ws/<channel>/<thread>`), so the WebSocket protocol no longer
//! needs to carry these fields in the payload.
//!
//! Inbound messages are routed to the channel's `ThreadManager::enqueue` after
//! loading routing metadata (channel_uid, external_id, thread_refs) from
//! `.jyc/thread-meta.json`. Outbound events come from the per-channel
//! `InspectContext.broadcast` bus populated by the `ActivityTracker`.

use std::path::PathBuf;
use std::sync::Arc;

use arc_swap::ArcSwap;
use async_trait::async_trait;
use futures_util::{SinkExt, StreamExt};
use jyc_core::thread_manager::ThreadManager;
use jyc_types::{InboundMessage, MessageContent, PatternMatch};
use serde::Deserialize;
use std::collections::HashMap;
use tokio::sync::broadcast;

use crate::server::{PrependStream, WebsocketHandler};

/// WebSocket messages accepted by `ThreadProxyHandler`.
///
/// `channel` and `thread` are bound at handler construction time from
/// the URL path. The optional `thread` field in `Message` is accepted for
/// protocol compatibility with `WebsocketInboundAdapter` but ignored.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ClientMessage {
    /// Inject a message into the thread for AI processing.
    #[serde(rename = "message")]
    Message {
        text: String,
        /// Accepted for protocol compatibility with `WebsocketInboundAdapter`;
        /// ignored by ThreadProxyHandler (the thread is already in the URL).
        #[serde(default)]
        #[allow(dead_code)]
        thread: Option<String>,
    },
    /// Reset the agent session for this thread.
    #[serde(rename = "reset_session")]
    ResetSession,
    /// Close the WebSocket connection cleanly.
    #[serde(rename = "disconnect")]
    Disconnect,
    /// Ping for keep-alive (no-op; tokio-tungstenite handles WS-level pings).
    #[serde(rename = "ping")]
    Ping,
}

/// Per-channel routing metadata persisted in `.jyc/thread-meta.json` on the
/// first inbound message. Loaded by `ThreadProxyHandler` to restore
/// channel-specific fields (github_number, chat_id, etc.) when injecting
/// messages from the dashboard.
#[derive(Debug, Default, Clone, serde::Deserialize)]
#[serde(default)]
struct ThreadMeta {
    channel_uid: String,
    external_id: Option<String>,
    thread_refs: Option<Vec<String>>,
    metadata: HashMap<String, serde_json::Value>,
}

/// WebSocket handler that proxies between the dashboard and a specific
/// `(channel, thread)` pair via the channel's `ThreadManager` and the
/// `InspectContext.broadcast` bus.
pub struct ThreadProxyHandler {
    channel: String,
    thread: String,
    thread_managers: Arc<ArcSwap<Vec<Arc<ThreadManager>>>>,
    inspect_broadcast: Arc<broadcast::Sender<String>>,
}

impl ThreadProxyHandler {
    pub fn new(
        channel: String,
        thread: String,
        thread_managers: Arc<ArcSwap<Vec<Arc<ThreadManager>>>>,
        inspect_broadcast: Arc<broadcast::Sender<String>>,
    ) -> Self {
        Self {
            channel,
            thread,
            thread_managers,
            inspect_broadcast,
        }
    }

    /// Find the `ThreadManager` for `channel`, returning a descriptive error
    /// if no such channel is registered.
    fn find_thread_manager(&self) -> anyhow::Result<Arc<ThreadManager>> {
        let tms = self.thread_managers.load();
        tms.iter()
            .find(|tm| tm.channel_name() == self.channel)
            .cloned()
            .ok_or_else(|| {
                anyhow::anyhow!("no thread manager found for channel '{}'", self.channel)
            })
    }

    /// Load routing metadata for this thread from `.jyc/thread-meta.json`.
    /// Returns sensible defaults if the file doesn't exist (e.g., for a
    /// never-before-seen thread).
    async fn load_thread_meta(&self, tm: &Arc<ThreadManager>) -> ThreadMeta {
        let Some(thread_path) = tm.thread_path(&self.thread).await else {
            return ThreadMeta::default();
        };
        let meta_path: PathBuf = thread_path.join(".jyc").join("thread-meta.json");
        let Ok(content) = tokio::fs::read_to_string(&meta_path).await else {
            return ThreadMeta::default();
        };
        serde_json::from_str(&content).unwrap_or_default()
    }

    /// Handle an inbound `Message { text }` by constructing a synthetic
    /// `InboundMessage` and enqueueing it via `ThreadManager::enqueue`.
    async fn handle_inbound_message(
        &self,
        tm: &Arc<ThreadManager>,
        text: String,
    ) -> anyhow::Result<()> {
        let meta = self.load_thread_meta(tm).await;

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
            topic: self.thread.clone(),
            content: MessageContent {
                text: Some(text),
                html: None,
                markdown: None,
            },
            timestamp: now,
            thread_refs: meta.thread_refs,
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

        tm.enqueue(
            message,
            self.thread.clone(),
            pattern_match,
            None,
            true,
            None,
        )
        .await;
        Ok(())
    }

    /// Handle a `reset_session` request by resetting the agent session for
    /// this thread via the ThreadManager.
    async fn handle_reset_session(&self, tm: &Arc<ThreadManager>) -> anyhow::Result<()> {
        let config = jyc_types::ResetCompressionConfig::default();
        if let Err(e) = tm.reset_session(&self.thread, &config).await {
            tracing::warn!(
                thread = %self.thread,
                error = %e,
                "Failed to reset session via thread manager"
            );
        }
        Ok(())
    }
}

#[async_trait]
impl WebsocketHandler for ThreadProxyHandler {
    async fn handle(
        &self,
        ws_stream: tokio_tungstenite::WebSocketStream<PrependStream>,
        addr: std::net::SocketAddr,
    ) -> anyhow::Result<()> {
        let tm = self.find_thread_manager()?;
        let mut broadcast_rx = self.inspect_broadcast.subscribe();
        let channel = self.channel.clone();
        let thread = self.thread.clone();

        tracing::info!(
            addr = %addr,
            channel = %channel,
            thread = %thread,
            "ThreadProxyHandler: dashboard client connected"
        );

        let (mut write, mut read) = ws_stream.split();

        loop {
            tokio::select! {
                msg = read.next() => {
                    match msg {
                        Some(Ok(tokio_tungstenite::tungstenite::Message::Text(text))) => {
                            let parsed: Result<ClientMessage, _> = serde_json::from_str(&text);
                            match parsed {
                                Ok(ClientMessage::Message { text, .. }) => {
                                    if let Err(e) = self.handle_inbound_message(&tm, text).await {
                                        tracing::warn!(error = %e, "handle_inbound_message failed");
                                    }
                                }
                                Ok(ClientMessage::ResetSession) => {
                                    if let Err(e) = self.handle_reset_session(&tm).await {
                                        tracing::warn!(error = %e, "handle_reset_session failed");
                                    }
                                }
                                Ok(ClientMessage::Disconnect) => {
                                    tracing::info!(
                                        channel = %channel,
                                        thread = %thread,
                                        "Dashboard sent disconnect"
                                    );
                                    break;
                                }
                                Ok(ClientMessage::Ping) => {
                                    // No-op; WS-level pings handled by tokio-tungstenite
                                }
                                Err(e) => {
                                    tracing::debug!(
                                        error = %e,
                                        text = %text,
                                        "Invalid ThreadProxy message (ignored)"
                                    );
                                }
                            }
                        }
                        Some(Ok(tokio_tungstenite::tungstenite::Message::Close(_))) => break,
                        Some(Ok(tokio_tungstenite::tungstenite::Message::Ping(payload))) => {
                            // WS-level ping → reply with pong at the protocol layer
                            if let Err(e) = write.send(
                                tokio_tungstenite::tungstenite::Message::Pong(payload)
                            ).await {
                                tracing::debug!(error = %e, "Failed to send pong");
                                break;
                            }
                        }
                        Some(Err(e)) => {
                            tracing::debug!(error = %e, "WebSocket read error");
                            break;
                        }
                        None => break,
                        _ => {}
                    }
                }
                broadcast = broadcast_rx.recv() => {
                    match broadcast {
                        Ok(payload) => {
                            // Filter to events for this (channel, thread) only.
                            // Payload format: {"type":..., "channel":..., "thread":..., ...}
                            if let Some(filtered) = filter_for_thread(&payload, &channel, &thread)
                                && let Err(e) = write.send(
                                    tokio_tungstenite::tungstenite::Message::Text(filtered)
                                ).await {
                                    tracing::debug!(error = %e, "Failed to forward event");
                                    break;
                                }
                        }
                        Err(broadcast::error::RecvError::Lagged(n)) => {
                            // Send resync event so client re-hydrates via REST
                            let resync = serde_json::json!({
                                "type": "resync",
                                "channel": channel,
                                "thread": thread,
                                "dropped": n,
                            });
                            if let Err(e) = write.send(
                                tokio_tungstenite::tungstenite::Message::Text(resync.to_string())
                            ).await {
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
            thread = %thread,
            "ThreadProxyHandler: dashboard client disconnected"
        );
        Ok(())
    }
}

/// Filter a broadcast payload to only include events for the given
/// `(channel, thread)`. Returns Some(payload) if it matches, None otherwise.
///
/// All payloads on the bus have the shape:
///   {"type": "...", "channel": "...", "thread": "...", ...}
fn filter_for_thread(payload: &str, channel: &str, thread: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(payload).ok()?;
    let p_channel = v.get("channel").and_then(|c| c.as_str())?;
    let p_thread = v.get("thread").and_then(|c| c.as_str())?;
    if p_channel == channel && p_thread == thread {
        Some(payload.to_string())
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filter_matches_channel_and_thread() {
        let payload = r#"{"type":"activity","channel":"c1","thread":"t1","entry":{}}"#;
        assert_eq!(
            filter_for_thread(payload, "c1", "t1").as_deref(),
            Some(payload)
        );
    }

    #[test]
    fn filter_rejects_other_thread() {
        let payload = r#"{"type":"activity","channel":"c1","thread":"t1","entry":{}}"#;
        assert!(filter_for_thread(payload, "c1", "t2").is_none());
        assert!(filter_for_thread(payload, "c2", "t1").is_none());
        assert!(filter_for_thread(payload, "c2", "t2").is_none());
    }

    #[test]
    fn filter_rejects_malformed_payload() {
        assert!(filter_for_thread("not json", "c", "t").is_none());
        assert!(filter_for_thread(r#"{"type":"x"}"#, "c", "t").is_none());
    }
}
