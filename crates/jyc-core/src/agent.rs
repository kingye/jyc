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
    /// Whether reply was already sent by MCP tool
    pub reply_sent_by_tool: bool,
    /// Raw AI response text (for outbound adapter to format + send + store)
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
    /// Returns `AgentResult` with either:
    /// - `reply_sent_by_tool: true` — MCP tool already sent the reply
    /// - `reply_text: Some(text)` — raw AI text for outbound adapter to handle
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

    /// Reset session for a topic with configurable compression.
    ///
    /// Default: delete session and context files (no compression).
    async fn reset_session(
        &self,
        topic_path: &Path,
        topic_name: &str,
        config: &jyc_types::channel::ResetCompressionConfig,
    ) -> Result<()>;
}
