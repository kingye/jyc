//! WebSocket channel inbound adapter and matcher.

use std::collections::HashMap;
use std::net::SocketAddr;

use anyhow::{Context, Result};
use async_trait::async_trait;
use futures_util::{SinkExt, StreamExt};
use tokio::net::TcpListener;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;
use tokio_tungstenite::accept_async;

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
/// Runs a TCP listener that accepts WebSocket connections from dashboard clients.
/// Handles the JSON protocol and dispatches messages to the agent via `on_message`.
pub struct WebsocketInboundAdapter {
    channel_name: String,
    bind: String,
    patterns: Vec<ChannelPattern>,
    /// Broadcast receiver for outbound replies — each connection subscribes to this.
    broadcast_rx: tokio::sync::Mutex<Option<broadcast::Receiver<String>>>,
}

impl WebsocketInboundAdapter {
    /// Create a new websocket inbound adapter.
    pub fn new(
        channel_name: String,
        bind: String,
        patterns: Vec<ChannelPattern>,
        broadcast_tx: broadcast::Sender<String>,
    ) -> Self {
        Self {
            channel_name,
            bind,
            patterns,
            broadcast_rx: tokio::sync::Mutex::new(Some(broadcast_tx.subscribe())),
        }
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
    async fn start(&self, options: InboundAdapterOptions, cancel: CancellationToken) -> Result<()> {
        let listener = TcpListener::bind(&self.bind)
            .await
            .with_context(|| format!("failed to bind WebSocket server to {}", self.bind))?;
        tracing::info!(
            channel = %self.channel_name,
            bind = %self.bind,
            "WebSocket server listening"
        );

        let pattern_names: Vec<String> = self
            .patterns
            .iter()
            .filter(|p| p.enabled)
            .map(|p| p.name.clone())
            .collect();

        // Wrap on_message in Arc so it can be shared across connections
        let on_message = std::sync::Arc::new(options.on_message);

        loop {
            tokio::select! {
                accept_result = listener.accept() => {
                    let (stream, addr) = match accept_result {
                        Ok(v) => v,
                        Err(e) => {
                            tracing::warn!(error = %e, "WebSocket accept error");
                            continue;
                        }
                    };

                    let ws_stream = match accept_async(stream).await {
                        Ok(ws) => ws,
                        Err(e) => {
                            tracing::warn!(error = %e, addr = %addr, "WebSocket handshake failed");
                            continue;
                        }
                    };

                    let broadcast_rx = {
                        let mut guard = self.broadcast_rx.lock().await;
                        guard.take()
                    };

                    // If no receiver available, create a new one from a dummy sender
                    let broadcast_rx = broadcast_rx.unwrap_or_else(|| {
                        // This should not happen in normal operation, but handle gracefully
                        let (tx, _rx) = broadcast::channel(1);
                        tx.subscribe()
                    });

                    let channel_name = self.channel_name.clone();
                    let pattern_names = pattern_names.clone();
                    let on_message = on_message.clone();
                    let cancel_conn = cancel.clone();

                    tokio::spawn(async move {
                        if let Err(e) = handle_connection(
                            ws_stream,
                            addr,
                            channel_name,
                            pattern_names,
                            on_message,
                            broadcast_rx,
                            cancel_conn,
                        )
                        .await
                        {
                            tracing::warn!(error = %e, addr = %addr, "WebSocket connection error");
                        }
                    });
                }
                _ = cancel.cancelled() => {
                    tracing::info!(channel = %self.channel_name, "WebSocket server shutting down");
                    break;
                }
            }
        }

        Ok(())
    }
}

async fn handle_connection(
    ws_stream: tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>,
    addr: SocketAddr,
    channel_name: String,
    pattern_names: Vec<String>,
    on_message: std::sync::Arc<Box<dyn Fn(InboundMessage) -> Result<()> + Send + Sync>>,
    mut broadcast_rx: broadcast::Receiver<String>,
    cancel: CancellationToken,
) -> Result<()> {
    let (mut ws_tx, mut ws_rx) = ws_stream.split();
    let mut _subscribed_thread: Option<String> = None;

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
                        _subscribed_thread = Some(thread.clone());
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

                        if let Err(e) = (on_message)(message) {
                            tracing::error!(error = %e, "WebSocket on_message error");
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
            _ = cancel.cancelled() => {
                tracing::info!(addr = %addr, "Connection cancelled");
                break;
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
