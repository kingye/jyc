//! Cross-channel outbound entries for channels the external `jyc-pipe` owns.
//!
//! The hub holds no credentials for those channels, so every send delegates to
//! the hub channel's outbound adapter, whose reply path reaches the pipe.
//! [`PipeProxyOutboundAdapter`] preserves the pipe channel's identity in the
//! hub's cross-channel outbound map, which `ask_user` uses to select the
//! adapter for a topic's origin channel.
//!
//! A proxy deliberately has no question box: a box drawn on the hub is
//! invisible to a user reading the real channel, so `ask_user` keeps asking in
//! plain text where nothing else can draw one.

use std::path::Path;
use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;

use jyc_types::{InboundMessage, OutboundAdapter, OutboundAttachment, SendResult};

/// One pipe-owned channel config and the hub channels its patterns route into.
#[derive(Debug, Clone)]
pub struct PipeChannel {
    /// Channel type from the config (e.g. `"wecom_bot"`).
    pub channel_type: String,
    /// Hub channels its patterns route into. The first existing hub channel
    /// provides the adapter the proxy delegates to.
    pub targets: Vec<String>,
}

/// Outbound entry for one pipe-owned channel, keyed by its config name in the
/// hub's cross-channel outbound map.
///
/// It holds no credentials of its own: everything it sends goes through the
/// hub channel's outbound adapter.
pub struct PipeProxyOutboundAdapter {
    /// Channel type from the config.
    channel_type: String,
    /// The hub channel's outbound adapter: the only route to the pipe.
    hub: Arc<dyn OutboundAdapter>,
}

impl PipeProxyOutboundAdapter {
    /// Wrap the hub channel's adapter as the outbound entry for a pipe channel.
    pub fn new(channel_type: String, hub: Arc<dyn OutboundAdapter>) -> Self {
        Self { channel_type, hub }
    }
}

#[async_trait]
impl OutboundAdapter for PipeProxyOutboundAdapter {
    fn channel_type(&self) -> &str {
        &self.channel_type
    }

    async fn connect(&self) -> Result<()> {
        Ok(())
    }

    async fn disconnect(&self) -> Result<()> {
        Ok(())
    }

    fn clean_body(&self, raw_body: &str) -> String {
        self.hub.clean_body(raw_body)
    }

    async fn send_reply(
        &self,
        original: &InboundMessage,
        reply_text: &str,
        topic_path: &Path,
        message_dir: &str,
        attachments: Option<&[OutboundAttachment]>,
    ) -> Result<SendResult> {
        // Replies for a piped topic are delivered by the hub channel's adapter,
        // which knows the topic; this entry only preserves channel identity for
        // cross-channel lookups such as `ask_user`.
        self.hub
            .send_reply(original, reply_text, topic_path, message_dir, attachments)
            .await
    }
}
