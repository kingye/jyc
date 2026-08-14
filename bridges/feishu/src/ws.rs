//! Minimal jyc WebSocket client for a bridge.
//!
//! Connects to `/ws/<channel>` on the jyc inspect server and exchanges JSON
//! frames per `docs/plugin-architecture.md` §5. One connection per jyc
//! channel; `thread` is carried in each frame's payload.

use anyhow::{Context, Result};
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::header;

/// Outbound frame: a message into a jyc thread.
#[derive(Debug, Clone, Serialize)]
pub struct InboundFrame {
    #[serde(rename = "type")]
    pub frame_type: String,
    pub thread: String,
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sender: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sender_address: Option<String>,
}

impl InboundFrame {
    pub fn message(
        thread: impl Into<String>,
        text: impl Into<String>,
        sender: Option<String>,
        sender_address: Option<String>,
    ) -> Self {
        Self {
            frame_type: "message".to_string(),
            thread: thread.into(),
            text: text.into(),
            sender,
            sender_address,
        }
    }
}

/// Reply frame broadcast by jyc for a thread.
#[derive(Debug, Clone, Deserialize)]
pub struct ReplyFrame {
    #[serde(rename = "type")]
    pub frame_type: String,
    pub thread: String,
    pub text: String,
}

/// A live WebSocket connection to one jyc channel (`/ws/<channel>`).
///
/// A background task owns the socket: it sends queued `InboundFrame`s and
/// forwards reply frames to `rx`. When the connection drops, the task exits
/// and `recv_reply` returns `None`.
pub struct ChannelClient {
    channel: String,
    tx: mpsc::UnboundedSender<InboundFrame>,
    rx: mpsc::UnboundedReceiver<ReplyFrame>,
}

impl ChannelClient {
    /// Connect to `/ws/<channel>` with bearer auth.
    pub async fn connect(jyc_url: &str, token: &str, channel: &str) -> Result<Self> {
        let url = format!("{}/ws/{}", jyc_url.trim_end_matches('/'), channel);
        let mut request = url
            .as_str()
            .into_client_request()
            .with_context(|| format!("invalid ws url {url}"))?;
        request.headers_mut().insert(
            header::AUTHORIZATION,
            format!("Bearer {token}")
                .parse()
                .context("invalid auth token")?,
        );
        let (ws, _resp) = tokio_tungstenite::connect_async(request)
            .await
            .with_context(|| format!("failed to connect to {url}"))?;

        let (mut sink, mut stream) = ws.split();
        let (tx, mut send_rx) = mpsc::unbounded_channel::<InboundFrame>();
        let (reply_tx, reply_rx) = mpsc::unbounded_channel::<ReplyFrame>();
        let task_channel = channel.to_string();

        tokio::spawn(async move {
            loop {
                tokio::select! {
                    frame = send_rx.recv() => {
                        let Some(frame) = frame else { break };
                        let Ok(payload) = serde_json::to_string(&frame) else { break };
                        if sink.send(Message::Text(payload.into())).await.is_err() {
                            tracing::warn!(channel = %task_channel, "jyc WS send failed");
                            break;
                        }
                    }
                    msg = stream.next() => {
                        match msg {
                            Some(Ok(Message::Text(text))) => {
                                if let Some(reply) = parse_reply(&text) {
                                    let _ = reply_tx.send(reply);
                                }
                            }
                            Some(Ok(_)) => {} // ping/pong/binary — ignore
                            _ => break,       // closed or error
                        }
                    }
                }
            }
            tracing::info!(channel = %task_channel, "jyc WS connection closed");
        });

        tracing::info!(channel = %channel, url = %url, "connected to jyc");
        Ok(Self {
            channel: channel.to_string(),
            tx,
            rx: reply_rx,
        })
    }

    pub fn channel(&self) -> &str {
        &self.channel
    }

    /// Queue a message for the channel.
    pub fn send_message(&self, frame: InboundFrame) -> Result<()> {
        self.tx.send(frame).context("jyc WS send queue closed")
    }

    /// Await the next reply frame, or `None` when the connection is closed.
    pub async fn recv_reply(&mut self) -> Option<ReplyFrame> {
        self.rx.recv().await
    }
}

/// Parse a reply frame, ignoring everything else (e.g. inspect activity
/// events that share the connection).
fn parse_reply(text: &str) -> Option<ReplyFrame> {
    let v: serde_json::Value = serde_json::from_str(text).ok()?;
    if v.get("type").and_then(|t| t.as_str()) != Some("reply") {
        return None;
    }
    serde_json::from_value(v).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inbound_frame_serializes_with_optional_sender() {
        let frame = InboundFrame::message(
            "thread-xxx",
            "hi",
            Some("张三".to_string()),
            Some("ou_abc".to_string()),
        );
        let v: serde_json::Value = serde_json::to_value(&frame).unwrap();
        assert_eq!(v["type"], "message");
        assert_eq!(v["thread"], "thread-xxx");
        assert_eq!(v["text"], "hi");
        assert_eq!(v["sender"], "张三");
        assert_eq!(v["sender_address"], "ou_abc");
    }

    #[test]
    fn inbound_frame_omits_none_sender() {
        let frame = InboundFrame::message("t", "hi", None, None);
        let s = serde_json::to_string(&frame).unwrap();
        assert!(!s.contains("sender"));
    }

    #[test]
    fn parse_reply_accepts_reply_frame() {
        let reply = parse_reply(r#"{"type":"reply","thread":"t1","text":"done"}"#).unwrap();
        assert_eq!(reply.thread, "t1");
        assert_eq!(reply.text, "done");
    }

    #[test]
    fn parse_reply_ignores_activity_frames() {
        assert!(
            parse_reply(r#"{"type":"activity","channel":"ch","thread":"t1","entry":{}}"#).is_none()
        );
        assert!(parse_reply("not json").is_none());
    }
}
