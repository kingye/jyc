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

use jyc_types::{InboundAttachmentConfig, InboundMessage, MessageAttachment, MessageContent};

use super::media;

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
    /// Inbound attachment policy. When `Some`, non-text items in
    /// `event.data.items[]` are downloaded and attached to the resulting
    /// `InboundMessage` (subject to allowlist + size cap). When `None` or
    /// `enabled = false`, attachments are dropped silently.
    attachment_config: Option<InboundAttachmentConfig>,
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
            attachment_config: None,
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

    /// Attach an inbound attachment policy. Without this, non-text items
    /// are dropped silently — the WS still produces an `InboundMessage`
    /// with the placeholder text body, but `attachments` will be empty.
    ///
    /// Builder-style so the call can be chained at construction time.
    pub fn with_attachment_config(mut self, cfg: Option<InboundAttachmentConfig>) -> Self {
        self.attachment_config = cfg;
        self
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
        let mut outbound_rx = self
            .outbound_rx
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
                    if let Err(e) = write.send(Message::Text(outbound_msg)).await {
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

        Err(anyhow::anyhow!(
            "WeChat WebSocket connection closed unexpectedly"
        ))
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
                return Err(anyhow::anyhow!(
                    "Failed to parse WeChat message as JSON: {}",
                    e
                ));
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
                serde_json::Value::String(message_id.clone()),
            );
        }
        if !event_id.is_empty() {
            metadata.insert("event_id".to_string(), serde_json::Value::String(event_id));
        }
        if !trace_id.is_empty() {
            metadata.insert("trace_id".to_string(), serde_json::Value::String(trace_id));
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
        metadata.insert("is_group".to_string(), serde_json::Value::Bool(is_group));
        if let Some(g) = group {
            metadata.insert("group".to_string(), g);
        }

        // Extract and download non-text items as attachments. The Bridge
        // delivers images / files / voice / video as
        // `event.data.items[*].media.url` (signed). For each such item,
        // we fetch the bytes and push a `MessageAttachment` into the
        // outgoing message. The actual on-disk persistence happens
        // post-route in the inbound adapter's
        // `save_attachments_to_thread_directory` (mirroring feishu).
        //
        // Each item is best-effort: if a single download fails, log a
        // warning and continue with the rest. The text body and other
        // attachments are still delivered.
        let attachments = self.extract_attachments(data, &message_id).await;

        // Body normalisation for non-text events.
        //
        // For non-text events (`message.image`, `message.file`,
        // `message.voice`, …), the OpenILink Bridge fills `data.content`
        // with a short bracketed placeholder like `"[image]"` so legacy
        // text-only clients don't choke on an empty body. For us this
        // placeholder is noise — the agent's prompt would receive the
        // literal four-character string `[image]` and reply confusingly
        // about it.
        //
        // Strip the placeholder so `thread_manager`'s body-empty guard
        // kicks in and the agent step is skipped entirely. The
        // attachment is still saved to disk and visible in the chat
        // history; a later PR will introduce a vision-aware path that
        // actually feeds the image to the LLM.
        //
        // Heuristic: non-text event AND the content is a single
        // bracketed token (e.g. `[image]`, `[file]`, `[voice]`,
        // `[video]`, `[image]\n`). Anything else (a real text message
        // with embedded brackets, a caption, etc.) flows through.
        let content = if event_type != "message.text" && is_placeholder_body(&content) {
            tracing::debug!(
                event_type = %event_type,
                "Stripping OpenILink placeholder body for non-text event"
            );
            String::new()
        } else {
            content
        };

        tracing::debug!(
            sender = %sender_id,
            content_len = content.len(),
            msg_type = %msg_type,
            event_type = %event_type,
            is_group,
            attachments_len = attachments.len(),
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
            attachments,
            metadata,
            matched_pattern: None,
        };

        on_message(message)?;
        Ok(())
    }

    /// Walk `event.data.items[]` and download every non-text item that
    /// passes the configured allowlist + size cap, returning the resulting
    /// `MessageAttachment`s. Each download error is logged at WARN and
    /// skipped; one failed item never blocks the others or the text body.
    ///
    /// Behaviour when `attachment_config` is `None` or `enabled = false`:
    /// silently skip everything (returns an empty Vec). The text body and
    /// metadata are still delivered to the agent, just without any files.
    async fn extract_attachments(
        &self,
        data: &serde_json::Value,
        message_id: &str,
    ) -> Vec<MessageAttachment> {
        let mut out: Vec<MessageAttachment> = Vec::new();

        let cfg = match &self.attachment_config {
            Some(c) if c.enabled => c,
            _ => {
                // Attachments disabled (or no config provided). Log so the
                // operator can correlate empty-attachment threads with
                // disabled config rather than guessing.
                if let Some(items) = data.get("items").and_then(|v| v.as_array()) {
                    let non_text = items
                        .iter()
                        .filter(|i| i.get("type").and_then(|v| v.as_str()).unwrap_or("") != "text")
                        .count();
                    if non_text > 0 {
                        tracing::debug!(
                            non_text_items = non_text,
                            "WeChat attachments disabled, skipping {} non-text item(s)",
                            non_text
                        );
                    }
                }
                return out;
            }
        };

        let items = match data.get("items").and_then(|v| v.as_array()) {
            Some(arr) => arr,
            None => return out,
        };

        // Resolve the size cap (best-effort parse; if it fails we apply
        // only the hard MAX_MEDIA_BYTES ceiling enforced inside
        // `media::download_media`).
        let max_size_bytes: Option<u64> = cfg
            .max_file_size
            .as_deref()
            .and_then(|s| jyc_utils::helpers::parse_file_size(s).ok());

        let max_per_message = cfg.max_per_message.unwrap_or(usize::MAX);

        for (idx, item) in items.iter().enumerate() {
            if out.len() >= max_per_message {
                tracing::debug!(
                    max_per_message,
                    "Reached max_per_message cap, skipping remaining WeChat items"
                );
                break;
            }

            let item_type = item.get("type").and_then(|v| v.as_str()).unwrap_or("");
            if item_type == "text" {
                continue; // text items are folded into `data.content`.
            }

            let media = match item.get("media") {
                Some(m) => m,
                None => {
                    tracing::debug!(
                        item_type,
                        idx,
                        "WeChat item has no `media` object, skipping"
                    );
                    continue;
                }
            };
            let url = match media.get("url").and_then(|v| v.as_str()) {
                Some(u) if !u.is_empty() => u,
                _ => {
                    tracing::debug!(item_type, idx, "WeChat item missing media.url, skipping");
                    continue;
                }
            };

            // Determine MIME and extension. Prefer the URL's `ct=` query
            // parameter (the Bridge always sets it); we'll override with
            // the response's `Content-Type` after fetch if available.
            let url_mime = media::mime::from_url_ct_param(url);
            let inferred_ext = url_mime
                .as_deref()
                .and_then(media::mime::extension_for)
                .unwrap_or_else(|| {
                    // Fallback: use the item type or media.media_type as a
                    // hint. Won't be perfect but is better than nothing.
                    media
                        .get("media_type")
                        .and_then(|v| v.as_str())
                        .unwrap_or(item_type)
                });

            // Allowlist gate: extension must appear in the operator's
            // configured `allowed_extensions`. Compare case-insensitively
            // and accept either dot-prefixed or bare values.
            if !cfg.allowed_extensions.is_empty() {
                let want = inferred_ext.trim_start_matches('.').to_ascii_lowercase();
                let permitted = cfg
                    .allowed_extensions
                    .iter()
                    .any(|e| e.trim_start_matches('.').eq_ignore_ascii_case(&want));
                if !permitted {
                    tracing::debug!(
                        ext = %inferred_ext,
                        item_type,
                        "WeChat item extension not in allowlist, skipping"
                    );
                    continue;
                }
            }

            // Fetch.
            let token: Option<&str> = if self.token.is_empty() {
                None
            } else {
                Some(self.token.as_str())
            };
            let resp = match media::download_media(url, token).await {
                Ok(r) => r,
                Err(e) => {
                    tracing::warn!(
                        error = %format!("{:#}", e),
                        item_type,
                        "Failed to download WeChat media item, skipping"
                    );
                    continue;
                }
            };

            // Server-provided Content-Type beats the URL hint.
            let content_type = resp
                .content_type
                .clone()
                .or(url_mime)
                .unwrap_or_else(|| "application/octet-stream".to_string());
            let final_ext = media::mime::extension_for(&content_type)
                .unwrap_or(inferred_ext)
                .to_string();

            // Post-download size check.
            if let Some(max) = max_size_bytes
                && (resp.bytes.len() as u64) > max
            {
                tracing::warn!(
                    size = resp.bytes.len(),
                    max,
                    item_type,
                    "WeChat media item exceeds max_file_size, skipping"
                );
                continue;
            }

            // Synthesise filename. Bridge doesn't ship one in the image
            // shape we've seen; voice/file shapes might (we'll surface it
            // when we observe one). For now: `<type>_<message_id>_<n>.<ext>`.
            // Sanitization is applied in `attachment_storage::generate_attachment_filename`
            // when the file lands on disk; we still produce a clean name
            // here for use in metadata / logs.
            let raw_name = item
                .get("filename")
                .and_then(|v| v.as_str())
                .or_else(|| media.get("filename").and_then(|v| v.as_str()))
                .map(|s| s.to_string())
                .unwrap_or_else(|| {
                    if message_id.is_empty() {
                        format!("{}_{}.{}", item_type, idx, final_ext)
                    } else {
                        format!("{}_{}_{}.{}", item_type, message_id, idx, final_ext)
                    }
                });
            let filename = jyc_core::attachment_storage::sanitize_attachment_filename(&raw_name);

            let size = resp.bytes.len();
            out.push(MessageAttachment {
                filename,
                content_type,
                size,
                content: Some(resp.bytes),
                saved_path: None,
            });
        }

        out
    }
}

/// Detect the OpenILink Bridge's bracketed placeholder bodies emitted
/// for non-text events.
///
/// Returns true when, after trimming, the body is a single bracketed
/// token like `[image]`, `[file]`, `[voice]`, `[video]`, or any other
/// `[…]` marker. False for normal text (including text that happens to
/// contain brackets, like `Look at [this]`).
///
/// Used by `handle_incoming` to blank out the body for non-text events
/// so the agent's body-empty guard skips the LLM call. The attachment
/// is still delivered via `MessageAttachment` and persisted to disk;
/// a later vision-aware path will feed the bytes to the LLM properly.
fn is_placeholder_body(s: &str) -> bool {
    let trimmed = s.trim();
    if trimmed.len() < 2 {
        return false;
    }
    if !(trimmed.starts_with('[') && trimmed.ends_with(']')) {
        return false;
    }
    // Inner must be non-empty and contain no surface punctuation that
    // would suggest a real sentence (closing brackets, newlines, etc.).
    let inner = &trimmed[1..trimmed.len() - 1];
    if inner.is_empty() {
        return false;
    }
    // Reject if the inner has additional brackets, newlines, or a
    // sentence-y character set. Keep the policy narrow to avoid eating
    // legitimate user text that happens to start and end with brackets.
    !inner
        .chars()
        .any(|c| c == '[' || c == ']' || c == '\n' || c == '\r' || c == '.' || c == '!' || c == '?')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ws_url_format() {
        let ws = WechatWebSocket::new("openilink.example.com", "test_token");
        let url = ws.ws_url();
        assert_eq!(
            url,
            "wss://openilink.example.com/bot/v1/ws?token=test_token"
        );
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
        assert_eq!(
            msg.channel_uid, "7464963577017103496",
            "channel_uid must use event.data.message_id (the WeChat-side id)"
        );
        assert_eq!(
            msg.sender_address, "o9cq8082DBb8Fd8p8DTRmzBFN7AM@im.wechat",
            "sender_address must come from event.data.sender.id"
        );
        assert_eq!(
            msg.sender, msg.sender_address,
            "v1: display name falls back to the WeChat ID"
        );
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
            msg.metadata
                .get("group")
                .and_then(|v| v.get("id"))
                .and_then(|v| v.as_str()),
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

    /// `message.image` event with a fetchable `event.data.items[].media.url`
    /// → the parser downloads the bytes and surfaces them as a
    /// `MessageAttachment` on the resulting `InboundMessage`.
    ///
    /// Mocks the OpenILink media endpoint with wiremock; uses a
    /// permissive `attachment_config` so the allowlist gate passes.
    #[tokio::test]
    async fn test_handle_incoming_image_message_event_attaches_media() {
        let media_server = wiremock::MockServer::start().await;
        let body: &[u8] = &[0xff, 0xd8, 0xff, 0xe0, 0x10, 0x20, 0x30];
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/media"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_bytes(body)
                    .insert_header("content-type", "image/jpeg"),
            )
            .mount(&media_server)
            .await;

        let media_url = format!("{}/media?ct=image%2Fjpeg", media_server.uri());
        let payload = serde_json::json!({
            "type": "event",
            "v": 1,
            "trace_id": "tr_test",
            "event": {
                "id": "evt_test",
                "type": "message.image",
                "data": {
                    "content": "[image]",
                    "items": [{
                        "type": "image",
                        "media": {
                            "url": &media_url,
                            "media_type": "image",
                        }
                    }],
                    "group": null,
                    "message_id": "msg_42",
                    "msg_type": "image",
                    "sender": {"id": "u1@im.wechat", "role": "user"}
                }
            }
        })
        .to_string();

        let captured = std::sync::Arc::new(std::sync::Mutex::new(None::<InboundMessage>));
        let captured_for_cb = captured.clone();
        let on_message = move |msg: InboundMessage| -> Result<()> {
            *captured_for_cb.lock().unwrap() = Some(msg);
            Ok(())
        };

        let cfg = jyc_types::InboundAttachmentConfig {
            enabled: true,
            allowed_extensions: vec![".jpg".to_string(), ".png".to_string()],
            max_file_size: Some("10mb".to_string()),
            max_per_message: Some(10),
            save_path: None,
        };

        let ws = WechatWebSocket::new("h", "t").with_attachment_config(Some(cfg));
        ws.handle_incoming("wechat_me", &payload, &on_message)
            .await
            .expect("handle_incoming should succeed");

        let msg = captured.lock().unwrap().take().unwrap();

        // Text body is BLANKED for non-text events so the agent's
        // body-empty guard skips the LLM call (the bracketed
        // placeholder `[image]` would otherwise be the entire prompt
        // body, which is not useful). A later vision-aware path will
        // feed the actual bytes to the model.
        assert_eq!(msg.content.text.as_deref(), Some(""));

        // Attachment populated.
        assert_eq!(msg.attachments.len(), 1, "expected one attachment");
        let att = &msg.attachments[0];
        assert_eq!(att.content_type, "image/jpeg");
        assert_eq!(att.size, body.len());
        assert_eq!(att.content.as_deref(), Some(body));
        assert!(att.saved_path.is_none(), "saved_path is set later by saver");
        assert!(
            att.filename.ends_with(".jpg"),
            "filename should end with .jpg, got: {}",
            att.filename
        );
        assert!(
            att.filename.contains("msg_42"),
            "filename should include message_id, got: {}",
            att.filename
        );

        // Metadata still set.
        assert_eq!(
            msg.metadata.get("event_type").and_then(|v| v.as_str()),
            Some("message.image")
        );
    }

    /// When `attachment_config` is `None`, non-text items are silently
    /// dropped — message is still delivered with placeholder text but
    /// no attachments. Documents the safe-default behaviour.
    #[tokio::test]
    async fn test_handle_incoming_drops_attachments_when_config_missing() {
        let payload = serde_json::json!({
            "type": "event",
            "v": 1,
            "event": {
                "id": "e",
                "type": "message.image",
                "data": {
                    "content": "[image]",
                    "items": [{
                        "type": "image",
                        "media": {
                            "url": "https://invalid.example/should-not-be-fetched",
                            "media_type": "image"
                        }
                    }],
                    "group": null,
                    "message_id": "m",
                    "msg_type": "image",
                    "sender": {"id": "u1", "role": "user"}
                }
            }
        })
        .to_string();

        let captured = std::sync::Arc::new(std::sync::Mutex::new(None::<InboundMessage>));
        let captured_for_cb = captured.clone();
        let on_message = move |msg: InboundMessage| -> Result<()> {
            *captured_for_cb.lock().unwrap() = Some(msg);
            Ok(())
        };

        // Default WechatWebSocket has no attachment_config.
        WechatWebSocket::new("h", "t")
            .handle_incoming("wechat_me", &payload, &on_message)
            .await
            .expect("handle_incoming should succeed");

        let msg = captured.lock().unwrap().take().unwrap();
        assert!(
            msg.attachments.is_empty(),
            "no fetch should happen when attachment_config is missing"
        );
        // Body still blanked for non-text events even when no attachment
        // was fetched: the placeholder `[image]` carries no information
        // for the agent, regardless of whether we managed to grab the
        // bytes. The body-empty guard in thread_manager skips the LLM
        // call uniformly.
        assert_eq!(msg.content.text.as_deref(), Some(""));
    }

    /// Items whose extension is not in `allowed_extensions` are skipped
    /// without fetching. Mirrors feishu's behaviour.
    #[tokio::test]
    async fn test_handle_incoming_skips_disallowed_extension() {
        let payload = serde_json::json!({
            "type": "event",
            "v": 1,
            "event": {
                "id": "e",
                "type": "message.image",
                "data": {
                    "content": "[image]",
                    "items": [{
                        "type": "image",
                        "media": {
                            "url": "https://invalid.example/never-fetched?ct=image%2Fjpeg",
                            "media_type": "image"
                        }
                    }],
                    "group": null,
                    "message_id": "m",
                    "msg_type": "image",
                    "sender": {"id": "u1", "role": "user"}
                }
            }
        })
        .to_string();

        let captured = std::sync::Arc::new(std::sync::Mutex::new(None::<InboundMessage>));
        let captured_for_cb = captured.clone();
        let on_message = move |msg: InboundMessage| -> Result<()> {
            *captured_for_cb.lock().unwrap() = Some(msg);
            Ok(())
        };

        // Allowlist excludes jpg.
        let cfg = jyc_types::InboundAttachmentConfig {
            enabled: true,
            allowed_extensions: vec![".pdf".to_string()],
            max_file_size: None,
            max_per_message: None,
            save_path: None,
        };

        WechatWebSocket::new("h", "t")
            .with_attachment_config(Some(cfg))
            .handle_incoming("wechat_me", &payload, &on_message)
            .await
            .expect("handle_incoming should succeed");

        let msg = captured.lock().unwrap().take().unwrap();
        assert!(
            msg.attachments.is_empty(),
            "extension allowlist should reject jpg"
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

    /// `is_placeholder_body` correctly identifies the OpenILink
    /// bracketed-token bodies and leaves real user text alone.
    #[test]
    fn test_is_placeholder_body() {
        // Known WeChat / OpenILink placeholders.
        assert!(is_placeholder_body("[image]"));
        assert!(is_placeholder_body("[file]"));
        assert!(is_placeholder_body("[voice]"));
        assert!(is_placeholder_body("[video]"));
        assert!(is_placeholder_body("[sticker]"));
        // Surrounding whitespace tolerated.
        assert!(is_placeholder_body(" [image] "));
        assert!(is_placeholder_body("[image]\n"));

        // Real user text — even with brackets — must NOT be detected as
        // a placeholder.
        assert!(!is_placeholder_body("hello"));
        assert!(!is_placeholder_body("Look at [this]"));
        assert!(!is_placeholder_body("[image] please review"));
        assert!(!is_placeholder_body("[]"));
        assert!(!is_placeholder_body(""));
        assert!(!is_placeholder_body(" "));
        assert!(!is_placeholder_body("[multi\nline]"));
        assert!(!is_placeholder_body("[a sentence.]"));
        // Nested brackets — not a single token.
        assert!(!is_placeholder_body("[a[b]c]"));
    }

    /// `message.text` events preserve the actual user text — the
    /// placeholder-blanking only applies to non-text event types.
    #[tokio::test]
    async fn test_handle_incoming_text_event_preserves_body() {
        let payload = r#"{
            "type": "event",
            "v": 1,
            "event": {
                "id": "e",
                "type": "message.text",
                "data": {
                    "content": "[image]",
                    "items": [{"type": "text", "text": "[image]"}],
                    "group": null,
                    "message_id": "m",
                    "msg_type": "text",
                    "sender": {"id": "u1", "role": "user"}
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
        // For message.text events, the body is preserved verbatim even
        // if it happens to look like a placeholder. The user actually
        // typed `[image]`; we must not eat their message.
        assert_eq!(msg.content.text.as_deref(), Some("[image]"));
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
