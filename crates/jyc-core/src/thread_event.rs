use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Events that can be published to a thread's event bus.
///
/// These events are specific to a single thread and are completely
/// isolated from events in other threads.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ThreadEvent {
    /// Heartbeat event indicating the agent is still processing.
    ///
    /// Sent at regular intervals during long-running tasks to
    /// let users know the agent is still working.
    Heartbeat {
        /// Name of the thread (for identification)
        thread_name: String,
        /// How long the agent has been running (in seconds)
        elapsed_secs: u64,
        /// Current activity (e.g., "generating", "tool:bash", "tool:git")
        activity: String,
        /// Progress summary (e.g., "processed 5 parts, generated 1200 chars")
        progress: String,
        /// When the heartbeat was generated
        timestamp: DateTime<Utc>,
    },

    /// Processing started event.
    ///
    /// Sent when the agent begins processing a message.
    ProcessingStarted {
        /// Name of the thread
        thread_name: String,
        /// ID of the message being processed
        message_id: String,
        /// When processing started
        timestamp: DateTime<Utc>,
    },

    /// Processing progress event.
    ///
    /// Sent periodically during processing to report progress.
    ProcessingProgress {
        /// Name of the thread
        thread_name: String,
        /// How long processing has been running (in seconds)
        elapsed_secs: u64,
        /// Current activity
        activity: String,
        /// Optional detailed progress information
        progress: Option<String>,
        /// Number of parts processed so far
        parts_count: usize,
        /// Length of output generated so far (in characters)
        output_length: usize,
        /// When the progress update was generated
        timestamp: DateTime<Utc>,
    },

    /// Processing completed event.
    ///
    /// Sent when the agent finishes processing a message.
    ProcessingCompleted {
        /// Name of the thread
        thread_name: String,
        /// ID of the message that was processed
        message_id: String,
        /// Whether processing was successful
        success: bool,
        /// How long processing took (in seconds)
        duration_secs: u64,
        /// When processing completed
        timestamp: DateTime<Utc>,
    },

    /// Tool started event.
    ///
    /// Sent when the agent starts executing a tool.
    ToolStarted {
        /// Name of the thread
        thread_name: String,
        /// Name of the tool being executed
        tool_name: String,
        /// Preview of the tool input (truncated)
        input: Option<String>,
        /// When the tool started
        timestamp: DateTime<Utc>,
    },

    /// Tool completed event.
    ///
    /// Sent when the agent finishes executing a tool.
    ToolCompleted {
        /// Name of the thread
        thread_name: String,
        /// Name of the tool that was executed
        tool_name: String,
        /// Whether the tool execution was successful
        success: bool,
        /// How long the tool took to execute (in seconds)
        duration_secs: u64,
        /// Error output preview (only set when tool failed, truncated)
        output: Option<String>,
        /// When the tool completed
        timestamp: DateTime<Utc>,
    },

    /// AI thinking/reasoning event.
    ///
    /// Sent when the AI model produces reasoning/thinking content
    /// (e.g., chain-of-thought before generating a response).
    Thinking {
        /// Name of the thread
        thread_name: String,
        /// Preview of the thinking text (truncated to ~300 chars)
        text: String,
        /// Full length of the thinking text in characters
        full_length: usize,
        /// When the thinking was received
        timestamp: DateTime<Utc>,
    },

    /// Session status change event.
    ///
    /// Sent when the AI session status changes (e.g., retry on overload,
    /// error, rate limit). Surfaces transient issues in the Activity panel
    /// so operators can see what's happening without checking journalctl.
    SessionStatus {
        /// Name of the thread
        thread_name: String,
        /// Status type (e.g., "retry", "error", "rate_limit")
        status_type: String,
        /// Retry attempt number (if applicable)
        attempt: Option<u32>,
        /// Human-readable message (e.g., "server overload, please retry later")
        message: Option<String>,
        /// When the status change occurred
        timestamp: DateTime<Utc>,
    },
}

impl ThreadEvent {
    /// Get the thread name from the event.
    pub fn thread_name(&self) -> &str {
        match self {
            ThreadEvent::Heartbeat { thread_name, .. } => thread_name,
            ThreadEvent::ProcessingStarted { thread_name, .. } => thread_name,
            ThreadEvent::ProcessingProgress { thread_name, .. } => thread_name,
            ThreadEvent::ProcessingCompleted { thread_name, .. } => thread_name,
            ThreadEvent::ToolStarted { thread_name, .. } => thread_name,
            ThreadEvent::ToolCompleted { thread_name, .. } => thread_name,
            ThreadEvent::Thinking { thread_name, .. } => thread_name,
            ThreadEvent::SessionStatus { thread_name, .. } => thread_name,
        }
    }

    /// Get the timestamp from the event.
    #[allow(dead_code)]
    pub fn timestamp(&self) -> DateTime<Utc> {
        match self {
            ThreadEvent::Heartbeat { timestamp, .. } => *timestamp,
            ThreadEvent::ProcessingStarted { timestamp, .. } => *timestamp,
            ThreadEvent::ProcessingProgress { timestamp, .. } => *timestamp,
            ThreadEvent::ProcessingCompleted { timestamp, .. } => *timestamp,
            ThreadEvent::ToolStarted { timestamp, .. } => *timestamp,
            ThreadEvent::ToolCompleted { timestamp, .. } => *timestamp,
            ThreadEvent::Thinking { timestamp, .. } => *timestamp,
            ThreadEvent::SessionStatus { timestamp, .. } => *timestamp,
        }
    }
}
