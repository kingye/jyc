//! `[agents.<name>]` configuration — websocket-based endpoints with
//! behavior but no matching rules.
//!
//! Each `[agents.<name>]` entry synthesizes a websocket channel
//! (`channel_type = "websocket"`, `channel_name = "agents"`) with one
//! implicit pattern whose name equals the agent name. The agent's
//! fields mirror `ChannelPattern`'s behavior surface, minus `rules`
//! and the pattern-identification fields (`name`, `channel`,
//! `enabled`, `pipe`, `topic_prefix`).
//!
//! Backward compat: `[channels.<name>] type = "websocket"` with
//! patterns is still accepted (deprecated, see
//! `serve::synthesize_agents_channel`).

use serde::Deserialize;
use std::path::PathBuf;

use super::{InboundAttachmentConfig, McpServerConfig};
use crate::channel::{
    AccessConfig, ChannelPattern, ContextStrategyConfig, PatternRules, ResetCompressionConfig,
};

/// Behavior surface for a single `[agents.<name>]` entry.
///
/// The `name` itself is the TOML table key, not a field here.
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct AgentConfig {
    /// Template name to initialize the topic (from workdir/templates/).
    #[serde(default)]
    pub template: Option<String>,

    /// Per-agent filesystem access whitelist.
    #[serde(default)]
    pub access: Option<AccessConfig>,

    /// Custom filesystem path for the topic directory.
    ///
    /// When unset, the default is `<data_home>/agents/<agent_name>/`,
    /// with the topic as a subdirectory beneath that.
    /// `~` expands to `$HOME`. Absolute paths are used as-is.
    #[serde(default)]
    pub topic_path: Option<String>,

    /// Per-agent attachment download configuration.
    #[serde(default)]
    pub attachments: Option<InboundAttachmentConfig>,

    /// Agent role name (e.g., "Planner", "Developer", "Reviewer").
    #[serde(default)]
    pub role: Option<String>,

    /// Whether to enable live message injection during AI processing.
    /// When true (default), new messages arriving while the AI is
    /// processing are injected into the active session immediately.
    /// When false, messages queue and are processed sequentially.
    #[serde(default = "default_true")]
    pub live_injection: bool,

    /// Whether to auto-inject inbound `image/*` attachments into the
    /// first user turn of the agent loop as multimodal content blocks.
    /// Only takes effect when the active model has `supports_images = true`.
    #[serde(default)]
    pub inject_inbound_images: bool,

    /// Per-agent model override (e.g., "anthropic/claude-opus-4-6").
    /// Takes priority over the global `[ai].model`.
    #[serde(default)]
    pub model: Option<String>,

    /// Model override for plan mode. Falls back to `model` if unset.
    #[serde(default)]
    pub plan_model: Option<String>,

    /// Model override for build mode. Falls back to `model` if unset.
    #[serde(default)]
    pub build_model: Option<String>,

    /// Per-agent `small_model` override.
    #[serde(default)]
    pub small_model: Option<String>,

    /// Initial agent mode: "plan" or "build".
    #[serde(default)]
    pub mode: Option<String>,

    /// Per-agent MCP server configurations.
    /// When set to `Some(list)`, only these MCP servers are loaded for
    /// topics routed to this agent. `Some([])` disables all MCP tools.
    #[serde(default)]
    pub mcps: Option<Vec<McpServerConfig>>,

    /// Per-agent tools to disable.
    /// Tool names match `Tool::name()` (e.g., `"bash"`, `"write"`).
    /// Merged with `disabled_builtin_tools` (deprecated alias).
    #[serde(default)]
    pub disabled_tools: Option<Vec<String>>,

    /// Per-agent built-in tools to disable.
    /// **Deprecated**: use `disabled_tools` instead.
    #[serde(default)]
    pub disabled_builtin_tools: Option<Vec<String>>,

    /// Per-agent MCP servers to disable.
    /// Server names match `McpServerConfig.name`.
    #[serde(default)]
    pub disabled_mcp_servers: Option<Vec<String>>,

    /// Per-agent skills whitelist.
    /// When set, only skills whose names appear in this list are loaded.
    #[serde(default)]
    pub skills: Option<Vec<String>>,

    /// Per-agent skills to disable.
    #[serde(default)]
    pub disabled_skills: Option<Vec<String>>,

    /// Per-agent compression configuration for session reset.
    #[serde(default)]
    pub reset_compression: Option<ResetCompressionConfig>,

    /// Auto-reset threshold as a fraction of context window (0.0~1.0).
    /// Falls back to `[ai].auto_reset_threshold` (default 0.95) when unset.
    #[serde(default)]
    pub auto_reset_threshold: Option<f64>,

    /// Per-agent context management strategy.
    ///
    /// Falls back to `[ai].context_strategy` when unset.
    #[serde(default)]
    pub context_strategy: Option<ContextStrategyConfig>,
}

fn default_true() -> bool {
    true
}

impl AgentConfig {
    /// Copy every AgentConfig behavior field into a `ChannelPattern`,
    /// setting the pattern's identity fields to point at this agent.
    ///
    /// Single source of truth for the AgentConfig → ChannelPattern
    /// mirror. Adding a new behavior field to AgentConfig only requires
    /// updating this method; both the CLI synthesis
    /// (`synthesize_agent_pattern`) and the validation pass
    /// (`validate_agent`) use it, so they cannot drift.
    ///
    /// The caller supplies `agent_name` (the TOML table key, not a
    /// field on AgentConfig itself) and the resolved default
    /// `topic_path` when the user did not set one.
    pub fn fill_into_pattern(
        &self,
        pattern: &mut ChannelPattern,
        agent_name: &str,
        default_topic_path: PathBuf,
    ) {
        pattern.name = agent_name.to_string();
        pattern.channel = "agents".to_string();
        pattern.enabled = true;
        pattern.rules = PatternRules::default();
        pattern.pipe = None;
        pattern.topic_path = Some(
            self.topic_path
                .clone()
                .unwrap_or_else(|| default_topic_path.to_string_lossy().into_owned()),
        );
        pattern.topic_name = None;
        pattern.topic_prefix = None;
        pattern.attachments = self.attachments.clone();
        pattern.template = self.template.clone();
        pattern.role = self.role.clone();
        pattern.live_injection = self.live_injection;
        pattern.inject_inbound_images = self.inject_inbound_images;
        pattern.model = self.model.clone();
        pattern.plan_model = self.plan_model.clone();
        pattern.build_model = self.build_model.clone();
        pattern.small_model = self.small_model.clone();
        pattern.mode = self.mode.clone();
        pattern.mcps = self.mcps.clone();
        pattern.disabled_tools = self.disabled_tools.clone();
        pattern.disabled_builtin_tools = self.disabled_builtin_tools.clone();
        pattern.disabled_mcp_servers = self.disabled_mcp_servers.clone();
        pattern.skills = self.skills.clone();
        pattern.disabled_skills = self.disabled_skills.clone();
        pattern.reset_compression = self.reset_compression.clone();
        pattern.auto_reset_threshold = self.auto_reset_threshold;
        pattern.access = self.access.clone();
        pattern.context_strategy = self.context_strategy.clone();
    }
}
