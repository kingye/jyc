//! WeCom Smart Robot (wecom_bot) inbound adapter implementation.
//!
//! Handles receiving messages from WeCom via WebSocket long connection
//! and provides channel-specific pattern matching and thread name derivation.
//!
//! Reference: doc 101463, 100719

use std::collections::HashMap;

use anyhow::Result;
use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

use jyc_types::{
    ChannelMatcher, ChannelPattern, InboundAdapterOptions, InboundMessage, MessageAttachment,
    MessageContent, PatternMatch, WecomBotConfig,
};
use jyc_utils::helpers::sanitize_for_filesystem;

use super::client::{ServerMessage, WecomBotWsClient};
use super::types::BotEvent;

/// WeCom Bot-specific pattern matching and thread name derivation.
pub struct WecomBotMatcher;

impl ChannelMatcher for WecomBotMatcher {
    fn channel_type(&self) -> &str {
        "wecom_bot"
    }

    fn derive_thread_name(
        &self,
        message: &InboundMessage,
        _patterns: &[ChannelPattern],
        _pattern_match: Option<&PatternMatch>,
    ) -> String {
        // Group chat: bot-{chatid}
        // Single chat: bot-{userid}
        let chat_type = message
            .metadata
            .get("chattype")
            .and_then(|v| v.as_str())
            .unwrap_or("single");

        if chat_type == "groupchat"
            && let Some(chatid) = message.metadata.get("chatid").and_then(|v| v.as_str())
        {
            return format!("bot-{}", sanitize_for_filesystem(chatid));
        }

        // Single chat or fallback
        if let Some(userid) = message.metadata.get("userid").and_then(|v| v.as_str()) {
            format!("bot-{}", sanitize_for_filesystem(userid))
        } else {
            format!("bot-{}", sanitize_for_filesystem(&message.channel_uid))
        }
    }

    fn match_message(
        &self,
        message: &InboundMessage,
        patterns: &[ChannelPattern],
    ) -> Option<PatternMatch> {
        wecom_bot_match_message(message, patterns)
    }

    /// WeCom Bot stores all messages for full conversation context, even if unmatched.
    fn store_unmatched_messages(&self) -> bool {
        true
    }
}

/// Match a message against wecom_bot-specific patterns.
///
/// Rules within a pattern use AND logic — all present rules must match.
/// Within each rule, sub-values use OR logic.
/// Returns the first matching pattern.
pub fn wecom_bot_match_message(
    message: &InboundMessage,
    patterns: &[ChannelPattern],
) -> Option<PatternMatch> {
    for pattern in patterns {
        if !pattern.enabled {
            continue;
        }

        let mut matches = true;
        let mut match_details = HashMap::new();

        // --- Keywords rule ---
        if matches && let Some(ref keywords) = pattern.rules.keywords {
            let body = message
                .content
                .text
                .as_deref()
                .or(message.content.markdown.as_deref())
                .unwrap_or("")
                .to_lowercase();

            let keyword_matches = keywords.iter().any(|kw| body.contains(&kw.to_lowercase()));

            if !keyword_matches {
                matches = false;
            } else {
                let matched_kw: Vec<&str> = keywords
                    .iter()
                    .filter(|kw| body.contains(&kw.to_lowercase()))
                    .map(|s| s.as_str())
                    .collect();
                match_details.insert("keywords".to_string(), matched_kw.join(","));
            }
        }

        // --- Sender rule (shared) ---
        // WeCom Bot uses sender_address as the user's userid
        if matches && let Some(ref sender_rule) = pattern.rules.sender {
            let addr = message.sender_address.to_lowercase();

            let sender_matches = {
                let mut any_rule_present = false;
                let mut any_rule_matched = false;

                if let Some(ref exact_addrs) = sender_rule.exact {
                    any_rule_present = true;
                    if exact_addrs.iter().any(|e| e.to_lowercase() == addr) {
                        any_rule_matched = true;
                        match_details.insert("sender.exact".to_string(), addr.clone());
                    }
                }

                if let Some(ref regex_str) = sender_rule.regex {
                    any_rule_present = true;
                    if let Ok(re) = regex::Regex::new(regex_str)
                        && re.is_match(&addr)
                    {
                        any_rule_matched = true;
                        match_details.insert("sender.regex".to_string(), addr.clone());
                    }
                }

                !any_rule_present || any_rule_matched
            };

            if !sender_matches {
                matches = false;
            }
        }

        if matches {
            return Some(PatternMatch {
                pattern_name: pattern.name.clone(),
                channel: "wecom_bot".to_string(),
                matches: match_details,
            });
        }
    }

    None
}

/// WeCom Bot inbound adapter for receiving messages via WebSocket.
pub struct WecomBotInboundAdapter {
    config: WecomBotConfig,
    channel_name: String,
    #[allow(dead_code)]
    workspace_root: std::path::PathBuf,
    /// Shared sender Arc for outbound adapter
    sender_arc: Option<
        std::sync::Arc<tokio::sync::Mutex<Option<tokio::sync::mpsc::UnboundedSender<String>>>>,
    >,
}

impl WecomBotInboundAdapter {
    /// Create a new WeCom Bot inbound adapter.
    pub fn new(config: &WecomBotConfig, channel_name: String) -> Self {
        let workspace_root =
            std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));

        Self {
            config: config.clone(),
            channel_name,
            workspace_root,
            sender_arc: None,
        }
    }

    /// Create a new adapter with a shared sender Arc for outbound adapter.
    pub fn with_shared_sender(
        config: &WecomBotConfig,
        channel_name: String,
        sender_arc: std::sync::Arc<
            tokio::sync::Mutex<Option<tokio::sync::mpsc::UnboundedSender<String>>>,
        >,
    ) -> Self {
        let workspace_root =
            std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));

        Self {
            config: config.clone(),
            channel_name,
            workspace_root,
            sender_arc: Some(sender_arc),
        }
    }

    /// Create a new adapter with custom workspace root.
    #[allow(dead_code)]
    pub fn new_with_workspace(
        config: &WecomBotConfig,
        channel_name: String,
        workspace_root: std::path::PathBuf,
    ) -> Self {
        Self {
            config: config.clone(),
            channel_name,
            workspace_root,
            sender_arc: None,
        }
    }

    /// Create a new adapter with custom workspace root and shared sender.
    #[allow(dead_code)]
    pub fn with_workspace_and_sender(
        config: &WecomBotConfig,
        channel_name: String,
        workspace_root: std::path::PathBuf,
        sender_arc: std::sync::Arc<
            tokio::sync::Mutex<Option<tokio::sync::mpsc::UnboundedSender<String>>>,
        >,
    ) -> Self {
        Self {
            config: config.clone(),
            channel_name,
            workspace_root,
            sender_arc: Some(sender_arc),
        }
    }
}

impl ChannelMatcher for WecomBotInboundAdapter {
    fn channel_type(&self) -> &str {
        "wecom_bot"
    }

    fn derive_thread_name(
        &self,
        message: &InboundMessage,
        patterns: &[ChannelPattern],
        pattern_match: Option<&PatternMatch>,
    ) -> String {
        WecomBotMatcher.derive_thread_name(message, patterns, pattern_match)
    }

    fn match_message(
        &self,
        message: &InboundMessage,
        patterns: &[ChannelPattern],
    ) -> Option<PatternMatch> {
        wecom_bot_match_message(message, patterns)
    }

    fn store_unmatched_messages(&self) -> bool {
        true
    }
}

#[async_trait]
impl jyc_types::InboundAdapter for WecomBotInboundAdapter {
    async fn start(&self, options: InboundAdapterOptions, cancel: CancellationToken) -> Result<()> {
        let (raw_tx, mut raw_rx) = tokio::sync::mpsc::unbounded_channel::<ServerMessage>();
        let channel_name = self.channel_name.clone();
        let config = self.config.clone();
        let shared_sender = self.sender_arc.clone();
        let ws_cancel = cancel.child_token();

        // Spawn WebSocket client task that forwards raw messages to the channel
        let ws_handle = tokio::spawn(async move {
            let mut ws_client = WecomBotWsClient::new(config);

            let on_ws_message = move |msg: ServerMessage| -> Result<()> {
                if raw_tx.send(msg).is_err() {
                    // Receiver dropped; WebSocket will reconnect or exit
                }
                Ok(())
            };

            let on_connect = move |sender: tokio::sync::mpsc::UnboundedSender<String>| {
                if let Some(ref shared) = shared_sender {
                    let shared = shared.clone();
                    tokio::spawn(async move {
                        let mut guard = shared.lock().await;
                        *guard = Some(sender);
                        tracing::info!("WeCom Bot outbound sender updated");
                    });
                }
            };

            ws_client
                .run(&on_ws_message, Some(&on_connect), &ws_cancel)
                .await
        });

        // Process messages from the channel with async attachment downloading
        let mut process_error = None;
        while let Some(msg) = raw_rx.recv().await {
            if cancel.is_cancelled() {
                break;
            }

            match msg {
                ServerMessage::Message(bot_msg) => {
                    let mut inbound = match convert_bot_message_to_inbound(&bot_msg, &channel_name)
                    {
                        Ok(i) => i,
                        Err(e) => {
                            tracing::warn!(
                                msgid = %bot_msg.msgid,
                                error = %e,
                                "Failed to convert BotMessage to InboundMessage"
                            );
                            continue;
                        }
                    };

                    // Download and decrypt attachments
                    match super::media::process_bot_attachments(&bot_msg).await {
                        Ok(attachments) => {
                            if !attachments.is_empty() {
                                inbound.attachments = attachments;
                                tracing::info!(
                                    msgid = %bot_msg.msgid,
                                    count = inbound.attachments.len(),
                                    "WeCom Bot attachments downloaded"
                                );
                            }
                        }
                        Err(e) => {
                            tracing::warn!(
                                msgid = %bot_msg.msgid,
                                error = %e,
                                "Failed to download WeCom Bot attachments"
                            );
                        }
                    }

                    if let Err(e) = (options.on_message)(inbound) {
                        process_error = Some(e);
                        break;
                    }
                }
                ServerMessage::Event(event) => {
                    if let Err(e) = handle_bot_event(&event, &channel_name, &options) {
                        process_error = Some(e);
                        break;
                    }
                }
            }
        }

        // Wait for WebSocket task to finish regardless of processing result
        let ws_result = ws_handle.await;

        if let Some(e) = process_error {
            return Err(e);
        }
        match ws_result {
            Ok(Ok(())) => {
                tracing::info!("WeCom Bot WebSocket stopped cleanly");
                Ok(())
            }
            Ok(Err(e)) => {
                tracing::error!(error = %e, "WeCom Bot WebSocket error");
                Err(e)
            }
            Err(e) => {
                tracing::error!(error = %e, "WeCom Bot WebSocket task panicked");
                Err(anyhow::anyhow!("WebSocket task panicked: {e}"))
            }
        }
    }
}

/// Convert a BotMessage to an InboundMessage.
fn convert_bot_message_to_inbound(
    bot_msg: &super::types::BotMessage,
    channel_name: &str,
) -> Result<InboundMessage> {
    let (text, attachments) = extract_content(bot_msg)?;

    let timestamp = if bot_msg.msgtime > 0 {
        chrono::DateTime::from_timestamp_millis(bot_msg.msgtime).unwrap_or_else(chrono::Utc::now)
    } else {
        chrono::Utc::now()
    };

    let mut metadata = HashMap::new();
    metadata.insert(
        "chatid".to_string(),
        serde_json::Value::String(bot_msg.chatid.clone()),
    );
    metadata.insert(
        "chattype".to_string(),
        serde_json::Value::String(bot_msg.chattype.clone()),
    );
    metadata.insert(
        "userid".to_string(),
        serde_json::Value::String(bot_msg.from.userid.clone()),
    );
    metadata.insert(
        "req_id".to_string(),
        serde_json::Value::String(bot_msg.req_id.clone()),
    );
    metadata.insert(
        "msgtype".to_string(),
        serde_json::Value::String(bot_msg.msgtype.clone()),
    );
    metadata.insert(
        "aibotid".to_string(),
        serde_json::Value::String(bot_msg.aibotid.clone()),
    );

    // For group chats, include chatid in the channel_uid (used as reply target)
    let channel_uid = if bot_msg.chattype == "groupchat" && !bot_msg.chatid.is_empty() {
        bot_msg.chatid.clone()
    } else {
        bot_msg.from.userid.clone()
    };

    Ok(InboundMessage {
        id: uuid::Uuid::new_v4().to_string(),
        channel: channel_name.to_string(),
        channel_uid,
        sender: bot_msg.from.userid.clone(),
        sender_address: bot_msg.from.userid.clone(),
        recipients: vec![],
        topic: if bot_msg.chattype == "groupchat" {
            format!("Group {}", bot_msg.chatid)
        } else {
            format!("User {}", bot_msg.from.userid)
        },
        content: MessageContent {
            text: Some(text),
            html: None,
            markdown: None,
        },
        timestamp,
        thread_refs: None,
        reply_to_id: None,
        external_id: Some(bot_msg.msgid.clone()),
        attachments,
        metadata,
        matched_pattern: None,
    })
}

/// Extract text content and attachments from a BotMessage.
fn extract_content(bot_msg: &super::types::BotMessage) -> Result<(String, Vec<MessageAttachment>)> {
    match bot_msg.msgtype.as_str() {
        "text" => {
            let content = bot_msg
                .text
                .as_ref()
                .map(|t| t.content.clone())
                .unwrap_or_default();
            Ok((content, vec![]))
        }
        "image" => {
            let content = bot_msg.image.as_ref();
            let text = content
                .map(|i| format!("[Image: {}]", i.url))
                .unwrap_or_else(|| "[Image]".to_string());
            // Note: AES decryption of image requires additional implementation
            Ok((text, vec![]))
        }
        "mixed" => {
            let mut text_parts = Vec::new();
            if let Some(ref mixed) = bot_msg.mixed {
                for item in &mixed.items {
                    match item.item_type.as_str() {
                        "text" => {
                            if let Some(ref content) = item.content {
                                text_parts.push(content.clone());
                            }
                        }
                        "image" => {
                            if let Some(ref url) = item.url {
                                text_parts.push(format!("[Image: {}]", url));
                            }
                        }
                        other => text_parts.push(format!("[Mixed item: {}]", other)),
                    }
                }
            }
            Ok((text_parts.join(" "), vec![]))
        }
        "voice" => {
            let content = bot_msg.voice.as_ref();
            let text = content
                .map(|v| format!("[Voice: {}]", v.url))
                .unwrap_or_else(|| "[Voice]".to_string());
            Ok((text, vec![]))
        }
        "file" => {
            let content = bot_msg.file.as_ref();
            let text = content
                .map(|f| format!("[File: {} - {}]", f.filename, f.url))
                .unwrap_or_else(|| "[File]".to_string());
            Ok((text, vec![]))
        }
        "video" => {
            let content = bot_msg.video.as_ref();
            let text = content
                .map(|v| format!("[Video: {}]", v.url))
                .unwrap_or_else(|| "[Video]".to_string());
            Ok((text, vec![]))
        }
        other => Ok((format!("[Unsupported message type: {}]", other), vec![])),
    }
}

/// Handle a bot event.
fn handle_bot_event(
    event: &BotEvent,
    _channel_name: &str,
    options: &InboundAdapterOptions,
) -> Result<()> {
    match event.event.as_str() {
        "enter_chat" => {
            tracing::info!(
                chatid = %event.chatid,
                "User entered WeCom Bot chat"
            );
            // Thread close callback is for chat disbanded, not enter_chat
            // We could extend this to create a welcome message thread in the future
        }
        "template_card_event" => {
            tracing::debug!("Received template_card_event");
        }
        "feedback_event" => {
            tracing::debug!("Received feedback_event");
        }
        "disconnected_event" => {
            tracing::warn!(
                chatid = %event.chatid,
                "Received disconnected_event"
            );
        }
        other => {
            tracing::debug!(event = %other, "Unknown WeCom Bot event");
        }
    }

    // Call on_thread_close if this is a thread close event (none currently)
    if let Some(ref callback) = options.on_thread_close
        && event.event == "chat_disbanded"
    {
        // Not documented in WeCom Bot events, but handle defensively
        let thread_name = format!("bot-{}", sanitize_for_filesystem(&event.chatid));
        callback(thread_name)?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::types::{BotMessage, SenderInfo, TextContent};
    use super::*;

    fn create_test_message(msgtype: &str, content: &str) -> BotMessage {
        BotMessage {
            msgid: "msg_123".to_string(),
            aibotid: "bot_xxx".to_string(),
            chatid: "chat_456".to_string(),
            chattype: "single".to_string(),
            from: SenderInfo {
                userid: "user_789".to_string(),
            },
            msgtime: 1704067200000,
            msgtype: msgtype.to_string(),
            req_id: "req_abc".to_string(),
            servertime: 1704067200000,
            text: if msgtype == "text" {
                Some(TextContent {
                    content: content.to_string(),
                })
            } else {
                None
            },
            image: None,
            mixed: None,
            voice: None,
            file: None,
            video: None,
        }
    }

    #[test]
    fn test_convert_text_message() {
        let bot_msg = create_test_message("text", "Hello bot");
        let inbound = convert_bot_message_to_inbound(&bot_msg, "test_channel").unwrap();

        assert_eq!(inbound.channel, "test_channel");
        assert_eq!(inbound.channel_uid, "user_789"); // single chat
        assert_eq!(inbound.sender, "user_789");
        assert_eq!(inbound.sender_address, "user_789");
        assert_eq!(inbound.content.text.as_ref().unwrap(), "Hello bot");
        assert_eq!(
            inbound.metadata.get("chattype").unwrap().as_str().unwrap(),
            "single"
        );
        assert_eq!(
            inbound.metadata.get("req_id").unwrap().as_str().unwrap(),
            "req_abc"
        );
    }

    #[test]
    fn test_convert_group_message() {
        let mut bot_msg = create_test_message("text", "Hello group");
        bot_msg.chattype = "groupchat".to_string();
        let inbound = convert_bot_message_to_inbound(&bot_msg, "test_channel").unwrap();

        assert_eq!(inbound.channel_uid, "chat_456"); // group chat uses chatid
        assert_eq!(inbound.topic, "Group chat_456");
    }

    #[test]
    fn test_derive_thread_name_single() {
        let bot_msg = create_test_message("text", "Hello");
        let inbound = convert_bot_message_to_inbound(&bot_msg, "test").unwrap();
        let matcher = WecomBotMatcher;
        let name = matcher.derive_thread_name(&inbound, &[], None);
        assert_eq!(name, "bot-user_789");
    }

    #[test]
    fn test_derive_thread_name_group() {
        let mut bot_msg = create_test_message("text", "Hello");
        bot_msg.chattype = "groupchat".to_string();
        let inbound = convert_bot_message_to_inbound(&bot_msg, "test").unwrap();
        let matcher = WecomBotMatcher;
        let name = matcher.derive_thread_name(&inbound, &[], None);
        assert_eq!(name, "bot-chat_456");
    }

    #[test]
    fn test_match_message_keywords() {
        let bot_msg = create_test_message("text", "Hello world test");
        let inbound = convert_bot_message_to_inbound(&bot_msg, "test").unwrap();

        let pattern = ChannelPattern {
            name: "test_pattern".to_string(),
            channel: "wecom_bot".to_string(),
            enabled: true,
            rules: jyc_types::PatternRules {
                keywords: Some(vec!["world".to_string()]),
                ..Default::default()
            },
            attachments: None,
            template: None,
            thread_name: None,
            thread_prefix: None,
            role: None,
            live_injection: true,
            repo_group: None,
            inject_inbound_images: false,
            model: None,
            small_model: None,
            mcps: None,
            disabled_tools: None,
            disabled_builtin_tools: None,
            disabled_mcp_servers: None,
        };

        let matcher = WecomBotMatcher;
        let result = matcher.match_message(&inbound, &[pattern]);
        assert!(result.is_some());
        assert_eq!(result.unwrap().pattern_name, "test_pattern");
    }
}
