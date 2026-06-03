use std::collections::HashMap;
use std::sync::Arc;

use jyc_types::{InboundAdapter, OutboundAdapter};

/// Registry for looking up channel adapters by name.
///
/// Each channel name (e.g., "work", "personal") maps to its
/// inbound and outbound adapters.
pub struct ChannelRegistry {
    inbound: HashMap<String, Arc<dyn InboundAdapter>>,
    outbound: HashMap<String, Arc<dyn OutboundAdapter>>,
}

impl ChannelRegistry {
    pub fn new() -> Self {
        Self {
            inbound: HashMap::new(),
            outbound: HashMap::new(),
        }
    }

    pub fn register_inbound(&mut self, name: String, adapter: Arc<dyn InboundAdapter>) {
        self.inbound.insert(name, adapter);
    }

    pub fn register_outbound(&mut self, name: String, adapter: Arc<dyn OutboundAdapter>) {
        self.outbound.insert(name, adapter);
    }

    pub fn get_inbound(&self, name: &str) -> Option<&Arc<dyn InboundAdapter>> {
        self.inbound.get(name)
    }

    pub fn get_outbound(&self, name: &str) -> Option<&Arc<dyn OutboundAdapter>> {
        self.outbound.get(name)
    }

    pub fn inbound_names(&self) -> Vec<&String> {
        self.inbound.keys().collect()
    }

    pub fn outbound_names(&self) -> Vec<&String> {
        self.outbound.keys().collect()
    }
}

impl Default for ChannelRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use jyc_types::{ChannelMatcher, ChannelPattern, InboundMessage, PatternMatch};

    struct DummyInbound;
    impl ChannelMatcher for DummyInbound {
        fn channel_type(&self) -> &str {
            "test"
        }
        fn derive_thread_name(
            &self,
            _message: &InboundMessage,
            _patterns: &[ChannelPattern],
            _pattern_match: Option<&PatternMatch>,
        ) -> String {
            "test".to_string()
        }
        fn match_message(
            &self,
            _message: &InboundMessage,
            _patterns: &[ChannelPattern],
        ) -> Option<PatternMatch> {
            None
        }
    }
    #[async_trait::async_trait]
    impl InboundAdapter for DummyInbound {
        async fn start(
            &self,
            _options: jyc_types::InboundAdapterOptions,
            _cancel: tokio_util::sync::CancellationToken,
        ) -> anyhow::Result<()> {
            Ok(())
        }
    }

    struct DummyOutbound;
    #[async_trait::async_trait]
    impl OutboundAdapter for DummyOutbound {
        fn channel_type(&self) -> &str {
            "test"
        }
        async fn connect(&self) -> anyhow::Result<()> {
            Ok(())
        }
        async fn disconnect(&self) -> anyhow::Result<()> {
            Ok(())
        }
        fn clean_body(&self, raw_body: &str) -> String {
            raw_body.to_string()
        }
        async fn send_reply(
            &self,
            _original: &jyc_types::InboundMessage,
            _reply_text: &str,
            _thread_path: &std::path::Path,
            _message_dir: &str,
            _attachments: Option<&[jyc_types::OutboundAttachment]>,
        ) -> anyhow::Result<jyc_types::SendResult> {
            Ok(jyc_types::SendResult {
                message_id: "test".to_string(),
            })
        }
        async fn send_message(
            &self,
            _recipient: &str,
            _subject: &str,
            _message: &str,
        ) -> anyhow::Result<jyc_types::SendResult> {
            Ok(jyc_types::SendResult {
                message_id: "test".to_string(),
            })
        }
    }

    #[test]
    fn new_registry_is_empty() {
        let reg = ChannelRegistry::new();
        assert!(reg.inbound_names().is_empty());
        assert!(reg.outbound_names().is_empty());
        assert!(reg.get_inbound("foo").is_none());
        assert!(reg.get_outbound("foo").is_none());
    }

    #[test]
    fn default_registry_is_empty() {
        let reg = ChannelRegistry::default();
        assert!(reg.inbound_names().is_empty());
        assert!(reg.outbound_names().is_empty());
    }

    #[test]
    fn register_and_get_inbound() {
        let mut reg = ChannelRegistry::new();
        let adapter: Arc<dyn InboundAdapter> = Arc::new(DummyInbound);
        reg.register_inbound("test".to_string(), adapter);
        assert!(reg.get_inbound("test").is_some());
        assert_eq!(reg.inbound_names().len(), 1);
        assert_eq!(reg.inbound_names()[0], "test");
    }

    #[test]
    fn register_and_get_outbound() {
        let mut reg = ChannelRegistry::new();
        let adapter: Arc<dyn OutboundAdapter> = Arc::new(DummyOutbound);
        reg.register_outbound("test".to_string(), adapter);
        assert!(reg.get_outbound("test").is_some());
        assert_eq!(reg.outbound_names().len(), 1);
        assert_eq!(reg.outbound_names()[0], "test");
    }

    #[test]
    fn get_missing_returns_none() {
        let reg = ChannelRegistry::new();
        assert!(reg.get_inbound("missing").is_none());
        assert!(reg.get_outbound("missing").is_none());
    }
}
