//! WebSocket client for WeCom Smart Robot (wecom_bot).
//!
//! Manages the lifecycle of a WebSocket connection:
//! connect → subscribe (aibot_subscribe) → receive messages/events → heartbeat (ping/pong) → reconnect.
//!
//! Reference: doc 101463 - Smart Robot WebSocket Long Connection

use std::time::Duration;

use anyhow::{Context, Result};
use futures_util::{SinkExt, StreamExt};
use tokio::sync::mpsc;
use tokio::time::interval;
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tokio_util::sync::CancellationToken;

use jyc_types::WecomBotConfig;

use super::types::{BotEvent, BotMessage};

/// Generate a req_id matching the Node.js SDK format: `{prefix}_{timestamp}_{random}`.
fn generate_req_id(prefix: &str) -> String {
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    // 8-char hex random string, matching SDK's generateRandomString(8)
    let random = uuid::Uuid::new_v4().to_string().replace('-', "")[..8].to_string();
    format!("{}_{}_{}", prefix, timestamp, random)
}

/// WebSocket connection state.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ConnectionState {
    Disconnected,
    Connecting,
    Connected,
    Subscribed,
}

/// A WebSocket message from the server (either a BotMessage or BotEvent).
#[derive(Debug, Clone)]
#[allow(clippy::large_enum_variant)]
pub enum ServerMessage {
    Message(BotMessage),
    Event(BotEvent),
}

/// WeCom Bot WebSocket client.
///
/// Handles low-level WebSocket operations: connect, subscribe, heartbeat, reconnect.
/// Higher-level message processing is delegated to the inbound adapter.
///
/// The `outbound_sender` is populated after successful connection and can be
/// used by the outbound adapter to send messages through the same WebSocket.
pub struct WecomBotWsClient {
    config: WecomBotConfig,
    state: ConnectionState,
    reconnect_count: u32,
    /// Sender for outbound messages. Set after WebSocket connection is established.
    outbound_sender: Option<mpsc::UnboundedSender<String>>,
}

impl WecomBotWsClient {
    /// Create a new WebSocket client.
    pub fn new(config: WecomBotConfig) -> Self {
        Self {
            config,
            state: ConnectionState::Disconnected,
            reconnect_count: 0,
            outbound_sender: None,
        }
    }

    /// Take the outbound sender after connection is established.
    ///
    /// Returns `None` if the connection has not been established yet.
    pub fn take_sender(&mut self) -> Option<mpsc::UnboundedSender<String>> {
        self.outbound_sender.take()
    }

    /// Set the outbound sender (used when sharing sender between inbound/outbound).
    #[allow(dead_code)]
    pub fn set_sender(&mut self, sender: mpsc::UnboundedSender<String>) {
        self.outbound_sender = Some(sender);
    }

    /// Run the WebSocket event loop.
    ///
    /// Connects to the WebSocket server, subscribes, and listens for messages.
    /// Reconnects automatically on disconnect (unless cancelled or max attempts reached).
    ///
    /// - `on_message`: callback for each received server message
    /// - `on_connect`: optional callback invoked after each successful connection/subscription
    /// - `cancel`: cancellation token to gracefully stop
    pub async fn run(
        &mut self,
        on_message: &(dyn Fn(ServerMessage) -> Result<()> + Send + Sync),
        on_connect: Option<&(dyn Fn(mpsc::UnboundedSender<String>) + Send + Sync)>,
        cancel: &CancellationToken,
    ) -> Result<()> {
        loop {
            if cancel.is_cancelled() {
                tracing::info!("WeCom Bot WebSocket cancelled before connection");
                return Ok(());
            }

            match self
                .connect_and_listen(on_message, on_connect, cancel)
                .await
            {
                Ok(()) => {
                    // Clean exit (cancelled)
                    tracing::info!("WeCom Bot WebSocket stopped cleanly");
                    return Ok(());
                }
                Err(e) => {
                    if cancel.is_cancelled() {
                        tracing::info!("WeCom Bot WebSocket shutting down (cancelled)");
                        return Ok(());
                    }
                    tracing::error!(error = %e, "WeCom Bot WebSocket error");

                    if !self.should_reconnect() {
                        tracing::error!(
                            max_attempts = self.config.max_reconnect_attempts,
                            "Max reconnection attempts reached, stopping"
                        );
                        return Err(e);
                    }

                    let delay = self.reconnect_delay();
                    tracing::info!(
                        attempt = self.reconnect_count,
                        max = self.config.max_reconnect_attempts,
                        delay_secs = delay,
                        "Reconnecting to WeCom Bot WebSocket"
                    );
                    tokio::select! {
                        _ = tokio::time::sleep(Duration::from_secs(delay)) => {}
                        _ = cancel.cancelled() => {
                            tracing::info!("WeCom Bot cancelled during reconnect delay");
                            return Ok(());
                        }
                    }
                }
            }
        }
    }

    /// Convenience method: run without on_connect callback.
    pub async fn run_simple(
        &mut self,
        on_message: &(dyn Fn(ServerMessage) -> Result<()> + Send + Sync),
        cancel: &CancellationToken,
    ) -> Result<()> {
        self.run(on_message, None, cancel).await
    }

    /// Single connection attempt: connect → subscribe → listen.
    async fn connect_and_listen(
        &mut self,
        on_message: &(dyn Fn(ServerMessage) -> Result<()> + Send + Sync),
        on_connect: Option<&(dyn Fn(mpsc::UnboundedSender<String>) + Send + Sync)>,
        cancel: &CancellationToken,
    ) -> Result<()> {
        self.state = ConnectionState::Connecting;
        tracing::info!(
            url = %self.config.ws_url,
            auth_timeout_secs = self.config.auth_timeout_secs,
            "Connecting to WeCom Bot WebSocket"
        );

        let (ws_stream, _) = connect_async(&self.config.ws_url).await.with_context(|| {
            format!(
                "Failed to connect to WeCom Bot WebSocket: {}",
                self.config.ws_url
            )
        })?;

        self.state = ConnectionState::Connected;
        self.reset_reconnect_count();
        tracing::info!("WeCom Bot WebSocket connected");

        // Split the stream into sender and receiver
        let (mut write, mut read) = ws_stream.split();

        // Send subscribe command with correct nested format:
        // {"cmd":"aibot_subscribe","headers":{"req_id":"..."},"body":{"bot_id":"...","secret":"..."}}
        let subscribe_json = serde_json::json!({
            "cmd": "aibot_subscribe",
            "headers": {
                "req_id": generate_req_id("aibot_subscribe")
            },
            "body": {
                "bot_id": self.config.bot_id,
                "secret": self.config.secret
            }
        })
        .to_string();
        tracing::debug!(subscribe_json = %subscribe_json, "Sending WeCom Bot subscribe command");
        write
            .send(Message::Text(subscribe_json))
            .await
            .context("Failed to send subscribe command")?;

        // Wait for subscribe response before starting heartbeat (matches SDK behavior)
        tracing::debug!(
            timeout_secs = self.config.auth_timeout_secs,
            "Waiting for WeCom Bot subscribe response"
        );
        let auth_start = std::time::Instant::now();
        let auth_ok = match tokio::time::timeout(
            Duration::from_secs(self.config.auth_timeout_secs),
            read.next(),
        )
        .await
        {
            Ok(Some(Ok(Message::Text(text)))) => {
                tracing::debug!(text = %text, "Received WebSocket text frame");
                self.handle_auth_response(&text).await?
            }
            Ok(Some(Ok(Message::Binary(bin)))) => {
                let text = String::from_utf8_lossy(&bin);
                tracing::debug!(text = %text, "Received WebSocket binary frame");
                self.handle_auth_response(&text).await?
            }
            Ok(Some(Ok(Message::Close(frame)))) => {
                tracing::warn!(frame = ?frame, "WebSocket closed by server during auth");
                return Err(anyhow::anyhow!(
                    "WeCom Bot WebSocket closed during authentication"
                ));
            }
            Ok(Some(Ok(Message::Ping(_))))
            | Ok(Some(Ok(Message::Pong(_))))
            | Ok(Some(Ok(Message::Frame(_)))) => {
                tracing::trace!("Ignoring WebSocket control frame during auth");
                return Err(anyhow::anyhow!(
                    "WeCom Bot WebSocket received unexpected control frame during authentication"
                ));
            }
            Ok(Some(Err(e))) => {
                tracing::warn!(error = %e, "WebSocket error during auth");
                return Err(anyhow::anyhow!(
                    "WeCom Bot WebSocket error during authentication: {}",
                    e
                ));
            }
            Ok(None) => {
                tracing::warn!("WebSocket stream ended during auth");
                return Err(anyhow::anyhow!(
                    "WeCom Bot WebSocket stream ended during authentication"
                ));
            }
            Err(_) => {
                let elapsed = auth_start.elapsed();
                tracing::warn!(
                    elapsed_ms = elapsed.as_millis(),
                    configured_timeout_secs = self.config.auth_timeout_secs,
                    "WebSocket auth timeout"
                );
                return Err(anyhow::anyhow!(
                    "WeCom Bot WebSocket authentication timeout after {:?} (configured timeout: {}s)",
                    elapsed,
                    self.config.auth_timeout_secs
                ));
            }
        };

        if !auth_ok {
            return Err(anyhow::anyhow!("WeCom Bot authentication failed"));
        }

        self.state = ConnectionState::Subscribed;
        tracing::info!(bot_id = %self.config.bot_id, "WeCom Bot subscribed");

        // Setup heartbeat (only after successful auth, matching SDK)
        let mut heartbeat = interval(Duration::from_secs(self.config.heartbeat_interval_secs));
        heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        // Create a channel for outbound messages (shared with outbound adapter)
        let (outbound_tx, mut outbound_rx) = mpsc::unbounded_channel::<String>();
        self.outbound_sender = Some(outbound_tx.clone());

        // Notify caller that connection is ready (for shared sender setup)
        if let Some(callback) = on_connect {
            callback(outbound_tx.clone());
        }

        // Spawn writer task: handles heartbeat + outbound messages
        let (heartbeat_tx, mut heartbeat_rx) = mpsc::unbounded_channel::<Message>();
        let writer_handle = {
            let mut write_clone = write;
            tokio::spawn(async move {
                loop {
                    tokio::select! {
                        Some(msg) = heartbeat_rx.recv() => {
                            if write_clone.send(msg).await.is_err() {
                                break;
                            }
                        }
                        Some(json_str) = outbound_rx.recv() => {
                            if write_clone.send(Message::Text(json_str)).await.is_err() {
                                break;
                            }
                        }
                        else => break,
                    }
                }
            })
        };

        // Main event loop
        loop {
            tokio::select! {
                _ = heartbeat.tick() => {
                    let ping_json = serde_json::json!({
                        "cmd": "ping",
                        "headers": {"req_id": generate_req_id("ping")}
                    }).to_string();
                    tracing::trace!("Sending heartbeat ping");
                    if heartbeat_tx.send(Message::Text(ping_json)).is_err() {
                        tracing::warn!("Heartbeat channel closed");
                        break;
                    }
                }
                msg = read.next() => {
                    match msg {
                        Some(Ok(Message::Text(text))) => {
                            tracing::debug!(text = %text, "Received WebSocket text frame");
                            if let Err(e) = self.handle_text_message(&text, on_message).await {
                                tracing::warn!(error = %e, text = %text, "Failed to handle WebSocket text message");
                            }
                        }
                        Some(Ok(Message::Binary(bin))) => {
                            let text = String::from_utf8_lossy(&bin);
                            tracing::debug!(text = %text, "Received WebSocket binary frame");
                            if let Err(e) = self.handle_text_message(&text, on_message).await {
                                tracing::warn!(error = %e, text = %text, "Failed to handle WebSocket binary message");
                            }
                        }
                        Some(Ok(Message::Ping(data))) => {
                            tracing::trace!("Received WebSocket ping");
                            if heartbeat_tx.send(Message::Pong(data)).is_err() {
                                tracing::warn!("Failed to send pong");
                            }
                        }
                        Some(Ok(Message::Close(frame))) => {
                            tracing::warn!(frame = ?frame, "WebSocket closed by server");
                            break;
                        }
                        Some(Ok(Message::Pong(_))) => {
                            tracing::trace!("Received WebSocket pong");
                        }
                        Some(Ok(Message::Frame(_))) => {
                            // Ignore raw frames
                        }
                        Some(Err(e)) => {
                            tracing::warn!(error = %e, "WebSocket error");
                            break;
                        }
                        None => {
                            tracing::warn!("WebSocket stream ended");
                            break;
                        }
                    }
                }
                _ = cancel.cancelled() => {
                    tracing::info!("WeCom Bot WebSocket cancelled");
                    // Send close frame
                    let _ = heartbeat_tx.send(Message::Close(None));
                    break;
                }
            }
        }

        // Cleanup: drop senders so writer task exits its select! loop naturally,
        // then wait for it to finish so the Close frame actually transmits on the wire.
        drop(heartbeat_tx);
        drop(outbound_tx);
        let _ = tokio::time::timeout(Duration::from_secs(5), writer_handle).await;
        self.outbound_sender = None;

        self.state = ConnectionState::Disconnected;

        if cancel.is_cancelled() {
            Ok(())
        } else {
            Err(anyhow::anyhow!("WeCom Bot WebSocket connection dropped"))
        }
    }

    /// Handle the authentication response from the WebSocket.
    ///
    /// Returns `true` if authentication succeeded (errcode == 0).
    async fn handle_auth_response(&self, text: &str) -> Result<bool> {
        let raw: serde_json::Value = match serde_json::from_str(text) {
            Ok(v) => v,
            Err(e) => {
                return Err(anyhow::anyhow!(
                    "Failed to parse auth response as JSON: {e} | text: {text}"
                ));
            }
        };

        if let Some(errcode) = raw.get("errcode").and_then(|v| v.as_i64()) {
            let errmsg = raw
                .get("errmsg")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown error");
            if errcode == 0 {
                tracing::info!(errmsg = %errmsg, "WeCom Bot authentication succeeded");
                return Ok(true);
            } else {
                tracing::error!(errcode = errcode, errmsg = %errmsg, "WeCom Bot authentication failed");
                return Ok(false);
            }
        }

        tracing::warn!(text = %text, "Unexpected auth response format");
        Ok(false)
    }

    /// Handle a text message received from the WebSocket.
    ///
    /// All server messages use nested format: {"cmd": "...", "headers": {...}, "body": {...}}
    async fn handle_text_message(
        &self,
        text: &str,
        on_message: &(dyn Fn(ServerMessage) -> Result<()> + Send + Sync),
    ) -> Result<()> {
        let raw: serde_json::Value = match serde_json::from_str(text) {
            Ok(v) => v,
            Err(e) => {
                return Err(anyhow::anyhow!(
                    "Failed to parse WebSocket text as JSON: {e} | text: {text}"
                ));
            }
        };

        // Check for server response frames (they don't have a `cmd` field)
        // Subscribe success: {"errcode": 0, "errmsg": "ok", "headers": {...}}
        // Subscribe failure: {"errcode": 853000, "errmsg": "invalid bot_id or secret", ...}
        // Reply ack: {"errcode": 40008, "errmsg": "invalid message type", "headers": {...}}
        if let Some(errcode) = raw.get("errcode").and_then(|v| v.as_i64()) {
            let errmsg = raw
                .get("errmsg")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown error");
            let req_id = raw
                .get("headers")
                .and_then(|h| h.get("req_id"))
                .and_then(|v| v.as_str())
                .unwrap_or("");

            if errcode == 0 {
                tracing::info!(req_id = %req_id, errmsg = %errmsg, "WeCom Bot operation succeeded");
                return Ok(());
            }

            // Only treat subscribe/ping errors as fatal; reply ack errors are non-fatal
            if req_id.starts_with("aibot_subscribe_") || req_id.starts_with("ping_") {
                tracing::error!(req_id = %req_id, errcode = errcode, errmsg = %errmsg, "WeCom Bot server error");
                return Err(anyhow::anyhow!(
                    "WeCom Bot server error: errcode={errcode}, errmsg={errmsg}"
                ));
            } else {
                tracing::warn!(req_id = %req_id, errcode = errcode, errmsg = %errmsg, "WeCom Bot reply ack error (non-fatal)");
                return Ok(());
            }
        }

        // Extract cmd from nested format
        let cmd = raw.get("cmd").and_then(|v| v.as_str()).unwrap_or("");

        let body = raw.get("body").cloned().unwrap_or(serde_json::Value::Null);

        match cmd {
            "pong" => {
                tracing::trace!("Received aibot pong");
            }
            "aibot_msg_callback" => {
                let mut message: super::types::BotMessage = serde_json::from_value(body)
                    .with_context(|| format!("Failed to parse BotMessage from body: {text}"))?;
                // Extract req_id from headers (not body)
                if let Some(req_id) = raw
                    .get("headers")
                    .and_then(|h| h.get("req_id"))
                    .and_then(|v| v.as_str())
                {
                    message.req_id = req_id.to_string();
                }
                tracing::debug!(
                    msgid = %message.msgid,
                    chatid = %message.chatid,
                    msgtype = %message.msgtype,
                    "Received WeCom Bot message"
                );
                on_message(ServerMessage::Message(message))?;
            }
            "aibot_event_callback" => {
                let mut event: super::types::BotEvent = serde_json::from_value(body)
                    .with_context(|| format!("Failed to parse BotEvent from body: {text}"))?;
                // Extract req_id from headers (not body)
                if let Some(req_id) = raw
                    .get("headers")
                    .and_then(|h| h.get("req_id"))
                    .and_then(|v| v.as_str())
                {
                    event.req_id = req_id.to_string();
                }
                tracing::debug!(
                    event = %event.event,
                    chatid = %event.chatid,
                    "Received WeCom Bot event"
                );
                on_message(ServerMessage::Event(event))?;
            }
            other => {
                tracing::warn!(cmd = %other, "Unexpected WebSocket frame from server");
            }
        }

        Ok(())
    }

    /// Check if we should attempt reconnection.
    fn should_reconnect(&self) -> bool {
        if !self.config.auto_reconnect {
            return false;
        }
        self.reconnect_count < self.config.max_reconnect_attempts
    }

    /// Calculate reconnect delay with exponential backoff (capped).
    fn reconnect_delay(&mut self) -> u64 {
        self.reconnect_count += 1;
        let base = self.config.reconnect_delay_secs;
        let backoff = base * (1u64 << self.reconnect_count.saturating_sub(1));
        std::cmp::min(backoff, 300) // Max 5 minutes
    }

    /// Reset reconnection count after successful connection.
    fn reset_reconnect_count(&mut self) {
        self.reconnect_count = 0;
    }

    /// Get current connection state.
    #[allow(dead_code)]
    pub fn state(&self) -> ConnectionState {
        self.state
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_reconnect_delay() {
        let config = WecomBotConfig {
            reconnect_delay_secs: 5,
            max_reconnect_attempts: 10,
            ..Default::default()
        };
        let mut client = WecomBotWsClient::new(config);

        assert_eq!(client.reconnect_delay(), 5); // 1st: 5 * 1
        assert_eq!(client.reconnect_delay(), 10); // 2nd: 5 * 2
        assert_eq!(client.reconnect_delay(), 20); // 3rd: 5 * 4
        assert_eq!(client.reconnect_delay(), 40); // 4th: 5 * 8
    }

    #[test]
    fn test_reconnect_delay_cap() {
        let config = WecomBotConfig {
            reconnect_delay_secs: 100,
            max_reconnect_attempts: 10,
            ..Default::default()
        };
        let mut client = WecomBotWsClient::new(config);

        client.reconnect_count = 5;
        assert_eq!(client.reconnect_delay(), 300); // capped at 300
    }

    #[test]
    fn test_should_reconnect() {
        let config = WecomBotConfig {
            auto_reconnect: true,
            max_reconnect_attempts: 3,
            ..Default::default()
        };
        let mut client = WecomBotWsClient::new(config);

        assert!(client.should_reconnect());
        client.reconnect_count = 3;
        assert!(!client.should_reconnect());
    }

    #[test]
    fn test_should_not_reconnect_when_disabled() {
        let config = WecomBotConfig {
            auto_reconnect: false,
            max_reconnect_attempts: 10,
            ..Default::default()
        };
        let client = WecomBotWsClient::new(config);
        assert!(!client.should_reconnect());
    }

    #[test]
    fn test_reset_reconnect_count() {
        let config = WecomBotConfig::default();
        let mut client = WecomBotWsClient::new(config);
        client.reconnect_count = 5;
        client.reset_reconnect_count();
        assert_eq!(client.reconnect_count, 0);
    }
}
