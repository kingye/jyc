//! Message formatting utilities for OpeniLink.
//!
//! This module provides utilities for extracting and formatting text
//! content from WeChat message items.

use super::types::{MessageItem, WeixinMessage};

/// Extract combined text content from a WeixinMessage's item_list.
///
/// For text items (type 1), the actual text is returned.
/// For non-text items (images, files, audio, video, etc.), descriptive
/// markers like `[图片]`, `[文件: report.pdf]` are generated.
///
/// Multiple items are joined with a space separator.
pub fn extract_text(message: &WeixinMessage) -> String {
    let mut parts: Vec<String> = Vec::new();

    for item in &message.item_list {
        parts.push(item_to_text(item));
    }

    parts.join(" ")
}

/// Convert a single message item to its text representation.
fn item_to_text(item: &MessageItem) -> String {
    match item.item_type {
        1 => {
            // Text
            item.text
                .as_ref()
                .map(|t| t.text.clone())
                .unwrap_or_default()
        }
        3 => {
            // Image
            "[图片]".to_string()
        }
        34 => {
            // Audio / voice message
            "[语音]".to_string()
        }
        43 | 62 => {
            // Video (43 = video, 62 = small video)
            "[视频]".to_string()
        }
        47 => {
            // Sticker / emoticon
            "[表情]".to_string()
        }
        49 => {
            // Shared link/card — try to extract title from extra data
            if let Some(title) = item.extra.as_ref().and_then(|e| e.get("title")).and_then(|v| v.as_str()).filter(|t| !t.is_empty()) {
                return format!("[分享: {}]", title);
            }
            "[分享]".to_string()
        }
        10000 => {
            // System notification
            String::new()
        }
        10002 => {
            // System message
            String::new()
        }
        other => {
            format!("[其他消息类型: {}]", other)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::openilink::types::{ImageItem, TextItem};

    fn make_text_item(text: &str) -> MessageItem {
        MessageItem {
            item_type: 1,
            text: Some(TextItem {
                text: text.to_string(),
            }),
            image: None,
            extra: None,
        }
    }

    fn make_image_item() -> MessageItem {
        MessageItem {
            item_type: 3,
            text: None,
            image: Some(ImageItem {
                url: Some("https://example.com/img.jpg".to_string()),
                path: None,
                width: None,
                height: None,
            }),
            extra: None,
        }
    }

    fn make_item(item_type: i32, extra: Option<serde_json::Value>) -> MessageItem {
        MessageItem {
            item_type,
            text: None,
            image: None,
            extra,
        }
    }

    #[test]
    fn test_extract_text_simple() {
        let msg = WeixinMessage {
            message_type: 1,
            from_user_id: "wxid_abc".to_string(),
            from_user_name: None,
            item_list: vec![make_text_item("Hello, World!")],
            context_token: None,
            timestamp: None,
        };
        assert_eq!(extract_text(&msg), "Hello, World!");
    }

    #[test]
    fn test_extract_text_chinese() {
        let msg = WeixinMessage {
            message_type: 1,
            from_user_id: "wxid_abc".to_string(),
            from_user_name: None,
            item_list: vec![make_text_item("你好，请问有什么可以帮助的吗？")],
            context_token: None,
            timestamp: None,
        };
        assert_eq!(
            extract_text(&msg),
            "你好，请问有什么可以帮助的吗？"
        );
    }

    #[test]
    fn test_extract_text_image_only() {
        let msg = WeixinMessage {
            message_type: 1,
            from_user_id: "wxid_abc".to_string(),
            from_user_name: None,
            item_list: vec![make_image_item()],
            context_token: None,
            timestamp: None,
        };
        assert_eq!(extract_text(&msg), "[图片]");
    }

    #[test]
    fn test_extract_text_text_and_image() {
        let msg = WeixinMessage {
            message_type: 1,
            from_user_id: "wxid_abc".to_string(),
            from_user_name: None,
            item_list: vec![
                make_text_item("看这张图片"),
                make_image_item(),
            ],
            context_token: None,
            timestamp: None,
        };
        assert_eq!(extract_text(&msg), "看这张图片 [图片]");
    }

    #[test]
    fn test_extract_text_audio() {
        let msg = WeixinMessage {
            message_type: 1,
            from_user_id: "wxid_abc".to_string(),
            from_user_name: None,
            item_list: vec![make_item(34, None)],
            context_token: None,
            timestamp: None,
        };
        assert_eq!(extract_text(&msg), "[语音]");
    }

    #[test]
    fn test_extract_text_video() {
        let msg = WeixinMessage {
            message_type: 1,
            from_user_id: "wxid_abc".to_string(),
            from_user_name: None,
            item_list: vec![make_item(43, None)],
            context_token: None,
            timestamp: None,
        };
        assert_eq!(extract_text(&msg), "[视频]");
    }

    #[test]
    fn test_extract_text_sticker() {
        let msg = WeixinMessage {
            message_type: 1,
            from_user_id: "wxid_abc".to_string(),
            from_user_name: None,
            item_list: vec![make_item(47, None)],
            context_token: None,
            timestamp: None,
        };
        assert_eq!(extract_text(&msg), "[表情]");
    }

    #[test]
    fn test_extract_text_shared_link() {
        let extra = serde_json::json!({
            "title": "Interesting Article"
        });
        let msg = WeixinMessage {
            message_type: 1,
            from_user_id: "wxid_abc".to_string(),
            from_user_name: None,
            item_list: vec![make_item(49, Some(extra))],
            context_token: None,
            timestamp: None,
        };
        assert_eq!(extract_text(&msg), "[分享: Interesting Article]");
    }

    #[test]
    fn test_extract_text_shared_link_no_title() {
        let msg = WeixinMessage {
            message_type: 1,
            from_user_id: "wxid_abc".to_string(),
            from_user_name: None,
            item_list: vec![make_item(49, None)],
            context_token: None,
            timestamp: None,
        };
        assert_eq!(extract_text(&msg), "[分享]");
    }

    #[test]
    fn test_extract_text_system_notification() {
        let msg = WeixinMessage {
            message_type: 1,
            from_user_id: "wxid_abc".to_string(),
            from_user_name: None,
            item_list: vec![make_item(10000, None)],
            context_token: None,
            timestamp: None,
        };
        assert_eq!(extract_text(&msg), "");
    }

    #[test]
    fn test_extract_text_mixed_types() {
        let msg = WeixinMessage {
            message_type: 1,
            from_user_id: "wxid_abc".to_string(),
            from_user_name: None,
            item_list: vec![
                make_text_item("你好"),
                make_image_item(),
                make_text_item("请看"),
                make_item(34, None), // audio
            ],
            context_token: None,
            timestamp: None,
        };
        assert_eq!(extract_text(&msg), "你好 [图片] 请看 [语音]");
    }

    #[test]
    fn test_extract_text_empty() {
        let msg = WeixinMessage {
            message_type: 1,
            from_user_id: "wxid_empty".to_string(),
            from_user_name: None,
            item_list: vec![],
            context_token: None,
            timestamp: None,
        };
        assert_eq!(extract_text(&msg), "");
    }
}
