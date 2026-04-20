use std::sync::Arc;

use crate::channels::types::{ChannelMatcher, ChannelPattern, InboundMessage};
use crate::core::message_storage::MessageStorage;
use crate::core::thread_manager::ThreadManager;

/// Routes inbound messages to the appropriate thread queue.
///
/// Channel-agnostic: delegates pattern matching and thread name derivation
/// to the `ChannelMatcher` provided by the caller.
pub struct MessageRouter {
    thread_manager: Arc<ThreadManager>,
    storage: Arc<MessageStorage>,
}

impl MessageRouter {
    pub fn new(thread_manager: Arc<ThreadManager>, storage: Arc<MessageStorage>) -> Self {
        Self { thread_manager, storage }
    }

    /// Route a message from any channel type.
    ///
    /// Pattern matching and thread name derivation are delegated to
    /// the channel-specific `ChannelMatcher` implementation.
    pub async fn route(
        &self,
        matcher: &dyn ChannelMatcher,
        mut message: InboundMessage,
        patterns: &[ChannelPattern],
    ) {
        let ch = &message.channel;

        // 1. Pattern matching (channel-specific)
        let pattern_match = match matcher.match_message(&message, patterns) {
            Some(m) => {
                tracing::info!(
                    channel = %ch,
                    pattern = %m.pattern_name,
                    sender = %message.sender_address,
                    topic = %message.topic,
                    "Pattern matched"
                );
                self.thread_manager.metrics.message_matched(&m.pattern_name);
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
                    self.store_unmatched_message(matcher, &message, patterns).await;
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

        // 2. Derive thread name
        // If the matched pattern has a fixed thread_name, use it (channel-agnostic).
        // Otherwise, derive from message content (channel-specific).
        let pattern_name = pattern_match.as_ref().expect("pattern_match should be Some").pattern_name.clone();
        
        let thread_name = patterns
            .iter()
            .find(|p| p.name == pattern_name)
            .and_then(|p| p.thread_name.clone())
            .unwrap_or_else(|| matcher.derive_thread_name(&message, patterns, pattern_match.as_ref()));

        tracing::info!(
            channel = %ch,
            thread = %thread_name,
            pattern = %pattern_name,
            "Routing to thread"
        );

        // 3. Get attachment config, template, and live_injection from the matched pattern
        let matched_pattern_name = pattern_name;
        let matched_pattern = patterns
            .iter()
            .find(|p| p.name == matched_pattern_name);
        let attachment_config = matched_pattern.and_then(|p| p.attachments.clone());
        let live_injection = matched_pattern.map(|p| p.live_injection).unwrap_or(true);
        
        // Store template name in message metadata for thread initialization
        if let Some(template) = matched_pattern.and_then(|p| p.template.clone())
        {
            message.metadata.insert("template".to_string(), serde_json::Value::String(template));
        }

        // Store role in message metadata for outbound adapter (e.g., GitHub comment prefix)
        if let Some(role) = matched_pattern.and_then(|p| p.role.clone())
        {
            message.metadata.insert("role".to_string(), serde_json::Value::String(role));
        }

        // 4. Enqueue (channel-agnostic)
        let pm = pattern_match.expect("pattern_match should be Some");
        self.thread_manager
            .enqueue(message, thread_name, pm, attachment_config, live_injection)
            .await;
    }

    /// Store an unmatched message for channels that want to keep full conversation context.
    async fn store_unmatched_message(
        &self,
        matcher: &dyn ChannelMatcher,
        message: &InboundMessage,
        patterns: &[ChannelPattern],
    ) {
        // Derive thread name even for unmatched messages
        let thread_name = matcher.derive_thread_name(message, patterns, None);

        tracing::info!(
            channel = %message.channel,
            thread = %thread_name,
            sender = %message.sender_address,
            topic = %message.topic,
            "Storing unmatched message"
        );

        // Store the message without processing (is_matched = false)
        match self.storage.store_with_match(message, &thread_name, false, None).await {
            Ok(store_result) => {
                tracing::debug!(
                    channel = %message.channel,
                    thread = %thread_name,
                    path = %store_result.message_dir,
                    "Unmatched message stored"
                );
            }
            Err(e) => {
                tracing::error!(
                    channel = %message.channel,
                    thread = %thread_name,
                    error = %e,
                    "Failed to store unmatched message"
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    
    use crate::channels::types::{
        ChannelMatcher, ChannelPattern, InboundMessage, MessageContent, PatternMatch,
    };
    use std::collections::HashMap;

    /// Mock matcher that always matches with the first pattern
    struct MockMatcher;

    impl ChannelMatcher for MockMatcher {
        fn channel_type(&self) -> &str {
            "mock"
        }

        fn derive_thread_name(
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
            thread_refs: None,
            reply_to_id: None,
            external_id: None,
            attachments: vec![],
            metadata: HashMap::new(),
            matched_pattern: None,
        }
    }

    fn test_pattern(name: &str, thread_name: Option<&str>) -> ChannelPattern {
        ChannelPattern {
            name: name.to_string(),
            thread_name: thread_name.map(|s| s.to_string()),
            ..Default::default()
        }
    }

    #[test]
    fn test_thread_name_override_used_when_set() {
        let matcher = MockMatcher;
        let message = test_message("Invoice for food");
        let patterns = vec![test_pattern("invoices", Some("invoices"))];

        let pattern_match = matcher.match_message(&message, &patterns);
        assert!(pattern_match.is_some());

        let pattern_name = pattern_match.as_ref().unwrap().pattern_name.clone();

        // With thread_name override, should use "invoices" not "Invoice for food"
        let thread_name = patterns
            .iter()
            .find(|p| p.name == pattern_name)
            .and_then(|p| p.thread_name.clone())
            .unwrap_or_else(|| matcher.derive_thread_name(&message, &patterns, pattern_match.as_ref()));

        assert_eq!(thread_name, "invoices");
    }

    #[test]
    fn test_thread_name_derived_when_no_override() {
        let matcher = MockMatcher;
        let message = test_message("Invoice for food");
        let patterns = vec![test_pattern("catch_all", None)];

        let pattern_match = matcher.match_message(&message, &patterns);
        assert!(pattern_match.is_some());

        let pattern_name = pattern_match.as_ref().unwrap().pattern_name.clone();

        // Without thread_name override, should derive from topic
        let thread_name = patterns
            .iter()
            .find(|p| p.name == pattern_name)
            .and_then(|p| p.thread_name.clone())
            .unwrap_or_else(|| matcher.derive_thread_name(&message, &patterns, pattern_match.as_ref()));

        assert_eq!(thread_name, "Invoice for food");
    }

    #[test]
    fn test_different_topics_same_thread_with_override() {
        let matcher = MockMatcher;
        let patterns = vec![test_pattern("invoices", Some("invoices"))];

        for topic in &["Invoice food", "发票 office", "Receipt hotel"] {
            let message = test_message(topic);
            let pattern_match = matcher.match_message(&message, &patterns);
            let pattern_name = pattern_match.as_ref().unwrap().pattern_name.clone();

            let thread_name = patterns
                .iter()
                .find(|p| p.name == pattern_name)
                .and_then(|p| p.thread_name.clone())
                .unwrap_or_else(|| matcher.derive_thread_name(&message, &patterns, pattern_match.as_ref()));

            assert_eq!(thread_name, "invoices", "Topic '{}' should route to 'invoices'", topic);
        }
    }

    #[test]
    fn test_live_injection_defaults_to_true() {
        let pattern = ChannelPattern::default();
        assert!(pattern.live_injection, "live_injection should default to true");
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

        assert!(!live_injection, "live_injection should be false when pattern sets it");
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

        assert!(live_injection, "live_injection should be true when pattern enables it");
    }

    #[test]
    fn test_live_injection_defaults_true_via_serde() {
        // Simulate deserialization without the live_injection field
        let pattern: ChannelPattern = toml::from_str(r#"
            name = "test"
            [rules]
        "#).unwrap();
        assert!(pattern.live_injection, "live_injection should default to true when omitted from config");
    }

    #[test]
    fn test_live_injection_false_via_serde() {
        let pattern: ChannelPattern = toml::from_str(r#"
            name = "test"
            live_injection = false
            [rules]
        "#).unwrap();
        assert!(!pattern.live_injection, "live_injection should be false when explicitly set in config");
    }
}
