use crate::channel::InboundMessage;
use crate::channel::PatternMatch;
use crate::config::InboundAttachmentConfig;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Metadata for a discovered skill, parsed from its `SKILL.md` frontmatter.
///
/// Lives here (not in `jyc-agent`) because the layers that name a skill are
/// not just the agent: `jyc-core` resolves the `/skill:<name>` commands for a
/// topic and `jyc-inspect` puts their descriptions on the wire for the TUI.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillMeta {
    /// Skill name (e.g., "coding-principles")
    pub name: String,
    /// Human-readable description
    pub description: String,
    /// Path to the skill's directory (contains SKILL.md)
    pub source_path: PathBuf,
}

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
