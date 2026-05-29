//! WeCom (企业微信) inbound adapter and matcher implementation.
//!
//! This module handles receiving messages from WeCom via the shared webhook server.
//! Unlike WeChat's WebSocket or Feishu's WebSocket, WeCom uses HTTP callbacks:
//! WeCom sends POST requests to the shared axum HTTP server at `/webhook/{channel_name}`.
//!
//! Thread name is derived from the `chat_id` field in the WeCom message metadata,
//! following the pattern `{channel_name}_{sanitized_chat_id}` — one thread per chat group.

use anyhow::Result;
use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

use jyc_types::{
    ChannelMatcher, ChannelPattern, InboundAdapter, InboundAdapterOptions, InboundMessage,
    PatternMatch,
};
use jyc_utils::helpers::sanitize_for_filesystem;

use crate::wecom::server::{ChannelWebhookConfig, ParsedWecomMessage, WecomWebhookServer};
use jyc_types::WecomConfig;

/// WeCom channel matcher — stateless pattern matching.
///
/// Supports:
/// - `keywords`: match messages containing specific words (case-insensitive)
/// - `sender`: match sender by exact address (shared with email/feishu/wechat)
///
/// All present rules use AND logic. Empty rules match all messages.
/// Thread name is derived from `metadata["chat_id"]`: `{channel_name}_{sanitized_chat_id}`.
pub struct WecomMatcher;

impl ChannelMatcher for WecomMatcher {
    fn channel_type(&self) -> &str {
        "wecom"
    }

    fn derive_thread_name(
        &self,
        message: &InboundMessage,
        _patterns: &[ChannelPattern],
        _pattern_match: Option<&PatternMatch>,
    ) -> String {
        // Thread name is derived from chat_id + channel_name in metadata:
        // {channel_name}_{sanitized_chat_id} (e.g. "my_bot_wrOgQhDgA...")
        // This ensures one thread per channel+group pair.
        if let Some(chat_id) = message
            .metadata
            .get("chat_id")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
        {
            let channel_name = message
                .metadata
                .get("channel_name")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .unwrap_or("wecom");
            format!(
                "{}_{}",
                sanitize_for_filesystem(channel_name),
                sanitize_for_filesystem(chat_id)
            )
        } else {
            // Fallback: use the channel name from sender_address
            message
                .sender_address
                .strip_prefix("wecom:")
                .map(sanitize_for_filesystem)
                .unwrap_or_else(|| "wecom".to_string())
        }
    }

    fn match_message(
        &self,
        message: &InboundMessage,
        patterns: &[ChannelPattern],
    ) -> Option<PatternMatch> {
        wecom_match_message(message, patterns)
    }
}

/// Match a message against WeCom-specific patterns.
///
/// Rules within a pattern use AND logic — all present rules must match.
/// Within each rule, sub-values use OR logic.
/// Returns the first matching pattern.
pub fn wecom_match_message(
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
        if let Some(ref keywords) = pattern.rules.keywords {
            if keywords.is_empty() {
                continue;
            }

            let body_lower = message.content.text.as_deref().unwrap_or("").to_lowercase();

            let keyword_match = keywords
                .iter()
                .any(|k| body_lower.contains(&k.to_lowercase()));
            if !keyword_match {
                matches = false;
            } else {
                match_details.insert("keywords".to_string(), keywords.join(","));
            }
        }

        // --- Sender rule ---
        if let Some(ref sender) = pattern.rules.sender {
            let sender_addr = &message.sender_address;

            // Check exact matches
            if let Some(ref exact) = sender.exact {
                let exact_match = exact.iter().any(|e| e.eq_ignore_ascii_case(sender_addr));
                if !exact_match {
                    matches = false;
                } else {
                    match_details.insert("sender_exact".to_string(), sender_addr.clone());
                }
            }

            // Check regex match
            if let Some(ref regex_str) = sender.regex
                && let Ok(re) = regex::Regex::new(regex_str)
            {
                if !re.is_match(sender_addr) {
                    matches = false;
                } else {
                    match_details.insert("sender_regex".to_string(), regex_str.clone());
                }
            }
        }

        if matches {
            return Some(PatternMatch {
                pattern_name: pattern.name.clone(),
                channel: "wecom".to_string(),
                matches: match_details,
            });
        }
    }

    None
}

/// WeCom inbound adapter.
///
/// Unlike Feishu/WeChat which maintain persistent connections (WebSocket),
/// WeCom uses HTTP callbacks. The inbound adapter:
/// 1. Registers a callback handler with the shared WecomWebhookServer
/// 2. Runs until cancellation (the actual work is done by the server)
pub struct WecomInboundAdapter {
    channel_name: String,
    thread_name: String,
    config: WecomConfig,
    server: Arc<WecomWebhookServer>,
}

impl WecomInboundAdapter {
    /// Create a new WeCom inbound adapter.
    pub fn new(config: &WecomConfig, channel_name: &str, server: Arc<WecomWebhookServer>) -> Self {
        Self {
            channel_name: channel_name.to_string(),
            thread_name: sanitize_for_filesystem(channel_name),
            config: config.clone(),
            server,
        }
    }
}

async fn register_handler(
    config: &WecomConfig,
    channel_name: &str,
    thread_name: &str,
    on_message: Box<dyn Fn(InboundMessage) -> Result<()> + Send + Sync>,
    server: Arc<WecomWebhookServer>,
) -> Result<()> {
    let channel_name_clone_1 = channel_name.to_string();
    let channel_name_clone_2 = channel_name_clone_1.clone();
    let on_message: Arc<dyn Fn(InboundMessage) -> Result<()> + Send + Sync> = Arc::from(on_message);

    // Build the per-channel webhook config
    let webhook_config = ChannelWebhookConfig {
        token: config.token.clone(),
        encoding_aes_key: config.encoding_aes_key.clone(),
        corp_id: config.corp_id.clone(),
        on_message: Arc::new(move |parsed: ParsedWecomMessage| {
            let message = InboundMessage {
                id: uuid::Uuid::new_v4().to_string(),
                channel: "wecom".to_string(),
                channel_uid: format!(
                    "wecom_{}",
                    chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0)
                ),
                sender: parsed.from_user.clone(),
                sender_address: format!("wecom:{}", parsed.from_user),
                recipients: vec![],
                topic: "WeCom Message".to_string(),
                content: jyc_types::MessageContent {
                    text: Some(parsed.content.clone()),
                    html: None,
                    markdown: None,
                },
                timestamp: chrono::Utc::now(),
                thread_refs: None,
                reply_to_id: None,
                external_id: Some(parsed.msg_id.clone()),
                attachments: vec![],
                metadata: {
                    let mut m = HashMap::new();
                    m.insert(
                        "chat_id".to_string(),
                        serde_json::Value::String(parsed.chat_id.clone()),
                    );
                    m.insert(
                        "msg_type".to_string(),
                        serde_json::Value::String(parsed.msg_type.clone()),
                    );
                    m.insert(
                        "from_user".to_string(),
                        serde_json::Value::String(parsed.from_user.clone()),
                    );
                    m.insert(
                        "msg_id".to_string(),
                        serde_json::Value::String(parsed.msg_id.clone()),
                    );
                    m.insert(
                        "channel_name".to_string(),
                        serde_json::Value::String(channel_name_clone_1.clone()),
                    );
                    m
                },
                matched_pattern: None,
            };

            let cb = on_message.clone();
            let err_channel = channel_name_clone_2.clone();
            tokio::spawn(async move {
                if let Err(e) = cb(message) {
                    tracing::error!(
                        error = %e,
                        channel = %err_channel,
                        "WeCom inbound: on_message callback error"
                    );
                }
            });

            Ok(())
        }),
    };

    server.register_channel(channel_name, webhook_config).await;

    tracing::info!(
        channel = %channel_name,
        thread = %thread_name,
        "WeCom inbound adapter registered webhook handler"
    );

    Ok(())
}

impl ChannelMatcher for WecomInboundAdapter {
    fn channel_type(&self) -> &str {
        "wecom"
    }

    fn derive_thread_name(
        &self,
        message: &InboundMessage,
        patterns: &[ChannelPattern],
        pattern_match: Option<&PatternMatch>,
    ) -> String {
        // Delegate to `WecomMatcher` so the saver and the router agree
        // on the thread name. They MUST agree — when they don't, the
        // attachment saver writes to a different directory than where
        // the agent thread actually runs.
        WecomMatcher.derive_thread_name(message, patterns, pattern_match)
    }

    fn match_message(
        &self,
        message: &InboundMessage,
        patterns: &[ChannelPattern],
    ) -> Option<PatternMatch> {
        wecom_match_message(message, patterns)
    }
}

#[async_trait]
impl InboundAdapter for WecomInboundAdapter {
    async fn start(&self, options: InboundAdapterOptions, cancel: CancellationToken) -> Result<()> {
        let channel_name = self.channel_name.clone();

        // Register the webhook callback handler with the on_message callback
        register_handler(
            &self.config,
            &channel_name,
            &self.thread_name,
            options.on_message,
            self.server.clone(),
        )
        .await?;

        tracing::info!(
            channel = %channel_name,
            "WeCom inbound adapter started (waiting for webhook callbacks)"
        );

        // Wait until cancellation
        cancel.cancelled().await;

        tracing::info!(
            channel = %channel_name,
            "WeCom inbound adapter stopped"
        );

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use jyc_types::{MessageContent, PatternRules, SenderRule};

    fn make_wecom_message(
        sender: &str,
        text: &str,
        metadata: HashMap<String, serde_json::Value>,
    ) -> InboundMessage {
        InboundMessage {
            id: "test-id".to_string(),
            channel: "wecom".to_string(),
            channel_uid: "test-uid".to_string(),
            sender: sender.to_string(),
            sender_address: sender.to_string(),
            recipients: vec![],
            topic: "Test".to_string(),
            content: MessageContent {
                text: Some(text.to_string()),
                html: None,
                markdown: None,
            },
            timestamp: chrono::Utc::now(),
            thread_refs: None,
            reply_to_id: None,
            external_id: None,
            attachments: vec![],
            metadata,
            matched_pattern: None,
        }
    }

    fn make_wecom_pattern(
        name: &str,
        keywords: Option<Vec<String>>,
        sender: Option<SenderRule>,
    ) -> ChannelPattern {
        ChannelPattern {
            name: name.to_string(),
            channel: "wecom".to_string(),
            enabled: true,
            rules: PatternRules {
                keywords,
                sender,
                ..Default::default()
            },
            ..Default::default()
        }
    }

    #[test]
    fn test_channel_type() {
        let matcher = WecomMatcher;
        assert_eq!(matcher.channel_type(), "wecom");
    }

    #[test]
    fn test_match_by_keywords() {
        let msg = make_wecom_message("user1", "I need 帮助 with this", HashMap::new());
        let patterns = vec![make_wecom_pattern(
            "help_pattern",
            Some(vec!["帮助".to_string(), "help".to_string()]),
            None,
        )];

        let result = wecom_match_message(&msg, &patterns);
        assert!(result.is_some());
        assert_eq!(result.unwrap().pattern_name, "help_pattern");
    }

    #[test]
    fn test_match_keywords_case_insensitive() {
        let msg = make_wecom_message("user1", "I need HELP", HashMap::new());
        let patterns = vec![make_wecom_pattern(
            "help_pattern",
            Some(vec!["help".to_string()]),
            None,
        )];

        assert!(wecom_match_message(&msg, &patterns).is_some());
    }

    #[test]
    fn test_no_match_wrong_keywords() {
        let msg = make_wecom_message("user1", "Just chatting", HashMap::new());
        let patterns = vec![make_wecom_pattern(
            "help_pattern",
            Some(vec!["帮助".to_string(), "help".to_string()]),
            None,
        )];

        assert!(wecom_match_message(&msg, &patterns).is_none());
    }

    #[test]
    fn test_empty_rules_matches_all() {
        let msg = make_wecom_message("user1", "Hello", HashMap::new());
        let patterns = vec![make_wecom_pattern("catch_all", None, None)];

        assert!(wecom_match_message(&msg, &patterns).is_some());
    }

    #[test]
    fn test_match_by_sender() {
        let msg = make_wecom_message("wecom:wx_abc123", "Hello", HashMap::new());
        let patterns = vec![make_wecom_pattern(
            "vip_user",
            None,
            Some(SenderRule {
                exact: Some(vec!["wecom:wx_abc123".to_string()]),
                ..Default::default()
            }),
        )];

        assert!(wecom_match_message(&msg, &patterns).is_some());
    }

    #[test]
    fn test_no_match_wrong_sender() {
        let msg = make_wecom_message("wecom:other_user", "Hello", HashMap::new());
        let patterns = vec![make_wecom_pattern(
            "vip_user",
            None,
            Some(SenderRule {
                exact: Some(vec!["wecom:wx_abc123".to_string()]),
                ..Default::default()
            }),
        )];

        assert!(wecom_match_message(&msg, &patterns).is_none());
    }

    #[test]
    fn test_disabled_pattern_ignored() {
        let msg = make_wecom_message("user1", "帮助 please", HashMap::new());
        let mut pattern = make_wecom_pattern("help_pattern", Some(vec!["帮助".to_string()]), None);
        pattern.enabled = false;

        let result = wecom_match_message(&msg, &[pattern]);
        assert!(result.is_none());
    }

    #[test]
    fn test_keywords_and_sender_and() {
        let msg = make_wecom_message("wecom:vip_user", "需要帮助", HashMap::new());
        let patterns = vec![make_wecom_pattern(
            "vip_help",
            Some(vec!["帮助".to_string()]),
            Some(SenderRule {
                exact: Some(vec!["wecom:vip_user".to_string()]),
                ..Default::default()
            }),
        )];

        assert!(wecom_match_message(&msg, &patterns).is_some());
    }

    #[test]
    fn test_keywords_and_sender_no_match() {
        let msg = make_wecom_message("wecom:other_user", "需要帮助", HashMap::new());
        let patterns = vec![make_wecom_pattern(
            "vip_help",
            Some(vec!["帮助".to_string()]),
            Some(SenderRule {
                exact: Some(vec!["wecom:vip_user".to_string()]),
                ..Default::default()
            }),
        )];

        assert!(wecom_match_message(&msg, &patterns).is_none());
    }

    #[test]
    fn test_derive_thread_name_from_chat_id() {
        let matcher = WecomMatcher;
        let mut metadata = HashMap::new();
        metadata.insert(
            "chat_id".to_string(),
            serde_json::Value::String("wr9876543210".to_string()),
        );
        metadata.insert(
            "channel_name".to_string(),
            serde_json::Value::String("my_bot".to_string()),
        );
        let msg = make_wecom_message("user1", "Hello", metadata);
        let name = matcher.derive_thread_name(&msg, &[], None);
        assert_eq!(name, "my_bot_wr9876543210");
    }

    #[test]
    fn test_derive_thread_name_from_chat_id_without_channel_name() {
        // If channel_name is missing from metadata, falls back to "wecom" prefix
        let matcher = WecomMatcher;
        let mut metadata = HashMap::new();
        metadata.insert(
            "chat_id".to_string(),
            serde_json::Value::String("wr9876543210".to_string()),
        );
        let msg = make_wecom_message("user1", "Hello", metadata);
        let name = matcher.derive_thread_name(&msg, &[], None);
        assert_eq!(name, "wecom_wr9876543210");
    }

    #[test]
    fn test_derive_thread_name_fallback() {
        let matcher = WecomMatcher;
        // No chat_id in metadata — falls back to sender_address
        let msg = make_wecom_message("wecom:my_bot", "Hello", HashMap::new());
        let name = matcher.derive_thread_name(&msg, &[], None);
        assert_eq!(name, "my_bot");
    }

    #[test]
    fn test_derive_thread_name_empty_chat_id_fallback() {
        let matcher = WecomMatcher;
        let mut metadata = HashMap::new();
        metadata.insert(
            "chat_id".to_string(),
            serde_json::Value::String("".to_string()),
        );
        metadata.insert(
            "channel_name".to_string(),
            serde_json::Value::String("my_bot".to_string()),
        );
        let msg = make_wecom_message("wecom:fallback_bot", "Hello", metadata);
        let name = matcher.derive_thread_name(&msg, &[], None);
        assert_eq!(name, "fallback_bot");
    }

    #[test]
    fn test_adapter_derive_thread_name_matches_matcher() {
        use crate::wecom::server::WecomWebhookServer;
        use std::sync::Arc;

        let config = WecomConfig {
            token: "test".to_string(),
            encoding_aes_key: "abc123abc123abc123abc123abc123abc123abc123abc123abc12".to_string(),
            corp_id: "test_corp".to_string(),
            corp_secret: "test_secret".to_string(),
            metadata: std::collections::HashMap::new(),
        };
        let server = Arc::new(WecomWebhookServer::new("127.0.0.1:1"));
        let adapter = WecomInboundAdapter::new(&config, "my_bot", server);

        let mut metadata = HashMap::new();
        metadata.insert(
            "chat_id".to_string(),
            serde_json::Value::String("wr12345".to_string()),
        );
        metadata.insert(
            "channel_name".to_string(),
            serde_json::Value::String("my_bot".to_string()),
        );
        let mut msg = make_wecom_message("wecom:user1", "Hello", metadata);
        msg.sender_address = "wecom:user1".to_string();

        let adapter_name = adapter.derive_thread_name(&msg, &[], None);
        let matcher_name = WecomMatcher.derive_thread_name(&msg, &[], None);
        assert_eq!(adapter_name, matcher_name);
        assert_eq!(adapter_name, "my_bot_wr12345");
    }
}
