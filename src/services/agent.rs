use anyhow::Result;
use async_trait::async_trait;
use std::path::Path;

use crate::channels::types::InboundMessage;

/// Result of agent processing.
///
/// The agent is channel-agnostic — it returns raw AI text.
/// The outbound adapter handles formatting, sending, and storing.
#[derive(Debug)]
pub struct AgentResult {
    /// Whether the reply was already sent by the MCP tool
    /// (the tool owns the full reply lifecycle: format + send + store)
    pub reply_sent_by_tool: bool,
    /// Raw AI response text (for outbound adapter to format + send + store)
    pub reply_text: Option<String>,
}

/// Trait for agent services that generate AI responses.
///
/// Each agent mode ("opencode", "static", future modes) implements this trait.
/// The agent is channel-agnostic — it does NOT know about email, FeiShu, etc.
///
/// The agent is responsible for:
/// - AI interaction (prompts, sessions, streaming, error recovery)
/// - Returning raw response text
///
/// The agent is NOT responsible for:
/// - Reply formatting (quoted history, email threading) — that's the outbound adapter
/// - Sending replies — that's the outbound adapter
/// - Storing replies — that's the outbound adapter
#[async_trait]
pub trait AgentService: Send + Sync {
    /// Generate a response for a message.
    ///
    /// Returns `AgentResult` with either:
    /// - `reply_sent_by_tool: true` — MCP tool already sent the reply
    /// - `reply_text: Some(text)` — raw AI text for outbound adapter to handle
    async fn process(
        &self,
        message: &InboundMessage,
        thread_name: &str,
        thread_path: &Path,
        message_dir: &str,
    ) -> Result<AgentResult>;
}
