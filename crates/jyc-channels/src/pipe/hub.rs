//! Hub websocket pipe client: one reconnecting connection per target hub
//! channel.
//!
//! A pipe process routes messages into hub channels by sending `message`
//! frames on the channel's websocket endpoint (`/ws/<channel>`); the hub
//! handles each frame exactly like an in-process `router.route(...)` call.
//! Server→client traffic on the same connection is demuxed into two
//! broadcast streams, mirroring what the in-process pipe adapters consume
//! from the hub's per-channel broadcast:
//!
//! - `reply` frames (raw payload) → [`HubPipe::subscribe_replies`]
//! - `topic_event` frames → [`HubPipe::subscribe_events`] (deserialized
//!   [`TopicEvent`] with the frame-level topic attached)
//!
//! Subscribers survive reconnects: the broadcasts are connection-level
//! state, like the in-process broadcast senders they replace.

use std::sync::Arc;

use anyhow::Result;
use futures_util::{SinkExt, StreamExt};
use jyc_core::topic_event::TopicEvent;
use tokio::sync::{Mutex, broadcast, mpsc};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_util::sync::CancellationToken;

/// One deserialized `topic_event` frame: the frame-level topic plus the
/// raw topic event (the hub streams the raw `TopicEvent` sequence,
/// excluding 1 Hz `LoopTick` heartbeats).
#[derive(Debug, Clone)]
pub struct PipeTopicEvent {
    pub topic: String,
    pub event: TopicEvent,
}

/// Classification of one inbound server frame.
enum FrameKind {
    /// A `reply` broadcast payload — forwarded verbatim to reply
    /// subscribers (same JSON the in-process broadcast carries).
    Reply(serde_json::Value),
    /// A `topic_event` frame, deserialized.
    TopicEvent(PipeTopicEvent),
    /// Everything else (activity/chat inspect events, unknown types).
    Other,
}

/// Parse and classify one inbound server frame.
fn classify_frame(text: &str) -> FrameKind {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(text) else {
        return FrameKind::Other;
    };
    match v.get("type").and_then(|t| t.as_str()) {
        Some("reply") => FrameKind::Reply(v),
        Some("topic_event") => {
            let (Some(topic), Some(event)) = (
                v.get("topic").and_then(|t| t.as_str()),
                v.get("event").cloned(),
            ) else {
                return FrameKind::Other;
            };
            match serde_json::from_value::<TopicEvent>(event) {
                Ok(event) => FrameKind::TopicEvent(PipeTopicEvent {
                    topic: topic.to_string(),
                    event,
                }),
                Err(e) => {
                    tracing::warn!(error = %e, "hub pipe: undecodable topic_event frame ignored");
                    FrameKind::Other
                }
            }
        }
        _ => FrameKind::Other,
    }
}

/// How the connection loop ended.
enum ConnectionOutcome {
    /// Caller cancellation (process shutdown).
    Cancelled,
    /// Authentication was rejected (401/403) or the URL is invalid —
    /// retrying cannot fix it.
    Fatal(anyhow::Error),
    /// The server closed the connection cleanly; reconnect with a short
    /// pause.
    Disconnected,
}

/// A reconnecting pipe connection to one hub channel.
///
/// Construct with [`HubPipe::new`], spawn [`HubPipe::run`] once, then use
/// the send/subscribe methods from anywhere.
pub struct HubPipe {
    target_channel: String,
    /// Full websocket URL (`ws://<host>:<port>/ws/<channel>`).
    url: String,
    token: Option<String>,
    /// Queued client frames, drained by the connection task. Bounded:
    /// while the hub is unreachable a full queue drops new frames with a
    /// warning instead of growing without bound.
    outbound: mpsc::Sender<String>,
    /// Taken by `run` on first connect.
    outbound_rx: Mutex<Option<mpsc::Receiver<String>>>,
    replies: broadcast::Sender<String>,
    events: broadcast::Sender<PipeTopicEvent>,
}

impl HubPipe {
    /// Create a pipe for `target_channel` on a hub at `ws_origin`
    /// (`ws://<host>:<port>`, no path). `token` is the inspect auth
    /// token (`None` → no auth header).
    pub fn new(target_channel: &str, ws_origin: &str, token: Option<String>) -> Arc<Self> {
        let origin = ws_origin.trim_end_matches('/');
        let url = format!("{origin}/ws/{target_channel}");
        let (outbound, outbound_rx) = mpsc::channel(256);
        // Capacity mirrors the in-process per-channel broadcasts.
        let (replies, _) = broadcast::channel(64);
        let (events, _) = broadcast::channel(64);
        Arc::new(Self {
            target_channel: target_channel.to_string(),
            url,
            token,
            outbound,
            outbound_rx: Mutex::new(Some(outbound_rx)),
            replies,
            events,
        })
    }

    /// Target hub channel this pipe routes into.
    pub fn target_channel(&self) -> &str {
        &self.target_channel
    }

    /// Subscribe to `reply` broadcast payloads (raw JSON strings, same
    /// shape the in-process broadcast carries).
    pub fn subscribe_replies(&self) -> broadcast::Receiver<String> {
        self.replies.subscribe()
    }

    /// Subscribe to deserialized `topic_event` frames.
    pub fn subscribe_events(&self) -> broadcast::Receiver<PipeTopicEvent> {
        self.events.subscribe()
    }

    /// Queue a `message` frame for the hub. Drops with a warning when the
    /// queue is full or no connection is draining it.
    pub fn send_message(
        &self,
        topic: &str,
        text: &str,
        sender: &str,
        sender_address: &str,
        metadata: std::collections::HashMap<String, serde_json::Value>,
    ) {
        let frame = serde_json::json!({
            "type": "message",
            "topic": topic,
            "text": text,
            "sender": sender,
            "sender_address": sender_address,
            "metadata": metadata,
        });
        self.queue_frame(frame, "message");
    }

    /// Queue a `close_topic` frame for the hub.
    pub fn send_close_topic(&self, topic: &str) {
        let frame = serde_json::json!({ "type": "close_topic", "topic": topic });
        self.queue_frame(frame, "close_topic");
    }

    fn queue_frame(&self, frame: serde_json::Value, kind: &str) {
        match self.outbound.try_send(frame.to_string()) {
            Ok(()) | Err(mpsc::error::TrySendError::Closed(_)) => {}
            Err(mpsc::error::TrySendError::Full(_)) => {
                tracing::warn!(
                    channel = %self.target_channel,
                    "hub pipe: outbound queue full, {kind} frame dropped (hub unreachable?)"
                );
            }
        }
    }

    /// Connection loop: connect with backoff, demux inbound frames until
    /// cancellation. Returns `Err` only on fatal (non-retryable) failures
    /// such as a rejected auth token.
    pub async fn run(self: &Arc<Self>, cancel: CancellationToken) -> Result<()> {
        // Take the outbound queue once, before the reconnect loop: it
        // survives reconnects, so frames queued while the hub is
        // unreachable flush once the pipe comes back.
        let mut guard = self.outbound_rx.lock().await;
        let Some(mut outbound_rx) = guard.take() else {
            return Err(anyhow::anyhow!("hub pipe: connection already running"));
        };
        drop(guard);

        let mut backoff = std::time::Duration::from_secs(1);
        loop {
            // Retryable disconnect (connect failure or clean close):
            // pause with exponential backoff — a persistent immediate-close
            // loop degrades to slow retries instead of spinning.
            let pause = match self.run_connection(&cancel, &mut outbound_rx).await {
                ConnectionOutcome::Cancelled => return Ok(()),
                ConnectionOutcome::Fatal(e) => return Err(e),
                ConnectionOutcome::Disconnected => backoff,
            };

            tracing::info!(
                channel = %self.target_channel,
                retry_in = ?pause,
                "hub pipe: reconnecting"
            );
            tokio::select! {
                _ = cancel.cancelled() => return Ok(()),
                _ = tokio::time::sleep(pause) => {}
            }
            backoff = (backoff * 2).min(std::time::Duration::from_secs(30));
        }
    }

    /// One connection attempt: handshake + read/write loop until the
    /// connection ends.
    async fn run_connection(
        self: &Arc<Self>,
        cancel: &CancellationToken,
        outbound_rx: &mut mpsc::Receiver<String>,
    ) -> ConnectionOutcome {
        let mut request = match self.url.as_str().into_client_request() {
            Ok(request) => request,
            Err(e) => {
                return ConnectionOutcome::Fatal(
                    anyhow::Error::new(e).context("invalid hub websocket URL"),
                );
            }
        };
        if let Some(token) = &self.token {
            match tokio_tungstenite::tungstenite::http::HeaderValue::from_str(&format!(
                "Bearer {token}"
            )) {
                Ok(value) => {
                    request.headers_mut().insert("Authorization", value);
                }
                Err(e) => {
                    return ConnectionOutcome::Fatal(
                        anyhow::Error::new(e).context("invalid inspect auth token"),
                    );
                }
            }
        }

        tracing::info!(channel = %self.target_channel, url = %self.url, "hub pipe: connecting");
        let result = tokio_tungstenite::connect_async(request).await;
        let (mut stream, _response) = match result {
            Ok(v) => v,
            Err(tokio_tungstenite::tungstenite::Error::Http(response)) => {
                let status = response.status();
                return if status == 401 || status == 403 {
                    ConnectionOutcome::Fatal(anyhow::anyhow!(
                        "hub pipe: hub rejected the connection with {status} — check the inspect auth token"
                    ))
                } else {
                    tracing::warn!(status = %status, "hub pipe: handshake failed");
                    ConnectionOutcome::Disconnected
                };
            }
            Err(e) => {
                tracing::warn!(error = %e, "hub pipe: connect failed");
                return ConnectionOutcome::Disconnected;
            }
        };

        tracing::info!(channel = %self.target_channel, "hub pipe: connected");
        let mut ping = tokio::time::interval(std::time::Duration::from_secs(30));
        loop {
            tokio::select! {
                _ = cancel.cancelled() => {
                    let _ = stream.close(None).await;
                    return ConnectionOutcome::Cancelled;
                }
                _ = ping.tick() => {
                    if let Err(e) = stream.send(Message::Ping(Vec::new().into())).await {
                        tracing::warn!(error = %e, "hub pipe: ping failed");
                        return ConnectionOutcome::Disconnected;
                    }
                }
                frame = outbound_rx.recv() => {
                    match frame {
                        Some(frame) => {
                            if let Err(e) = stream.send(Message::Text(frame.into())).await {
                                tracing::warn!(error = %e, "hub pipe: frame send failed");
                                return ConnectionOutcome::Disconnected;
                            }
                        }
                        None => return ConnectionOutcome::Cancelled,
                    }
                }
                msg = stream.next() => {
                    match msg {
                        Some(Ok(Message::Text(text))) => {
                            match classify_frame(&text) {
                                FrameKind::Reply(v) => {
                                    // Lagging subscribers lose the frame —
                                    // same as the in-process broadcast.
                                    let _ = self.replies.send(v.to_string());
                                }
                                FrameKind::TopicEvent(pe) => {
                                    let _ = self.events.send(pe);
                                }
                                FrameKind::Other => {}
                            }
                        }
                        Some(Ok(Message::Close(_))) | None => {
                            return ConnectionOutcome::Disconnected;
                        }
                        Some(Ok(_)) => {}
                        Some(Err(e)) => {
                            tracing::warn!(error = %e, "hub pipe: receive error");
                            return ConnectionOutcome::Disconnected;
                        }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_reply_forwards_payload() {
        let frame = r#"{"type":"reply","topic":"t1","text":"hello"}"#;
        match classify_frame(frame) {
            FrameKind::Reply(v) => {
                assert_eq!(v["topic"], "t1");
                assert_eq!(v["text"], "hello");
            }
            _ => panic!("expected Reply"),
        }
    }

    #[test]
    fn classify_topic_event_deserializes_with_topic() {
        // TopicEvent serializes as an externally-tagged enum: the variant
        // name is the single key of the `event` object.
        let frame = r#"{"type":"topic_event","channel":"agents","topic":"t1","event":{"ProcessingStarted":{"topic_name":"t1","message_id":"m1","timestamp":"2026-10-09T00:00:00Z"}}}"#;
        match classify_frame(frame) {
            FrameKind::TopicEvent(pe) => {
                assert_eq!(pe.topic, "t1");
                assert!(matches!(pe.event, TopicEvent::ProcessingStarted { .. }));
            }
            _ => panic!("expected TopicEvent"),
        }
    }

    #[test]
    fn classify_ignores_other_and_malformed_frames() {
        assert!(matches!(classify_frame("not json"), FrameKind::Other));
        assert!(matches!(
            classify_frame(r#"{"type":"activity"}"#),
            FrameKind::Other
        ));
        // topic_event without a topic is ignored
        assert!(matches!(
            classify_frame(r#"{"type":"topic_event","event":{"ProcessingStarted":{}}}"#),
            FrameKind::Other
        ));
    }

    #[tokio::test]
    async fn send_message_and_close_topic_queue_wire_frames() {
        let pipe = HubPipe::new("agents", "ws://127.0.0.1:9876", None);
        let mut metadata = std::collections::HashMap::new();
        metadata.insert("chat_id".to_string(), serde_json::json!("oc_x"));
        pipe.send_message("t1", "hi", "jin", "ou_1", metadata);
        pipe.send_close_topic("t1");

        let mut rx = pipe.outbound_rx.lock().await;
        let mut rx = rx.take().unwrap();

        let msg: serde_json::Value =
            serde_json::from_str(rx.recv().await.unwrap().as_str()).unwrap();
        assert_eq!(msg["type"], "message");
        assert_eq!(msg["topic"], "t1");
        assert_eq!(msg["text"], "hi");
        assert_eq!(msg["sender"], "jin");
        assert_eq!(msg["sender_address"], "ou_1");
        assert_eq!(msg["metadata"]["chat_id"], "oc_x");

        let close: serde_json::Value =
            serde_json::from_str(rx.recv().await.unwrap().as_str()).unwrap();
        assert_eq!(
            close,
            serde_json::json!({"type": "close_topic", "topic": "t1"})
        );
    }

    #[test]
    fn pipe_url_builds_from_origin_and_channel() {
        let pipe = HubPipe::new("adhoc", "ws://127.0.0.1:9876/", None);
        assert_eq!(pipe.url, "ws://127.0.0.1:9876/ws/adhoc");
        assert_eq!(pipe.target_channel(), "adhoc");
    }
}
