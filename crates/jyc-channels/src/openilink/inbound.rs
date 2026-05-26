//! OpeniLink inbound adapter implementation.
//!
//! This module handles receiving WeChat messages from the OpeniLink Hub
//! via WebSocket and provides channel-specific pattern matching and
//! thread name derivation.

use anyhow::Result;
use async_trait::async_trait;
use std::collections::HashMap;
use tokio_util::sync::CancellationToken;

use jyc_types::{
    ChannelMatcher, ChannelPattern, InboundAdapterOptions, InboundMessage, PatternMatch,
};
use jyc_types::OpenilinkConfig;

use super::websocket::OpenilinkWebSocket;

/// OpeniLink-specific pattern matching logic.
///
/// Matches messages based on:
/// - `sender`: wxid (exact match or regex)
/// - `keywords`: message body contains any of the configured keywords
///
/// All present rules use AND logic. Within each rule, sub-values use OR logic.
pub struct OpenilinkMatcher;

impl ChannelMatcher for OpenilinkMatcher {
    fn channel_type(&self) -> &str {
        "openilink"
    }

    fn derive_thread_name(
        &self,
        message: &InboundMessage,
        _patterns: &[ChannelPattern],
        _pattern_match: Option<&PatternMatch>,
    ) -> String {
        // Each WeChat user gets an independent thread based on their wxid
        format!("wx_{}", message.sender_address)
    }

    fn match_message(
        &self,
        message: &InboundMessage,
        patterns: &[ChannelPattern],
    ) -> Option<PatternMatch> {
        openilink_match_message(message, patterns)
    }

    /// OpeniLink does NOT store unmatched messages by default.
    /// This avoids creating threads for every random WeChat message.
    fn store_unmatched_messages(&self) -> bool {
        false
    }
}

/// Match a message against openilink-specific patterns.
///
/// Rules within a pattern use AND logic — all present rules must match.
/// Within each rule, sub-values use OR logic.
/// Returns the first matching pattern.
pub fn openilink_match_message(
    message: &InboundMessage,
    patterns: &[ChannelPattern],
) -> Option<PatternMatch> {
    #[allow(clippy::collapsible_if)]
    for pattern in patterns {
        if !pattern.enabled {
            continue;
        }

        let mut matches = true;
        let mut match_details = HashMap::new();

        // --- Sender rule ---
        // Match by wxid (exact or regex)
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

        // --- Keywords rule ---
        if matches {
            if let Some(ref keywords) = pattern.rules.keywords {
                let body = message
                    .content
                    .text
                    .as_deref()
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
                    match_details.insert("keywords".to_string(), matched_kw.join(","));
                }
            }
        }

        if matches {
            return Some(PatternMatch {
                pattern_name: pattern.name.clone(),
                channel: "openilink".to_string(),
                matches: match_details,
            });
        }
    }

    None
}

/// OpeniLink inbound adapter for receiving WeChat messages via WebSocket.
pub struct OpenilinkInboundAdapter {
    config: OpenilinkConfig,
    /// Channel name from config (e.g., "wechat")
    channel_name: String,
}

impl OpenilinkInboundAdapter {
    /// Create a new OpeniLink inbound adapter.
    pub fn new(config: &OpenilinkConfig, channel_name: String) -> Self {
        Self {
            config: config.clone(),
            channel_name,
        }
    }
}

impl ChannelMatcher for OpenilinkInboundAdapter {
    fn channel_type(&self) -> &str {
        "openilink"
    }

    fn derive_thread_name(
        &self,
        message: &InboundMessage,
        patterns: &[ChannelPattern],
        pattern_match: Option<&PatternMatch>,
    ) -> String {
        // Delegate to the stateless OpenilinkMatcher
        OpenilinkMatcher.derive_thread_name(message, patterns, pattern_match)
    }

    fn match_message(
        &self,
        message: &InboundMessage,
        patterns: &[ChannelPattern],
    ) -> Option<PatternMatch> {
        openilink_match_message(message, patterns)
    }
}

#[async_trait]
impl jyc_types::InboundAdapter for OpenilinkInboundAdapter {
    async fn start(
        &self,
        options: InboundAdapterOptions,
        cancel: CancellationToken,
    ) -> Result<()> {
        // Create WebSocket handler and run the event loop
        let mut ws = OpenilinkWebSocket::new(&self.config);
        let channel_name = self.channel_name.clone();

        loop {
            tracing::info!("Starting OpeniLink WebSocket connection...");

            match ws
                .run(&channel_name, &*options.on_message, &cancel)
                .await
            {
                Ok(()) => {
                    // Clean exit (cancelled)
                    tracing::info!("OpeniLink WebSocket stopped cleanly");
                    break;
                }
                Err(e) => {
                    if cancel.is_cancelled() {
                        tracing::info!("OpeniLink WebSocket shutting down (cancelled)");
                        break;
                    }
                    tracing::error!(error = %e, "OpeniLink WebSocket error");

                    if !ws.handle_reconnection().await {
                        tracing::error!(
                            "Max reconnection attempts reached, stopping OpeniLink channel"
                        );
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
    use jyc_types::{MessageContent, PatternRules, SenderRule};
    use chrono::Utc;

    fn make_openilink_message(
        sender_addr: &str,
        body: &str,
        context_token: Option<&str>,
    ) -> InboundMessage {
        let mut metadata = HashMap::new();
        if let Some(token) = context_token {
            metadata.insert(
                "context_token".to_string(),
                serde_json::Value::String(token.to_string()),
            );
        }

        InboundMessage {
            id: "test".to_string(),
            channel: "openilink".to_string(),
            channel_uid: sender_addr.to_string(),
            sender: sender_addr.to_string(),
            sender_address: sender_addr.to_string(),
            recipients: vec![],
            topic: String::new(),
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

    fn make_pattern(
        name: &str,
        keywords: Option<Vec<String>>,
        sender: Option<SenderRule>,
    ) -> ChannelPattern {
        ChannelPattern {
            name: name.to_string(),
            channel: "openilink".to_string(),
            enabled: true,
            rules: PatternRules {
                sender,
                subject: None,
                mentions: None,
                keywords,
                chat_name: None,
                github_type: None,
                labels: None,
                assignees: None,
                exclude_labels: None,
            },
            attachments: None,
            ..Default::default()
        }
    }

    #[test]
    fn test_match_by_keywords() {
        let msg = make_openilink_message("wxid_abc", "我需要帮助", None);
        let patterns = vec![make_pattern(
            "help_bot",
            Some(vec!["帮助".to_string(), "help".to_string()]),
            None,
        )];

        let result = openilink_match_message(&msg, &patterns);
        assert!(result.is_some());
        assert_eq!(result.unwrap().pattern_name, "help_bot");
    }

    #[test]
    fn test_match_keywords_case_insensitive() {
        let msg = make_openilink_message("wxid_abc", "I need HELP", None);
        let patterns = vec![make_pattern(
            "help_bot",
            Some(vec!["help".to_string()]),
            None,
        )];

        assert!(openilink_match_message(&msg, &patterns).is_some());
    }

    #[test]
    fn test_no_match_wrong_keywords() {
        let msg = make_openilink_message("wxid_abc", "Just chatting", None);
        let patterns = vec![make_pattern(
            "help_bot",
            Some(vec!["帮助".to_string(), "help".to_string()]),
            None,
        )];

        assert!(openilink_match_message(&msg, &patterns).is_none());
    }

    #[test]
    fn test_match_by_sender() {
        let msg = make_openilink_message("wxid_vip123", "Hello", None);
        let patterns = vec![make_pattern(
            "vip_user",
            None,
            Some(SenderRule {
                exact: Some(vec!["wxid_vip123".to_string()]),
                ..Default::default()
            }),
        )];

        assert!(openilink_match_message(&msg, &patterns).is_some());
    }

    #[test]
    fn test_no_match_wrong_sender() {
        let msg = make_openilink_message("wxid_other", "Hello", None);
        let patterns = vec![make_pattern(
            "vip_user",
            None,
            Some(SenderRule {
                exact: Some(vec!["wxid_vip123".to_string()]),
                ..Default::default()
            }),
        )];

        assert!(openilink_match_message(&msg, &patterns).is_none());
    }

    #[test]
    fn test_match_sender_and_keywords_and_logic() {
        let msg = make_openilink_message("wxid_vip", "help me please", None);
        let patterns = vec![make_pattern(
            "vip_help",
            Some(vec!["help".to_string()]),
            Some(SenderRule {
                exact: Some(vec!["wxid_vip".to_string()]),
                ..Default::default()
            }),
        )];

        assert!(openilink_match_message(&msg, &patterns).is_some());

        // Sender matches but keywords don't
        let msg2 = make_openilink_message("wxid_vip", "random text", None);
        assert!(openilink_match_message(&msg2, &patterns).is_none());

        // Keywords match but sender doesn't
        let msg3 = make_openilink_message("wxid_other", "help me", None);
        assert!(openilink_match_message(&msg3, &patterns).is_none());
    }

    #[test]
    fn test_disabled_pattern_skipped() {
        let msg = make_openilink_message("wxid_abc", "help", None);
        let mut pattern = make_pattern(
            "help_bot",
            Some(vec!["help".to_string()]),
            None,
        );
        pattern.enabled = false;

        assert!(openilink_match_message(&msg, &[pattern]).is_none());
    }

    #[test]
    fn test_first_pattern_wins() {
        let msg = make_openilink_message("wxid_abc", "help", None);
        let patterns = vec![
            make_pattern("first", Some(vec!["help".to_string()]), None),
            make_pattern("second", Some(vec!["help".to_string()]), None),
        ];

        let result = openilink_match_message(&msg, &patterns).unwrap();
        assert_eq!(result.pattern_name, "first");
    }

    #[test]
    fn test_empty_rules_matches_all() {
        let msg = make_openilink_message("wxid_abc", "anything", None);
        let patterns = vec![make_pattern("catch_all", None, None)];

        assert!(openilink_match_message(&msg, &patterns).is_some());
    }

    #[test]
    fn test_derive_thread_name() {
        let msg = make_openilink_message("wxid_abc123", "Hello", Some("token_xxx"));
        let matcher = OpenilinkMatcher;
        let name = matcher.derive_thread_name(&msg, &[], None);
        assert_eq!(name, "wx_wxid_abc123");
    }

    #[test]
    fn test_store_unmatched_default() {
        let matcher = OpenilinkMatcher;
        assert!(!matcher.store_unmatched_messages());
    }
}
