use crate::channel::InboundMessage;
use crate::channel::PatternMatch;
use crate::config::InboundAttachmentConfig;
use std::path::PathBuf;

/// An item in a topic's message queue.
#[derive(Debug)]
pub struct QueueItem {
    pub topic_name: String,
    pub message: InboundMessage,
    #[allow(dead_code)]
    pub pattern_match: PatternMatch,
    pub attachment_config: Option<InboundAttachmentConfig>,
    pub template: Option<String>,
    pub live_injection: bool,
    /// Custom filesystem path for the topic directory (from pattern's `topic_path`).
    /// When set, overrides the default `<workspace>/<topic_name>/` path.
    pub topic_path_override: Option<PathBuf>,
}
