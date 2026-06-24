//! WebSocket channel inbound adapter and matcher.

use std::collections::HashMap;
use std::net::SocketAddr;

use anyhow::Result;
use async_trait::async_trait;
use futures_util::{SinkExt, StreamExt};
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

use jyc_types::{
    ChannelMatcher, ChannelPattern, InboundAdapter, InboundAdapterOptions, InboundMessage,
    MessageContent, PatternMatch,
};

/// WebSocket channel-specific pattern matching and thread name derivation.
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

    fn derive_thread_name(
        &self,
        _message: &InboundMessage,
        _patterns: &[ChannelPattern],
        _pattern_match: Option<&PatternMatch>,
    ) -> String {
        // Each websocket channel has exactly one thread named after the channel.
        self.channel_name.clone()
    }

    fn match_message(
        &self,
        _message: &InboundMessage,
        patterns: &[ChannelPattern],
    ) -> Option<PatternMatch> {
        // Websocket input is always for this channel — match the first enabled pattern.
        patterns.iter().find(|p| p.enabled).map(|p| PatternMatch {
            pattern_name: p.name.clone(),
            channel: "websocket".to_string(),
            matches: HashMap::new(),
        })
    }
}

/// Client-bound JSON protocol messages.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(tag = "type")]
enum ServerMessage {
    #[serde(rename = "patterns")]
    Patterns { patterns: Vec<String> },
}

/// Inbound JSON protocol messages from clients.
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(tag = "type")]
enum ClientMessage {
    #[serde(rename = "list_patterns")]
    ListPatterns,
    #[serde(rename = "subscribe")]
    Subscribe { thread: String },
    #[serde(rename = "message")]
    Message { thread: String, text: String },
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
    patterns: Vec<ChannelPattern>,
    /// Broadcast sender — cloned for each new connection via `subscribe()`.
    broadcast_tx: broadcast::Sender<String>,
    /// Message callback — set during `start()`, used by the WebSocket handler.
    on_message: std::sync::Arc<tokio::sync::Mutex<Option<OnMessageCallback>>>,
}

impl WebsocketInboundAdapter {
    /// Create a new websocket inbound adapter.
    pub fn new(
        channel_name: String,
        patterns: Vec<ChannelPattern>,
        broadcast_tx: broadcast::Sender<String>,
    ) -> Self {
        Self {
            channel_name,
            patterns,
            broadcast_tx,
            on_message: std::sync::Arc::new(tokio::sync::Mutex::new(None)),
        }
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
        ws_stream: tokio_tungstenite::WebSocketStream<jyc_inspect::server::PrependStream>,
        addr: SocketAddr,
    ) -> anyhow::Result<()> {
        let pattern_names: Vec<String> = self
            .patterns
            .iter()
            .filter(|p| p.enabled)
            .map(|p| p.name.clone())
            .collect();

        let broadcast_rx = self.broadcast_tx.subscribe();
        let channel_name = self.channel_name.clone();
        let on_message = self.on_message.clone();

        handle_connection_impl(
            ws_stream,
            addr,
            channel_name,
            pattern_names,
            broadcast_rx,
            on_message,
        )
        .await
    }
}

impl ChannelMatcher for WebsocketInboundAdapter {
    fn channel_type(&self) -> &str {
        "websocket"
    }

    fn derive_thread_name(
        &self,
        message: &InboundMessage,
        patterns: &[ChannelPattern],
        pattern_match: Option<&PatternMatch>,
    ) -> String {
        WebsocketMatcher::new(self.channel_name.clone()).derive_thread_name(
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

async fn handle_connection_impl<S>(
    ws_stream: tokio_tungstenite::WebSocketStream<S>,
    addr: SocketAddr,
    channel_name: String,
    pattern_names: Vec<String>,
    mut broadcast_rx: broadcast::Receiver<String>,
    on_message: std::sync::Arc<tokio::sync::Mutex<Option<OnMessageCallback>>>,
) -> anyhow::Result<()>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + Sync + 'static,
{
    let (mut ws_tx, mut ws_rx) = ws_stream.split();

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

                if msg.is_close() {
                    tracing::info!(addr = %addr, "WebSocket client closed connection");
                    break;
                }

                let text = match msg.to_text() {
                    Ok(t) => t,
                    Err(_) => continue, // ignore binary frames
                };

                let client_msg: ClientMessage = match serde_json::from_str(text) {
                    Ok(m) => m,
                    Err(e) => {
                        tracing::warn!(error = %e, text = %text, "Invalid WebSocket message");
                        continue;
                    }
                };

                match client_msg {
                    ClientMessage::ListPatterns => {
                        let response = ServerMessage::Patterns {
                            patterns: pattern_names.clone(),
                        };
                        let json = serde_json::to_string(&response)?;
                        if let Err(e) = ws_tx.send(tokio_tungstenite::tungstenite::Message::Text(json)).await {
                            tracing::warn!(error = %e, addr = %addr, "Failed to send patterns");
                            break;
                        }
                    }
                    ClientMessage::Subscribe { thread } => {
                        tracing::info!(addr = %addr, thread = %thread, "Client subscribed to thread");
                    }
                    ClientMessage::Message { thread, text } => {
                        let message = InboundMessage {
                            id: uuid::Uuid::new_v4().to_string(),
                            channel: channel_name.clone(),
                            channel_uid: "websocket".to_string(),
                            sender: "user".to_string(),
                            sender_address: addr.to_string(),
                            recipients: vec![],
                            topic: thread.clone(),
                            content: MessageContent {
                                text: Some(text),
                                html: None,
                                markdown: None,
                            },
                            timestamp: chrono::Utc::now(),
                            thread_refs: None,
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
                }
            }
            broadcast = broadcast_rx.recv() => {
                match broadcast {
                    Ok(payload) => {
                        if let Err(e) = ws_tx.send(tokio_tungstenite::tungstenite::Message::Text(payload)).await {
                            tracing::warn!(error = %e, addr = %addr, "Failed to send broadcast");
                            break;
                        }
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        tracing::info!(addr = %addr, "Broadcast channel closed");
                        break;
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        // Client is slow, just continue
                    }
                }
            }
        }
    }

    let _ = ws_tx
        .send(tokio_tungstenite::tungstenite::Message::Close(None))
        .await;
    tracing::info!(addr = %addr, "WebSocket connection closed");
    Ok(())
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
            thread_refs: None,
            reply_to_id: None,
            external_id: None,
            attachments: vec![],
            metadata: HashMap::new(),
            matched_pattern: None,
        }
    }

    #[test]
    fn test_derive_thread_name() {
        let matcher = WebsocketMatcher::new("my-ws".to_string());
        let msg = create_test_message();
        let name = matcher.derive_thread_name(&msg, &[], None);
        assert_eq!(name, "my-ws");
    }

    #[test]
    fn test_match_message_first_enabled() {
        let matcher = WebsocketMatcher::new("my-ws".to_string());
        let msg = create_test_message();

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
                enabled: false,
                ..Default::default()
            },
        ];

        let result = matcher.match_message(&msg, &patterns);
        assert!(result.is_some());
        assert_eq!(result.unwrap().pattern_name, "p1");
    }

    #[test]
    fn test_match_message_skips_disabled() {
        let matcher = WebsocketMatcher::new("my-ws".to_string());
        let msg = create_test_message();

        let patterns = vec![
            ChannelPattern {
                name: "p1".to_string(),
                channel: "websocket".to_string(),
                enabled: false,
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
        assert_eq!(result.unwrap().pattern_name, "p2");
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
}
