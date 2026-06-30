use crate::channel::InboundMessage;
use crate::channel::PatternMatch;
use crate::config::InboundAttachmentConfig;
use std::path::PathBuf;

/// An item in a thread's message queue.
#[derive(Debug)]
pub struct QueueItem {
    pub thread_name: String,
    pub message: InboundMessage,
    #[allow(dead_code)]
    pub pattern_match: PatternMatch,
    pub attachment_config: Option<InboundAttachmentConfig>,
    pub template: Option<String>,
    pub live_injection: bool,
    /// Custom filesystem path for the thread directory (from pattern's `thread_path`).
    /// When set, overrides the default `<workspace>/<thread_name>/` path.
    pub thread_path_override: Option<PathBuf>,
}
