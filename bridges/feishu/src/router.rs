//! Route decision logic: turn a feishu `InboundMessage` into a forward
//! decision for jyc. Pure and unit-testable (no I/O).

use jyc_types::InboundMessage;

use crate::config::BridgeConfig;

/// The decision to forward one feishu event into jyc.
#[derive(Debug, Clone)]
pub struct Forward {
    /// Target jyc channel (route channel or the bridge default).
    pub channel: String,
    /// Target jyc thread (= jyc pattern name).
    pub thread: String,
    /// Feishu reply target: group `chat_id` or p2p `open_id`.
    pub receive_id: String,
    /// `"chat_id"` for groups, `"open_id"` for p2p.
    pub receive_id_type: String,
    /// Message text to forward.
    pub text: String,
    /// Channel-native sender identity (display name).
    pub sender: Option<String>,
    /// Channel-native sender address (open_id).
    pub sender_address: Option<String>,
}

/// Decide whether/how to forward a feishu message.
///
/// Returns `None` when no route matches (chat name, or chat_id fallback) or
/// the route's @mention respond filter rejects the message.
pub fn forward_for(cfg: &BridgeConfig, message: &InboundMessage) -> Option<Forward> {
    let chat_name = metadata_str(message, "chat_name");
    let chat_id = metadata_str(message, "chat_id");
    let chat_type = metadata_str(message, "chat_type");

    let route = cfg.route(&chat_name, &chat_id)?;

    // Respond filter: when the route declares mentions, forward only if one
    // of them is @-mentioned (matches id or display name).
    if let Some(required) = &route.mentions {
        let mentioned: Vec<String> = message
            .metadata
            .get("mentions")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|m| {
                        m.as_str()
                            .or_else(|| m.get("name").and_then(|n| n.as_str()))
                            .or_else(|| m.get("id").and_then(|i| i.as_str()))
                    })
                    .map(|s| s.to_lowercase())
                    .collect()
            })
            .unwrap_or_default();
        if !required
            .iter()
            .any(|r| mentioned.contains(&r.to_lowercase()))
        {
            return None;
        }
    }

    // For p2p, the metadata chat_id holds the sender's open_id (see
    // feishu/websocket.rs convert_to_inbound), so replies must target the
    // user with receive_id_type=open_id rather than a group chat_id.
    let (receive_id_type, receive_id) = if chat_type == "p2p" {
        ("open_id", chat_id)
    } else {
        ("chat_id", chat_id)
    };

    Some(Forward {
        channel: route.channel.clone().unwrap_or_else(|| cfg.channel.clone()),
        thread: route.thread.clone(),
        receive_id,
        receive_id_type: receive_id_type.to_string(),
        text: message.content.text.clone().unwrap_or_default(),
        sender: Some(message.sender.clone()),
        sender_address: Some(message.sender_address.clone()),
    })
}

fn metadata_str(message: &InboundMessage, key: &str) -> String {
    message
        .metadata
        .get(key)
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use jyc_types::{InboundMessage, MessageContent};
    use std::collections::HashMap;

    fn message(
        chat_name: &str,
        chat_id: &str,
        chat_type: &str,
        mentions: &[(&str, &str)],
    ) -> InboundMessage {
        let mut metadata = HashMap::new();
        metadata.insert("chat_name".to_string(), serde_json::json!(chat_name));
        metadata.insert("chat_id".to_string(), serde_json::json!(chat_id));
        metadata.insert("chat_type".to_string(), serde_json::json!(chat_type));
        if !mentions.is_empty() {
            let arr: Vec<serde_json::Value> = mentions
                .iter()
                .map(|(id, name)| serde_json::json!({ "id": id, "name": name }))
                .collect();
            metadata.insert("mentions".to_string(), serde_json::Value::Array(arr));
        }
        InboundMessage {
            id: "test".to_string(),
            channel: "feishu".to_string(),
            channel_uid: chat_id.to_string(),
            sender: "张三".to_string(),
            sender_address: "ou_sender".to_string(),
            recipients: vec![],
            topic: chat_name.to_string(),
            content: MessageContent {
                text: Some("hello".to_string()),
                html: None,
                markdown: None,
            },
            timestamp: Utc::now(),
            thread_refs: None,
            reply_to_id: None,
            external_id: None,
            attachments: vec![],
            metadata,
            matched_pattern: None,
        }
    }

    fn cfg(toml_src: &str) -> BridgeConfig {
        toml::from_str(toml_src).unwrap()
    }

    const CFG: &str = r#"
name = "feishu"
channel = "feishu_bot"
app_id = "a"
app_secret = "b"

[[routes]]
chat_name = "greenfield"
chat_id = "oc_greenfield"
channel = "channel-b"
thread = "thread-xxx"

[[routes]]
chat_name = "张三"
thread = "p2p-thread"
"#;

    #[test]
    fn routes_group_message_and_defaults_channel() {
        let cfg = cfg(CFG);
        let f = forward_for(&cfg, &message("greenfield", "oc_greenfield", "group", &[])).unwrap();
        assert_eq!(f.channel, "channel-b");
        assert_eq!(f.thread, "thread-xxx");
        assert_eq!(f.receive_id, "oc_greenfield");
        assert_eq!(f.receive_id_type, "chat_id");
        assert_eq!(f.sender.as_deref(), Some("张三"));
    }

    #[test]
    fn routes_p2p_message_to_user_open_id() {
        let cfg = cfg(CFG);
        let f = forward_for(&cfg, &message("张三", "ou_zhang", "p2p", &[])).unwrap();
        assert_eq!(f.thread, "p2p-thread");
        assert_eq!(f.channel, "feishu_bot"); // no explicit channel -> default
        assert_eq!(f.receive_id, "ou_zhang");
        assert_eq!(f.receive_id_type, "open_id");
    }

    #[test]
    fn routes_by_chat_id_when_name_missing() {
        let cfg = cfg(CFG);
        // Name lookup failed -> empty chat_name, but chat_id matches.
        let f = forward_for(&cfg, &message("", "oc_greenfield", "group", &[])).unwrap();
        assert_eq!(f.thread, "thread-xxx");
    }

    #[test]
    fn drops_unrouted_chat() {
        let cfg = cfg(CFG);
        assert!(forward_for(&cfg, &message("other", "oc_x", "group", &[])).is_none());
    }

    #[test]
    fn mentions_filter_requires_mention() {
        let cfg = cfg(r#"
name = "feishu"
app_id = "a"
app_secret = "b"

[[routes]]
chat_name = "greenfield"
thread = "t"
mentions = ["jyc"]
"#);
        // Not @-mentioned -> dropped.
        assert!(
            forward_for(
                &cfg,
                &message("greenfield", "oc", "group", &[("ou_user", "user")])
            )
            .is_none()
        );
        // @-mentioned by name -> forwarded.
        let f = forward_for(
            &cfg,
            &message("greenfield", "oc", "group", &[("ou_bot", "jyc")]),
        )
        .unwrap();
        assert_eq!(f.thread, "t");
        // @-mentioned by id -> forwarded.
        assert!(
            forward_for(
                &cfg,
                &message("greenfield", "oc", "group", &[("jyc", "bot")])
            )
            .is_some()
        );
    }
}
