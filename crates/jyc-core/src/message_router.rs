use std::path::{Path, PathBuf};
use std::sync::Arc;

use arc_swap::ArcSwap;

use crate::message_storage::MessageStorage;
use crate::topic_manager::TopicManager;
use jyc_types::{ChannelMatcher, ChannelPattern, InboundMessage};

/// Resolve which directory a topic's files (chat history, session state,
/// repo checkout) live in, given the pattern it matched.
///
/// Two rules, in order:
///
/// 1. A configured `topic_path` is the pattern's *own* home directory, so
///    it pins only the self-named topic (agent `jyc` ↔ topic `jyc`).
///    Sibling topics stay out of it — a pinned path is usually a code
///    checkout, not a container for unrelated conversations.
/// 2. On the synthesized `agents` channel every other topic nests under
///    the agent's root: `<agents-workspace>/<agent>/<topic>`. Each topic
///    then owns its directory instead of sharing the agent's, which is
///    what keeps two pipe-routed topics (`plan-197`, `plan-198`) from
///    writing into one chat history.
///
/// `None` means "no override" — the caller falls back to
/// `<workspace>/<topic>`.
fn topic_dir_for(
    matched: Option<&ChannelPattern>,
    topic_name: &str,
    channel_name: &str,
    data_root: &Path,
    workspace_dir: &Path,
) -> Option<PathBuf> {
    let pattern = matched?;
    if let Some(tp) = pattern
        .topic_path
        .as_ref()
        .filter(|_| pattern.name == topic_name)
    {
        return Some(crate::topic_path::resolve_topic_path(tp, data_root));
    }
    (channel_name == "agents").then(|| workspace_dir.join(&pattern.name).join(topic_name))
}

/// Routes inbound messages to the appropriate topic queue.
///
/// Channel-agnostic: delegates pattern matching and topic name derivation
/// to the `ChannelMatcher` provided by the caller.
///
/// Patterns are read dynamically from the live config on each route call,
/// so changes to config.toml are effective immediately after reload.
pub struct MessageRouter {
    topic_manager: Arc<TopicManager>,
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
            storage,
            config,
            channel_name,
        }
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

        // 4. Resolve topic_path override: prefer explicit metadata from
        // WebSocket create_topic, then the pattern/agent layout rules.
        let topic_path_override: Option<PathBuf> = message
            .metadata
            .get("topic_path_override")
            .and_then(|v| v.as_str())
            // Dashboard client sends absolute paths; raw PathBuf::from is
            // correct here (resolves relative paths against the CLI cwd).
            .map(PathBuf::from)
            .or_else(|| {
                topic_dir_for(
                    matched_pattern,
                    &topic_name,
                    &self.channel_name,
                    self.topic_manager.data_root(),
                    self.topic_manager.workspace_dir(),
                )
            });
        // 5. Enqueue (channel-agnostic)
        let pm = pattern_match.expect("pattern_match should be Some");
        self.topic_manager
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
    fn test_topic_dir_layout_rules() {
        use super::topic_dir_for;
        use std::path::{Path, PathBuf};

        let data_root = Path::new("/data");
        let agents_ws = Path::new("/data/agents");

        let planner = test_pattern("planner", None);
        let mut pinned = test_pattern("jyc", None);
        pinned.topic_path = Some("/projects/jyc".to_string());

        // Pipe-routed topics nest under the agent root, one dir each.
        assert_eq!(
            topic_dir_for(Some(&planner), "plan-197", "agents", data_root, agents_ws),
            Some(PathBuf::from("/data/agents/planner/plan-197"))
        );
        assert_eq!(
            topic_dir_for(Some(&planner), "plan-198", "agents", data_root, agents_ws),
            Some(PathBuf::from("/data/agents/planner/plan-198")),
            "two topics of one agent must not share a directory"
        );

        // No pipe topic → topic name defaults to the agent name, which
        // still gets its own directory under the agent root.
        assert_eq!(
            topic_dir_for(Some(&planner), "planner", "agents", data_root, agents_ws),
            Some(PathBuf::from("/data/agents/planner/planner"))
        );

        // A pinned topic_path holds the self-named topic only...
        assert_eq!(
            topic_dir_for(Some(&pinned), "jyc", "agents", data_root, agents_ws),
            Some(PathBuf::from("/projects/jyc"))
        );
        // ...and never swallows the agent's other topics.
        assert_eq!(
            topic_dir_for(
                Some(&pinned),
                "mail-invoice",
                "agents",
                data_root,
                agents_ws
            ),
            Some(PathBuf::from("/data/agents/jyc/mail-invoice"))
        );

        // Regular channels: only the self-named pin, otherwise no override
        // (caller falls back to `<workspace>/<topic>`).
        assert_eq!(
            topic_dir_for(Some(&pinned), "jyc", "email", data_root, agents_ws),
            Some(PathBuf::from("/projects/jyc"))
        );
        assert_eq!(
            topic_dir_for(Some(&planner), "anything", "email", data_root, agents_ws),
            None
        );
        assert_eq!(
            topic_dir_for(None, "x", "agents", data_root, agents_ws),
            None
        );
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
