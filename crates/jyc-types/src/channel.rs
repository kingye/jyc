use anyhow::Result;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use tokio_util::sync::CancellationToken;

use crate::config::InboundAttachmentConfig;
use crate::config::McpServerConfig;

/// Channel type identifier (e.g., "email", "feishu", "slack")
pub type ChannelType = String;

/// Channel-agnostic normalized message.
///
/// Produced by InboundAdapter from channel-specific raw data.
/// All downstream consumers (storage, router, prompt builder) work with this type.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InboundMessage {
    /// Internal UUID
    pub id: String,
    /// Channel type: "email", "feishu", etc.
    pub channel: ChannelType,
    /// Channel-specific message ID (e.g., IMAP UID, feishu msg ID)
    pub channel_uid: String,
    /// Display name of the sender
    pub sender: String,
    /// Canonical sender address (email address, feishu user ID, etc.)
    pub sender_address: String,
    /// Recipient addresses/IDs
    pub recipients: Vec<String>,
    /// Subject (email) / title (feishu) — cleaned at ingest (no Re:/回复: prefixes)
    pub topic: String,
    /// Message content in multiple formats
    pub content: MessageContent,
    /// When the message was sent/received
    pub timestamp: DateTime<Utc>,
    /// Email: References header values; FeiShu: topic ID
    pub references: Option<Vec<String>>,
    /// Email: In-Reply-To header; FeiShu: parent msg ID
    pub reply_to_id: Option<String>,
    /// Email: Message-ID header; FeiShu: message ID
    pub external_id: Option<String>,
    /// Message attachments
    pub attachments: Vec<MessageAttachment>,
    /// Channel-specific extra data (email headers, feishu chat_id, etc.)
    pub metadata: HashMap<String, serde_json::Value>,
    /// Name of the pattern that matched this message (set by router)
    pub matched_pattern: Option<String>,
}

/// Message content in multiple formats.
/// At least one format should be present.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MessageContent {
    /// Plain text body
    pub text: Option<String>,
    /// HTML body (email)
    pub html: Option<String>,
    /// Markdown body (feishu, slack)
    pub markdown: Option<String>,
}

/// A message attachment.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessageAttachment {
    /// Original filename
    pub filename: String,
    /// MIME content type
    pub content_type: String,
    /// Size in bytes
    pub size: usize,
    /// Binary content — transient, only present during processing.
    /// Freed after saving to disk.
    #[serde(skip)]
    #[allow(dead_code)]
    pub content: Option<Vec<u8>>,
    /// Path where the attachment was saved (set after saving to disk)
    pub saved_path: Option<PathBuf>,
}

/// Result of pattern matching on an inbound message.
#[derive(Debug, Clone)]
pub struct PatternMatch {
    /// Name of the matched pattern
    pub pattern_name: String,
    /// Channel type of the matched pattern
    #[allow(dead_code)]
    pub channel: ChannelType,
    /// Channel-specific match details
    #[allow(dead_code)]
    pub matches: HashMap<String, String>,
}

/// Outbound attachment to include in a reply.
#[derive(Debug, Clone)]
pub struct OutboundAttachment {
    pub filename: String,
    pub path: PathBuf,
    pub content_type: String,
}

/// Result of sending a message.
#[derive(Debug)]
pub struct SendResult {
    pub message_id: String,
}

/// Options passed to an inbound adapter's `start()` method.
pub struct InboundAdapterOptions {
    /// Callback for each received message (fire-and-forget)
    pub on_message: Box<dyn Fn(InboundMessage) -> Result<()> + Send + Sync>,
    /// Callback for topic close events (e.g., chat disbanded)
    pub on_topic_close: Option<Box<dyn Fn(String) -> Result<()> + Send + Sync>>,
    /// Callback for errors
    #[allow(dead_code)]
    pub on_error: Box<dyn Fn(anyhow::Error) + Send + Sync>,
    /// Attachment download configuration
    pub attachment_config: Option<InboundAttachmentConfig>,
}

/// Channel-specific message matching and topic name derivation.
///
/// Pure-logic trait used by MessageRouter. Every channel type implements this.
/// Separated from InboundAdapter to allow use without the lifecycle (start/stop) —
/// e.g., email uses ImapMonitor for the connection lifecycle but EmailMatcher
/// for pattern matching and topic name derivation.
pub trait ChannelMatcher: Send + Sync {
    /// The channel type this matcher handles (e.g., "email", "feishu")
    #[allow(dead_code)]
    fn channel_type(&self) -> &str;

    /// Derive a topic name from the message and patterns.
    ///
    /// Each channel type has its own topic naming strategy:
    /// - Email: strips Re:/Fwd: prefixes and subject pattern prefixes, sanitizes
    /// - Feishu: uses chat_id or user_id from metadata
    fn derive_topic_name(
        &self,
        message: &InboundMessage,
        patterns: &[ChannelPattern],
        pattern_match: Option<&PatternMatch>,
    ) -> String;

    /// Check if a message matches any of the given patterns.
    ///
    /// Returns the first matching pattern, or None.
    /// Each channel type checks only the PatternRules fields relevant to it.
    fn match_message(
        &self,
        message: &InboundMessage,
        patterns: &[ChannelPattern],
    ) -> Option<PatternMatch>;

    /// Determine whether unmatched messages should be stored for this channel type.
    ///
    /// Defaults to `false` for backward compatibility (skip unmatched messages).
    /// Can be overridden by channel implementations that want to store all messages
    /// regardless of pattern matching (e.g., Feishu for full conversation context).
    fn store_unmatched_messages(&self) -> bool {
        false
    }
}

/// Inbound adapter trait — adds connection lifecycle on top of ChannelMatcher.
///
/// Responsible for:
/// - Receiving messages from the channel (WebSocket, polling, etc.)
/// - Delivering received messages via the `on_message` callback
#[async_trait]
pub trait InboundAdapter: ChannelMatcher {
    /// Start the adapter (e.g., connect to WebSocket and begin monitoring).
    /// Should run until the cancellation token is triggered.
    async fn start(&self, options: InboundAdapterOptions, cancel: CancellationToken) -> Result<()>;
}

/// Outbound adapter trait — one implementation per channel type.
///
/// Responsible for:
/// - Sending replies through the channel (including full-lifecycle: format + send + store)
/// - Format conversion (e.g., markdown → HTML for email, markdown for feishu)
/// - Adding channel-specific headers (References, etc.)
/// - Channel-specific body cleaning (e.g., stripping quoted email history)
#[async_trait]
pub trait OutboundAdapter: Send + Sync {
    /// The channel type this adapter handles
    #[allow(dead_code)]
    fn channel_type(&self) -> &str;

    /// Establish connection to the outbound service
    async fn connect(&self) -> Result<()>;

    /// Disconnect from the outbound service
    #[allow(dead_code)]
    async fn disconnect(&self) -> Result<()>;

    /// Strip channel-specific artifacts from a message body.
    ///
    /// For email: strips quoted reply history ("> On ... wrote:" blocks).
    /// For feishu/other channels: may be a no-op or strip channel-specific quoting.
    fn clean_body(&self, raw_body: &str) -> String;

    /// Send a reply with full lifecycle management.
    ///
    /// This is the primary method called by TopicManager and process_message.
    /// Each channel implementation handles:
    /// - Channel-specific formatting (quoted history for email, etc.)
    /// - Sending via the channel's transport (SMTP, HTTP API, etc.)
    /// - Storing the reply to the chat log
    async fn send_reply(
        &self,
        original: &InboundMessage,
        reply_text: &str,
        topic_path: &Path,
        message_dir: &str,
        attachments: Option<&[OutboundAttachment]>,
    ) -> Result<SendResult>;

    /// Send a fresh (non-reply) message to an arbitrary recipient.
    async fn send_message(&self, recipient: &str, subject: &str, body: &str) -> Result<SendResult>;

    /// Send a fresh message with file attachments.
    ///
    /// Default implementation returns an error so non-email channels fail
    /// gracefully rather than silently dropping attachments.
    async fn send_message_with_attachments(
        &self,
        _recipient: &str,
        _subject: &str,
        _body: &str,
        _attachments: Option<&[OutboundAttachment]>,
    ) -> Result<SendResult> {
        Err(anyhow::anyhow!(
            "Attachments not supported for this channel type"
        ))
    }

    /// Send a processing indicator to inform the user that AI is working.
    ///
    /// Channels that support streaming (e.g., WeCom Bot) can show a
    /// "thinking..." message before the final reply arrives.
    ///
    /// Returns an optional handle that the channel can use to correlate
    /// the final reply with the indicator.
    async fn send_processing_indicator(
        &self,
        _original: &InboundMessage,
    ) -> Result<Option<String>> {
        Ok(None)
    }

    /// Update a previously sent processing indicator with new content.
    ///
    /// Channels that support streaming can update the indicator text
    /// in-place (e.g., showing a rotating spinner or changing activity).
    ///
    /// The `handle` is the value returned by `send_processing_indicator`.
    async fn update_processing_indicator(
        &self,
        _original: &InboundMessage,
        _handle: &str,
        _content: &str,
    ) -> Result<()> {
        Ok(())
    }

    /// Clear a previously sent processing indicator.
    ///
    /// Called when AI processing fails or produces no reply, to ensure
    /// the indicator does not remain stuck in an intermediate state.
    /// The `handle` is the value returned by `send_processing_indicator`.
    async fn clear_processing_indicator(&self, _handle: Option<String>) -> Result<()> {
        Ok(())
    }
}

// --- Pattern Types ---

/// Message metadata key carrying a pipe's explicit target pattern name.
/// Written by the pipe retarget path (jyc-cli) and honored by the target
/// channel's matcher (e.g. `WebsocketMatcher`). Defined here so writer and
/// reader share one source of truth.
pub const PIPE_PATTERN_METADATA_KEY: &str = "pipe_pattern";

/// Target of a `ChannelPattern.pipe`: which (channel, topic) to forward
/// matching messages into.
///
/// `pattern` names a pattern on the target channel whose config
/// (`topic_path`, template, skills, model) applies to the piped message.
/// `topic` is the target topic name and may embed the runtime placeholder
/// `${msg.chat_name}`. Resolution: `effective_topic = topic ?? pattern`;
/// at least one must be set. Legacy `topic`-only form keeps the implicit
/// "topic name == pattern name" selection of websocket channels.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PipeTarget {
    /// Target channel name (typically a websocket-type channel).
    pub channel: String,
    /// Target pattern name whose config applies. When `topic` is omitted,
    /// the pattern name doubles as the topic name.
    #[serde(default)]
    pub pattern: Option<String>,
    /// Target topic name; supports the `${msg.chat_name}` runtime
    /// placeholder. Defaults to `pattern` when omitted.
    #[serde(default)]
    pub topic: Option<String>,
}

/// A channel pattern defines matching rules for a specific channel.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChannelPattern {
    /// Pattern name (used as identifier)
    pub name: String,
    /// Channel this pattern applies to
    #[serde(default)]
    pub channel: ChannelType,
    /// Whether this pattern is active
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Pipe messages matching this pattern into another (websocket) channel's
    /// topic.
    ///
    /// When set, matching messages act as a client of the target channel:
    /// they are re-targeted to `channel`/`topic` and routed through the
    /// target channel's own MessageRouter (identical to a chat-pane message),
    /// so the target topic's pattern — `topic_path`, template, skills,
    /// model — applies. Replies come back via the target channel's broadcast
    /// and are relayed to this channel's users. When `None` (default), the
    /// message is routed normally through this channel's own TopicManager.
    #[serde(default)]
    pub pipe: Option<PipeTarget>,
    /// Matching rules (channel-specific)
    #[serde(default)]
    pub rules: PatternRules,
    /// Attachment download configuration for messages matching this pattern
    pub attachments: Option<InboundAttachmentConfig>,
    /// Template name to initialize topic (from workdir/templates/)
    /// If not specified, no template is applied
    #[serde(default)]
    pub template: Option<String>,
    /// Fixed topic name override.
    /// If set, all messages matching this pattern are routed to this topic
    /// instead of deriving the topic name from the message content.
    /// Channel-agnostic: works for email, Feishu, or any channel.
    #[serde(default)]
    pub topic_name: Option<String>,
    /// Topic name prefix for channels that derive topic names from message
    /// identity (e.g. GitHub issue/PR number). Combined as `{prefix}-{id}`.
    ///
    /// When two patterns can match the same identity (e.g. distinguished by
    /// labels on the same issue), they MUST declare distinct `topic_prefix`
    /// values, otherwise both patterns route to the same topic directory and
    /// the second pattern's template / AGENTS.md is silently dropped.
    ///
    /// If unset, the channel's default derivation is used (e.g. GitHub uses
    /// `issue-{N}` or `pr-{N}` based on event type).
    ///
    /// Distinct from `topic_name`, which forces a single fixed topic name
    /// regardless of message identity.
    #[serde(default)]
    pub topic_prefix: Option<String>,
    /// Custom filesystem path for the topic directory.
    ///
    /// When set, the topic's working directory is this path instead of the
    /// default `<workspace>/<topic_name>/`. Supports `~` expansion to
    /// `$HOME`. Absolute paths are used as-is.
    ///
    /// The `topic_name` (or `topic_prefix`) still controls the logical
    /// routing key — this field only changes where files are stored on disk.
    #[serde(default)]
    pub topic_path: Option<String>,
    /// Agent role name for this pattern (e.g., "Planner", "Developer", "Reviewer").
    /// Used by GitHub OutboundAdapter to prefix comments with `[Role]`.
    /// Also used to filter out the agent's own comments during polling.
    #[serde(default)]
    pub role: Option<String>,
    /// Whether to enable live message injection during AI processing.
    /// When true (default), new messages arriving while the AI is processing
    /// are injected into the active session immediately.
    /// When false, messages queue and are processed sequentially.
    #[serde(default = "default_true")]
    pub live_injection: bool,
    /// Repo group key for shared repo directories among GitHub topics.
    /// When set, topics matching this pattern share a single repo clone
    /// via symlinks, saving disk space. The group key is `"{repo_group}-{github_number}"`.
    /// Patterns without `repo_group` keep existing behavior (no symlink, no sharing).
    #[serde(default)]
    pub repo_group: Option<String>,
    /// Whether to auto-inject inbound `image/*` attachments into the first
    /// user turn of the agent loop as multimodal content blocks.
    ///
    /// Only takes effect when the active model has `supports_images = true`.
    /// When false (default), image attachments stay on disk and the agent
    /// must use the `read_image` built-in tool to load them on demand.
    #[serde(default)]
    pub inject_inbound_images: bool,
    /// Override model for this pattern's topic (e.g., "anthropic/claude-opus-4-6").
    /// Takes priority over channel-level model and global [agent].model,
    /// but below the runtime `.jyc/model-override` file.
    #[serde(default)]
    pub model: Option<String>,
    /// Override model for plan (read-only) mode. Falls back to `model` if unset.
    #[serde(default)]
    pub plan_model: Option<String>,
    /// Override model for build (full execution) mode. Falls back to `model` if unset.
    #[serde(default)]
    pub build_model: Option<String>,
    /// Override small_model for this pattern's topic.
    /// Takes priority over channel-level small_model and global [agent].small_model,
    /// but below the runtime `.jyc/model-override` file.
    #[serde(default)]
    pub small_model: Option<String>,
    /// Initial agent mode for topics matching this pattern.
    /// Valid values: "plan" or "build".
    /// Takes priority over the default "build" but below the runtime
    /// `.jyc/mode-override` file.
    #[serde(default)]
    pub mode: Option<String>,
    /// Per-pattern MCP server configurations.
    ///
    /// When set to `Some(list)`, only these MCP servers are loaded for topics
    /// matching this pattern. When `None` (default), the global `[[mcps]]` list
    /// is used for backward compatibility.
    ///
    /// Set to `Some([])` (empty list) to disable all MCP tools for this pattern.
    #[serde(default)]
    pub mcps: Option<Vec<McpServerConfig>>,
    /// Tools to disable for this pattern.
    ///
    /// Tool names match `Tool::name()` (e.g. `"bash"`, `"write"`,
    /// `"jyc_send_message"`, `"invoice/process"`).
    ///
    /// When set to `Some(list)`, those tools are removed from the registry
    /// before the agent loop starts. When `None` (default), all tools remain
    /// enabled.
    ///
    /// Merged with `disabled_builtin_tools` (backward-compatible alias) and
    /// channel-level `disabled_tools`.
    #[serde(default)]
    pub disabled_tools: Option<Vec<String>>,

    /// Built-in tools to disable for this pattern.
    ///
    /// **Deprecated**: use `disabled_tools` instead. This field is kept for
    /// backward compatibility and behaves as an alias — its entries are
    /// merged with `disabled_tools`.
    #[serde(default)]
    pub disabled_builtin_tools: Option<Vec<String>>,

    /// MCP servers to disable for this pattern.
    ///
    /// Server names match `McpServerConfig.name`. Servers listed here are
    /// skipped during tool loading even if they appear in global `[[mcps]]`,
    /// channel `mcps`, or pattern `mcps`.
    ///
    /// Merged with channel-level `disabled_mcp_servers`.
    #[serde(default)]
    pub disabled_mcp_servers: Option<Vec<String>>,

    /// Per-pattern skills whitelist.
    ///
    /// When set, only skills whose names appear in this list are loaded
    /// for topics matching this pattern. Takes priority over channel-level
    /// `skills`. When both are unset, all discovered skills are loaded.
    #[serde(default)]
    pub skills: Option<Vec<String>>,

    /// Per-pattern skills to disable.
    ///
    /// Skill names match the `name` field in SKILL.md frontmatter.
    /// Merged with channel-level `disabled_skills`.
    #[serde(default)]
    pub disabled_skills: Option<Vec<String>>,

    /// Per-pattern compression configuration for session reset.
    ///
    /// Controls how context is compressed when `/reset` or auto-reset triggers.
    /// Falls back to `[agent].reset_compression` when unset.
    #[serde(default)]
    pub reset_compression: Option<ResetCompressionConfig>,

    /// Auto-reset threshold as a fraction of context window (0.0~1.0).
    /// When `context_input_tokens >= context_window * auto_reset_threshold`,
    /// auto-reset is triggered.
    /// Falls back to `[agent].auto_reset_threshold` (default 0.95) when unset.
    #[serde(default)]
    pub auto_reset_threshold: Option<f64>,

    /// Per-pattern filesystem access whitelist.
    ///
    /// Extends the agent's read/write boundary beyond the topic working
    /// directory. Paths listed in `write` are also readable automatically.
    ///
    /// Tilde (`~`) expands to `$HOME`. Relative paths resolve against the
    /// topic working directory and are ignored (already accessible).
    ///
    /// ```toml
    /// [channels.jyc_repo.patterns.access]
    /// read = ["~/.cargo/registry/src"]
    /// write = ["/tmp/jyc-builds"]
    /// ```
    #[serde(default)]
    pub access: Option<AccessConfig>,

    /// Agent this pattern routes matched messages to.
    ///
    /// References a `[agents.<name>]` entry whose behavior config
    /// (template, skills, model, …) is overlaid under this pattern's own
    /// fields at load time (see [`AppConfig::apply_agent_overlays`]).
    #[serde(default)]
    pub agent: Option<String>,
}

/// Behavior configuration for an agent — the `[agents.<name>]` table.
///
/// An agent owns topics and works inside them; it has **no connection
/// config and no matching rules** (those live on the channel). A channel
/// pattern references an agent via `agent = "<name>"`; at load time the
/// agent's fields are overlaid under the pattern's own fields (pattern
/// wins when both set), so all downstream resolution chains are unchanged.
///
/// All fields are optional and mirror the behavior fields of
/// [`ChannelPattern`]. Not mirrored (stay pattern-only): routing fields
/// (`rules`, `pipe`, `topic_name`, `topic_prefix`), channel-specific
/// semantics (`role`, `repo_group`, `attachments`), and bool-with-default
/// fields (`live_injection`, `inject_inbound_images`) which cannot be
/// overlaid cleanly.
///
/// See `docs/agents-migration.md` (PR-3).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentInstanceConfig {
    /// Template name to initialize topics (from workdir/templates/).
    pub template: Option<String>,
    /// Custom filesystem path for topic directories.
    pub topic_path: Option<String>,
    /// Model override (e.g., "anthropic/claude-opus-4-6").
    pub model: Option<String>,
    /// Model override for plan (read-only) mode.
    pub plan_model: Option<String>,
    /// Model override for build (full execution) mode.
    pub build_model: Option<String>,
    /// Small/fast model override for ancillary LLM work.
    pub small_model: Option<String>,
    /// Initial agent mode for topics: "plan" or "build".
    pub mode: Option<String>,
    /// MCP server configurations for this agent's topics.
    pub mcps: Option<Vec<McpServerConfig>>,
    /// Tools to disable.
    pub disabled_tools: Option<Vec<String>>,
    /// MCP servers to disable.
    pub disabled_mcp_servers: Option<Vec<String>>,
    /// Skills whitelist.
    pub skills: Option<Vec<String>>,
    /// Skills to disable.
    pub disabled_skills: Option<Vec<String>>,
    /// Compression configuration for session reset.
    pub reset_compression: Option<ResetCompressionConfig>,
    /// Auto-reset threshold as a fraction of context window (0.0~1.0).
    pub auto_reset_threshold: Option<f64>,
    /// Filesystem access whitelist.
    pub access: Option<AccessConfig>,
}

impl ChannelPattern {
    /// Overlay an agent's behavior fields under this pattern's own fields.
    ///
    /// For every behavior field: the pattern's explicit value wins; when the
    /// pattern leaves it unset, the agent's value (if any) applies. Routing
    /// fields (`rules`, `pipe`, `topic_name`, …) are untouched.
    pub fn overlay_agent(&mut self, agent: &AgentInstanceConfig) {
        macro_rules! overlay {
            ($($field:ident),* $(,)?) => {
                $(if self.$field.is_none() {
                    self.$field = agent.$field.clone();
                })*
            };
        }
        overlay!(
            template,
            topic_path,
            model,
            plan_model,
            build_model,
            small_model,
            mode,
            mcps,
            disabled_tools,
            disabled_mcp_servers,
            skills,
            disabled_skills,
            reset_compression,
            auto_reset_threshold,
            access,
        );
    }
}

/// Compression strategy for session reset.
///
/// Controls how the context is compressed when a session reset is triggered
/// (either manually via `/reset` or automatically when tokens exceed the threshold).
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CompressionMode {
    /// No compression — delete all context on reset.
    None,
    /// Heuristic compaction: keep the last N user+assistant text pairs.
    #[default]
    Heuristic,
    /// LLM-based summarization: use a separate LLM call to generate a summary.
    #[serde(alias = "llm")]
    Llm,
}

/// Configuration for compression behavior on session reset.
///
/// Can be set per-pattern (`ChannelPattern.reset_compression`) or globally
/// (`AiConfig.reset_compression`). Pattern-level config takes priority.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResetCompressionConfig {
    /// Compression mode: "llm" | "heuristic" | "none". Default: "heuristic".
    #[serde(default)]
    pub mode: CompressionMode,
    /// Number of user+assistant pairs to keep in heuristic mode. Default: 3.
    #[serde(default = "default_keep_pairs")]
    pub keep_pairs: usize,
}

impl Default for ResetCompressionConfig {
    fn default() -> Self {
        Self {
            mode: CompressionMode::default(),
            keep_pairs: default_keep_pairs(),
        }
    }
}

fn default_keep_pairs() -> usize {
    3
}

/// Per-pattern filesystem access whitelist.
///
/// `read` paths widen the read boundary for tools like `read`, `bash`,
/// `grep`, and `glob`. `write` paths widen the write boundary for `write`,
/// `edit`, and `bash` — and are automatically readable too.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AccessConfig {
    /// Additional read paths outside the working directory.
    #[serde(default)]
    pub read: Vec<String>,

    /// Additional write paths outside the working directory.
    /// Write paths are also readable automatically.
    #[serde(default)]
    pub write: Vec<String>,
}

impl Default for ChannelPattern {
    fn default() -> Self {
        Self {
            name: String::new(),
            channel: String::new(),
            enabled: true,
            rules: PatternRules::default(),
            pipe: None,
            attachments: None,
            template: None,
            topic_name: None,
            topic_prefix: None,
            topic_path: None,
            role: None,
            live_injection: true,
            repo_group: None,
            inject_inbound_images: false,
            model: None,
            plan_model: None,
            build_model: None,
            small_model: None,
            mode: None,
            mcps: None,
            disabled_tools: None,
            disabled_builtin_tools: None,
            disabled_mcp_servers: None,
            skills: None,
            disabled_skills: None,
            reset_compression: None,
            auto_reset_threshold: None,
            access: None,
            agent: None,
        }
    }
}

/// Channel-agnostic pattern matching rules.
///
/// All present rules must match (AND logic).
/// Each channel's ChannelMatcher implementation only checks the fields relevant to it:
/// - Email checks: `sender`, `subject`
/// - Feishu checks: `mentions`, `keywords`, `sender`, `chat_name`
/// - GitHub checks: `github_type`, `labels`, `assignees`
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PatternRules {
    // --- Shared rules ---
    /// Sender matching rules (email address, feishu user ID, etc.)
    #[serde(default)]
    pub sender: Option<SenderRule>,

    // --- Email rules ---
    /// Subject matching rules (email only)
    #[serde(default)]
    pub subject: Option<SubjectRule>,

    // --- Feishu rules ---
    /// Feishu @mention user/bot IDs or names to match (OR logic within this rule)
    #[serde(default)]
    pub mentions: Option<Vec<String>>,
    /// Keywords to match in message body (OR logic, case-insensitive)
    #[serde(default)]
    pub keywords: Option<Vec<String>>,
    /// Feishu group chat names to match (OR logic, case-insensitive)
    /// Matches against the chat name from the Feishu API (metadata["chat_name"])
    #[serde(default)]
    pub chat_name: Option<Vec<String>>,

    // --- GitHub rules ---
    /// GitHub entity type: "issue" or "pull_request" (OR logic within this rule)
    #[serde(default)]
    pub github_type: Option<Vec<String>>,
    /// GitHub labels to match.
    /// - Flat list `["bug", "enhancement"]` → OR logic (backward compatible)
    /// - Nested list `[["bug", "enhancement"], ["test"]]` → outer AND, inner OR
    #[serde(default)]
    pub labels: Option<LabelRule>,
    /// GitHub assignees to match (OR logic: match if ANY assignee is assigned to the issue/PR)
    #[serde(default)]
    pub assignees: Option<Vec<String>>,
    /// GitHub labels that must NOT be present for the pattern to match.
    /// OR logic: if ANY exclude label is found in the message labels, the pattern does not match.
    #[serde(default)]
    pub exclude_labels: Option<Vec<String>>,
}

/// Rules for matching the sender of a message.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SenderRule {
    /// Exact email addresses (case-insensitive)
    pub exact: Option<Vec<String>>,
    /// Domain names to match (case-insensitive)
    pub domain: Option<Vec<String>>,
    /// Regex pattern to match against sender address
    pub regex: Option<String>,
}

/// Rules for matching the subject of a message.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SubjectRule {
    /// Subject prefixes to match (also stripped from topic name)
    pub prefix: Option<Vec<String>>,
    /// Regex pattern to match against subject
    pub regex: Option<String>,
}

/// Label matching rule supporting both flat (OR) and nested (AND/OR) logic.
///
/// - `Flat(vec)` → OR logic: match if ANY label in the list is present (backward compatible)
/// - `Nested(vec_of_vecs)` → CNF: outer AND, inner OR — each group must have at least one match
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum LabelRule {
    /// Flat list: `["bug", "enhancement"]` → OR logic (backward compatible)
    Flat(Vec<String>),
    /// Nested list: `[["bug", "enhancement"], ["test"]]` → outer AND, inner OR
    Nested(Vec<Vec<String>>),
}

impl LabelRule {
    /// Check if the given message labels satisfy this rule.
    ///
    /// - Flat: OR logic — at least one label in the list matches
    /// - Nested: outer AND, inner OR — each group must have at least one match
    pub fn matches(&self, msg_labels: &[String]) -> bool {
        match self {
            LabelRule::Flat(labels) => labels
                .iter()
                .any(|l| msg_labels.contains(&l.to_lowercase())),
            LabelRule::Nested(groups) => groups.iter().all(|group| {
                group.is_empty() || group.iter().any(|l| msg_labels.contains(&l.to_lowercase()))
            }),
        }
    }
}

fn default_true() -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_topic_path_deserialization() {
        let toml_str = r#"
name = "jyc"
enabled = true
topic_path = "~/projects/jyc"
[rules]
"#;
        let p: ChannelPattern = toml::from_str(toml_str).unwrap();
        assert_eq!(
            p.topic_path.as_deref(),
            Some("~/projects/jyc"),
            "topic_path should deserialize correctly"
        );
    }
}
