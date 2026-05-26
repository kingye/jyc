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

        // OpenILink Bridge envelope shape (v1):
        //   {
        //     "type": "event",
        //     "v": 1,
        //     "bot": {"id": "..."},
        //     "installation_id": "...",
        //     "trace_id": "...",
        //     "event": {
        //       "id": "evt_...",
        //       "type": "message.text",
        //       "timestamp": 1779786008,
        //       "data": {
        //         "content": "...",
        //         "items": [...],
        //         "group": null | {"id": "...", ...},
        //         "message_id": "...",
        //         "msg_type": "text",
        //         "sender": {"id": "...@im.wechat", "role": "user"}
        //       }
        //     }
        //   }
        //
        // Bridge sends other top-level types and other event.types
        // (presence pings, status updates, attachments-only events, …).
        // We only route message.* events; everything else is acknowledged
        // and dropped without invoking on_message so we don't fabricate
        // empty thread routes (the prior bug — sender=unknown content_len=0
        // routes turned into "No message body, stopping (no AI)").

        // Top-level envelope type must be "event"; anything else is an
        // out-of-band frame we don't handle (e.g. server-side keep-alive).
        let envelope_type = json.get("type").and_then(|v| v.as_str()).unwrap_or("");
        if envelope_type != "event" {
            tracing::debug!(
                envelope_type = %envelope_type,
                "Ignoring non-event WeChat frame"
            );
            return Ok(());
        }

        // Pull the inner event object. Without it, there's nothing to route.
        let Some(event) = json.get("event") else {
            tracing::debug!("WeChat frame missing `event` object, ignoring");
            return Ok(());
        };

        // Only handle `message.*` events (e.g. message.text, message.image).
        // Other event types: presence, contact updates, etc.
        let event_type = event.get("type").and_then(|v| v.as_str()).unwrap_or("");
        if !event_type.starts_with("message.") {
            tracing::debug!(
                event_type = %event_type,
                "Ignoring non-message WeChat event"
            );
            return Ok(());
        }

        // Pull event.data — required for any message event.
        let Some(data) = event.get("data") else {
            tracing::warn!(
                event_type = %event_type,
                "WeChat message event missing `data` object, ignoring"
            );
            return Ok(());
        };

        // Extract message text. For text events `data.content` is set; for
        // richer types (image, file, …) `data.content` may be empty and the
        // semantics live in `data.items`. v1 handles text only — non-text
        // events fall through with empty content; we route them anyway so
        // the operator can see them on the dashboard, but the agent step
        // will skip with "No message body" until v1.x adds richer handling.
        let content = data
            .get("content")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        // Sender is a nested object: {"id": "...", "role": "..."}.
        // We use `id` as both the address (for dedup / display) and the
        // human-readable name (the OpenILink schema doesn't carry a
        // separate display name in the envelope; the WeChat user ID is
        // the best we have until the bot resolves nicknames out-of-band).
        let sender_obj = data.get("sender");
        let sender_id = sender_obj
            .and_then(|s| s.get("id"))
            .and_then(|v| v.as_str())
            .unwrap_or("unknown")
            .to_string();
        let sender_role = sender_obj
            .and_then(|s| s.get("role"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        // Channel-unique ID is `event.data.message_id` (the WeChat-side
        // message ID). Falls back to `event.id` (Bridge-side event ID)
        // and finally a fresh UUID.
        let message_id = data
            .get("message_id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let event_id = event
            .get("id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let channel_uid = if !message_id.is_empty() {
            message_id.clone()
        } else if !event_id.is_empty() {
            event_id.clone()
        } else {
            uuid::Uuid::new_v4().to_string()
        };

        // msg_type: "text" / "image" / "file" / etc. Used for routing
        // decisions later if a pattern wants to match by type.
        let msg_type = data
            .get("msg_type")
            .and_then(|v| v.as_str())
            .unwrap_or("text")
            .to_string();

        // Group context: present and non-null when the message is from a
        // group chat (vs. a 1:1 DM). Stash the whole subobject in metadata
        // so future patterns can route by group.
        let group = data.get("group").cloned();
        let is_group = group.as_ref().map(|g| !g.is_null()).unwrap_or(false);

        let trace_id = json
            .get("trace_id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        // Build metadata. Stable keys for downstream pattern-match rules.
        let mut metadata = HashMap::new();
        if !message_id.is_empty() {
            metadata.insert(
                "msg_id".to_string(),
                serde_json::Value::String(message_id),
            );
        }
        if !event_id.is_empty() {
            metadata.insert(
                "event_id".to_string(),
                serde_json::Value::String(event_id),
            );
        }
        if !trace_id.is_empty() {
            metadata.insert(
                "trace_id".to_string(),
                serde_json::Value::String(trace_id),
            );
        }
        metadata.insert(
            "msg_type".to_string(),
            serde_json::Value::String(msg_type.clone()),
        );
        metadata.insert(
            "event_type".to_string(),
            serde_json::Value::String(event_type.to_string()),
        );
        metadata.insert(
            "sender".to_string(),
            serde_json::Value::String(sender_id.clone()),
        );
        if !sender_role.is_empty() {
            metadata.insert(
                "sender_role".to_string(),
                serde_json::Value::String(sender_role),
            );
        }
        metadata.insert(
            "is_group".to_string(),
            serde_json::Value::Bool(is_group),
        );
        if let Some(g) = group {
            metadata.insert("group".to_string(), g);
        }

        tracing::debug!(
            sender = %sender_id,
            content_len = content.len(),
            msg_type = %msg_type,
            event_type = %event_type,
            is_group,
            "WeChat message received"
        );

        let message = InboundMessage {
            id: uuid::Uuid::new_v4().to_string(),
            channel: channel_name.to_string(),
            channel_uid,
            // Sender display name — OpenILink doesn't ship a separate name
            // in v1, so fall back to the WeChat ID. If the schema later
            // adds e.g. `data.sender.name`, surface it here.
            sender: sender_id.clone(),
            sender_address: sender_id,
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


    /// Test parsing the canonical OpenILink Bridge envelope shape captured
    /// from production: a `message.text` event with a sender object.
    /// End-to-end through `handle_incoming` — asserts the resulting
    /// `InboundMessage` has the right content, sender, and metadata.
    #[tokio::test]
    async fn test_handle_incoming_text_message_event() {
        // Real OpenILink payload (May 26 2026 capture, slightly trimmed):
        let payload = r#"{
            "bot": {"id": "68bed918-0d1d-4d01-8750-c7f2bbfb9951"},
            "event": {
                "data": {
                    "content": "8+3=?",
                    "group": null,
                    "items": [{"type": "text", "text": "8+3=?"}],
                    "message_id": "7464963577017103496",
                    "msg_type": "text",
                    "sender": {
                        "id": "o9cq8082DBb8Fd8p8DTRmzBFN7AM@im.wechat",
                        "role": "user"
                    }
                },
                "id": "evt_211898dc-d747-4928-8980-2dfba8ede09c",
                "timestamp": 1779786008,
                "type": "message.text"
            },
            "installation_id": "0a086118-3a61-4ab7-ab0e-e01c5f715f61",
            "trace_id": "tr_b6b187a2df230dcb4e9143b161865b71",
            "type": "event",
            "v": 1
        }"#;

        let captured: std::sync::Arc<std::sync::Mutex<Option<InboundMessage>>> =
            std::sync::Arc::new(std::sync::Mutex::new(None));
        let captured_for_cb = captured.clone();
        let on_message = move |msg: InboundMessage| -> Result<()> {
            *captured_for_cb.lock().unwrap() = Some(msg);
            Ok(())
        };

        let ws = WechatWebSocket::new("hub.openilink.com", "test_token");
        ws.handle_incoming("wechat_me", payload, &on_message)
            .await
            .expect("handle_incoming should succeed for a well-formed message event");

        let msg = captured
            .lock()
            .unwrap()
            .take()
            .expect("on_message must be invoked for message.text events");

        assert_eq!(msg.channel, "wechat_me");
        assert_eq!(msg.channel_uid, "7464963577017103496",
            "channel_uid must use event.data.message_id (the WeChat-side id)");
        assert_eq!(
            msg.sender_address,
            "o9cq8082DBb8Fd8p8DTRmzBFN7AM@im.wechat",
            "sender_address must come from event.data.sender.id"
        );
        assert_eq!(msg.sender, msg.sender_address,
            "v1: display name falls back to the WeChat ID");
        assert_eq!(
            msg.content.text.as_deref(),
            Some("8+3=?"),
            "content must come from event.data.content"
        );

        // Metadata sanity.
        assert_eq!(
            msg.metadata.get("msg_id").and_then(|v| v.as_str()),
            Some("7464963577017103496")
        );
        assert_eq!(
            msg.metadata.get("event_id").and_then(|v| v.as_str()),
            Some("evt_211898dc-d747-4928-8980-2dfba8ede09c")
        );
        assert_eq!(
            msg.metadata.get("trace_id").and_then(|v| v.as_str()),
            Some("tr_b6b187a2df230dcb4e9143b161865b71")
        );
        assert_eq!(
            msg.metadata.get("event_type").and_then(|v| v.as_str()),
            Some("message.text")
        );
        assert_eq!(
            msg.metadata.get("msg_type").and_then(|v| v.as_str()),
            Some("text")
        );
        assert_eq!(
            msg.metadata.get("sender").and_then(|v| v.as_str()),
            Some("o9cq8082DBb8Fd8p8DTRmzBFN7AM@im.wechat")
        );
        assert_eq!(
            msg.metadata.get("sender_role").and_then(|v| v.as_str()),
            Some("user")
        );
        assert_eq!(
            msg.metadata.get("is_group").and_then(|v| v.as_bool()),
            Some(false),
            "1:1 message: group is null"
        );
    }

    /// Group messages should set `is_group=true` and stash the group object.
    #[tokio::test]
    async fn test_handle_incoming_group_message() {
        let payload = r#"{
            "type": "event",
            "v": 1,
            "trace_id": "tr_grp",
            "event": {
                "id": "evt_grp",
                "type": "message.text",
                "data": {
                    "content": "hi group",
                    "group": {"id": "grp_abc", "name": "Group X"},
                    "message_id": "msg_42",
                    "msg_type": "text",
                    "sender": {"id": "u1@im.wechat", "role": "user"}
                }
            }
        }"#;

        let captured = std::sync::Arc::new(std::sync::Mutex::new(None::<InboundMessage>));
        let captured_for_cb = captured.clone();
        let on_message = move |msg: InboundMessage| -> Result<()> {
            *captured_for_cb.lock().unwrap() = Some(msg);
            Ok(())
        };

        WechatWebSocket::new("h", "t")
            .handle_incoming("wechat_me", payload, &on_message)
            .await
            .unwrap();

        let msg = captured.lock().unwrap().take().unwrap();
        assert_eq!(
            msg.metadata.get("is_group").and_then(|v| v.as_bool()),
            Some(true),
        );
        assert_eq!(
            msg.metadata.get("group").and_then(|v| v.get("id")).and_then(|v| v.as_str()),
            Some("grp_abc"),
        );
    }

    /// Non-event envelope frames (e.g. server keep-alive, presence
    /// notifications) must be acknowledged but NOT routed — otherwise the
    /// pattern matcher gets a fabricated empty message and spawns a
    /// useless thread (the `sender=unknown content_len=0` bug from May 26).
    #[tokio::test]
    async fn test_handle_incoming_skips_non_event_frames() {
        let payload = r#"{"type": "ping", "v": 1}"#;

        let invoked = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let invoked_for_cb = invoked.clone();
        let on_message = move |_msg: InboundMessage| -> Result<()> {
            invoked_for_cb.store(true, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        };

        WechatWebSocket::new("h", "t")
            .handle_incoming("wechat_me", payload, &on_message)
            .await
            .unwrap();

        assert!(
            !invoked.load(std::sync::atomic::Ordering::SeqCst),
            "non-event frames must not invoke on_message"
        );
    }

    /// Event frames whose `event.type` is not `message.*` (e.g. presence,
    /// contact updates) must also be skipped without routing.
    #[tokio::test]
    async fn test_handle_incoming_skips_non_message_events() {
        let payload = r#"{
            "type": "event",
            "v": 1,
            "event": {"id": "e", "type": "presence.online", "data": {}}
        }"#;

        let invoked = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let invoked_for_cb = invoked.clone();
        let on_message = move |_msg: InboundMessage| -> Result<()> {
            invoked_for_cb.store(true, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        };

        WechatWebSocket::new("h", "t")
            .handle_incoming("wechat_me", payload, &on_message)
            .await
            .unwrap();

        assert!(
            !invoked.load(std::sync::atomic::Ordering::SeqCst),
            "non-message events must not invoke on_message"
        );
    }

    /// Test outgoing message JSON format — the WebSocket-side outbound
    /// frame format that the OpenILink Bridge accepts:
    /// `{"type":"send","content":"..."}`. Per the OpenILink docs.
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
