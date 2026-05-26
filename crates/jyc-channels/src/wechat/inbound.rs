//! WeChat inbound adapter and matcher implementation.
//!
//! This module handles receiving messages from WeChat via the OpenILink WebSocket Bridge
//! and provides channel-specific pattern matching and thread name derivation.
//!
//! Unlike Feishu which supports multiple chats/threads, WeChat in this implementation
//! uses one bot = one fixed thread. The thread name is derived directly from the channel
//! configuration name (e.g., "wechat_bot").

use anyhow::Result;
use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{mpsc, Mutex};
use tokio_util::sync::CancellationToken;

use jyc_types::{
    ChannelMatcher, ChannelPattern, InboundAdapter, InboundAdapterOptions, InboundMessage,
    PatternMatch,
};

use super::websocket::WechatWebSocket;
use jyc_types::WechatConfig;

/// WeChat channel matcher — stateless pattern matching.
///
/// Supports:
/// - `keywords`: match messages containing specific words (case-insensitive)
/// - `sender`: match sender by exact address (shared with email/feishu)
///
/// All present rules use AND logic. Empty rules match all messages.
/// One bot = one fixed thread: thread name is the channel name.
pub struct WechatMatcher;

impl ChannelMatcher for WechatMatcher {
    fn channel_type(&self) -> &str {
        "wechat"
    }

    fn derive_thread_name(
        &self,
        _message: &InboundMessage,
        _patterns: &[ChannelPattern],
        _pattern_match: Option<&PatternMatch>,
    ) -> String {
        // WeChat uses one bot = one fixed thread.
        // The thread name is derived from the channel name by the inbound adapter.
        // This method is called by the router as a fallback when no channel-level
        // override is available.
        "wechat".to_string()
    }

    fn match_message(
        &self,
        message: &InboundMessage,
        patterns: &[ChannelPattern],
    ) -> Option<PatternMatch> {
        wechat_match_message(message, patterns)
    }
}

/// Match a message against WeChat-specific patterns.
///
/// Rules within a pattern use AND logic — all present rules must match.
/// Within each rule, sub-values use OR logic.
/// Returns the first matching pattern.
pub fn wechat_match_message(
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
        // Check if the message body contains any of the configured keywords
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
                match_details.insert(
                    "keywords".to_string(),
                    matched_kw.join(","),
                );
            }
        }

        // --- Sender rule (shared) ---
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
                channel: "wechat".to_string(),
                matches: match_details,
            });
        }
    }

    None
}

/// WeChat inbound adapter for receiving messages via WebSocket.
pub struct WechatInboundAdapter {
    config: WechatConfig,
    /// Channel name from config (e.g., "wechat_bot")
    channel_name: String,
    /// Shared sender Arc pointing to the same storage as WechatOutboundAdapter.
    /// On each reconnection, a new WebSocket is created and its sender is pushed
    /// here so the outbound adapter always has a live sender.
    shared_sender: Option<Arc<Mutex<Option<mpsc::UnboundedSender<String>>>>>,
}

impl WechatInboundAdapter {
    /// Create a new WeChat inbound adapter.
    pub fn new(config: &WechatConfig, channel_name: String) -> Self {
        Self {
            config: config.clone(),
            channel_name,
            shared_sender: None,
        }
    }

    /// Create a new WeChat inbound adapter with a shared sender.
    ///
    /// The `shared_sender` must point to the same `Arc<Mutex<Option<...>>>` as
    /// the outbound adapter's sender storage. On each reconnect, a fresh
    /// WebSocket is created and the new sender is written into this shared slot.
    pub fn with_shared_sender(
        config: &WechatConfig,
        channel_name: String,
        shared_sender: Arc<Mutex<Option<mpsc::UnboundedSender<String>>>>,
    ) -> Self {
        Self {
            config: config.clone(),
            channel_name,
            shared_sender: Some(shared_sender),
        }
    }
}

impl ChannelMatcher for WechatInboundAdapter {
    fn channel_type(&self) -> &str {
        "wechat"
    }

    fn derive_thread_name(
        &self,
        _message: &InboundMessage,
        _patterns: &[ChannelPattern],
        _pattern_match: Option<&PatternMatch>,
    ) -> String {
        // WeChat uses the channel name as the fixed thread name
        self.channel_name.clone()
    }

    fn match_message(
        &self,
        message: &InboundMessage,
        patterns: &[ChannelPattern],
    ) -> Option<PatternMatch> {
        wechat_match_message(message, patterns)
    }
}

#[async_trait]
impl InboundAdapter for WechatInboundAdapter {
    async fn start(
        &self,
        options: InboundAdapterOptions,
        cancel: CancellationToken,
    ) -> Result<()> {
        if !self.config.websocket.enabled {
            tracing::info!("WeChat WebSocket disabled, holding channel alive until cancel");
            cancel.cancelled().await;
            return Ok(());
        }

        let channel_name = self.channel_name.clone();
        let mut reconnect_count = 0usize;

        loop {
            tracing::info!("Starting WeChat WebSocket connection...");

            // Create a FRESH WebSocket on every iteration.
            // This avoids the `outbound_rx.take()` panic on re-run and ensures
            // the outbound sender always has a live channel pair.
            let mut ws = WechatWebSocket::new_with_config(
                &self.config.base_url,
                &self.config.token,
                self.config.websocket.max_reconnect_attempts,
                self.config.websocket.reconnect_delay_secs,
            );

            // Push the new WebSocket's sender into the shared slot so the
            // outbound adapter always sends through the live connection.
            if let Some(ref shared) = self.shared_sender {
                let mut guard = shared.lock().await;
                *guard = Some(ws.sender());
            }

            match ws.run(&channel_name, &*options.on_message, &cancel).await {
                Ok(()) => {
                    // Clean exit (cancelled)
                    tracing::info!("WeChat WebSocket stopped cleanly");
                    break;
                }
                Err(e) => {
                    if cancel.is_cancelled() {
                        tracing::info!("WeChat WebSocket shutting down (cancelled)");
                        break;
                    }
                    // Use `:#` so anyhow's full Context chain renders on one line
                    // (e.g. "Failed to connect to WeChat OpenILink WebSocket: \
                    // WebSocket protocol error: Handshake failed: HTTP 401").
                    // Plain `%e` would only show the outermost message and hide
                    // the actual cause.
                    tracing::error!(error = %format!("{:#}", e), "WeChat WebSocket error");

                    // Exponential backoff: reconnect_delay_secs * 2^attempt, capped at 60s
                    let max_attempts = self.config.websocket.max_reconnect_attempts;
                    if reconnect_count >= max_attempts {
                        tracing::error!(max_attempts, "Max reconnection attempts reached, stopping WeChat channel");
                        break;
                    }

                    let delay_secs = std::cmp::min(
                        self.config.websocket.reconnect_delay_secs << reconnect_count,
                        60,
                    );
                    reconnect_count += 1;
                    tracing::info!(
                        attempt = reconnect_count,
                        max_attempts,
                        delay_secs,
                        "Reconnecting to WeChat WebSocket"
                    );

                    tokio::time::sleep(std::time::Duration::from_secs(delay_secs)).await;
                    // Loop creates a fresh WS next iteration
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

    fn make_wechat_message(
        sender_addr: &str,
        body: &str,
    ) -> InboundMessage {
        InboundMessage {
            id: "test".to_string(),
            channel: "wechat".to_string(),
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
            metadata: HashMap::new(),
            matched_pattern: None,
        }
    }

    fn make_wechat_pattern(
        name: &str,
        keywords: Option<Vec<String>>,
        sender: Option<SenderRule>,
    ) -> ChannelPattern {
        ChannelPattern {
            name: name.to_string(),
            channel: "wechat".to_string(),
            enabled: true,
            rules: PatternRules {
                sender,
                keywords,
                ..Default::default()
            },
            ..Default::default()
        }
    }

    #[test]
    fn test_match_by_keywords() {
        let msg = make_wechat_message("user1", "I need 帮助 with this");
        let patterns = vec![make_wechat_pattern(
            "help_pattern",
            Some(vec!["帮助".to_string(), "help".to_string()]),
            None,
        )];

        let result = wechat_match_message(&msg, &patterns);
        assert!(result.is_some());
        assert_eq!(result.unwrap().pattern_name, "help_pattern");
    }

    #[test]
    fn test_match_keywords_case_insensitive() {
        let msg = make_wechat_message("user1", "I need HELP");
        let patterns = vec![make_wechat_pattern(
            "help_pattern",
            Some(vec!["help".to_string()]),
            None,
        )];

        assert!(wechat_match_message(&msg, &patterns).is_some());
    }

    #[test]
    fn test_no_match_wrong_keywords() {
        let msg = make_wechat_message("user1", "Just chatting");
        let patterns = vec![make_wechat_pattern(
            "help_pattern",
            Some(vec!["帮助".to_string(), "help".to_string()]),
            None,
        )];

        assert!(wechat_match_message(&msg, &patterns).is_none());
    }

    #[test]
    fn test_empty_rules_matches_all() {
        let msg = make_wechat_message("user1", "Hello");
        let patterns = vec![make_wechat_pattern("catch_all", None, None)];

        assert!(wechat_match_message(&msg, &patterns).is_some());
    }

    #[test]
    fn test_match_by_sender() {
        let msg = make_wechat_message("wx_abc123", "Hello");
        let patterns = vec![make_wechat_pattern(
            "vip_user",
            None,
            Some(SenderRule {
                exact: Some(vec!["wx_abc123".to_string()]),
                ..Default::default()
            }),
        )];

        assert!(wechat_match_message(&msg, &patterns).is_some());
    }

    #[test]
    fn test_no_match_wrong_sender() {
        let msg = make_wechat_message("wx_other", "Hello");
        let patterns = vec![make_wechat_pattern(
            "vip_user",
            None,
            Some(SenderRule {
                exact: Some(vec!["wx_abc123".to_string()]),
                ..Default::default()
            }),
        )];

        assert!(wechat_match_message(&msg, &patterns).is_none());
    }

    #[test]
    fn test_match_keywords_and_sender_and_logic() {
        let msg = make_wechat_message("wx_abc123", "Help me please");
        let patterns = vec![make_wechat_pattern(
            "both",
            Some(vec!["help".to_string()]),
            Some(SenderRule {
                exact: Some(vec!["wx_abc123".to_string()]),
                ..Default::default()
            }),
        )];

        assert!(wechat_match_message(&msg, &patterns).is_some());

        // Keywords match but sender doesn't → no match
        let msg2 = make_wechat_message("wx_other", "Help me please");
        assert!(wechat_match_message(&msg2, &patterns).is_none());

        // Sender matches but keywords don't → no match
        let msg3 = make_wechat_message("wx_abc123", "Random text");
        assert!(wechat_match_message(&msg3, &patterns).is_none());
    }

    #[test]
    fn test_disabled_pattern_skipped() {
        let msg = make_wechat_message("user1", "Hello");
        let mut pattern = make_wechat_pattern(
            "catch_all",
            None,
            None,
        );
        pattern.enabled = false;

        assert!(wechat_match_message(&msg, &[pattern]).is_none());
    }

    #[test]
    fn test_first_pattern_wins() {
        let msg = make_wechat_message("user1", "help me");
        let patterns = vec![
            make_wechat_pattern("first", None, None),
            make_wechat_pattern("second", Some(vec!["help".to_string()]), None),
        ];

        let result = wechat_match_message(&msg, &patterns).unwrap();
        assert_eq!(result.pattern_name, "first");
    }

    #[test]
    fn test_derive_thread_name() {
        let matcher = WechatMatcher;
        let msg = make_wechat_message("user1", "Hello");
        let name = matcher.derive_thread_name(&msg, &[], None);
        assert_eq!(name, "wechat");
    }

    #[test]
    fn test_channel_name_derived_via_adapter() {
        let adapter = WechatInboundAdapter::new(
            &WechatConfig::default(),
            "my_wechat_bot".to_string(),
        );
        let msg = make_wechat_message("user1", "Hello");
        let name = adapter.derive_thread_name(&msg, &[], None);
        assert_eq!(name, "my_wechat_bot");
    }
}
