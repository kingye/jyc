//! Feishu-specific types and conversions.
//!
//! This module contains types for converting between Feishu API formats
//! and JYC internal formats.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Feishu message content in various formats.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[allow(dead_code)]
pub struct FeishuMessageContent {
    /// Text content
    pub text: Option<String>,
    /// Markdown content
    pub markdown: Option<String>,
    /// Image key (for image messages)
    pub image_key: Option<String>,
    /// File key (for file messages)
    pub file_key: Option<String>,
}

/// Feishu user information.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[allow(dead_code)]
pub struct FeishuUser {
    /// User ID
    pub user_id: String,
    /// Display name
    pub name: String,
    /// Email address
    pub email: Option<String>,
}

/// Feishu chat information.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[allow(dead_code)]
pub struct FeishuChat {
    /// Chat ID
    pub chat_id: String,
    /// Chat type: "p2p" or "group"
    pub chat_type: String,
    /// Chat name
    pub name: String,
}

/// Feishu message event from WebSocket.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[allow(dead_code)]
pub struct FeishuMessageEvent {
    /// Message ID
    pub message_id: String,
    /// Chat information
    pub chat: FeishuChat,
    /// Sender information
    pub sender: FeishuUser,
    /// Message content
    pub content: FeishuMessageContent,
    /// Create timestamp (milliseconds)
    pub create_time: i64,
    /// Parent message ID (for replies)
    pub parent_id: Option<String>,
    /// Mentioned users
    pub mentions: Vec<FeishuUser>,
    /// Additional metadata
    pub extra: HashMap<String, serde_json::Value>,
}

/// Feishu event types.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
#[allow(dead_code)]
pub enum FeishuEvent {
    /// Message received event
    #[serde(rename = "im.message.receive_v1")]
    MessageReceived {
        /// Event data
        event: FeishuMessageEvent,
    },
    /// Bot added to chat event
    #[serde(rename = "im.chat.member.bot.added_v1")]
    BotAddedToChat {
        /// Chat ID
        chat_id: String,
        /// Operator user ID
        operator_id: String,
    },
    /// Chat disbanded event (group chat deleted)
    #[serde(rename = "im.chat.disband_v1")]
    ChatDisbanded {
        /// Chat ID
        chat_id: String,
        /// Operator user ID
        operator_id: String,
    },
}

// --- WebSocket event payload types ---
//
// These structs match the JSON payloads delivered by Feishu's WebSocket
// long-connection (via openlark's LarkWsClient). The structure follows
// the actual Feishu API format, as seen in the openlark echo_bot example.

/// Top-level envelope for Feishu WebSocket event payloads.
#[derive(Debug, Deserialize)]
pub struct EventEnvelope {
    pub header: EventHeader,
    pub event: EventBody,
}

/// Event header with type and metadata.
#[derive(Debug, Deserialize)]
pub struct EventHeader {
    /// Unique event ID (for deduplication)
    pub event_id: Option<String>,
    /// Event type, e.g. "im.message.receive_v1"
    pub event_type: String,
    /// Event creation time (Unix timestamp string in milliseconds)
    #[allow(dead_code)]
    pub create_time: Option<String>,
    /// App ID that received the event
    #[allow(dead_code)]
    pub app_id: Option<String>,
    /// Tenant key
    #[allow(dead_code)]
    pub tenant_key: Option<String>,
}

/// Event body containing sender and message data.
#[derive(Debug, Deserialize)]
pub struct EventBody {
    pub sender: EventSender,
    pub message: EventMessage,
    /// Chat disbanded event data (present when event_type is "im.chat.disband_v1")
    #[serde(rename = "chat_disbanded", default)]
    pub chat_disbanded: Option<ChatDisbandedEvent>,
}

/// Chat disbanded event data.
#[derive(Debug, Clone, Deserialize)]
pub struct ChatDisbandedEvent {
    /// Chat ID
    pub chat_id: String,
    /// Operator user ID
    pub operator_id: String,
}

/// Sender information from the event payload.
#[derive(Debug, Deserialize)]
pub struct EventSender {
    pub sender_id: SenderIds,
    /// "user" or "bot"
    #[allow(dead_code)]
    pub sender_type: Option<String>,
    #[allow(dead_code)]
    pub tenant_key: Option<String>,
}

/// Sender ID fields — Feishu provides multiple ID formats.
#[derive(Debug, Deserialize)]
pub struct SenderIds {
    pub open_id: Option<String>,
    #[allow(dead_code)]
    pub user_id: Option<String>,
    #[allow(dead_code)]
    pub union_id: Option<String>,
}

/// Message data from the event payload.
#[derive(Debug, Deserialize)]
pub struct EventMessage {
    /// Unique message ID (e.g. "om_xxxxx")
    pub message_id: String,
    /// Message type: "text", "image", "file", "interactive", etc.
    pub message_type: String,
    /// Message content as a JSON string. Must be parsed based on message_type.
    /// For text: '{"text":"hello"}'
    /// For image: '{"image_key":"img_xxx"}'
    /// For file: '{"file_key":"file_xxx","file_name":"doc.pdf"}'
    pub content: String,
    /// Chat type: "p2p" (direct message) or "group"
    pub chat_type: String,
    /// Chat ID (present for group messages; for p2p, use sender's open_id)
    pub chat_id: Option<String>,
    /// Message creation time (Unix timestamp string in milliseconds)
    pub create_time: Option<String>,
    /// Users/bots mentioned in this message
    #[serde(default)]
    pub mentions: Option<Vec<EventMention>>,
}

/// A mention within a message.
#[derive(Debug, Deserialize)]
pub struct EventMention {
    /// Mention key in the content string, e.g. "@_user_1"
    pub key: String,
    /// Mentioned user/bot IDs
    pub id: MentionIds,
    /// Display name of the mentioned user/bot
    pub name: String,
}

/// Mention target ID fields.
#[derive(Debug, Deserialize)]
pub struct MentionIds {
    pub open_id: Option<String>,
    #[allow(dead_code)]
    pub user_id: Option<String>,
    #[allow(dead_code)]
    pub union_id: Option<String>,
}

// --- Message content types ---
//
// Extracted from EventMessage.content (which is a JSON string).

/// Text message content: {"text": "hello @_user_1"}
#[derive(Debug, Deserialize)]
pub struct TextContent {
    pub text: String,
}

/// Image message content: {"image_key": "img_xxx"}
#[derive(Debug, Deserialize)]
pub struct ImageContent {
    pub image_key: String,
}

/// File message content: {"file_key": "file_xxx", "file_name": "report.pdf"}
#[derive(Debug, Deserialize)]
pub struct FileContent {
    pub file_key: String,
    pub file_name: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_chat_disbanded_event_parsing() {
        let json = r#"{
            "type": "im.chat.disband_v1",
            "chat_id": "oc_12345678",
            "operator_id": "ou_87654321"
        }"#;

        let event: FeishuEvent = serde_json::from_str(json).unwrap();
        match event {
            FeishuEvent::ChatDisbanded {
                chat_id,
                operator_id,
            } => {
                assert_eq!(chat_id, "oc_12345678");
                assert_eq!(operator_id, "ou_87654321");
            }
            _ => panic!("Expected ChatDisbanded event"),
        }
    }

    #[test]
    fn test_chat_disbanded_serialization() {
        let event = FeishuEvent::ChatDisbanded {
            chat_id: "oc_12345678".to_string(),
            operator_id: "ou_87654321".to_string(),
        };

        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("im.chat.disband_v1"));
        assert!(json.contains("oc_12345678"));
        assert!(json.contains("ou_87654321"));
    }
}
