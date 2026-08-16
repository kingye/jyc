//! WebSocket channel inbound adapter and matcher.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use futures_util::{SinkExt, StreamExt};
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

use jyc_types::{
    ChannelMatcher, ChannelPattern, InboundAdapter, InboundAdapterOptions, InboundMessage,
    MessageContent, PatternMatch,
};
use std::sync::Mutex as StdMutex;

/// WebSocket channel-specific pattern matching and topic name derivation.
pub struct WebsocketMatcher {
    channel_name: String,
}

impl WebsocketMatcher {
    /// Create a new websocket matcher.
    pub fn new(channel_name: String) -> Self {
        Self { channel_name }
    }
}

impl ChannelMatcher for WebsocketMatcher {
    fn channel_type(&self) -> &str {
        "websocket"
    }

    fn derive_topic_name(
        &self,
        message: &InboundMessage,
        _patterns: &[ChannelPattern],
        _pattern_match: Option<&PatternMatch>,
    ) -> String {
        // Use the topic name specified by the client (e.g. from the WebSocket
        // protocol's `topic` field). Fall back to the channel name when empty.
        if message.topic.is_empty() {
            self.channel_name.clone()
        } else {
            message.topic.clone()
        }
    }

    fn match_message(
        &self,
        message: &InboundMessage,
        patterns: &[ChannelPattern],
    ) -> Option<PatternMatch> {
        let topic = &message.topic;

        // Pipe hint: a piped message may name the target pattern explicitly
        // (`pipe = { pattern = "..." }`), decoupling the pattern's config
        // from the (possibly dynamic) topic name.
        if let Some(hint) = message
            .metadata
            .get("pipe_pattern")
            .and_then(|v| v.as_str())
        {
            if let Some(p) = patterns.iter().find(|p| p.enabled && p.name == hint) {
                return Some(PatternMatch {
                    pattern_name: p.name.clone(),
                    channel: "websocket".to_string(),
                    matches: HashMap::new(),
                });
            }
            tracing::warn!(
                pattern = %hint,
                topic = %topic,
                "pipe_pattern hint names no enabled pattern, falling back to topic-name matching"
            );
        }

        let pattern_name = if !topic.is_empty() {
            // Prefer the pattern whose name matches the client's topic name.
            // This allows per-topic config like `topic_path` to take effect.
            // If no pattern matches the topic name, treat the topic name itself
            // as the pattern name instead of falling back to an arbitrary enabled
            // pattern, which would leak the wrong pattern to ad-hoc topics.
            // If the channel has no enabled patterns at all, the channel is
            // effectively inactive and we return None.
            patterns
                .iter()
                .find(|p| p.enabled && p.name == *topic)
                .map(|p| p.name.clone())
                .or_else(|| {
                    if patterns.iter().any(|p| p.enabled) {
                        Some(topic.clone())
                    } else {
                        None
                    }
                })?
        } else {
            // For empty topic, fall back to the first enabled pattern.
            patterns.iter().find(|p| p.enabled)?.name.clone()
        };

        Some(PatternMatch {
            pattern_name,
            channel: "websocket".to_string(),
            matches: HashMap::new(),
        })
    }
}

/// Inbound JSON protocol messages from clients.
///
/// The legacy `list_patterns` / `subscribe` / `create_topic` commands
/// have been replaced by REST endpoints on the inspect server
/// (`list_patterns` and `create_topic`). The WebSocket protocol now
/// only carries the live-message stream:
/// - `message`: send a chat message to the bound topic
/// - `disconnect`: close the connection cleanly
/// - `ping`: keep-alive (tokio-tungstenite also handles WS-level pings)
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ClientMessage {
    #[serde(rename = "message")]
    Message {
        /// Optional: when the connection is scoped to a single topic via
        /// `/ws/<channel>/<topic>`, the inspect server propagates the URL
        /// topic name here so the client doesn't need to include it in
        /// the payload. Clients connecting to `/ws/<channel>` (no topic
        /// scope) must still include `topic` here.
        #[serde(default)]
        topic: Option<String>,
        text: String,
        /// Optional sender display name (e.g. a feishu user name). Defaults
        /// to `"user"`.
        #[serde(default)]
        sender: Option<String>,
        /// Optional canonical sender address (e.g. a feishu open_id).
        /// Defaults to the connection address.
        #[serde(default)]
        sender_address: Option<String>,
    },
    /// Close the connection cleanly. The handler breaks the read loop and
    /// the post-loop helper sends a WS Close frame (`inbound.rs:405-407`).
    Disconnect,
    /// Keep-alive ping. tokio-tungstenite already handles WS-level pings
    /// at the protocol layer; this is a no-op for application-level pings.
    Ping,
}

/// WebSocket inbound adapter.
///
type OnMessageCallback = Box<dyn Fn(InboundMessage) -> Result<()> + Send + Sync>;

/// Does NOT run its own TCP listener. Instead, it implements
/// `jyc_inspect::server::WebsocketHandler` and is registered with the inspect
/// server, which shares the same port for both JSON queries and WebSocket
/// upgrades.
pub struct WebsocketInboundAdapter {
    channel_name: String,
    /// Broadcast sender — cloned for each new connection via `subscribe()`.
    broadcast_tx: broadcast::Sender<String>,
    /// Message callback — set during `start()`, used by the WebSocket handler.
    on_message: std::sync::Arc<tokio::sync::Mutex<Option<OnMessageCallback>>>,
    /// Workspace directory for loading chat history (default location).
    workspace_dir: Option<PathBuf>,
    /// TopicManager reference for resolving custom topic_path overrides.
    topic_manager: Arc<StdMutex<Option<Arc<jyc_core::topic_manager::TopicManager>>>>,
    /// Optional inspect-broadcast bus from the inspect server.
    /// When set, events from this bus are forwarded to WebSocket clients
    /// alongside the per-channel `broadcast_tx` events. This enables live
    /// activity / thinking / processing updates for websocket-type channels.
    inspect_broadcast: Option<Arc<broadcast::Sender<String>>>,
}

impl WebsocketInboundAdapter {
    /// Create a new websocket inbound adapter.
    pub fn new(channel_name: String, broadcast_tx: broadcast::Sender<String>) -> Self {
        Self {
            channel_name,
            broadcast_tx,
            on_message: std::sync::Arc::new(tokio::sync::Mutex::new(None)),
            workspace_dir: None,
            topic_manager: Arc::new(StdMutex::new(None)),
            inspect_broadcast: None,
        }
    }

    /// Set the workspace directory for loading chat history.
    pub fn set_workspace_dir(&mut self, dir: PathBuf) {
        self.workspace_dir = Some(dir);
    }

    /// Set the inspect-broadcast bus for live activity/thinking events.
    pub fn set_inspect_broadcast(&mut self, bus: Arc<broadcast::Sender<String>>) {
        self.inspect_broadcast = Some(bus);
    }

    /// Set the TopicManager for resolving custom `topic_path` overrides.
    pub fn set_topic_manager(&self, tm: Arc<jyc_core::topic_manager::TopicManager>) {
        *self.topic_manager.lock().unwrap() = Some(tm);
    }

    /// Return the channel name for this adapter.
    /// Used by the inspect server for path-based handler routing.
    pub fn channel_name(&self) -> &str {
        &self.channel_name
    }
}

#[async_trait::async_trait]
impl jyc_inspect::server::WebsocketHandler for WebsocketInboundAdapter {
    async fn handle(
        &self,
        ws: axum::extract::ws::WebSocket,
        addr: SocketAddr,
        scoped_topic: Option<&str>,
    ) -> anyhow::Result<()> {
        let broadcast_rx = self.broadcast_tx.subscribe();
        let inspect_broadcast_rx = self.inspect_broadcast.as_ref().map(|s| s.subscribe());
        let channel_name = self.channel_name.clone();
        let on_message = self.on_message.clone();

        handle_connection_impl(
            ws,
            addr,
            channel_name,
            broadcast_rx,
            inspect_broadcast_rx,
            on_message,
            scoped_topic,
        )
        .await
    }
}

impl ChannelMatcher for WebsocketInboundAdapter {
    fn channel_type(&self) -> &str {
        "websocket"
    }

    fn derive_topic_name(
        &self,
        message: &InboundMessage,
        patterns: &[ChannelPattern],
        pattern_match: Option<&PatternMatch>,
    ) -> String {
        WebsocketMatcher::new(self.channel_name.clone()).derive_topic_name(
            message,
            patterns,
            pattern_match,
        )
    }

    fn match_message(
        &self,
        message: &InboundMessage,
        patterns: &[ChannelPattern],
    ) -> Option<PatternMatch> {
        WebsocketMatcher::new(self.channel_name.clone()).match_message(message, patterns)
    }
}

#[async_trait]
impl InboundAdapter for WebsocketInboundAdapter {
    async fn start(
        &self,
        options: InboundAdapterOptions,
        _cancel: CancellationToken,
    ) -> Result<()> {
        // Store the on_message callback so the WebsocketHandler can use it.
        let mut guard = self.on_message.lock().await;
        *guard = Some(options.on_message);
        tracing::info!(channel = %self.channel_name, "WebSocket inbound adapter registered (no independent listener)");
        Ok(())
    }
}

/// Per-connection message loop for a websocket-type channel.
///
/// `axum::extract::ws::WebSocket` is what `WebSocketUpgrade::on_upgrade`
/// hands the inspect server, so the inbound adapter accepts that type
/// directly. The loop is equivalent to the previous tungstenite-based
/// version: read text frames and parse `ClientMessage`; forward
/// per-channel broadcast and inspect-broadcast events to the client
/// (filtered for the current channel/topic); handle graceful close.
async fn handle_connection_impl(
    ws: axum::extract::ws::WebSocket,
    addr: SocketAddr,
    channel_name: String,
    mut broadcast_rx: broadcast::Receiver<String>,
    mut inspect_broadcast_rx: Option<broadcast::Receiver<String>>,
    on_message: std::sync::Arc<tokio::sync::Mutex<Option<OnMessageCallback>>>,
    scoped_topic: Option<&str>,
) -> anyhow::Result<()> {
    use axum::extract::ws::Message;

    let (mut ws_tx, mut ws_rx) = ws.split();

    tracing::info!(addr = %addr, channel = %channel_name, "WebSocket client connected");

    loop {
        tokio::select! {
            msg = ws_rx.next() => {
                let msg = match msg {
                    Some(Ok(m)) => m,
                    Some(Err(e)) => {
                        tracing::warn!(error = %e, addr = %addr, "WebSocket receive error");
                        break;
                    }
                    None => {
                        tracing::info!(addr = %addr, "WebSocket client disconnected");
                        break;
                    }
                };

                match msg {
                    Message::Close(_) => {
                        tracing::info!(addr = %addr, "WebSocket client closed connection");
                        break;
                    }
                    Message::Text(text) => {
                        let client_msg: ClientMessage = match serde_json::from_str(&text) {
                            Ok(m) => m,
                            Err(e) => {
                                tracing::warn!(error = %e, text = %text, "Invalid WebSocket message");
                                continue;
                            }
                        };

                        match client_msg {
                            ClientMessage::Message {
                                topic,
                                text,
                                sender,
                                sender_address,
                            } => {
                                // Prefer the payload's `topic` field (an
                                // explicit override); fall back to the
                                // URL-scoped topic for `/ws/<channel>/<topic>`
                                // connections where the payload omits it.
                                let topic_name = match topic
                                    .or_else(|| scoped_topic.map(|s| s.to_string()))
                                {
                                    Some(t) => t,
                                    None => {
                                        tracing::warn!(
                                            channel = %channel_name,
                                            "WebSocket Message without topic; ignoring"
                                        );
                                        continue;
                                    }
                                };
                                let message = InboundMessage {
                                    id: uuid::Uuid::new_v4().to_string(),
                                    channel: channel_name.clone(),
                                    channel_uid: "websocket".to_string(),
                                    sender: sender.unwrap_or_else(|| "user".to_string()),
                                    sender_address: sender_address
                                        .unwrap_or_else(|| addr.to_string()),
                                    recipients: vec![],
                                    topic: topic_name,
                                    content: MessageContent {
                                        text: Some(text),
                                        html: None,
                                        markdown: None,
                                    },
                                    timestamp: chrono::Utc::now(),
                                    references: None,
                                    reply_to_id: None,
                                    external_id: None,
                                    attachments: vec![],
                                    metadata: HashMap::new(),
                                    matched_pattern: None,
                                };

                                let guard = on_message.lock().await;
                                if let Some(ref callback) = *guard {
                                    if let Err(e) = (callback)(message) {
                                        tracing::error!(error = %e, "WebSocket on_message error");
                                    }
                                } else {
                                    tracing::warn!("WebSocket on_message callback not set — message dropped");
                                }
                            }
                            ClientMessage::Disconnect => {
                                tracing::info!(addr = %addr, "WebSocket client requested disconnect");
                                break;
                            }
                            ClientMessage::Ping => {
                                // No-op; axum auto-replies to WS pings at the
                                // protocol layer.
                            }
                        }
                    }
                    // Binary / Ping / Pong frames: axum handles pings/pongs;
                    // binary is not part of this protocol.
                    _ => {}
                }
            }
            broadcast = broadcast_rx.recv() => {
                match broadcast {
                    Ok(payload) => {
                        if let Err(e) = ws_tx.send(Message::Text(payload.into())).await {
                            tracing::warn!(error = %e, addr = %addr, "Failed to send broadcast");
                            break;
                        }
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        tracing::info!(addr = %addr, "Broadcast channel closed");
                        break;
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!(addr = %addr, dropped = %n, "Per-channel broadcast lagged; messages may have been lost");
                    }
                }
            }
            // Inspect-broadcast events: activity/thinking/chat messages from
            // the ActivityTracker. Filter by (channel, topic) and forward
            // to the WebSocket client alongside the per-channel broadcasts.
            inspect = async {
                match &mut inspect_broadcast_rx {
                    Some(rx) => rx.recv().await,
                    None => std::future::pending::<Result<String, broadcast::error::RecvError>>().await,
                }
            } => {
                match inspect {
                    Ok(payload) => {
                        // Only forward events for our channel. If the
                        // connection is topic-scoped, also filter by topic.
                        if should_forward_inspect(&payload, &channel_name, scoped_topic)
                            && let Err(e) = ws_tx.send(Message::Text(payload.into())).await
                        {
                            tracing::warn!(error = %e, addr = %addr, "Failed to send inspect event");
                            break;
                        }
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        // The inspect broadcast was closed — not a fatal error.
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!(addr = %addr, dropped = %n, "Inspect broadcast lagged; events may have been lost");
                    }
                }
            }
        }
    }

    // Best-effort graceful close; the client may have already disconnected.
    let _ = ws_tx.send(Message::Close(None)).await;
    tracing::info!(addr = %addr, "WebSocket connection closed");
    Ok(())
}

/// Decide whether to forward an inspect-broadcast payload to the WebSocket
/// client. Events are forwarded only if:
/// - The JSON payload has a `channel` field matching `channel_name`
/// - AND either:
///   - `scoped_topic` is `None` (all topics for this channel), or
///   - The payload also has a `topic` field matching `scoped_topic`
fn should_forward_inspect(payload: &str, channel_name: &str, scoped_topic: Option<&str>) -> bool {
    let v: serde_json::Value = match serde_json::from_str(payload) {
        Ok(v) => v,
        Err(_) => return false,
    };
    let p_channel = match v.get("channel").and_then(|c| c.as_str()) {
        Some(c) => c,
        None => return false,
    };
    if p_channel != channel_name {
        return false;
    }
    // If topic-scoped and the payload specifies a topic, reject mismatches.
    if let Some(st) = scoped_topic
        && let Some(p_topic) = v.get("topic").and_then(|t| t.as_str())
        && p_topic != st
    {
        return false;
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn create_test_message() -> InboundMessage {
        InboundMessage {
            id: "test".to_string(),
            channel: "websocket".to_string(),
            channel_uid: "user".to_string(),
            sender: "user".to_string(),
            sender_address: "user".to_string(),
            recipients: vec![],
            topic: "Test".to_string(),
            content: MessageContent {
                text: Some("hello".to_string()),
                html: None,
                markdown: None,
            },
            timestamp: chrono::Utc::now(),
            references: None,
            reply_to_id: None,
            external_id: None,
            attachments: vec![],
            metadata: HashMap::new(),
            matched_pattern: None,
        }
    }

    #[test]
    fn test_derive_topic_name_uses_topic() {
        let matcher = WebsocketMatcher::new("my-ws".to_string());
        let msg = create_test_message();
        let name = matcher.derive_topic_name(&msg, &[], None);
        assert_eq!(name, "Test");
    }

    #[test]
    fn test_derive_topic_name_empty_topic_fallback() {
        let matcher = WebsocketMatcher::new("my-ws".to_string());
        let mut msg = create_test_message();
        msg.topic = String::new();
        let name = matcher.derive_topic_name(&msg, &[], None);
        assert_eq!(name, "my-ws");
    }

    #[test]
    fn test_match_message_by_topic_name() {
        let matcher = WebsocketMatcher::new("my-ws".to_string());
        let mut msg = create_test_message();
        msg.topic = "my-project".to_string();

        let patterns = vec![
            ChannelPattern {
                name: "default".to_string(),
                channel: "websocket".to_string(),
                enabled: true,
                ..Default::default()
            },
            ChannelPattern {
                name: "my-project".to_string(),
                channel: "websocket".to_string(),
                enabled: true,
                ..Default::default()
            },
        ];

        // Should match "my-project" by name, not "default" (first enabled)
        let result = matcher.match_message(&msg, &patterns);
        assert!(result.is_some());
        assert_eq!(result.unwrap().pattern_name, "my-project");
    }

    #[test]
    fn test_match_message_by_topic_name_skips_disabled() {
        let matcher = WebsocketMatcher::new("my-ws".to_string());
        let mut msg = create_test_message();
        msg.topic = "my-project".to_string();

        let patterns = vec![
            ChannelPattern {
                name: "my-project".to_string(),
                channel: "websocket".to_string(),
                enabled: false,
                ..Default::default()
            },
            ChannelPattern {
                name: "fallback".to_string(),
                channel: "websocket".to_string(),
                enabled: true,
                ..Default::default()
            },
        ];

        // Name match is disabled, so use the topic itself as the pattern name
        // rather than falling back to an arbitrary enabled pattern.
        let result = matcher.match_message(&msg, &patterns);
        assert!(result.is_some());
        assert_eq!(result.unwrap().pattern_name, "my-project");
    }

    #[test]
    fn test_match_message_uses_topic_when_no_name_match() {
        let matcher = WebsocketMatcher::new("my-ws".to_string());
        let msg = create_test_message(); // topic = "Test", no pattern named "Test"

        let patterns = vec![
            ChannelPattern {
                name: "p1".to_string(),
                channel: "websocket".to_string(),
                enabled: true,
                ..Default::default()
            },
            ChannelPattern {
                name: "p2".to_string(),
                channel: "websocket".to_string(),
                enabled: true,
                ..Default::default()
            },
        ];

        // No name match, use the topic itself as the pattern name so ad-hoc
        // topics do not inherit an unrelated pattern.
        let result = matcher.match_message(&msg, &patterns);
        assert!(result.is_some());
        assert_eq!(result.unwrap().pattern_name, "Test");
    }

    #[test]
    fn test_match_message_adhoc_topic_does_not_inherit_other_pattern() {
        let matcher = WebsocketMatcher::new("local_dev".to_string());
        let mut msg = create_test_message();
        msg.topic = "adhoc".to_string();

        let patterns = vec![
            ChannelPattern {
                name: "jin".to_string(),
                channel: "websocket".to_string(),
                enabled: true,
                ..Default::default()
            },
            ChannelPattern {
                name: "jyc".to_string(),
                channel: "websocket".to_string(),
                enabled: true,
                ..Default::default()
            },
        ];

        let result = matcher.match_message(&msg, &patterns);
        assert!(result.is_some());
        assert_eq!(result.unwrap().pattern_name, "adhoc");
    }

    /// A `pipe_pattern` metadata hint forces the named pattern even when the
    /// (dynamic) topic name matches nothing.
    #[test]
    fn test_match_message_pipe_pattern_hint() {
        let matcher = WebsocketMatcher::new("local_dev".to_string());
        let mut msg = create_test_message();
        msg.topic = "dev-jyc".to_string(); // dynamic topic, no such pattern
        msg.metadata
            .insert("pipe_pattern".to_string(), serde_json::json!("group_chat"));

        let patterns = vec![ChannelPattern {
            name: "group_chat".to_string(),
            channel: "websocket".to_string(),
            enabled: true,
            ..Default::default()
        }];

        let result = matcher.match_message(&msg, &patterns);
        assert_eq!(result.unwrap().pattern_name, "group_chat");
    }

    /// Hint naming a nonexistent/disabled pattern: warn + fall back to
    /// topic-name matching.
    #[test]
    fn test_match_message_pipe_pattern_hint_fallback() {
        let matcher = WebsocketMatcher::new("local_dev".to_string());
        let mut msg = create_test_message();
        msg.topic = "adhoc".to_string();
        msg.metadata.insert(
            "pipe_pattern".to_string(),
            serde_json::json!("typo_pattern"),
        );

        let patterns = vec![ChannelPattern {
            name: "group_chat".to_string(),
            channel: "websocket".to_string(),
            enabled: true,
            ..Default::default()
        }];

        let result = matcher.match_message(&msg, &patterns);
        assert_eq!(result.unwrap().pattern_name, "adhoc");
    }

    #[test]
    fn test_match_message_empty_topic_falls_back_to_first_enabled() {
        let matcher = WebsocketMatcher::new("my-ws".to_string());
        let mut msg = create_test_message();
        msg.topic = String::new();

        let patterns = vec![
            ChannelPattern {
                name: "p1".to_string(),
                channel: "websocket".to_string(),
                enabled: true,
                ..Default::default()
            },
            ChannelPattern {
                name: "p2".to_string(),
                channel: "websocket".to_string(),
                enabled: true,
                ..Default::default()
            },
        ];

        let result = matcher.match_message(&msg, &patterns);
        assert!(result.is_some());
        assert_eq!(result.unwrap().pattern_name, "p1");
    }

    #[test]
    fn test_match_message_none_when_all_disabled() {
        let matcher = WebsocketMatcher::new("my-ws".to_string());
        let msg = create_test_message();

        let patterns = vec![ChannelPattern {
            name: "p1".to_string(),
            channel: "websocket".to_string(),
            enabled: false,
            ..Default::default()
        }];

        let result = matcher.match_message(&msg, &patterns);
        assert!(result.is_none());
    }

    #[test]
    fn should_forward_inspect_for_matching_channel() {
        let payload = r#"{"type":"activity","channel":"ws1","topic":"t1","entry":{}}"#;
        assert!(should_forward_inspect(payload, "ws1", None));
        assert!(should_forward_inspect(payload, "ws1", Some("t1")));
    }

    #[test]
    fn should_reject_inspect_wrong_channel() {
        let payload = r#"{"type":"activity","channel":"ws1","topic":"t1","entry":{}}"#;
        assert!(!should_forward_inspect(payload, "ws2", None));
        assert!(!should_forward_inspect(payload, "ws2", Some("t1")));
    }

    #[test]
    fn should_reject_inspect_wrong_topic_when_scoped() {
        let payload = r#"{"type":"activity","channel":"ws1","topic":"t1","entry":{}}"#;
        assert!(!should_forward_inspect(payload, "ws1", Some("t2")));
        // Without scope, same payload should pass
        assert!(should_forward_inspect(payload, "ws1", None));
    }

    #[test]
    fn should_reject_inspect_malformed_json() {
        assert!(!should_forward_inspect("not json", "ws", None));
        assert!(!should_forward_inspect(r#"{"no":"channel"}"#, "ws", None));
    }

    #[test]
    fn test_client_message_parses_sender_fields() {
        let msg: ClientMessage = serde_json::from_str(
            r#"{"type":"message","topic":"t1","text":"hi","sender":"张三","sender_address":"ou_abc"}"#,
        )
        .unwrap();
        match msg {
            ClientMessage::Message {
                topic,
                text,
                sender,
                sender_address,
            } => {
                assert_eq!(topic.as_deref(), Some("t1"));
                assert_eq!(text, "hi");
                assert_eq!(sender.as_deref(), Some("张三"));
                assert_eq!(sender_address.as_deref(), Some("ou_abc"));
            }
            _ => panic!("expected Message"),
        }
    }

    #[test]
    fn test_client_message_sender_fields_optional() {
        // The dashboard chat pane sends the minimal frame — sender fields
        // must stay optional for backward compatibility.
        let msg: ClientMessage =
            serde_json::from_str(r#"{"type":"message","topic":"t1","text":"hi"}"#).unwrap();
        match msg {
            ClientMessage::Message {
                sender,
                sender_address,
                ..
            } => {
                assert!(sender.is_none());
                assert!(sender_address.is_none());
            }
            _ => panic!("expected Message"),
        }
    }
}
