//! Local TUI channel inbound adapter and matcher.

use anyhow::Result;
use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

use jyc_types::{
    ChannelMatcher, ChannelPattern, InboundAdapterOptions, InboundMessage, PatternMatch,
};

/// Local channel-specific pattern matching and thread name derivation.
pub struct LocalMatcher {
    channel_name: String,
}

impl LocalMatcher {
    /// Create a new local matcher.
    pub fn new(channel_name: String) -> Self {
        Self { channel_name }
    }
}

impl ChannelMatcher for LocalMatcher {
    fn channel_type(&self) -> &str {
        "local"
    }

    fn derive_thread_name(
        &self,
        _message: &InboundMessage,
        _patterns: &[ChannelPattern],
        _pattern_match: Option<&PatternMatch>,
    ) -> String {
        // Each local channel has exactly one thread named after the channel.
        self.channel_name.clone()
    }

    fn match_message(
        &self,
        _message: &InboundMessage,
        patterns: &[ChannelPattern],
    ) -> Option<PatternMatch> {
        // Local input is always for this channel — match the first enabled pattern.
        patterns.iter().find(|p| p.enabled).map(|p| PatternMatch {
            pattern_name: p.name.clone(),
            channel: "local".to_string(),
            matches: std::collections::HashMap::new(),
        })
    }
}

/// Local TUI inbound adapter.
///
/// Bridges TUI (blocking terminal I/O) ↔ async message processing
/// via mpsc channels and `tokio::task::spawn_blocking`.
pub struct LocalInboundAdapter {
    channel_name: String,
}

impl LocalInboundAdapter {
    /// Create a new local inbound adapter.
    pub fn new(channel_name: String) -> Self {
        Self { channel_name }
    }
}

impl ChannelMatcher for LocalInboundAdapter {
    fn channel_type(&self) -> &str {
        "local"
    }

    fn derive_thread_name(
        &self,
        message: &InboundMessage,
        patterns: &[ChannelPattern],
        pattern_match: Option<&PatternMatch>,
    ) -> String {
        LocalMatcher::new(self.channel_name.clone()).derive_thread_name(
            message,
            patterns,
            pattern_match,
        )
    }

    fn match_message(
        &self,
        message: &InboundMessage,
        patterns: &[ChannelPattern],
    ) -> Option<PatternMatch> {
        LocalMatcher::new(self.channel_name.clone()).match_message(message, patterns)
    }
}

#[async_trait]
impl jyc_types::InboundAdapter for LocalInboundAdapter {
    async fn start(
        &self,
        _options: InboundAdapterOptions,
        _cancel: CancellationToken,
    ) -> Result<()> {
        unimplemented!()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn create_test_message() -> InboundMessage {
        InboundMessage {
            id: "test".to_string(),
            channel: "local".to_string(),
            channel_uid: "user".to_string(),
            sender: "user".to_string(),
            sender_address: "user".to_string(),
            recipients: vec![],
            topic: "Test".to_string(),
            content: jyc_types::MessageContent {
                text: Some("hello".to_string()),
                html: None,
                markdown: None,
            },
            timestamp: chrono::Utc::now(),
            thread_refs: None,
            reply_to_id: None,
            external_id: None,
            attachments: vec![],
            metadata: std::collections::HashMap::new(),
            matched_pattern: None,
        }
    }

    #[test]
    fn test_derive_thread_name() {
        let matcher = LocalMatcher::new("my-local".to_string());
        let msg = create_test_message();
        let name = matcher.derive_thread_name(&msg, &[], None);
        assert_eq!(name, "my-local");
    }

    #[test]
    fn test_match_message_first_enabled() {
        let matcher = LocalMatcher::new("my-local".to_string());
        let msg = create_test_message();

        let patterns = vec![
            ChannelPattern {
                name: "p1".to_string(),
                channel: "local".to_string(),
                enabled: true,
                ..Default::default()
            },
            ChannelPattern {
                name: "p2".to_string(),
                channel: "local".to_string(),
                enabled: false,
                ..Default::default()
            },
        ];

        let result = matcher.match_message(&msg, &patterns);
        assert!(result.is_some());
        assert_eq!(result.unwrap().pattern_name, "p1");
    }

    #[test]
    fn test_match_message_skips_disabled() {
        let matcher = LocalMatcher::new("my-local".to_string());
        let msg = create_test_message();

        let patterns = vec![
            ChannelPattern {
                name: "p1".to_string(),
                channel: "local".to_string(),
                enabled: false,
                ..Default::default()
            },
            ChannelPattern {
                name: "p2".to_string(),
                channel: "local".to_string(),
                enabled: true,
                ..Default::default()
            },
        ];

        let result = matcher.match_message(&msg, &patterns);
        assert!(result.is_some());
        assert_eq!(result.unwrap().pattern_name, "p2");
    }

    #[test]
    fn test_match_message_none_when_all_disabled() {
        let matcher = LocalMatcher::new("my-local".to_string());
        let msg = create_test_message();

        let patterns = vec![ChannelPattern {
            name: "p1".to_string(),
            channel: "local".to_string(),
            enabled: false,
            ..Default::default()
        }];

        let result = matcher.match_message(&msg, &patterns);
        assert!(result.is_none());
    }
}
