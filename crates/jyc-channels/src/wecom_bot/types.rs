//! WeCom Smart Robot (wecom_bot) WebSocket message types.
//!
//! Reference: doc 101463 - Smart Robot WebSocket Long Connection
//!            doc 100719 - Receiving Messages (JSON format)
//!            doc 101031 - Passive Reply Messages (including streaming)

use serde::{Deserialize, Serialize};

// ─── WebSocket Frame Commands ─────────────────────────────────────

/// Client-to-server commands.
pub const CMD_AIBOT_SUBSCRIBE: &str = "aibot_subscribe";
pub const CMD_AIBOT_PING: &str = "ping";
pub const CMD_AIBOT_RESPOND_MSG: &str = "aibot_respond_msg";
pub const CMD_AIBOT_RESPOND_WELCOME_MSG: &str = "aibot_respond_welcome_msg";
pub const CMD_AIBOT_SEND_MSG: &str = "aibot_send_msg";

/// Server-to-client commands.
pub const CMD_AIBOT_MSG_CALLBACK: &str = "aibot_msg_callback";
pub const CMD_AIBOT_EVENT_CALLBACK: &str = "aibot_event_callback";
pub const CMD_AIBOT_PONG: &str = "pong";

// ─── WebSocket Frame ──────────────────────────────────────────────

/// Generic WebSocket frame for both client→server and server→client.
///
/// All frames are JSON objects with at least a `cmd` field.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "cmd")]
#[allow(clippy::large_enum_variant)]
pub enum WsFrame {
    /// Client → Server: subscribe to bot events
    #[serde(rename = "aibot_subscribe")]
    Subscribe {
        #[serde(rename = "bot_id")]
        bot_id: String,
        secret: String,
    },

    /// Client → Server: heartbeat ping
    #[serde(rename = "ping")]
    Ping,

    /// Server → Client: heartbeat pong
    #[serde(rename = "pong")]
    Pong,

    /// Server → Client: message callback
    #[serde(rename = "aibot_msg_callback")]
    MsgCallback {
        #[serde(flatten)]
        message: Box<BotMessage>,
    },

    /// Server → Client: event callback
    #[serde(rename = "aibot_event_callback")]
    EventCallback {
        #[serde(flatten)]
        event: Box<BotEvent>,
    },

    /// Client → Server: respond to a message
    #[serde(rename = "aibot_respond_msg")]
    RespondMsg {
        #[serde(flatten)]
        payload: Box<RespondPayload>,
    },

    /// Client → Server: send a welcome message (on enter_chat event)
    #[serde(rename = "aibot_respond_welcome_msg")]
    RespondWelcomeMsg {
        #[serde(flatten)]
        payload: Box<RespondPayload>,
    },

    /// Client → Server: proactively send a message
    #[serde(rename = "aibot_send_msg")]
    SendMsg {
        #[serde(flatten)]
        payload: Box<RespondPayload>,
    },
}

// ─── Message Types ────────────────────────────────────────────────

/// A message received from WeCom Smart Robot via WebSocket.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BotMessage {
    /// Unique message ID
    pub msgid: String,
    /// Smart Robot ID
    pub aibotid: String,
    /// Chat ID (group or p2p). Empty for single chat messages.
    #[serde(default)]
    pub chatid: String,
    /// Chat type: "groupchat" or "single"
    pub chattype: String,
    /// Sender information
    pub from: SenderInfo,
    /// Message timestamp (milliseconds since epoch). 0 if not provided.
    #[serde(default)]
    pub msgtime: i64,
    /// Message type: text, image, mixed, voice, file, video
    pub msgtype: String,
    /// Request ID for reply correlation (from headers, not body)
    #[serde(default)]
    pub req_id: String,
    /// Server timestamp
    #[serde(default)]
    pub servertime: i64,

    // Message content fields (only one is populated based on msgtype)
    #[serde(default)]
    pub text: Option<TextContent>,
    #[serde(default)]
    pub image: Option<ImageContent>,
    #[serde(default)]
    pub mixed: Option<MixedContent>,
    #[serde(default)]
    pub voice: Option<VoiceContent>,
    #[serde(default)]
    pub file: Option<FileContent>,
    #[serde(default)]
    pub video: Option<VideoContent>,
}

/// Sender information.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SenderInfo {
    /// User ID of the sender
    pub userid: String,
}

/// Text message content.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TextContent {
    pub content: String,
}

/// Image message content.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImageContent {
    /// Image file URL (download requires aeskey decryption)
    pub url: String,
    /// AES key for decrypting the downloaded image
    pub aeskey: String,
}

/// Mixed message content (text + image combination).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MixedContent {
    /// Mixed content items
    #[serde(default)]
    pub items: Vec<MixedItem>,
}

/// Single item in a mixed message.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MixedItem {
    /// Item type: "text" or "image"
    #[serde(rename = "type")]
    pub item_type: String,
    /// Text content (if type is "text")
    #[serde(default)]
    pub content: Option<String>,
    /// Image URL (if type is "image")
    #[serde(default)]
    pub url: Option<String>,
    /// AES key for decrypting the image (if type is "image")
    #[serde(default)]
    pub aeskey: Option<String>,
}

/// Voice message content.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VoiceContent {
    pub url: String,
    pub aeskey: String,
}

/// File message content.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileContent {
    pub filename: String,
    pub url: String,
    pub aeskey: String,
}

/// Video message content.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VideoContent {
    pub url: String,
    pub aeskey: String,
}

// ─── Event Types ──────────────────────────────────────────────────

/// An event received from WeCom Smart Robot via WebSocket.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BotEvent {
    /// Smart Robot ID
    pub aibotid: String,
    /// Chat ID
    pub chatid: String,
    /// Event type: enter_chat, template_card_event, feedback_event, disconnected_event
    pub event: String,
    /// Event data (type-specific)
    #[serde(default)]
    pub data: Option<serde_json::Value>,
    /// Request ID (from headers, not body)
    #[serde(default)]
    pub req_id: String,
}

/// Payload for responding/sending messages.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct RespondPayload {
    /// Message type: text, markdown, stream
    pub msgtype: String,

    /// Chat ID (required for aibot_send_msg, optional for aibot_respond_msg)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub chatid: Option<String>,

    /// Text content (for msgtype = "text")
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<TextPayload>,

    /// Markdown content (for msgtype = "markdown")
    #[serde(skip_serializing_if = "Option::is_none")]
    pub markdown: Option<MarkdownPayload>,

    /// Stream content (for msgtype = "stream")
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream: Option<StreamPayload>,

    /// Request ID (must echo the req_id from the callback)
    pub req_id: String,
}

/// Text payload for respond/send.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct TextPayload {
    pub content: String,
}

/// Markdown payload for respond/send.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct MarkdownPayload {
    pub content: String,
}

/// Stream payload for streaming replies.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct StreamPayload {
    /// Stream ID (set on first chunk, reuse for subsequent chunks)
    pub id: String,
    /// Content chunk (supports markdown)
    pub content: String,
    /// Whether this is the final chunk
    pub finish: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_text_message() {
        // Nested format: cmd + headers + body
        let json = r#"{
            "cmd": "aibot_msg_callback",
            "headers": {"req_id": "req_abc"},
            "body": {
                "msgid": "msg_123",
                "aibotid": "bot_xxx",
                "chatid": "chat_456",
                "chattype": "single",
                "from": {"userid": "user_789"},
                "msgtime": 1704067200000,
                "msgtype": "text",
                "text": {"content": "Hello bot"}
            }
        }"#;
        let raw: serde_json::Value = serde_json::from_str(json).unwrap();
        let body = raw.get("body").cloned().unwrap();
        let message: BotMessage = serde_json::from_value(body).unwrap();
        assert_eq!(message.msgid, "msg_123");
        assert_eq!(message.chatid, "chat_456");
        assert_eq!(message.chattype, "single");
        assert_eq!(message.from.userid, "user_789");
        assert_eq!(message.msgtype, "text");
        assert_eq!(message.text.as_ref().unwrap().content, "Hello bot");
    }

    #[test]
    fn test_parse_event() {
        let json = r#"{
            "cmd": "aibot_event_callback",
            "headers": {"req_id": "req_def"},
            "body": {
                "aibotid": "bot_xxx",
                "chatid": "chat_456",
                "event": "enter_chat"
            }
        }"#;
        let raw: serde_json::Value = serde_json::from_str(json).unwrap();
        let body = raw.get("body").cloned().unwrap();
        let event: BotEvent = serde_json::from_value(body).unwrap();
        assert_eq!(event.event, "enter_chat");
        assert_eq!(event.chatid, "chat_456");
    }

    #[test]
    fn test_build_stream_response() {
        // Outbound uses nested format: cmd + headers + body
        let json = serde_json::json!({
            "cmd": "aibot_respond_msg",
            "headers": {"req_id": "req_abc"},
            "body": {
                "msgtype": "stream",
                "stream": {
                    "id": "stream_123",
                    "content": "Partial content",
                    "finish": false
                }
            }
        })
        .to_string();
        assert!(json.contains("\"cmd\":\"aibot_respond_msg\""));
        assert!(json.contains("\"msgtype\":\"stream\""));
        assert!(json.contains("\"id\":\"stream_123\""));
        assert!(json.contains("\"finish\":false"));
        assert!(json.contains("\"req_id\":\"req_abc\""));
    }

    #[test]
    fn test_build_subscribe() {
        let json = serde_json::json!({
            "cmd": "aibot_subscribe",
            "headers": {"req_id": "sub_123"},
            "body": {
                "bot_id": "my_bot",
                "secret": "my_secret"
            }
        })
        .to_string();
        assert!(json.contains("\"cmd\":\"aibot_subscribe\""));
        assert!(json.contains("\"bot_id\":\"my_bot\""));
        assert!(json.contains("\"secret\":\"my_secret\""));
    }

    #[test]
    fn test_parse_mixed_message() {
        let json = r#"{
            "cmd": "aibot_msg_callback",
            "headers": {"req_id": "req_mixed"},
            "body": {
                "msgid": "msg_mixed",
                "aibotid": "bot_xxx",
                "chatid": "chat_456",
                "chattype": "groupchat",
                "from": {"userid": "user_789"},
                "msgtime": 1704067200000,
                "msgtype": "mixed",
                "mixed": {
                    "items": [
                        {"type": "text", "content": "Check this out:"},
                        {"type": "image", "url": "https://example.com/img.jpg", "aeskey": "key123"}
                    ]
                }
            }
        }"#;
        let raw: serde_json::Value = serde_json::from_str(json).unwrap();
        let body = raw.get("body").cloned().unwrap();
        let message: BotMessage = serde_json::from_value(body).unwrap();
        let mixed = message.mixed.unwrap();
        assert_eq!(mixed.items.len(), 2);
        assert_eq!(mixed.items[0].item_type, "text");
        assert_eq!(mixed.items[0].content.as_ref().unwrap(), "Check this out:");
        assert_eq!(mixed.items[1].item_type, "image");
        assert_eq!(
            mixed.items[1].url.as_ref().unwrap(),
            "https://example.com/img.jpg"
        );
    }
}
