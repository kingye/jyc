use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use arc_swap::ArcSwap;

use crate::message_storage::MessageStorage;
use crate::topic_manager::TopicManager;
use jyc_types::{ChannelMatcher, ChannelPattern, InboundMessage};

/// Routes inbound messages to the appropriate topic queue.
///
/// Channel-agnostic: delegates pattern matching and topic name derivation
/// to the `ChannelMatcher` provided by the caller.
///
/// Patterns are read dynamically from the live config on each route call,
/// so changes to config.toml are effective immediately after reload.
pub struct MessageRouter {
    topic_manager: Arc<TopicManager>,
    /// Agent-keyed TopicManagers (migration PR-5): agent name → manager.
    agent_topic_managers: HashMap<String, Arc<TopicManager>>,
    storage: Arc<MessageStorage>,
    config: Arc<ArcSwap<jyc_types::AppConfig>>,
    channel_name: String,
}

impl MessageRouter {
    pub fn new(
        topic_manager: Arc<TopicManager>,
        storage: Arc<MessageStorage>,
        config: Arc<ArcSwap<jyc_types::AppConfig>>,
        channel_name: String,
    ) -> Self {
        Self {
            topic_manager,
            agent_topic_managers: HashMap::new(),
            storage,
            config,
            channel_name,
        }
    }

    /// Attach per-agent TopicManagers (agent-keyed runtime, migration PR-5).
    ///
    /// Messages whose matched pattern references an agent are processed by
    /// that agent's TopicManager (which owns `agents/<agent>/` topics),
    /// instead of this channel's manager. Maps agent name → manager.
    pub fn with_agent_topic_managers(
        mut self,
        agent_topic_managers: HashMap<String, Arc<TopicManager>>,
    ) -> Self {
        self.agent_topic_managers = agent_topic_managers;
        self
    }

    /// The TopicManager that owns a given pattern's topics: the agent's
    /// manager when the pattern references an agent, else this channel's.
    pub fn topic_manager_for(&self, pattern: Option<&ChannelPattern>) -> Arc<TopicManager> {
        pattern
            .and_then(|p| p.agent.as_deref())
            .and_then(|a| self.agent_topic_managers.get(a))
            .cloned()
            .unwrap_or_else(|| self.topic_manager.clone())
    }

    /// Read the current patterns for this channel from the live config.
    pub fn patterns(&self) -> Vec<ChannelPattern> {
        let cfg = self.config.load();
        cfg.channels
            .get(&self.channel_name)
            .and_then(|c| c.patterns.clone())
            .unwrap_or_default()
    }

    /// Route a message from any channel type.
    ///
    /// Pattern matching and topic name derivation are delegated to
    /// the channel-specific `ChannelMatcher` implementation.
    /// Patterns are read dynamically from the live config.
    pub async fn route(&self, matcher: &dyn ChannelMatcher, mut message: InboundMessage) {
        let ch = &message.channel;
        let patterns = self.patterns();
        let patterns_ref: &[ChannelPattern] = &patterns;

        // 1. Pattern matching (channel-specific)
        let pattern_match = match matcher.match_message(&message, patterns_ref) {
            Some(m) => {
                tracing::info!(
                    channel = %ch,
                    pattern = %m.pattern_name,
                    sender = %message.sender_address,
                    topic = %message.topic,
                    "Pattern matched"
                );
                self.topic_manager.metrics.message_matched(&m.pattern_name);
                message.matched_pattern = Some(m.pattern_name.clone());
                Some(m)
            }
            None => {
                // Check if we should store unmatched messages for this channel
                if matcher.store_unmatched_messages() {
                    tracing::debug!(
                        channel = %ch,
                        sender = %message.sender_address,
                        topic = %message.topic,
                        "No pattern matched, but storing for channel context"
                    );
                    // Store the message but don't process it
                    self.store_unmatched_message(matcher, &message).await;
                } else {
                    tracing::debug!(
                        channel = %ch,
                        sender = %message.sender_address,
                        topic = %message.topic,
                        "No pattern matched, skipping"
                    );
                }
                return;
            }
        };

        // 2. Derive topic name
        // If the matched pattern has a fixed topic_name, use it (channel-agnostic).
        // Otherwise, derive from message content (channel-specific).
        let pattern_name = pattern_match
            .as_ref()
            .expect("pattern_match should be Some")
            .pattern_name
            .clone();

        let topic_name = patterns_ref
            .iter()
            .find(|p| p.name == pattern_name)
            .and_then(|p| p.topic_name.clone())
            .unwrap_or_else(|| {
                matcher.derive_topic_name(&message, patterns_ref, pattern_match.as_ref())
            });

        tracing::info!(
            channel = %ch,
            topic = %topic_name,
            pattern = %pattern_name,
            "Routing to topic"
        );

        // 3. Get attachment config, template, and live_injection from the matched pattern
        let matched_pattern_name = pattern_name;
        let matched_pattern = patterns_ref.iter().find(|p| p.name == matched_pattern_name);
        let attachment_config = matched_pattern.and_then(|p| p.attachments.clone());
        let live_injection = matched_pattern.map(|p| p.live_injection).unwrap_or(true);

        // Store template name in message metadata for topic initialization
        let matched_template = matched_pattern.and_then(|p| p.template.clone());
        tracing::debug!(
            pattern = %matched_pattern_name,
            template = ?matched_template,
            "MessageRouter: resolved template from matched pattern"
        );
        if let Some(template) = matched_template {
            message
                .metadata
                .insert("template".to_string(), serde_json::Value::String(template));
        }

        // Store role in message metadata for outbound adapter (e.g., GitHub comment prefix)
        if let Some(role) = matched_pattern.and_then(|p| p.role.clone()) {
            message
                .metadata
                .insert("role".to_string(), serde_json::Value::String(role));
        }

        // Store repo_group_key in message metadata if repo_group is configured.
        // Supports GitHub (github_number u64) and Gitee (gitee_number string or u64).
        let number_opt = message
            .metadata
            .get("github_number")
            .and_then(|v| v.as_u64())
            .map(|n| n.to_string())
            .or_else(|| {
                message.metadata.get("gitee_number").and_then(|v| {
                    v.as_str()
                        .map(|s| s.to_string())
                        .or_else(|| v.as_u64().map(|n| n.to_string()))
                })
            });

        if let Some(repo_group) = matched_pattern.and_then(|p| p.repo_group.clone())
            && let Some(number) = number_opt
        {
            let key = crate::topic_path::compute_repo_group_key(&repo_group, &number);
            message
                .metadata
                .insert("repo_group_key".to_string(), serde_json::Value::String(key));
        }

        // 4. Resolve the effective topic dir (metadata override > pattern
        // topic_path > agent dir, with lazy migration) — shared logic with
        // the send_to_topic tool.
        let owner_tm = self.topic_manager_for(matched_pattern);
        let topic_path_override: Option<PathBuf> = crate::topic_path::resolve_topic_path_override(
            matched_pattern,
            &topic_name,
            owner_tm.data_root(),
            &self.channel_name,
            message
                .metadata
                .get("topic_path_override")
                .and_then(|v| v.as_str()),
        );
        // 5. Enqueue into the owning manager (the agent's TopicManager for
        // agent-routed patterns — channel-agnostic).
        let pm = pattern_match.expect("pattern_match should be Some");
        owner_tm
            .enqueue(
                message,
                topic_name,
                pm,
                attachment_config,
                live_injection,
                topic_path_override,
            )
            .await;
    }

    /// Store an unmatched message for channels that want to keep full conversation context.
    async fn store_unmatched_message(
        &self,
        matcher: &dyn ChannelMatcher,
        message: &InboundMessage,
    ) {
        let patterns = self.patterns();
        let patterns_ref: &[ChannelPattern] = &patterns;

        // Derive topic name even for unmatched messages
        let topic_name = matcher.derive_topic_name(message, patterns_ref, None);

        tracing::info!(
            channel = %message.channel,
            topic = %topic_name,
            sender = %message.sender_address,
            topic = %message.topic,
            "Storing unmatched message"
        );

        // Store the message without processing (is_matched = false)
        match self
            .storage
            .store_with_match(message, &topic_name, false, None)
            .await
        {
            Ok(store_result) => {
                tracing::debug!(
                    channel = %message.channel,
                    topic = %topic_name,
                    path = %store_result.message_dir,
                    "Unmatched message stored"
                );
            }
            Err(e) => {
                tracing::error!(
                    channel = %message.channel,
                    topic = %topic_name,
                    error = %e,
                    "Failed to store unmatched message"
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {

    use jyc_types::{ChannelMatcher, ChannelPattern, InboundMessage, MessageContent, PatternMatch};
    use std::collections::HashMap;

    /// Mock matcher that always matches with the first pattern
    struct MockMatcher;

    impl ChannelMatcher for MockMatcher {
        fn channel_type(&self) -> &str {
            "mock"
        }

        fn derive_topic_name(
            &self,
            message: &InboundMessage,
            _patterns: &[ChannelPattern],
            _pattern_match: Option<&PatternMatch>,
        ) -> String {
            // Default: derive from topic
            message.topic.clone()
        }

        fn match_message(
            &self,
            _message: &InboundMessage,
            patterns: &[ChannelPattern],
        ) -> Option<PatternMatch> {
            patterns.first().map(|p| PatternMatch {
                pattern_name: p.name.clone(),
                channel: "mock".to_string(),
                matches: HashMap::new(),
            })
        }
    }

    fn test_message(topic: &str) -> InboundMessage {
        InboundMessage {
            id: "1".to_string(),
            channel: "test".to_string(),
            channel_uid: "1".to_string(),
            sender: "user".to_string(),
            sender_address: "user@test".to_string(),
            recipients: vec![],
            topic: topic.to_string(),
            content: MessageContent::default(),
            timestamp: chrono::Utc::now(),
            references: None,
            reply_to_id: None,
            external_id: None,
            attachments: vec![],
            metadata: HashMap::new(),
            matched_pattern: None,
        }
    }

    fn test_pattern(name: &str, topic_name: Option<&str>) -> ChannelPattern {
        ChannelPattern {
            name: name.to_string(),
            topic_name: topic_name.map(|s| s.to_string()),
            ..Default::default()
        }
    }

    #[test]
    fn test_topic_name_override_used_when_set() {
        let matcher = MockMatcher;
        let message = test_message("Invoice for food");
        let patterns = vec![test_pattern("invoices", Some("invoices"))];

        let pattern_match = matcher.match_message(&message, &patterns);
        assert!(pattern_match.is_some());

        let pattern_name = pattern_match.as_ref().unwrap().pattern_name.clone();

        // With topic_name override, should use "invoices" not "Invoice for food"
        let topic_name = patterns
            .iter()
            .find(|p| p.name == pattern_name)
            .and_then(|p| p.topic_name.clone())
            .unwrap_or_else(|| {
                matcher.derive_topic_name(&message, &patterns, pattern_match.as_ref())
            });

        assert_eq!(topic_name, "invoices");
    }

    #[test]
    fn test_topic_name_derived_when_no_override() {
        let matcher = MockMatcher;
        let message = test_message("Invoice for food");
        let patterns = vec![test_pattern("catch_all", None)];

        let pattern_match = matcher.match_message(&message, &patterns);
        assert!(pattern_match.is_some());

        let pattern_name = pattern_match.as_ref().unwrap().pattern_name.clone();

        // Without topic_name override, should derive from topic
        let topic_name = patterns
            .iter()
            .find(|p| p.name == pattern_name)
            .and_then(|p| p.topic_name.clone())
            .unwrap_or_else(|| {
                matcher.derive_topic_name(&message, &patterns, pattern_match.as_ref())
            });

        assert_eq!(topic_name, "Invoice for food");
    }

    #[test]
    fn test_different_topics_same_topic_with_override() {
        let matcher = MockMatcher;
        let patterns = vec![test_pattern("invoices", Some("invoices"))];

        for topic in &["Invoice food", "发票 office", "Receipt hotel"] {
            let message = test_message(topic);
            let pattern_match = matcher.match_message(&message, &patterns);
            let pattern_name = pattern_match.as_ref().unwrap().pattern_name.clone();

            let topic_name = patterns
                .iter()
                .find(|p| p.name == pattern_name)
                .and_then(|p| p.topic_name.clone())
                .unwrap_or_else(|| {
                    matcher.derive_topic_name(&message, &patterns, pattern_match.as_ref())
                });

            assert_eq!(
                topic_name, "invoices",
                "Topic '{}' should route to 'invoices'",
                topic
            );
        }
    }

    #[test]
    fn test_live_injection_defaults_to_true() {
        let pattern = ChannelPattern::default();
        assert!(
            pattern.live_injection,
            "live_injection should default to true"
        );
    }

    #[test]
    fn test_live_injection_extracted_from_pattern() {
        let matcher = MockMatcher;
        let message = test_message("Hello");
        let patterns = vec![ChannelPattern {
            name: "no_inject".to_string(),
            live_injection: false,
            ..Default::default()
        }];

        let pattern_match = matcher.match_message(&message, &patterns);
        assert!(pattern_match.is_some());

        let pattern_name = &pattern_match.as_ref().unwrap().pattern_name;
        let matched = patterns.iter().find(|p| &p.name == pattern_name);
        let live_injection = matched.map(|p| p.live_injection).unwrap_or(true);

        assert!(
            !live_injection,
            "live_injection should be false when pattern sets it"
        );
    }

    #[test]
    fn test_live_injection_true_when_pattern_enables_it() {
        let matcher = MockMatcher;
        let message = test_message("Hello");
        let patterns = vec![ChannelPattern {
            name: "with_inject".to_string(),
            live_injection: true,
            ..Default::default()
        }];

        let pattern_match = matcher.match_message(&message, &patterns);
        let pattern_name = &pattern_match.as_ref().unwrap().pattern_name;
        let matched = patterns.iter().find(|p| &p.name == pattern_name);
        let live_injection = matched.map(|p| p.live_injection).unwrap_or(true);

        assert!(
            live_injection,
            "live_injection should be true when pattern enables it"
        );
    }

    #[test]
    fn test_live_injection_defaults_true_via_serde() {
        // Simulate deserialization without the live_injection field
        let pattern: ChannelPattern = toml::from_str(
            r#"
            name = "test"
            [rules]
        "#,
        )
        .unwrap();
        assert!(
            pattern.live_injection,
            "live_injection should default to true when omitted from config"
        );
    }

    #[test]
    fn test_live_injection_false_via_serde() {
        let pattern: ChannelPattern = toml::from_str(
            r#"
            name = "test"
            live_injection = false
            [rules]
        "#,
        )
        .unwrap();
        assert!(
            !pattern.live_injection,
            "live_injection should be false when explicitly set in config"
        );
    }
}
