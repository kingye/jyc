//! Feishu inbound adapter implementation.
//!
//! This module handles receiving messages from Feishu via WebSocket connections
//! and provides channel-specific pattern matching and thread name derivation.

use anyhow::Result;
use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

use crate::channels::types::{
    ChannelMatcher, ChannelPattern, InboundAdapterOptions, InboundMessage, PatternMatch,
};
use crate::config::types::InboundAttachmentConfig;
use crate::utils::helpers::sanitize_for_filesystem;

use super::client::FeishuClient;
use super::config::FeishuConfig;
use super::websocket::FeishuWebSocket;

/// Feishu-specific pattern matching and thread name derivation.
///
/// Stateless struct implementing `ChannelMatcher` — can be cheaply created
/// wherever feishu pattern matching is needed (e.g., on_message callbacks).
///
/// Supports:
/// - `mentions`: match messages where specific bot/user IDs are @-mentioned
/// - `keywords`: match messages containing specific words (case-insensitive)
/// - `sender`: match sender by exact address, domain, or regex (shared with email)
///
/// All present rules use AND logic. Within each rule, sub-values use OR logic.
pub struct FeishuMatcher;

impl ChannelMatcher for FeishuMatcher {
    fn channel_type(&self) -> &str {
        "feishu"
    }

    fn derive_thread_name(
        &self,
        message: &InboundMessage,
        _patterns: &[ChannelPattern],
        _pattern_match: Option<&PatternMatch>,
    ) -> String {
        // Prefer readable names from metadata (populated by WebSocket name lookups)
        // Group chat: use chat name directly (e.g., "self-hosting-jyc")
        // P2P: use sender display name (e.g., "Zhang San")
        // Fallback: use opaque IDs with prefix
        if let Some(chat_name) = message.metadata.get("chat_name").and_then(|v| v.as_str()) {
            if !chat_name.is_empty() {
                return sanitize_for_filesystem(chat_name);
            }
        }

        let chat_type = message.metadata.get("chat_type").and_then(|v| v.as_str()).unwrap_or("");
        if chat_type == "p2p" {
            if let Some(sender_name) = message.metadata.get("sender_name").and_then(|v| v.as_str()) {
                if !sender_name.is_empty() {
                    return sanitize_for_filesystem(sender_name);
                }
            }
        }

        // Fallback to opaque IDs with prefix (if name API calls failed)
        if let Some(chat_id) = message.metadata.get("chat_id").and_then(|v| v.as_str()) {
            format!("feishu_chat_{}", chat_id)
        } else if let Some(user_id) = message.metadata.get("user_id").and_then(|v| v.as_str()) {
            format!("feishu_user_{}", user_id)
        } else {
            format!("feishu_{}", message.channel_uid)
        }
    }

    fn match_message(
        &self,
        message: &InboundMessage,
        patterns: &[ChannelPattern],
    ) -> Option<PatternMatch> {
        feishu_match_message(message, patterns)
    }

    /// Feishu stores all messages for full conversation context, even if unmatched.
    fn store_unmatched_messages(&self) -> bool {
        true
    }
}

/// Match a message against feishu-specific patterns.
///
/// Rules within a pattern use AND logic — all present rules must match.
/// Within each rule, sub-values use OR logic (e.g., any mention ID matches).
/// Returns the first matching pattern.
pub fn feishu_match_message(
    message: &InboundMessage,
    patterns: &[ChannelPattern],
) -> Option<PatternMatch> {
    for pattern in patterns {
        if !pattern.enabled {
            continue;
        }

        let mut matches = true;
        let mut match_details = HashMap::new();

        // --- Mentions rule ---
        // Check if any of the configured mention values match the message's mentions.
        // Supports matching by user_id (e.g., "ou_xxxxx") or display name (e.g., "jyc").
        //
        // The metadata["mentions"] can be:
        // - Array of strings: ["ou_xxx", "ou_yyy"] — matched as IDs
        // - Array of objects: [{"id": "ou_xxx", "name": "jyc"}] — matched by id OR name
        if let Some(ref mention_ids) = pattern.rules.mentions {
            let mentions_val = message.metadata.get("mentions").and_then(|v| v.as_array());

            let mention_matches = if let Some(arr) = mentions_val {
                // Collect all matchable strings from the mentions array
                let mut matchable: Vec<String> = Vec::new();
                for item in arr {
                    if let Some(s) = item.as_str() {
                        // Flat string format: just an ID
                        matchable.push(s.to_lowercase());
                    } else if let Some(obj) = item.as_object() {
                        // Object format: {"id": "ou_xxx", "name": "jyc"}
                        if let Some(id) = obj.get("id").and_then(|v| v.as_str()) {
                            matchable.push(id.to_lowercase());
                        }
                        if let Some(name) = obj.get("name").and_then(|v| v.as_str()) {
                            matchable.push(name.to_lowercase());
                        }
                    }
                }

                // Check if any configured mention value matches (case-insensitive)
                mention_ids.iter().any(|configured| {
                    let lower = configured.to_lowercase();
                    matchable.iter().any(|m| *m == lower)
                })
            } else {
                false
            };

            if !mention_matches {
                matches = false;
            } else {
                let display: Vec<String> = mentions_val
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|v| {
                                v.as_str().map(|s| s.to_string()).or_else(|| {
                                    v.as_object().and_then(|o| {
                                        o.get("name")
                                            .and_then(|n| n.as_str())
                                            .or_else(|| o.get("id").and_then(|i| i.as_str()))
                                            .map(|s| s.to_string())
                                    })
                                })
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                match_details.insert("mentions".to_string(), display.join(","));
            }
        }

        // --- Keywords rule ---
        // Check if the message body contains any of the configured keywords
        if matches {
            if let Some(ref keywords) = pattern.rules.keywords {
                let body = message
                    .content
                    .text
                    .as_deref()
                    .or(message.content.markdown.as_deref())
                    .unwrap_or("")
                    .to_lowercase();

                let keyword_matches = keywords
                    .iter()
                    .any(|kw| body.contains(&kw.to_lowercase()));

                if !keyword_matches {
                    matches = false;
                } else {
                    let matched_kw: Vec<&str> = keywords
                        .iter()
                        .filter(|kw| body.contains(&kw.to_lowercase()))
                        .map(|s| s.as_str())
                        .collect();
                    match_details.insert(
                        "keywords".to_string(),
                        matched_kw.join(","),
                    );
                }
            }
        }

        // --- Chat name rule ---
        // Check if the message's group chat name matches any configured name (case-insensitive)
        if matches {
            if let Some(ref chat_names) = pattern.rules.chat_name {
                let msg_chat_name = message
                    .metadata
                    .get("chat_name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_lowercase();

                let chat_name_matches = chat_names
                    .iter()
                    .any(|cn| msg_chat_name.starts_with(&cn.to_lowercase()));

                if !chat_name_matches {
                    matches = false;
                } else {
                    match_details.insert(
                        "chat_name".to_string(),
                        msg_chat_name,
                    );
                }
            }
        }

        // --- Sender rule (shared) ---
        // Feishu uses sender_address as the user's open_id
        if matches {
            if let Some(ref sender_rule) = pattern.rules.sender {
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
                        if let Ok(re) = regex::Regex::new(regex_str) {
                            if re.is_match(&addr) {
                                any_rule_matched = true;
                                match_details.insert("sender.regex".to_string(), addr.clone());
                            }
                        }
                    }

                    !any_rule_present || any_rule_matched
                };

                if !sender_matches {
                    matches = false;
                }
            }
        }

        if matches {
            return Some(PatternMatch {
                pattern_name: pattern.name.clone(),
                channel: "feishu".to_string(),
                matches: match_details,
            });
        }
    }

    None
}

/// Feishu inbound adapter for receiving messages via WebSocket.
pub struct FeishuInboundAdapter {
    config: FeishuConfig,
    /// Channel name from config (e.g., "feishu_bot")
    channel_name: String,
    /// Workspace root path (e.g., "/home/jiny/projects/jyc-data")
    workspace_root: std::path::PathBuf,
}

impl FeishuInboundAdapter {
    /// Create a new Feishu inbound adapter.
    pub fn new(config: &FeishuConfig, channel_name: String) -> Self {
        // Determine workspace root from current working directory
        let workspace_root = std::env::current_dir()
            .unwrap_or_else(|_| std::path::PathBuf::from("."));
        
        Self {
            config: config.clone(),
            channel_name,
            workspace_root,
        }
    }
    
    /// Create a new Feishu inbound adapter with custom workspace root.
    #[allow(dead_code)]
    pub fn new_with_workspace(config: &FeishuConfig, channel_name: String, workspace_root: std::path::PathBuf) -> Self {
        Self {
            config: config.clone(),
            channel_name,
            workspace_root,
        }
    }
}

impl ChannelMatcher for FeishuInboundAdapter {
    fn channel_type(&self) -> &str {
        "feishu"
    }

    fn derive_thread_name(
        &self,
        message: &InboundMessage,
        patterns: &[ChannelPattern],
        pattern_match: Option<&PatternMatch>,
    ) -> String {
        // Delegate to the stateless FeishuMatcher
        FeishuMatcher.derive_thread_name(message, patterns, pattern_match)
    }

    fn match_message(
        &self,
        message: &InboundMessage,
        patterns: &[ChannelPattern],
    ) -> Option<PatternMatch> {
        feishu_match_message(message, patterns)
    }
}

impl FeishuInboundAdapter {
    /// Save attachments to thread directory.
    /// This method calculates the thread name and saves attachments.
    pub async fn save_attachments_to_thread_directory(
        &self,
        message: &mut InboundMessage,
        patterns: &[ChannelPattern],
        attachment_config: Option<&InboundAttachmentConfig>,
    ) -> Result<()> {
        let thread_name = FeishuMatcher.derive_thread_name(message, patterns, None);
        crate::core::attachment_storage::save_attachments_to_thread_directory(
            message,
            &self.workspace_root,
            &self.channel_name,
            &thread_name,
            attachment_config,
        )
        .await
    }
}

#[async_trait]
impl crate::channels::types::InboundAdapter for FeishuInboundAdapter {
    async fn start(
        &self,
        options: InboundAdapterOptions,
        cancel: CancellationToken,
    ) -> Result<()> {
        if !self.config.websocket.enabled {
            tracing::info!("Feishu WebSocket disabled, holding channel alive until cancel");
            cancel.cancelled().await;
            return Ok(());
        }

        let client = Arc::new(FeishuClient::new(self.config.clone()));
        client.initialize().await.ok(); // Pre-warm for name lookups

        let mut ws = FeishuWebSocket::new_with_attachments(
            &self.config,
            client,
            options.attachment_config.clone(),
        );
        let channel_name = self.channel_name.clone();

        loop {
            tracing::info!("Starting Feishu WebSocket connection...");

            match ws.run(&channel_name, &*options.on_message, &cancel).await {
                Ok(()) => {
                    // Clean exit (cancelled)
                    tracing::info!("Feishu WebSocket stopped cleanly");
                    break;
                }
                Err(e) => {
                    if cancel.is_cancelled() {
                        tracing::info!("Feishu WebSocket shutting down (cancelled)");
                        break;
                    }
                    tracing::error!(error = %e, "Feishu WebSocket error");

                    if !ws.handle_reconnection().await {
                        tracing::error!("Max reconnection attempts reached, stopping Feishu channel");
                        break;
                    }
                    // Loop continues → reconnect
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::channels::types::{MessageContent, PatternRules, SenderRule};
    use chrono::Utc;

    fn make_feishu_message(
        sender_addr: &str,
        body: &str,
        mentions: Vec<&str>,
        chat_id: Option<&str>,
    ) -> InboundMessage {
        let mut metadata = HashMap::new();
        if !mentions.is_empty() {
            let mentions_val: Vec<serde_json::Value> = mentions
                .iter()
                .map(|m| serde_json::Value::String(m.to_string()))
                .collect();
            metadata.insert(
                "mentions".to_string(),
                serde_json::Value::Array(mentions_val),
            );
        }
        if let Some(cid) = chat_id {
            metadata.insert(
                "chat_id".to_string(),
                serde_json::Value::String(cid.to_string()),
            );
        }

        InboundMessage {
            id: "test".to_string(),
            channel: "feishu".to_string(),
            channel_uid: "msg_001".to_string(),
            sender: "Test User".to_string(),
            sender_address: sender_addr.to_string(),
            recipients: vec![],
            topic: "".to_string(),
            content: MessageContent {
                text: Some(body.to_string()),
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

    fn make_feishu_pattern(
        name: &str,
        mentions: Option<Vec<String>>,
        keywords: Option<Vec<String>>,
        sender: Option<SenderRule>,
    ) -> ChannelPattern {
        ChannelPattern {
            name: name.to_string(),
            channel: "feishu".to_string(),
            enabled: true,
            rules: PatternRules {
                sender,
                subject: None,
                mentions,
                keywords,
                chat_name: None,
            },
            attachments: None,
            ..Default::default()
        }
    }

    #[test]
    fn test_match_by_mentions() {
        let msg = make_feishu_message("user1", "Hello", vec!["bot_abc"], None);
        let patterns = vec![make_feishu_pattern(
            "mention_bot",
            Some(vec!["bot_abc".to_string()]),
            None,
            None,
        )];

        let result = feishu_match_message(&msg, &patterns);
        assert!(result.is_some());
        assert_eq!(result.unwrap().pattern_name, "mention_bot");
    }

    #[test]
    fn test_no_match_wrong_mention() {
        let msg = make_feishu_message("user1", "Hello", vec!["other_bot"], None);
        let patterns = vec![make_feishu_pattern(
            "mention_bot",
            Some(vec!["bot_abc".to_string()]),
            None,
            None,
        )];

        assert!(feishu_match_message(&msg, &patterns).is_none());
    }

    #[test]
    fn test_no_match_no_mentions_in_message() {
        let msg = make_feishu_message("user1", "Hello", vec![], None);
        let patterns = vec![make_feishu_pattern(
            "mention_bot",
            Some(vec!["bot_abc".to_string()]),
            None,
            None,
        )];

        assert!(feishu_match_message(&msg, &patterns).is_none());
    }

    #[test]
    fn test_match_by_keywords() {
        let msg = make_feishu_message("user1", "I need 帮助 with this", vec![], None);
        let patterns = vec![make_feishu_pattern(
            "help_pattern",
            None,
            Some(vec!["帮助".to_string(), "help".to_string()]),
            None,
        )];

        let result = feishu_match_message(&msg, &patterns);
        assert!(result.is_some());
        assert_eq!(result.unwrap().pattern_name, "help_pattern");
    }

    #[test]
    fn test_match_keywords_case_insensitive() {
        let msg = make_feishu_message("user1", "I need HELP", vec![], None);
        let patterns = vec![make_feishu_pattern(
            "help_pattern",
            None,
            Some(vec!["help".to_string()]),
            None,
        )];

        assert!(feishu_match_message(&msg, &patterns).is_some());
    }

    #[test]
    fn test_no_match_wrong_keywords() {
        let msg = make_feishu_message("user1", "Just chatting", vec![], None);
        let patterns = vec![make_feishu_pattern(
            "help_pattern",
            None,
            Some(vec!["帮助".to_string(), "help".to_string()]),
            None,
        )];

        assert!(feishu_match_message(&msg, &patterns).is_none());
    }

    #[test]
    fn test_match_mentions_and_keywords_and_logic() {
        // Both mentions AND keywords must match
        let msg = make_feishu_message("user1", "Help me please", vec!["bot_abc"], None);
        let patterns = vec![make_feishu_pattern(
            "both",
            Some(vec!["bot_abc".to_string()]),
            Some(vec!["help".to_string()]),
            None,
        )];

        assert!(feishu_match_message(&msg, &patterns).is_some());

        // Mentions match but keywords don't → no match
        let msg2 = make_feishu_message("user1", "Random text", vec!["bot_abc"], None);
        assert!(feishu_match_message(&msg2, &patterns).is_none());

        // Keywords match but mentions don't → no match
        let msg3 = make_feishu_message("user1", "Help me", vec!["other_bot"], None);
        assert!(feishu_match_message(&msg3, &patterns).is_none());
    }

    #[test]
    fn test_disabled_pattern_skipped() {
        let msg = make_feishu_message("user1", "Hello", vec!["bot_abc"], None);
        let mut pattern = make_feishu_pattern(
            "mention_bot",
            Some(vec!["bot_abc".to_string()]),
            None,
            None,
        );
        pattern.enabled = false;

        assert!(feishu_match_message(&msg, &[pattern]).is_none());
    }

    #[test]
    fn test_first_pattern_wins() {
        let msg = make_feishu_message("user1", "help me", vec!["bot_abc"], None);
        let patterns = vec![
            make_feishu_pattern("first", Some(vec!["bot_abc".to_string()]), None, None),
            make_feishu_pattern("second", None, Some(vec!["help".to_string()]), None),
        ];

        let result = feishu_match_message(&msg, &patterns).unwrap();
        assert_eq!(result.pattern_name, "first");
    }

    #[test]
    fn test_match_by_sender() {
        let msg = make_feishu_message("ou_abc123", "Hello", vec![], None);
        let patterns = vec![make_feishu_pattern(
            "vip_user",
            None,
            None,
            Some(SenderRule {
                exact: Some(vec!["ou_abc123".to_string()]),
                ..Default::default()
            }),
        )];

        assert!(feishu_match_message(&msg, &patterns).is_some());
    }

    #[test]
    fn test_empty_rules_matches_all() {
        // A pattern with no rules matches everything (all rules vacuously true)
        let msg = make_feishu_message("user1", "Hello", vec![], None);
        let patterns = vec![make_feishu_pattern("catch_all", None, None, None)];

        assert!(feishu_match_message(&msg, &patterns).is_some());
    }

    #[test]
    fn test_match_by_chat_name() {
        let mut msg = make_feishu_message("user1", "Hello", vec![], Some("oc_12345"));
        msg.metadata.insert(
            "chat_name".to_string(),
            serde_json::Value::String("self-hosting-jyc".to_string()),
        );
        let mut pattern = make_feishu_pattern("private_group", None, None, None);
        pattern.rules.chat_name = Some(vec!["self-hosting-jyc".to_string()]);

        let result = feishu_match_message(&msg, &[pattern]);
        assert!(result.is_some());
        assert_eq!(result.unwrap().pattern_name, "private_group");
    }

    #[test]
    fn test_match_by_chat_name_case_insensitive() {
        let mut msg = make_feishu_message("user1", "Hello", vec![], Some("oc_12345"));
        msg.metadata.insert(
            "chat_name".to_string(),
            serde_json::Value::String("Self-Hosting-JYC".to_string()),
        );
        let mut pattern = make_feishu_pattern("private_group", None, None, None);
        pattern.rules.chat_name = Some(vec!["self-hosting-jyc".to_string()]);

        assert!(feishu_match_message(&msg, &[pattern]).is_some());
    }

    #[test]
    fn test_no_match_wrong_chat_name() {
        let mut msg = make_feishu_message("user1", "Hello", vec![], Some("oc_12345"));
        msg.metadata.insert(
            "chat_name".to_string(),
            serde_json::Value::String("other-group".to_string()),
        );
        let mut pattern = make_feishu_pattern("private_group", None, None, None);
        pattern.rules.chat_name = Some(vec!["self-hosting-jyc".to_string()]);

        assert!(feishu_match_message(&msg, &[pattern]).is_none());
    }

    #[test]
    fn test_chat_name_and_mentions_and_logic() {
        let mut msg = make_feishu_message("user1", "Hello", vec!["bot_abc"], Some("oc_12345"));
        msg.metadata.insert(
            "chat_name".to_string(),
            serde_json::Value::String("self-hosting-jyc".to_string()),
        );
        let mut pattern = make_feishu_pattern(
            "both",
            Some(vec!["bot_abc".to_string()]),
            None,
            None,
        );
        pattern.rules.chat_name = Some(vec!["self-hosting-jyc".to_string()]);

        assert!(feishu_match_message(&msg, &[pattern]).is_some());

        // chat_name matches but mentions don't
        let mut msg2 = make_feishu_message("user1", "Hello", vec!["other_bot"], Some("oc_12345"));
        msg2.metadata.insert(
            "chat_name".to_string(),
            serde_json::Value::String("self-hosting-jyc".to_string()),
        );
        let mut pattern2 = make_feishu_pattern(
            "both",
            Some(vec!["bot_abc".to_string()]),
            None,
            None,
        );
        pattern2.rules.chat_name = Some(vec!["self-hosting-jyc".to_string()]);
        assert!(feishu_match_message(&msg2, &[pattern2]).is_none());
    }

    #[test]
    fn test_chat_name_first_pattern_wins() {
        let mut msg = make_feishu_message("user1", "Hello", vec!["bot_abc"], Some("oc_12345"));
        msg.metadata.insert(
            "chat_name".to_string(),
            serde_json::Value::String("self-hosting-jyc".to_string()),
        );
        let mut pattern1 = make_feishu_pattern("private_group", None, None, None);
        pattern1.rules.chat_name = Some(vec!["self-hosting-jyc".to_string()]);

        let pattern2 = make_feishu_pattern(
            "group_mention",
            Some(vec!["bot_abc".to_string()]),
            None,
            None,
        );

        let result = feishu_match_message(&msg, &[pattern1, pattern2]).unwrap();
        assert_eq!(result.pattern_name, "private_group");
    }

    #[test]
    fn test_derive_thread_name_chat_id() {
        let msg = make_feishu_message("user1", "Hello", vec![], Some("oc_12345"));
        let matcher = FeishuMatcher;
        let name = matcher.derive_thread_name(&msg, &[], None);
        assert_eq!(name, "feishu_chat_oc_12345");
    }

    #[test]
    fn test_derive_thread_name_user_id() {
        let mut msg = make_feishu_message("user1", "Hello", vec![], None);
        msg.metadata.insert(
            "user_id".to_string(),
            serde_json::Value::String("ou_abc".to_string()),
        );
        let matcher = FeishuMatcher;
        let name = matcher.derive_thread_name(&msg, &[], None);
        assert_eq!(name, "feishu_user_ou_abc");
    }

    #[test]
    fn test_derive_thread_name_fallback() {
        let msg = make_feishu_message("user1", "Hello", vec![], None);
        let matcher = FeishuMatcher;
        let name = matcher.derive_thread_name(&msg, &[], None);
        assert_eq!(name, "feishu_msg_001");
    }

    #[test]
    fn test_derive_thread_name_chat_name() {
        let mut msg = make_feishu_message("user1", "Hello", vec![], Some("oc_12345"));
        msg.metadata.insert(
            "chat_name".to_string(),
            serde_json::Value::String("Project Alpha".to_string()),
        );
        let matcher = FeishuMatcher;
        let name = matcher.derive_thread_name(&msg, &[], None);
        assert_eq!(name, "Project Alpha");
    }

    #[test]
    fn test_derive_thread_name_p2p_sender_name() {
        let mut msg = make_feishu_message("user1", "Hello", vec![], None);
        msg.metadata.insert(
            "chat_type".to_string(),
            serde_json::Value::String("p2p".to_string()),
        );
        msg.metadata.insert(
            "sender_name".to_string(),
            serde_json::Value::String("Zhang San".to_string()),
        );
        let matcher = FeishuMatcher;
        let name = matcher.derive_thread_name(&msg, &[], None);
        assert_eq!(name, "Zhang San");
    }

    #[test]
    fn test_derive_thread_name_chat_name_with_special_chars() {
        let mut msg = make_feishu_message("user1", "Hello", vec![], Some("oc_12345"));
        msg.metadata.insert(
            "chat_name".to_string(),
            serde_json::Value::String("项目/测试群".to_string()),
        );
        let matcher = FeishuMatcher;
        let name = matcher.derive_thread_name(&msg, &[], None);
        // sanitize_for_filesystem replaces / with _
        assert_eq!(name, "项目_测试群");
    }

    #[tokio::test]
    async fn test_save_attachments_to_thread_directory() -> anyhow::Result<()> {
        use tempfile::tempdir;
        use crate::channels::types::{InboundMessage, MessageAttachment, MessageContent};
        use crate::channels::feishu::config::{FeishuConfig, WebSocketConfig};
        
        // Create a temporary directory for testing
        let temp_dir = tempdir()?;
        
        // Create a simple Feishu config
        let config = FeishuConfig {
            app_id: "test_app_id".to_string(),
            app_secret: "test_app_secret".to_string(),
            base_url: "https://open.feishu.cn".to_string(),
            websocket: WebSocketConfig::default(),
            events: vec!["im.message.receive_v1".to_string()],
            message_format: "text".to_string(),
            metadata: Default::default(),
        };

        // Create the adapter with custom workspace root
        let adapter = FeishuInboundAdapter::new_with_workspace(&config, "feishu".to_string(), temp_dir.path().to_path_buf());

        // Create a test message with attachments
        let mut message = InboundMessage {
            id: "test_message_123".to_string(),
            channel: "feishu".to_string(),
            channel_uid: "msg_123".to_string(),
            sender: "Test User".to_string(),
            sender_address: "user_123".to_string(),
            recipients: vec![],
            topic: "Test".to_string(),
            content: MessageContent {
                text: Some("Test message with attachment".to_string()),
                html: None,
                markdown: None,
            },
            timestamp: chrono::Utc::now(),
            thread_refs: Some(vec!["thread_123".to_string()]),
            reply_to_id: None,
            external_id: Some("ext_123".to_string()),
            attachments: vec![
                MessageAttachment {
                    filename: "test.txt".to_string(),
                    content_type: "text/plain".to_string(),
                    size: 15,
                    content: Some(b"Hello, World!\n".to_vec()),
                    saved_path: None,
                },
                MessageAttachment {
                    filename: "data.json".to_string(),
                    content_type: "application/json".to_string(),
                    size: 25,
                    content: Some(br#"{"test": "data"}"#.to_vec()),
                    saved_path: None,
                },
            ],
            metadata: {
                let mut map = std::collections::HashMap::new();
                map.insert("chat_id".to_string(), serde_json::Value::String("oc_12345".to_string()));
                map
            },
            matched_pattern: None,
        };

        // Create a simple pattern for thread matching
        let patterns = vec![]; // Empty patterns - will use default thread name

        // Save attachments
        adapter.save_attachments_to_thread_directory(&mut message, &patterns, None)
            .await?;

        // Verify attachments were saved
        assert_eq!(message.attachments.len(), 2);
        
        // Check saved_path was set
        for attachment in &message.attachments {
            assert!(attachment.saved_path.is_some());
            
            // Verify file exists
            let saved_path = attachment.saved_path.as_ref().unwrap();
            assert!(saved_path.exists(), "File should exist: {}", saved_path.display());
            
            // Verify file content
            let content = std::fs::read(saved_path)?;
            assert_eq!(&content, attachment.content.as_ref().unwrap());
        }

        // Verify directory structure
        let expected_thread_dir = temp_dir.path().join("feishu").join("workspace").join("feishu_chat_oc_12345");
        let expected_attachment_dir = expected_thread_dir.join("attachments");
        
        // Verify directories exist
        assert!(expected_thread_dir.exists(), "Thread directory should exist: {}", expected_thread_dir.display());
        assert!(expected_attachment_dir.exists(), "Attachment directory should exist: {}", expected_attachment_dir.display());
        
        // Count files in attachment directory
        let file_count = std::fs::read_dir(&expected_attachment_dir)?.count();
        assert_eq!(file_count, 2, "Should have 2 files in attachment directory");

        Ok(())
    }

    #[tokio::test]
    async fn test_save_attachments_no_attachments() -> anyhow::Result<()> {
        use tempfile::tempdir;
        use crate::channels::types::{InboundMessage, MessageContent};
        use crate::channels::feishu::config::{FeishuConfig, WebSocketConfig};
        
        // Create a temporary directory
        let temp_dir = tempdir()?;
        
        // Create a simple Feishu config
        let config = FeishuConfig {
            app_id: "test_app_id".to_string(),
            app_secret: "test_app_secret".to_string(),
            base_url: "https://open.feishu.cn".to_string(),
            websocket: WebSocketConfig::default(),
            events: vec!["im.message.receive_v1".to_string()],
            message_format: "text".to_string(),
            metadata: Default::default(),
        };

        // Create the adapter with custom workspace root
        let adapter = FeishuInboundAdapter::new_with_workspace(&config, "feishu".to_string(), temp_dir.path().to_path_buf());

        // Create a test message WITHOUT attachments
        let mut message = InboundMessage {
            id: "test_message_no_attach".to_string(),
            channel: "feishu".to_string(),
            channel_uid: "msg_456".to_string(),
            sender: "Another User".to_string(),
            sender_address: "user_456".to_string(),
            recipients: vec![],
            topic: "Test".to_string(),
            content: MessageContent {
                text: Some("Test message without attachment".to_string()),
                html: None,
                markdown: None,
            },
            timestamp: chrono::Utc::now(),
            thread_refs: Some(vec!["thread_456".to_string()]),
            reply_to_id: None,
            external_id: Some("ext_456".to_string()),
            attachments: vec![],
            metadata: {
                let mut map = std::collections::HashMap::new();
                map.insert("chat_id".to_string(), serde_json::Value::String("oc_67890".to_string()));
                map
            },
            matched_pattern: None,
        };

        // Save attachments (should do nothing)
        adapter.save_attachments_to_thread_directory(&mut message, &[], None)
            .await?;

        // Verify no error and attachments remain empty
        assert_eq!(message.attachments.len(), 0);

        Ok(())
    }
}
