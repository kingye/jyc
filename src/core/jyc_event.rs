use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type")]
pub enum JycEvent {
    SystemStarted {
        version: String,
        channels: Vec<String>,
    },

    SystemStopping,

    ChannelConnected {
        channel: String,
        channel_type: String,
    },

    ChannelDisconnected {
        channel: String,
        reason: String,
    },

    MessageReceived {
        channel: String,
        thread: String,
        sender: String,
        topic: Option<String>,
    },

    MessageMatched {
        channel: String,
        thread: String,
        pattern: String,
    },

    MessageDropped {
        channel: String,
        thread: String,
        reason: String,
    },

    ThreadCreated {
        thread: String,
        channel: String,
    },

    ThreadClosed {
        thread: String,
        channel: String,
    },

    ProcessingStarted {
        thread: String,
        message_id: String,
        sender: String,
        topic: Option<String>,
    },

    ProcessingProgress {
        thread: String,
        elapsed_secs: u64,
        activity: String,
        progress: Option<f32>,
    },

    ProcessingCompleted {
        thread: String,
        success: bool,
        duration_secs: u64,
    },

    ToolStarted {
        thread: String,
        tool: String,
    },

    ToolCompleted {
        thread: String,
        tool: String,
        success: bool,
        duration_secs: u64,
    },

    ReplySent {
        thread: String,
        channel: String,
        via: String,
    },

    ReplyFailed {
        thread: String,
        error: String,
    },

    Heartbeat {
        thread: String,
        elapsed_secs: u64,
        activity: String,
    },
}
