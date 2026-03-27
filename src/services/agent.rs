use anyhow::Result;
use async_trait::async_trait;
use std::path::Path;

use crate::channels::types::InboundMessage;

/// Result of agent processing.
#[derive(Debug)]
pub struct AgentResult {
    /// Whether a reply was sent (via tool, fallback, or directly)
    pub reply_sent: bool,
    /// Summary for logging
    pub summary: String,
}

/// Trait for agent services that process messages and send replies.
///
/// Each agent mode ("opencode", "static", future modes) implements this trait.
/// The ThreadManager dispatches to the appropriate agent without knowing
/// any mode-specific logic.
///
/// The agent is responsible for:
/// - Building the full reply (with quoted history via `build_full_reply_text`)
/// - Sending the reply (via MCP tool, outbound adapter, or other mechanism)
/// - Storing the reply (reply.md)
///
/// The ThreadManager is responsible for:
/// - Queue management, concurrency control
/// - Storing received messages (received.md)
/// - Command processing (parse, execute, strip)
/// - Sending command results
/// - Checking body emptiness
/// - Dispatching to the agent
#[async_trait]
pub trait AgentService: Send + Sync {
    /// Process a message and send a reply.
    ///
    /// - `message`: The inbound message (with cleaned body after command stripping)
    /// - `thread_name`: Thread identifier
    /// - `thread_path`: Path to the thread workspace directory
    /// - `message_dir`: Name of the message subdirectory (e.g., "2026-03-27_10-00-00")
    async fn process(
        &self,
        message: &InboundMessage,
        thread_name: &str,
        thread_path: &Path,
        message_dir: &str,
    ) -> Result<AgentResult>;
}
