use anyhow::Result;
use async_trait::async_trait;
use std::path::Path;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::topic_event_bus::TopicEventBusRef;
use jyc_types::InboundMessage;
use jyc_types::QueueItem;

/// Result of agent processing.
///
/// The agent is channel-agnostic — it returns raw AI text.
/// The outbound adapter handles formatting, sending, and storing.
#[derive(Debug)]
pub struct AgentResult {
    /// Whether the agent already delivered the reply: directly through the
    /// outbound adapter, or queued via the `reply.md`/`reply-sent.flag`
    /// file relay (the signal files are still on disk in that case).
    pub reply_delivered: bool,
    /// Raw AI response text (for logging/metrics; delivery already happened
    /// inside the agent loop)
    pub reply_text: Option<String>,
}

/// Trait for agent services that generate AI responses.
///
/// Each agent mode ("agent", "static") implements this trait.
/// The agent is channel-agnostic — it does NOT know about email, FeiShu, etc.
///
/// The agent is responsible for:
/// - AI interaction (prompts, sessions, streaming, error recovery)
/// - Monitoring the queue for live message injection during processing
/// - Returning raw response text
///
/// The agent is NOT responsible for:
/// - Reply formatting (quoted history, email References) — that's the outbound adapter
/// - Sending replies — that's the outbound adapter
/// - Storing replies — that's the outbound adapter
#[async_trait]
pub trait AgentService: Send + Sync {
    /// Get the agent service base URL.
    /// Returns error if this agent mode doesn't support it.
    async fn base_url(&self) -> Result<String>;

    /// Generate a response for a message.
    ///
    /// `pending_rx` allows the agent to monitor for new messages arriving
    /// during AI processing (live message injection).
    ///
    /// Returns `AgentResult` carrying whether the reply was already
    /// delivered (directly or queued via the file relay) and the reply
    /// text itself for logging/metrics.
    async fn process(
        &self,
        message: &InboundMessage,
        topic_name: &str,
        topic_path: &Path,
        message_dir: &str,
        pending_rx: &mut mpsc::Receiver<QueueItem>,
        topic_cancel: CancellationToken,
    ) -> Result<AgentResult>;

    /// Set topic event bus for this topic.
    /// This is optional - some agent implementations may not use event buses.
    async fn set_topic_event_bus(&self, _topic_name: &str, _event_bus: Option<TopicEventBusRef>) {}

    /// Skills the given topic can invoke, resolved live.
    ///
    /// This is the *invocation* set, not the prompt's list: every
    /// discovered skill, whatever the topic's `skills` allow-list says.
    /// The allow-list decides what the system prompt lists (context
    /// cost); a skill that is not listed is still reachable via
    /// `/skill:<name>`, so narrowing the list must not remove the
    /// command row.
    ///
    /// Default: none, for agents that have no skill concept.
    fn available_skills(&self, _topic_name: &str, _topic_path: &Path) -> Vec<jyc_types::SkillMeta> {
        Vec::new()
    }

    /// Reset the session for a topic: delete the session and context
    /// files. The next turn starts from an empty context.
    async fn reset_session(&self, topic_path: &Path, topic_name: &str) -> Result<()>;
}
