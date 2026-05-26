//! WebSocket connection handler for WeChat OpenILink Bridge.
//!
//! Manages the lifecycle of a single WebSocket connection:
//! connect → receive events → parse JSON → convert to InboundMessage → callback.
//! Also provides a sender channel for outbound messages on the same connection.
//!
//! Auto-reconnect with exponential backoff and CancellationToken support.

use anyhow::{Context, Result};
use futures_util::{SinkExt, StreamExt};
use std::collections::HashMap;
use tokio::sync::mpsc;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;
use tokio_util::sync::CancellationToken;

use jyc_types::{InboundMessage, MessageContent};

/// WebSocket connection handler for WeChat OpenILink Bridge.
///
/// Manages a single WebSocket connection that both receives and sends messages.
/// The `sender` field is exposed to the outbound adapter for sending replies.
pub struct WechatWebSocket {
    /// Hostname of the OpenILink server (e.g., "openilink.example.com")
    base_url: String,
    /// Access token
    token: String,
    /// Sender half of the outbound channel — clones can be shared with `WechatOutboundAdapter`
    sender: mpsc::UnboundedSender<String>,
    /// Receiver half of the outbound channel
    outbound_rx: Option<mpsc::UnboundedReceiver<String>>,
}

impl WechatWebSocket {
    /// Create a new WeChat WebSocket handler.
    ///
    /// Returns the handler and a sender handle that can be cloned and shared
    /// with the `WechatOutboundAdapter`.
    pub fn new(base_url: &str, token: &str) -> Self {
        let (tx, rx) = mpsc::unbounded_channel::<String>();
        Self {
            base_url: base_url.to_string(),
            token: token.to_string(),
            sender: tx,
            outbound_rx: Some(rx),
        }
    }

    /// Create a new WeChat WebSocket handler with custom reconnect settings.
    /// Reconnect parameters are stored in the adapter, not on the WS instance,
    /// since a fresh WS is created on each connection attempt.
    #[allow(unused_variables)]
    pub fn new_with_config(
        base_url: &str,
        token: &str,
        max_reconnect_attempts: usize,
        reconnect_delay_secs: u64,
    ) -> Self {
        Self::new(base_url, token)
    }

    /// Get a clone of the sender for outbound messages.
    ///
    /// This can be shared with `WechatOutboundAdapter` so both inbound and
    /// outbound use the same WebSocket connection.
    pub fn sender(&self) -> mpsc::UnboundedSender<String> {
        self.sender.clone()
    }

    /// Build the WebSocket URL.
    fn ws_url(&self) -> String {
        format!("wss://{}/bot/v1/ws?token={}", self.base_url, self.token)
    }

    /// Run the WebSocket event loop.
    ///
    /// Connects to the OpenILink WebSocket, listens for incoming messages,
    /// parses them as JSON, extracts the `content` field, and calls the
    /// `on_message` callback. Simultaneously listens on the outbound channel
    /// and sends messages through the WebSocket.
    ///
    /// Blocks until the cancellation token fires or the connection drops.
    /// Returns `Ok(())` on clean cancellation, `Err(...)` on connection failure.
    pub async fn run(
        &mut self,
        channel_name: &str,
        on_message: &(dyn Fn(InboundMessage) -> Result<()> + Send + Sync),
        cancel: &CancellationToken,
    ) -> Result<()> {
        let ws_url = self.ws_url();
        let masked_url = format!("wss://{}/bot/v1/ws?token=***", self.base_url);
        tracing::info!(url = %masked_url, "Connecting to WeChat OpenILink WebSocket...");

        let (ws_stream, _) = connect_async(&ws_url)
            .await
            .context("Failed to connect to WeChat OpenILink WebSocket")?;

        let (mut write, mut read) = ws_stream.split();

        tracing::info!("WeChat WebSocket connected");

        // Take the outbound receiver
        let mut outbound_rx = self.outbound_rx
            .take()
            .expect("WechatWebSocket::run called more than once");

        // Event loop: handle both incoming messages and outbound sends
        loop {
            tokio::select! {
                // Incoming message from WebSocket
                msg = read.next() => {
                    match msg {
                        Some(Ok(Message::Text(text))) => {
                            if let Err(e) = self.handle_incoming(channel_name, &text, on_message).await {
                                tracing::warn!(error = %format!("{:#}", e), "Failed to process WeChat message");
                            }
                        }
                        Some(Ok(Message::Ping(_))) => {
                            // Tungstenite handles pong automatically
                        }
                        Some(Ok(Message::Close(frame))) => {
                            tracing::info!(?frame, "WeChat WebSocket closed by server");
                            break;
                        }
                        Some(Ok(_)) => {
                            // Binary, Pong frames: ignore
                        }
                        Some(Err(e)) => {
                            tracing::error!(error = %format!("{:#}", e), "WeChat WebSocket read error");
                            break;
                        }
                        None => {
                            // Stream ended
                            tracing::warn!("WeChat WebSocket read stream ended");
                            break;
                        }
                    }
                }

                // Outbound message to send
                Some(outbound_msg) = outbound_rx.recv() => {
                    if let Err(e) = write.send(Message::Text(outbound_msg.into())).await {
                        tracing::error!(error = %format!("{:#}", e), "Failed to send WeChat outbound message");
                        break;
                    }
                }

                // Cancellation
                _ = cancel.cancelled() => {
                    tracing::info!("WeChat WebSocket cancelled");
                    return Ok(());
                }
            }
        }

        Err(anyhow::anyhow!("WeChat WebSocket connection closed unexpectedly"))
    }

    /// Handle an incoming text message from the WebSocket.
    ///
    /// Parses the JSON payload, extracts the `content` field, builds an
    /// `InboundMessage`, and calls the `on_message` callback.
    async fn handle_incoming(
        &self,
        channel_name: &str,
        text: &str,
        on_message: &(dyn Fn(InboundMessage) -> Result<()> + Send + Sync),
    ) -> Result<()> {
        let json: serde_json::Value = match serde_json::from_str(text) {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(error = %format!("{:#}", e), payload = %text, "Failed to parse WeChat message as JSON");
                return Err(anyhow::anyhow!("Failed to parse WeChat message as JSON: {}", e));
            }
        };

        // Diagnostic: log the raw payload and its top-level keys so we can
        // see the exact shape the OpenILink Bridge sends. Needed because
        // the current parser extracts `content`, `sender`, `sender_name`,
        // etc. from flat top-level fields and produces empty strings — the
        // real schema is something else (nested? differently named? keyed
        // by event type?) and we want one capture to know what to parse.
        //
        // Truncates the raw payload at 2000 chars to bound log size on the
        // off chance OpenILink ever sends large media metadata.
        if tracing::enabled!(tracing::Level::DEBUG) {
            let preview: String = if text.len() > 2000 {
                format!("{}…(truncated, {} bytes total)", &text[..2000], text.len())
            } else {
                text.to_string()
            };
            let top_keys: Vec<&str> = json
                .as_object()
                .map(|obj| obj.keys().map(|k| k.as_str()).collect())
                .unwrap_or_default();
            tracing::debug!(
                raw_payload = %preview,
                top_level_keys = ?top_keys,
                "WeChat raw payload received"
            );
        }

        // Extract the content field
        let content = json
            .get("content")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        // Extract sender info
        let sender = json
            .get("sender")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown")
            .to_string();

        let sender_name = json
            .get("sender_name")
            .and_then(|v| v.as_str())
            .unwrap_or(&sender)
            .to_string();

        // Extract message ID
        let msg_id = json
            .get("id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        let msg_id_display = if msg_id.is_empty() {
            uuid::Uuid::new_v4().to_string()
        } else {
            msg_id.clone()
        };

        // Extract message type
        let msg_type = json
            .get("type")
            .and_then(|v| v.as_str())
            .unwrap_or("text")
            .to_string();

        // Build metadata
        let mut metadata = HashMap::new();
        if !msg_id.is_empty() {
            metadata.insert("msg_id".to_string(), serde_json::Value::String(msg_id));
        }
        metadata.insert("msg_type".to_string(), serde_json::Value::String(msg_type));
        metadata.insert("sender".to_string(), serde_json::Value::String(sender.clone()));

        tracing::debug!(
            sender = %sender,
            content_len = content.len(),
            "WeChat message received"
        );

        let message = InboundMessage {
            id: uuid::Uuid::new_v4().to_string(),
            channel: channel_name.to_string(),
            channel_uid: msg_id_display,
            sender: sender_name,
            sender_address: sender,
            recipients: vec![],
            topic: String::new(),
            content: MessageContent {
                text: Some(content),
                html: None,
                markdown: None,
            },
            timestamp: chrono::Utc::now(),
            thread_refs: None,
            reply_to_id: None,
            external_id: None,
            attachments: vec![],
            metadata,
            matched_pattern: None,
        };

        on_message(message)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ws_url_format() {
        let ws = WechatWebSocket::new("openilink.example.com", "test_token");
        let url = ws.ws_url();
        assert_eq!(url, "wss://openilink.example.com/bot/v1/ws?token=test_token");
    }

    #[test]
    fn test_new_with_config() {
        let ws = WechatWebSocket::new_with_config("example.com", "token", 5, 3);
        // new_with_config delegates to new(), ignoring reconnect params
        // since reconnect tracking is in the adapter, not the WS instance
        assert!(ws.sender().send("test".to_string()).is_ok());
    }

    #[test]
    fn test_sender_clone() {
        let ws = WechatWebSocket::new("example.com", "token");
        let sender1 = ws.sender();
        let sender2 = ws.sender();
        // Both senders should be able to send
        sender1.send("test1".to_string()).ok();
        sender2.send("test2".to_string()).ok();
    }


    /// Test incoming JSON message format parsing
    #[test]
    fn test_incoming_message_json_format() {
        let json = r#"{
            "id": "msg_001",
            "type": "text",
            "content": "Hello, this is a test message",
            "sender": "wx_user_123",
            "sender_name": "张三",
            "timestamp": 1234567890
        }"#;

        let parsed: serde_json::Value = serde_json::from_str(json).unwrap();
        assert_eq!(parsed["id"], "msg_001");
        assert_eq!(parsed["type"], "text");
        assert_eq!(parsed["content"], "Hello, this is a test message");
        assert_eq!(parsed["sender"], "wx_user_123");
        assert_eq!(parsed["sender_name"], "张三");
    }

    /// Test minimal incoming JSON message (only required fields)
    #[test]
    fn test_incoming_message_minimal() {
        let json = r#"{
            "content": "Hello"
        }"#;

        let parsed: serde_json::Value = serde_json::from_str(json).unwrap();
        assert_eq!(parsed["content"], "Hello");
        assert!(parsed.get("id").is_none());
        assert!(parsed.get("sender").is_none());
    }

    /// Test outgoing message JSON format
    #[test]
    fn test_outgoing_message_json_format() {
        let json = serde_json::json!({
            "type": "send",
            "content": "AI reply message"
        });

        assert_eq!(json["type"], "send");
        assert_eq!(json["content"], "AI reply message");
    }

    /// Test outgoing message with Unicode content
    #[test]
    fn test_outgoing_message_unicode() {
        let json = serde_json::json!({
            "type": "send",
            "content": "你好，有什么可以帮助你的？"
        });

        assert_eq!(json["type"], "send");
        assert_eq!(json["content"], "你好，有什么可以帮助你的？");
    }

    /// Test that sender is properly extracted when sender_name is absent
    #[test]
    fn test_sender_fallback_to_name() {
        let json = r#"{
            "content": "test",
            "sender": "wx_user_456"
        }"#;

        let parsed: serde_json::Value = serde_json::from_str(json).unwrap();
        let sender = parsed.get("sender").and_then(|v| v.as_str()).unwrap_or("unknown");
        let sender_name = parsed.get("sender_name").and_then(|v| v.as_str()).unwrap_or(sender);
        assert_eq!(sender, "wx_user_456");
        assert_eq!(sender_name, "wx_user_456"); // Falls back to sender
    }

    /// Documents the rendering contract relied upon by every
    /// `tracing::error!(error = %format!("{:#}", e), ...)` site in this
    /// module: anyhow's alternate-Display (`{:#}`) renders the entire
    /// `.context(...)` chain on a single line, joined by ": ", so the
    /// outermost message AND the underlying cause both reach the log.
    ///
    /// Regression for the May 26 wechat_me incident, where a misconfigured
    /// WebSocket URL surfaced only as `error=Failed to connect to WeChat
    /// OpenILink WebSocket` — the wrapped tungstenite cause was dropped
    /// because the log site used plain `%e` (Display), not `{:#}`.
    #[test]
    fn anyhow_alternate_display_renders_full_context_chain() {
        // Inner: simulate a tungstenite-style transport error.
        fn inner() -> anyhow::Result<()> {
            Err(anyhow::anyhow!(
                "WebSocket protocol error: Handshake failed: HTTP 401"
            ))
        }
        // Outer: wrap with context, the way `connect_async` does at
        // `websocket.rs::run`'s call site.
        let outer = inner()
            .context("Failed to connect to WeChat OpenILink WebSocket")
            .unwrap_err();

        // Plain Display only shows the outermost message — this was the bug.
        let plain = format!("{}", outer);
        assert_eq!(plain, "Failed to connect to WeChat OpenILink WebSocket");

        // Alternate Display walks the whole chain.
        let chained = format!("{:#}", outer);
        assert!(
            chained.contains("Failed to connect to WeChat OpenILink WebSocket"),
            "outer message must appear, got: {chained}"
        );
        assert!(
            chained.contains("WebSocket protocol error"),
            "underlying cause must appear, got: {chained}"
        );
        assert!(
            chained.contains("HTTP 401"),
            "deepest cause must appear, got: {chained}"
        );
    }
}
