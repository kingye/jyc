use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Events that can be published to a topic's event bus.
///
/// These events are specific to a single topic and are completely
/// isolated from events in other topics.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum TopicEvent {
    /// Processing started event.
    ///
    /// Sent when the agent begins processing a message.
    ProcessingStarted {
        /// Name of the topic
        topic_name: String,
        /// ID of the message being processed
        message_id: String,
        /// When processing started
        timestamp: DateTime<Utc>,
    },

    /// Processing progress event.
    ///
    /// Sent periodically during processing to report progress.
    ProcessingProgress {
        /// Name of the topic
        topic_name: String,
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
        /// Name of the topic
        topic_name: String,
        /// ID of the message that was processed
        message_id: String,
        /// Whether processing was successful
        success: bool,
        /// How long processing took (in seconds)
        duration_secs: u64,
        /// When processing completed
        timestamp: DateTime<Utc>,
    },

    /// Loop tick event.
    ///
    /// Sent periodically (default: every 1 s, with the first tick fired
    /// immediately at t=0) by the agent loop while it is running, so the
    /// dashboard can show a live elapsed-duration indicator that ticks
    /// even during silent LLM/tool work. Marked as `is_internal` by the
    /// inspect server so it doesn't pollute the activity pane or
    /// activity.jsonl.
    LoopTick {
        /// Name of the topic
        topic_name: String,
        /// Wall-clock milliseconds since the current agent loop started.
        elapsed_ms: u64,
        /// When the tick was generated
        timestamp: DateTime<Utc>,
    },

    /// Tool started event.
    ///
    /// Sent when the agent starts executing a tool.
    ToolStarted {
        /// Name of the topic
        topic_name: String,
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
        /// Name of the topic
        topic_name: String,
        /// Name of the tool that was executed
        tool_name: String,
        /// Whether the tool execution was successful
        success: bool,
        /// How long the tool took to execute (in seconds)
        duration_secs: u64,
        /// Error output preview (only set when tool failed, truncated)
        output: Option<String>,
        /// Preview of the tool input (truncated), included so the activity
        /// panel can show what command was run even for fast tools.
        input: Option<String>,
        /// When the tool completed
        timestamp: DateTime<Utc>,
    },

    /// LLM request started event.
    ///
    /// Sent when the agent sends a request to the LLM and is waiting
    /// for the response. Lets the activity panel show "Thinking..." or
    /// "Sending to LLM..." between tool execution and response.
    LLMRequestStarted {
        /// Name of the topic
        topic_name: String,
        /// Iteration number within the current agent loop run
        iteration: usize,
        /// When the request was sent
        timestamp: DateTime<Utc>,
    },

    /// AI thinking/reasoning event.
    ///
    /// Sent when the AI model produces reasoning/thinking content
    /// (e.g., chain-of-thought before generating a response).
    Thinking {
        /// Name of the topic
        topic_name: String,
        /// Full thinking text (untruncated).
        text: String,
        /// Length of the thinking text in characters.
        /// Kept for API compatibility; always equals `text.len()`.
        full_length: usize,
        /// When the thinking was received
        timestamp: DateTime<Utc>,
    },

    /// A new message arrived in this topic.
    ///
    /// Published when `TopicManager::enqueue()` receives a message from any
    /// source (remote user, scheduled job, dashboard injection, cross-topic).
    /// Enables the dashboard to display live chat messages for non-WebSocket
    /// topics.
    IncomingMessage {
        /// Name of the topic
        topic_name: String,
        /// Sender identifier (e.g., "user", display name, "job")
        sender: String,
        /// Message body preview (may be truncated)
        text: String,
        /// When the message arrived
        timestamp: DateTime<Utc>,
    },

    /// The AI sent a reply for this topic.
    ///
    /// Published after `outbound.send_reply()` succeeds. Enables the dashboard
    /// to display live AI replies for non-WebSocket topics.
    ReplySent {
        /// Name of the topic
        topic_name: String,
        /// The AI reply text
        text: String,
        /// When the reply was sent
        timestamp: DateTime<Utc>,
    },

    /// Session status change event.
    ///
    /// Sent when the AI session status changes (e.g., retry on overload,
    /// error, rate limit). Surfaces transient issues in the Activity panel
    /// so operators can see what's happening without checking journalctl.
    SessionStatus {
        /// Name of the topic
        topic_name: String,
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

impl TopicEvent {
    /// Get the topic name from the event.
    pub fn topic_name(&self) -> &str {
        match self {
            TopicEvent::ProcessingStarted { topic_name, .. } => topic_name,
            TopicEvent::ProcessingProgress { topic_name, .. } => topic_name,
            TopicEvent::ProcessingCompleted { topic_name, .. } => topic_name,
            TopicEvent::LoopTick { topic_name, .. } => topic_name,
            TopicEvent::ToolStarted { topic_name, .. } => topic_name,
            TopicEvent::ToolCompleted { topic_name, .. } => topic_name,
            TopicEvent::LLMRequestStarted { topic_name, .. } => topic_name,
            TopicEvent::Thinking { topic_name, .. } => topic_name,
            TopicEvent::IncomingMessage { topic_name, .. } => topic_name,
            TopicEvent::ReplySent { topic_name, .. } => topic_name,
            TopicEvent::SessionStatus { topic_name, .. } => topic_name,
        }
    }

    /// Get the timestamp from the event.
    #[allow(dead_code)]
    pub fn timestamp(&self) -> DateTime<Utc> {
        match self {
            TopicEvent::ProcessingStarted { timestamp, .. } => *timestamp,
            TopicEvent::ProcessingProgress { timestamp, .. } => *timestamp,
            TopicEvent::ProcessingCompleted { timestamp, .. } => *timestamp,
            TopicEvent::LoopTick { timestamp, .. } => *timestamp,
            TopicEvent::ToolStarted { timestamp, .. } => *timestamp,
            TopicEvent::ToolCompleted { timestamp, .. } => *timestamp,
            TopicEvent::LLMRequestStarted { timestamp, .. } => *timestamp,
            TopicEvent::Thinking { timestamp, .. } => *timestamp,
            TopicEvent::IncomingMessage { timestamp, .. } => *timestamp,
            TopicEvent::ReplySent { timestamp, .. } => *timestamp,
            TopicEvent::SessionStatus { timestamp, .. } => *timestamp,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dummy_time() -> DateTime<Utc> {
        Utc::now()
    }

    #[test]
    fn processing_started_event() {
        let ts = dummy_time();
        let ev = TopicEvent::ProcessingStarted {
            topic_name: "t1".to_string(),
            message_id: "m1".to_string(),
            timestamp: ts,
        };
        assert_eq!(ev.topic_name(), "t1");
        assert_eq!(ev.timestamp(), ts);
    }

    #[test]
    fn processing_progress_event() {
        let ts = dummy_time();
        let ev = TopicEvent::ProcessingProgress {
            topic_name: "t1".to_string(),
            elapsed_secs: 10,
            activity: "working".to_string(),
            progress: Some("50%".to_string()),
            parts_count: 2,
            output_length: 100,
            timestamp: ts,
        };
        assert_eq!(ev.topic_name(), "t1");
        assert_eq!(ev.timestamp(), ts);
    }

    #[test]
    fn processing_completed_event() {
        let ts = dummy_time();
        let ev = TopicEvent::ProcessingCompleted {
            topic_name: "t1".to_string(),
            message_id: "m1".to_string(),
            success: true,
            duration_secs: 5,
            timestamp: ts,
        };
        assert_eq!(ev.topic_name(), "t1");
        assert_eq!(ev.timestamp(), ts);
    }

    #[test]
    fn loop_tick_event() {
        let ts = dummy_time();
        let ev = TopicEvent::LoopTick {
            topic_name: "t1".to_string(),
            elapsed_ms: 12_400,
            timestamp: ts,
        };
        assert_eq!(ev.topic_name(), "t1");
        assert_eq!(ev.timestamp(), ts);
    }

    #[test]
    fn tool_started_event() {
        let ts = dummy_time();
        let ev = TopicEvent::ToolStarted {
            topic_name: "t1".to_string(),
            tool_name: "bash".to_string(),
            input: Some("echo hi".to_string()),
            timestamp: ts,
        };
        assert_eq!(ev.topic_name(), "t1");
        assert_eq!(ev.timestamp(), ts);
    }

    #[test]
    fn tool_completed_event() {
        let ts = dummy_time();
        let ev = TopicEvent::ToolCompleted {
            topic_name: "t1".to_string(),
            tool_name: "bash".to_string(),
            success: true,
            duration_secs: 1,
            output: Some("hi".to_string()),
            input: Some(r#"{"command":"ls"}"#.to_string()),
            timestamp: ts,
        };
        assert_eq!(ev.topic_name(), "t1");
        assert_eq!(ev.timestamp(), ts);
    }

    #[test]
    fn llm_request_started_event() {
        let ts = dummy_time();
        let ev = TopicEvent::LLMRequestStarted {
            topic_name: "t1".to_string(),
            iteration: 3,
            timestamp: ts,
        };
        assert_eq!(ev.topic_name(), "t1");
        assert_eq!(ev.timestamp(), ts);
    }

    #[test]
    fn thinking_event() {
        let ts = dummy_time();
        let ev = TopicEvent::Thinking {
            topic_name: "t1".to_string(),
            text: "thinking...".to_string(),
            full_length: 100,
            timestamp: ts,
        };
        assert_eq!(ev.topic_name(), "t1");
        assert_eq!(ev.timestamp(), ts);
    }

    #[test]
    fn session_status_event() {
        let ts = dummy_time();
        let ev = TopicEvent::SessionStatus {
            topic_name: "t1".to_string(),
            status_type: "retry".to_string(),
            attempt: Some(1),
            message: Some("retrying".to_string()),
            timestamp: ts,
        };
        assert_eq!(ev.topic_name(), "t1");
        assert_eq!(ev.timestamp(), ts);
    }
}
