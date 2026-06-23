//! Local TUI channel inbound adapter and matcher.

use anyhow::Result;
use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

use jyc_types::{
    ChannelMatcher, ChannelPattern, InboundAdapterOptions, InboundMessage, PatternMatch,
};

/// Local channel-specific pattern matching and thread name derivation.
pub struct LocalMatcher;

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
        unimplemented!()
    }

    fn match_message(
        &self,
        _message: &InboundMessage,
        _patterns: &[ChannelPattern],
    ) -> Option<PatternMatch> {
        unimplemented!()
    }
}

/// Local TUI inbound adapter.
///
/// Bridges TUI (blocking terminal I/O) ↔ async message processing
/// via mpsc channels and `tokio::task::spawn_blocking`.
pub struct LocalInboundAdapter {
    #[allow(dead_code)]
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
        _message: &InboundMessage,
        _patterns: &[ChannelPattern],
        _pattern_match: Option<&PatternMatch>,
    ) -> String {
        unimplemented!()
    }

    fn match_message(
        &self,
        _message: &InboundMessage,
        _patterns: &[ChannelPattern],
    ) -> Option<PatternMatch> {
        unimplemented!()
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
