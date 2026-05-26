//! OpeniLink-specific types and message definitions.
//!
//! This module contains types for WebSocket message events received from
//! OpeniLink Hub, and HTTP request/response types for the Hub REST API.
//!
//! The message format follows the OpeniLink Node SDK's `WeixinMessage` structure.

use serde::{Deserialize, Serialize};

// ─── WebSocket Message Types ───────────────────────────────────────────

/// A WeChat message received via OpeniLink Hub WebSocket.
///
/// Represents the message format sent by the OpeniLink Hub through the
/// WebSocket connection. Messages are received in real-time when someone
/// sends a message to the connected WeChat account.
#[derive(Debug, Clone, Deserialize)]
pub struct WeixinMessage {
    /// Message type identifier:
    /// - 1: received from a WeChat user
    /// - 2: sent by the bot itself (should be filtered out)
    /// - 3: system notification
    pub message_type: i32,

    /// The WeChat ID (wxid) of the user who sent the message
    pub from_user_id: String,

    /// Display name of the sender (may be empty for some accounts)
    pub from_user_name: Option<String>,

    /// The message content items
    pub item_list: Vec<MessageItem>,

    /// Context token for replying to this message through the HTTP API.
    /// Must be stored in metadata for outbound reply.
    pub context_token: Option<String>,

    /// Unix timestamp in milliseconds when the message was received by the hub
    pub timestamp: Option<i64>,
}

/// A single item within a WeChat message.
///
/// A WeChat message can contain multiple items (e.g., text + image),
/// each with a specific type.
#[derive(Debug, Clone, Deserialize)]
pub struct MessageItem {
    /// Item type:
    /// - 1: Text
    /// - 3: Image
    /// - 34: Audio (voice)
    /// - 43: Video
    /// - 47: Sticker/emoticon
    /// - 49: Shared link/card
    /// - 62: Small video
    /// - 10000: System notification
    /// - 10002: System message
    #[serde(rename = "type")]
    pub item_type: i32,

    /// Text content (present when type == 1)
    pub text: Option<TextItem>,

    /// Image content (present when type == 3)
    pub image: Option<ImageItem>,

    /// File/video/other content
    #[serde(flatten)]
    pub extra: Option<serde_json::Value>,
}

/// Text content within a message item.
#[derive(Debug, Clone, Deserialize)]
pub struct TextItem {
    /// The text content of the message
    pub text: String,
}

/// Image content within a message item.
#[derive(Debug, Clone, Deserialize)]
pub struct ImageItem {
    /// Image URL or file path
    pub url: Option<String>,

    /// Image file path on the hub server
    pub path: Option<String>,

    /// Image width in pixels
    pub width: Option<i32>,

    /// Image height in pixels
    pub height: Option<i32>,
}

// ─── WebSocket Connection Types ────────────────────────────────────────

/// Response from the WebSocket connect endpoint.
#[derive(Debug, Clone, Deserialize)]
pub struct WsConnectResponse {
    /// Connection status
    pub code: i32,

    /// Status message
    pub message: Option<String>,
}

// ─── HTTP API Request/Response Types ───────────────────────────────────

/// Request body for sending a message via the HTTP API.
///
/// POST {hub_url}/api/v1/channels/send
#[derive(Debug, Clone, Serialize)]
pub struct SendMessageRequest {
    /// The WeChat user ID (wxid) to send the message to
    pub to_user_id: String,

    /// The text content to send
    pub content: String,

    /// Context token from the original message (for reply mode).
    /// When absent, the message is sent as an active push.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_token: Option<String>,
}

/// Response from the send message API.
#[derive(Debug, Clone, Deserialize)]
pub struct SendMessageResponse {
    /// Result code (0 = success)
    pub code: i32,

    /// Status message
    pub message: Option<String>,

    /// Message ID assigned by the hub
    pub message_id: Option<String>,
}

/// Request body for sending a typing indicator.
///
/// POST {hub_url}/api/v1/channels/typing
#[derive(Debug, Clone, Serialize)]
pub struct SendTypingRequest {
    /// The WeChat user ID (wxid) to send typing indicator to
    pub to_user_id: String,

    /// Typing ticket from the original message
    #[serde(skip_serializing_if = "Option::is_none")]
    pub typing_ticket: Option<String>,

    /// Action: "TypingBegin" or "TypingEnd"
    pub action: String,
}

/// Response from the typing API.
#[derive(Debug, Clone, Deserialize)]
pub struct SendTypingResponse {
    /// Result code (0 = success)
    pub code: i32,

    /// Status message
    pub message: Option<String>,
}

/// Hub configuration retrieved from the GET config endpoint.
///
/// GET {hub_url}/api/v1/channels/config
#[derive(Debug, Clone, Deserialize)]
pub struct HubConfig {
    /// Hub version
    pub version: Option<String>,

    /// Whether the hub is connected to WeChat
    pub connected: bool,

    /// Current WeChat login user info
    pub wechat_user: Option<WechatUserInfo>,

    /// Supported features
    pub features: Option<Vec<String>>,
}

/// WeChat user information from the hub.
#[derive(Debug, Clone, Deserialize)]
pub struct WechatUserInfo {
    /// WeChat ID
    pub wxid: Option<String>,

    /// WeChat nickname
    pub nickname: Option<String>,

    /// Avatar URL
    pub avatar: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_text_message() {
        let json = r#"{
            "message_type": 1,
            "from_user_id": "wxid_abc123",
            "from_user_name": "张三",
            "item_list": [
                {
                    "type": 1,
                    "text": { "text": "你好，请问有什么可以帮助的吗？" }
                }
            ],
            "context_token": "token_xxx",
            "timestamp": 1704067200000
        }"#;

        let msg: WeixinMessage = serde_json::from_str(json).unwrap();
        assert_eq!(msg.message_type, 1);
        assert_eq!(msg.from_user_id, "wxid_abc123");
        assert_eq!(msg.from_user_name, Some("张三".to_string()));
        assert_eq!(msg.item_list.len(), 1);
        assert_eq!(msg.item_list[0].item_type, 1);
        assert_eq!(
            msg.item_list[0].text.as_ref().unwrap().text,
            "你好，请问有什么可以帮助的吗？"
        );
        assert_eq!(msg.context_token, Some("token_xxx".to_string()));
    }

    #[test]
    fn test_parse_image_message() {
        let json = r#"{
            "message_type": 1,
            "from_user_id": "wxid_def456",
            "from_user_name": null,
            "item_list": [
                {
                    "type": 3,
                    "image": {
                        "url": "https://example.com/image.jpg",
                        "path": "/data/images/abc.jpg",
                        "width": 800,
                        "height": 600
                    }
                }
            ],
            "context_token": null,
            "timestamp": 1704067200000
        }"#;

        let msg: WeixinMessage = serde_json::from_str(json).unwrap();
        assert_eq!(msg.message_type, 1);
        assert_eq!(msg.item_list.len(), 1);
        assert_eq!(msg.item_list[0].item_type, 3);
        let image = msg.item_list[0].image.as_ref().unwrap();
        assert_eq!(image.url.as_deref(), Some("https://example.com/image.jpg"));
        assert_eq!(image.width, Some(800));
        assert_eq!(image.height, Some(600));
    }

    #[test]
    fn test_parse_multi_item_message() {
        let json = r#"{
            "message_type": 1,
            "from_user_id": "wxid_ghi789",
            "item_list": [
                {
                    "type": 1,
                    "text": { "text": "看这张图片" }
                },
                {
                    "type": 3,
                    "image": {
                        "url": "https://example.com/photo.png",
                        "width": 1920,
                        "height": 1080
                    }
                }
            ],
            "context_token": "token_yyy",
            "timestamp": 1704067201000
        }"#;

        let msg: WeixinMessage = serde_json::from_str(json).unwrap();
        assert_eq!(msg.item_list.len(), 2);
        assert_eq!(msg.item_list[0].item_type, 1);
        assert_eq!(msg.item_list[1].item_type, 3);
        assert_eq!(msg.context_token, Some("token_yyy".to_string()));
    }

    #[test]
    fn test_filter_bot_message() {
        let json = r#"{
            "message_type": 2,
            "from_user_id": "wxid_bot",
            "item_list": [
                {
                    "type": 1,
                    "text": { "text": "这是机器人自己发的消息" }
                }
            ],
            "context_token": null,
            "timestamp": 1704067200000
        }"#;

        let msg: WeixinMessage = serde_json::from_str(json).unwrap();
        assert_eq!(msg.message_type, 2);
        // Messages with message_type == 2 should be filtered out by the websocket handler
    }

    #[test]
    fn test_send_message_request_serialize() {
        let req = SendMessageRequest {
            to_user_id: "wxid_abc".to_string(),
            content: "Hello, World!".to_string(),
            context_token: Some("token_xxx".to_string()),
        };

        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains(r#""to_user_id":"wxid_abc""#));
        assert!(json.contains(r#""content":"Hello, World!""#));
        assert!(json.contains(r#""context_token":"token_xxx""#));
    }

    #[test]
    fn test_send_message_request_no_context_token() {
        let req = SendMessageRequest {
            to_user_id: "wxid_abc".to_string(),
            content: "Hello!".to_string(),
            context_token: None,
        };

        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains(r#""to_user_id":"wxid_abc""#));
        assert!(!json.contains("context_token"));
    }

    #[test]
    fn test_send_message_response() {
        let json = r#"{
            "code": 0,
            "message": "success",
            "message_id": "msg_id_123"
        }"#;

        let resp: SendMessageResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.code, 0);
        assert_eq!(resp.message_id, Some("msg_id_123".to_string()));
    }

    #[test]
    fn test_hub_config() {
        let json = r#"{
            "version": "1.0.0",
            "connected": true,
            "wechat_user": {
                "wxid": "wxid_bot",
                "nickname": "My Bot",
                "avatar": "https://example.com/avatar.png"
            },
            "features": ["text", "image", "file"]
        }"#;

        let config: HubConfig = serde_json::from_str(json).unwrap();
        assert!(config.connected);
        assert_eq!(config.version, Some("1.0.0".to_string()));
        assert_eq!(config.wechat_user.as_ref().unwrap().wxid.as_deref(), Some("wxid_bot"));
        assert_eq!(config.features.as_ref().unwrap().len(), 3);
    }
}
