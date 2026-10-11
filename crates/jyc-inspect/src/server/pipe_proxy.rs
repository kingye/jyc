//! Outbound relaying to the channels the external `jyc-pipe` process owns.
//!
//! The hub holds no credentials for those channels and shares no filesystem
//! with the pipe, so a proactive message (text, or files) reaches them as a
//! broadcast frame on the hub channel the pipe's patterns route into; the
//! pipe's forwarder for that channel does the actual send. Two pieces carry
//! that:
//!
//! - [`PipeRoute`] — which channel configs the pipe owns, what type each is,
//!   and which hub channel it routes into. The websocket outbound adapter uses
//!   it to name the addressed channel in the frame, and to tell whether a
//!   channel can carry files at all.
//! - [`PipeProxyOutboundAdapter`] — the hub's cross-channel outbound entry,
//!   keyed by a pipe channel's config name, forwarding to the hub channel's
//!   adapter (the only route to the pipe).
//!
//! A proxy deliberately has no question box: a box drawn on the hub is
//! invisible to a user reading the real channel, so `ask_user` keeps asking in
//! plain text where nothing else can draw one.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;

use jyc_types::{InboundMessage, OutboundAdapter, OutboundAttachment, OutboundMessage, SendResult};

/// One pipe-owned channel config: what it is and where its patterns route.
#[derive(Debug, Clone)]
pub struct PipeChannel {
    /// Channel type from the config (e.g. `"wecom_bot"`).
    pub channel_type: String,
    /// Hub channels its patterns route into. The pipe subscribes to each one,
    /// so a proactive frame sent over any of them reaches the forwarder that
    /// owns this channel.
    pub targets: Vec<String>,
}

/// Routing table for proactive messages addressed to pipe-owned channels.
///
/// Built once by `jyc serve` from the config and handed to the outbound
/// adapter of the hub channel it describes — that adapter serves exactly one
/// hub channel.
#[derive(Debug, Default)]
pub struct PipeRoute {
    /// Hub channel the adapter holding this route broadcasts on.
    hub_channel: String,
    /// Channel config name → what it is and where it routes.
    channels: HashMap<String, PipeChannel>,
}

impl PipeRoute {
    /// Build the route of one hub channel.
    pub fn new(
        hub_channel: impl Into<String>,
        channels: impl IntoIterator<Item = (String, PipeChannel)>,
    ) -> Self {
        Self {
            hub_channel: hub_channel.into(),
            channels: channels.into_iter().collect(),
        }
    }

    /// The channel a proactive message is addressed to: `requested` when the
    /// caller named one, else the only pipe channel routing into this hub
    /// channel.
    ///
    /// `Ok(None)` means no pipe channel is involved — a plain hub channel, or
    /// a name that is not pipe-owned — and the caller keeps the behaviour it
    /// had before the split. `Err` means the answer would be a guess: several
    /// pipe channels route into this hub channel and the caller named none.
    pub fn resolve(&self, requested: &str) -> Result<Option<(&str, &PipeChannel)>> {
        if !requested.is_empty() {
            return Ok(self
                .channels
                .get_key_value(requested)
                .map(|(name, channel)| (name.as_str(), channel)));
        }
        let mut candidates = self
            .channels
            .iter()
            .filter(|(_, channel)| channel.targets.contains(&self.hub_channel));
        match (candidates.next(), candidates.next()) {
            (Some((name, channel)), None) => Ok(Some((name.as_str(), channel))),
            (None, _) => Ok(None),
            (Some(_), Some(_)) => anyhow::bail!(
                "several pipe channels route into '{}'; name one with the `channel` parameter",
                self.hub_channel
            ),
        }
    }
}

/// Outbound entry for one pipe-owned channel, keyed by its config name in the
/// hub's cross-channel outbound map.
///
/// It holds no credentials of its own: everything it sends goes through the
/// hub channel's outbound adapter, whose broadcast frame is what reaches the
/// pipe. The addressed channel is always named, so exactly the pipe forwarder
/// that owns the recipient relays the message.
pub struct PipeProxyOutboundAdapter {
    /// Channel config name — this adapter's key, and what the frame names.
    name: String,
    /// Channel type from the config.
    channel_type: String,
    /// The hub channel's outbound adapter: the only route to the pipe.
    hub: Arc<dyn OutboundAdapter>,
}

impl PipeProxyOutboundAdapter {
    /// Wrap the hub channel's adapter as the outbound entry for `name`.
    pub fn new(name: String, channel_type: String, hub: Arc<dyn OutboundAdapter>) -> Self {
        Self {
            name,
            channel_type,
            hub,
        }
    }

    /// Relay through the hub adapter, naming this channel.
    async fn relay(&self, message: &OutboundMessage<'_>) -> Result<SendResult> {
        self.hub
            .send_proactive(&OutboundMessage {
                channel: &self.name,
                recipient: message.recipient,
                subject: message.subject,
                body: message.body,
                attachments: message.attachments,
                origin_channel: message.origin_channel,
                origin_topic: message.origin_topic,
            })
            .await
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
        // which knows the topic; this entry only sees cross-channel sends.
        self.hub
            .send_reply(original, reply_text, topic_path, message_dir, attachments)
            .await
    }

    async fn send_message(&self, recipient: &str, subject: &str, body: &str) -> Result<SendResult> {
        self.relay(&OutboundMessage {
            recipient,
            channel: &self.name,
            subject,
            body,
            attachments: None,
            origin_channel: "",
            origin_topic: "",
        })
        .await
    }

    // `send_message_with_attachments` is deliberately left to the trait
    // default: a caller reaching this entry without the origin (which only the
    // tool knows) cannot have its files delivered, and the default says so
    // rather than failing deeper with a misleading message.

    async fn send_proactive(&self, message: &OutboundMessage<'_>) -> Result<SendResult> {
        self.relay(message).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn channel(channel_type: &str, target: &str) -> PipeChannel {
        PipeChannel {
            channel_type: channel_type.to_string(),
            targets: vec![target.to_string()],
        }
    }

    fn route() -> PipeRoute {
        PipeRoute::new(
            "agents",
            [
                ("wecom_bot".to_string(), channel("wecom_bot", "agents")),
                ("feishu".to_string(), channel("feishu", "feishu-hub")),
            ],
        )
    }

    /// A named channel wins over the hub channel the adapter serves: the caller
    /// asked for exactly that channel.
    #[test]
    fn resolve_prefers_the_named_channel() {
        let route = route();
        let (name, channel) = route.resolve("feishu").unwrap().unwrap();
        assert_eq!(name, "feishu");
        assert_eq!(channel.channel_type, "feishu");
    }

    /// Without a name, the only pipe channel routing into this hub channel is
    /// the answer — that is the case for a scheduled job, which has no inbound
    /// message to carry the origin channel.
    #[test]
    fn resolve_falls_back_to_the_only_pipe_channel() {
        let route = route();
        let (name, _) = route.resolve("").unwrap().unwrap();
        assert_eq!(name, "wecom_bot");
    }

    /// Two pipe channels on one hub channel: guessing would send the message
    /// to the wrong channel, so the caller has to name one.
    #[test]
    fn resolve_refuses_to_guess_between_several_pipe_channels() {
        let route = PipeRoute::new(
            "agents",
            [
                ("wecom_bot".to_string(), channel("wecom_bot", "agents")),
                ("feishu".to_string(), channel("feishu", "agents")),
            ],
        );
        let err = route.resolve("").unwrap_err().to_string();
        assert!(err.contains("several pipe channels"), "{err}");
    }

    /// A hub channel no pipe channel routes into, and a name the pipe does not
    /// own: both report "no pipe channel", so the caller keeps the pre-split
    /// behaviour instead of inventing a route.
    #[test]
    fn resolve_reports_no_pipe_channel() {
        let elsewhere = PipeRoute::new(
            "other",
            [("wecom_bot".to_string(), channel("wecom_bot", "agents"))],
        );
        assert!(elsewhere.resolve("").unwrap().is_none());
        assert!(route().resolve("no_such_channel").unwrap().is_none());
        assert!(PipeRoute::default().resolve("").unwrap().is_none());
    }
}
