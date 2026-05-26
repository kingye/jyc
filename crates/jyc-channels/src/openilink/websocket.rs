//! WebSocket connection handler for OpeniLink real-time message receiving.
//!
//! This module manages the WebSocket connection to the OpeniLink Hub,
//! parsing incoming JSON messages, filtering bot's own messages,
//! and delivering them through a callback.

use anyhow::{Context, Result};
use futures_util::{SinkExt, StreamExt};
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio_tungstenite::{
    connect_async,
    tungstenite::Message,
};
use tokio_util::sync::CancellationToken;

use jyc_types::{InboundMessage, MessageContent};
use jyc_types::OpenilinkConfig;

use super::types::WeixinMessage;

/// WebSocket connection handler for OpeniLink.
///
/// Manages the lifecycle of a WebSocket connection to the Hub:
/// connect → receive messages → parse → convert to InboundMessage → callback.
///
/// Supports:
/// - Exponential backoff reconnection
/// - Heartbeat (periodic ping)
/// - Graceful shutdown via CancellationToken
pub struct OpenilinkWebSocket {
    config: OpenilinkConfig,
    /// Current reconnection attempt count
    reconnect_count: usize,
}

impl OpenilinkWebSocket {
    /// Create a new OpeniLink WebSocket handler.
    pub fn new(config: &OpenilinkConfig) -> Self {
        Self {
            config: config.clone(),
            reconnect_count: 0,
        }
    }

    /// Get the WebSocket URL with authentication query parameter.
    fn ws_url(&self) -> String {
        let base = self.config.hub_url.trim_end_matches('/');
        // Strip https:// prefix and replace with wss://
        let ws_base = if base.starts_with("https://") {
            base.replacen("https://", "wss://", 1)
        } else if base.starts_with("http://") {
            base.replacen("http://", "wss://", 1)
        } else {
            format!("wss://{}", base)
        };
        format!("{}/api/v1/channels/connect?token={}", ws_base, self.config.api_key)
    }

    /// Run the WebSocket event loop.
    ///
    /// Connects to the Hub, receives messages, and delivers them via
    /// the `on_message` callback. Blocks until cancelled or connection
    /// failure.
    pub async fn run(
        &mut self,
        channel_name: &str,
        on_message: &(dyn Fn(InboundMessage) -> Result<()> + Send + Sync),
        cancel: &CancellationToken,
    ) -> Result<()> {
        let url = self.ws_url();
        tracing::info!(url = %url, "Connecting to OpeniLink WebSocket...");

        // Connect to WebSocket
        let (ws_stream, _) = connect_async(&url)
            .await
            .with_context(|| format!("Failed to connect to OpeniLink WebSocket: {url}"))?;

        tracing::info!("OpeniLink WebSocket connected, listening for messages");
        self.reset_reconnection_count();

        let (write, mut read) = ws_stream.split();
        let write = Arc::new(Mutex::new(write));
        let cancel_clone = cancel.clone();

        // Spawn a heartbeat task that sends pings every 30 seconds
        let heartbeat_write = write.clone();
        let heartbeat_handle = tokio::spawn(async move {
            let interval = std::time::Duration::from_secs(30);
            loop {
                tokio::select! {
                    _ = tokio::time::sleep(interval) => {
                        let mut w = heartbeat_write.lock().await;
                        if let Err(e) = w.send(Message::Ping(vec![])).await {
                            tracing::debug!("Failed to send ping: {}", e);
                            break;
                        }
                    }
                    _ = cancel_clone.cancelled() => {
                        break;
                    }
                }
            }
        });

        // Receive messages
        loop {
            tokio::select! {
                msg = read.next() => {
                    match msg {
                        Some(Ok(Message::Text(text))) => {
                            if let Err(e) = self.handle_message(channel_name, &text, on_message) {
                                tracing::warn!(error = %e, "Failed to process message");
                            }
                        }
                        Some(Ok(Message::Pong(_))) => {
                            // Pong received, connection is healthy
                            tracing::trace!("WebSocket pong received");
                        }
                        Some(Ok(Message::Ping(data))) => {
                            // Respond to server ping
                            let mut w = write.lock().await;
                            if let Err(e) = w.send(Message::Pong(data.to_vec())).await {
                                tracing::debug!("Failed to send pong: {}", e);
                            }
                        }
                        Some(Ok(Message::Close(_))) => {
                            tracing::info!("WebSocket connection closed by server");
                            break;
                        }
                        Some(Ok(other)) => {
                            tracing::debug!("Received non-text WebSocket message: {:?}", other);
                        }
                        Some(Err(e)) => {
                            tracing::error!(error = %e, "WebSocket error");
                            return Err(anyhow::anyhow!("WebSocket error: {e}"));
                        }
                        None => {
                            // Stream ended
                            tracing::info!("WebSocket stream ended");
                            break;
                        }
                    }
                }
                _ = cancel.cancelled() => {
                    tracing::info!("OpeniLink WebSocket cancelled");
                    heartbeat_handle.abort();
                    return Ok(());
                }
            }
        }

        tracing::info!("OpeniLink WebSocket disconnected");
        heartbeat_handle.abort();

        Err(anyhow::anyhow!("WebSocket connection closed"))
    }

    /// Parse a WebSocket text message and deliver it as an `InboundMessage`.
    fn handle_message(
        &self,
        channel_name: &str,
        text: &str,
        on_message: &(dyn Fn(InboundMessage) -> Result<()> + Send + Sync),
    ) -> Result<()> {
        // Parse the JSON message
        let weixin_msg: WeixinMessage = serde_json::from_str(text)
            .with_context(|| format!("Failed to parse Weixin message: {}", &text[..text.len().min(200)]))?;

        // Filter out bot's own messages (message_type == 2)
        if weixin_msg.message_type == 2 {
            tracing::debug!(
                from_user_id = %weixin_msg.from_user_id,
                "Skipping bot's own message (message_type=2)"
            );
            return Ok(());
        }

        // Extract text content from item_list
        let text_content = self.extract_text(&weixin_msg);

        let from_user_id = weixin_msg.from_user_id.clone();
        let display_name = weixin_msg
            .from_user_name
            .clone()
            .unwrap_or_else(|| from_user_id.clone());

        // Build metadata
        let mut metadata = std::collections::HashMap::new();
        if let Some(token) = &weixin_msg.context_token {
            metadata.insert(
                "context_token".to_string(),
                serde_json::Value::String(token.clone()),
            );
        }
        metadata.insert(
            "from_user_id".to_string(),
            serde_json::Value::String(from_user_id.clone()),
        );

        let message = InboundMessage {
            id: uuid::Uuid::new_v4().to_string(),
            channel: channel_name.to_string(),
            channel_uid: from_user_id.clone(),
            sender: display_name,
            sender_address: from_user_id,
            recipients: vec![],
            topic: String::new(),
            content: MessageContent {
                text: Some(text_content),
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

        tracing::info!(
            message_id = %message.channel_uid,
            sender = %message.sender_address,
            "OpeniLink message received"
        );

        on_message(message)?;
        Ok(())
    }

    /// Extract text content from a WeixinMessage's item_list.
    ///
    /// For text items, returns the text content directly.
    /// For non-text items (images, files, etc.), returns a descriptive marker.
    fn extract_text(&self, msg: &WeixinMessage) -> String {
        let mut parts: Vec<String> = Vec::new();

        for item in &msg.item_list {
            match item.item_type {
                1 => {
                    // Text
                    if let Some(ref text_item) = item.text {
                        parts.push(text_item.text.clone());
                    }
                }
                3 => {
                    // Image
                    parts.push("[图片]".to_string());
                }
                34 => {
                    // Audio
                    parts.push("[语音]".to_string());
                }
                43 | 62 => {
                    // Video
                    parts.push("[视频]".to_string());
                }
                47 => {
                    // Sticker
                    parts.push("[表情]".to_string());
                }
                49 => {
                    // Shared link/card - try to extract title
                    parts.push("[分享]".to_string());
                }
                10000 => {
                    // System notification - skip
                }
                _ => {
                    parts.push(format!("[其他消息类型: {}]", item.item_type));
                }
            }
        }

        if parts.is_empty() {
            String::new()
        } else {
            parts.join(" ")
        }
    }

    /// Handle reconnection with exponential backoff.
    ///
    /// Returns `true` if we should retry, `false` if max attempts reached.
    pub async fn handle_reconnection(&mut self) -> bool {
        if self.reconnect_count >= self.config.websocket.max_reconnect_attempts {
            tracing::error!(
                max_attempts = self.config.websocket.max_reconnect_attempts,
                "Maximum reconnection attempts reached"
            );
            return false;
        }

        // Exponential backoff: base_delay * 2^attempt, capped at max_delay
        let base_delay = self.config.websocket.reconnect_delay_secs;
        let delay_secs = base_delay * (1u64 << self.reconnect_count.min(5));
        self.reconnect_count += 1;

        tracing::info!(
            attempt = self.reconnect_count,
            max_attempts = self.config.websocket.max_reconnect_attempts,
            delay_secs = delay_secs,
            "Reconnecting to OpeniLink WebSocket"
        );

        tokio::time::sleep(std::time::Duration::from_secs(delay_secs)).await;
        true
    }

    /// Reset reconnection count after successful connection.
    pub fn reset_reconnection_count(&mut self) {
        self.reconnect_count = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_ws() -> OpenilinkWebSocket {
        let config = OpenilinkConfig {
            api_key: "sk-test".to_string(),
            hub_url: "https://hub.example.com".to_string(),
            ..OpenilinkConfig::default()
        };
        OpenilinkWebSocket::new(&config)
    }

    #[test]
    fn test_ws_url_construction() {
        let ws = make_ws();
        let url = ws.ws_url();
        assert!(url.starts_with("wss://"));
        assert!(url.contains("hub.example.com"));
        assert!(url.contains("/api/v1/channels/connect"));
        assert!(url.contains("token=sk-test"));
    }

    #[test]
    fn test_ws_url_with_http() {
        let config = OpenilinkConfig {
            api_key: "sk-test".to_string(),
            hub_url: "http://hub.example.com".to_string(),
            ..OpenilinkConfig::default()
        };
        let ws = OpenilinkWebSocket::new(&config);
        let url = ws.ws_url();
        assert!(url.starts_with("wss://"));
    }

    #[test]
    fn test_ws_url_without_protocol() {
        let config = OpenilinkConfig {
            api_key: "sk-test".to_string(),
            hub_url: "hub.example.com".to_string(),
            ..OpenilinkConfig::default()
        };
        let ws = OpenilinkWebSocket::new(&config);
        let url = ws.ws_url();
        assert!(url.starts_with("wss://hub.example.com"));
    }

    #[test]
    fn test_extract_text_simple() {
        let ws = make_ws();
        let msg = WeixinMessage {
            message_type: 1,
            from_user_id: "wxid_abc".to_string(),
            from_user_name: None,
            item_list: vec![super::super::types::MessageItem {
                item_type: 1,
                text: Some(super::super::types::TextItem {
                    text: "Hello, World!".to_string(),
                }),
                image: None,
                extra: None,
            }],
            context_token: None,
            timestamp: None,
        };
        assert_eq!(ws.extract_text(&msg), "Hello, World!");
    }

    #[test]
    fn test_extract_text_mixed() {
        let ws = make_ws();
        let msg = WeixinMessage {
            message_type: 1,
            from_user_id: "wxid_abc".to_string(),
            from_user_name: None,
            item_list: vec![
                super::super::types::MessageItem {
                    item_type: 1,
                    text: Some(super::super::types::TextItem {
                        text: "看这张图片".to_string(),
                    }),
                    image: None,
                    extra: None,
                },
                super::super::types::MessageItem {
                    item_type: 3,
                    text: None,
                    image: Some(super::super::types::ImageItem {
                        url: Some("https://example.com/img.jpg".to_string()),
                        path: None,
                        width: None,
                        height: None,
                    }),
                    extra: None,
                },
            ],
            context_token: None,
            timestamp: None,
        };
        assert_eq!(ws.extract_text(&msg), "看这张图片 [图片]");
    }

    #[test]
    fn test_extract_text_empty() {
        let ws = make_ws();
        let msg = WeixinMessage {
            message_type: 1,
            from_user_id: "wxid_abc".to_string(),
            from_user_name: None,
            item_list: vec![],
            context_token: None,
            timestamp: None,
        };
        assert_eq!(ws.extract_text(&msg), "");
    }

    #[test]
    fn test_handle_message_filter_bot() {
        // Test that message_type == 2 is filtered: we should get Ok(()) and
        // the callback should NOT be called.
        // We'll test by checking the handler doesn't call the callback.
        let ws = make_ws();
        let json = r#"{
            "message_type": 2,
            "from_user_id": "wxid_bot",
            "item_list": [
                {"type": 1, "text": {"text": "bot message"}}
            ],
            "context_token": null
        }"#;

        let callback_called = std::sync::atomic::AtomicBool::new(false);
        let callback = |_: InboundMessage| {
            callback_called.store(true, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        };

        let result = ws.handle_message("test_channel", json, &callback);
        assert!(result.is_ok());
        assert!(!callback_called.load(std::sync::atomic::Ordering::SeqCst));
    }

    #[test]
    fn test_reconnection_count() {
        let mut config = OpenilinkConfig::default();
        config.websocket.max_reconnect_attempts = 3;
        let mut ws = OpenilinkWebSocket::new(&config);
        assert_eq!(ws.reconnect_count, 0);

        ws.reconnect_count = 2;
        ws.reset_reconnection_count();
        assert_eq!(ws.reconnect_count, 0);
    }
}
