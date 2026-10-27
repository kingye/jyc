use anyhow::Result;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use tokio_util::sync::CancellationToken;

use crate::config::InboundAttachmentConfig;
use crate::config::McpServerConfig;

/// Channel type identifier (e.g., "email", "feishu", "slack")
pub type ChannelType = String;

/// Channel types the `jyc-pipe` process runs adapters for.
///
/// Ownership between the two processes is derived from this list on both
/// sides — `jyc serve` skips in-process spawn for configured channels whose
/// type appears here, and the pipe claims exactly those — so no config section
/// is needed to coordinate them. The split is complete: every pipe-capable
/// channel type is listed here and the hub spawns none of them.
///
/// It lives in this dependency-light crate so the hub can read it without
/// depending on the adapter crate.
pub const SUPPORTED_CHANNEL_TYPES: &[&str] = &[
    "feishu",
    "github",
    "gitee",
    "wecom_bot",
    "wecom",
    "wecomkf",
    "email",
];

/// Collect the distinct hub-channel targets that a pipe channel's patterns
/// route through — one pipe connection (and reply relay) per entry.
///
/// Two forms accepted per pattern:
/// - legacy `pipe = { channel = "x" }` → inserts "x"
/// - new `pipe = { agent = "x", topic = "..." }` → inserts "agents"
///   (the synthesized channel name)
///
/// Lives here rather than with the pipe adapters because both sides need it:
/// the pipe to open one connection per target, and the hub to know which hub
/// channel a proactive message for a pipe-owned channel must travel over.
pub fn collect_pipe_target_channels(patterns: &[ChannelPattern]) -> HashSet<String> {
    let mut out = HashSet::new();
    for p in patterns.iter().filter(|p| p.enabled) {
        if let Some(pipe) = &p.pipe {
            if let Some(ch) = &pipe.channel {
                out.insert(ch.clone());
            } else if pipe.agent.is_some() {
                out.insert("agents".to_string());
            }
        }
    }
    out
}

/// Channel types a proactive message can reach.
///
/// These are the channels whose adapter sent proactive messages of its own
/// before the pipe split, so the ones their pipe forwarder still accepts —
/// files included (`email`, `feishu`, `wecom_bot`). Every other type answers
/// with the long-standing error instead of reporting a success that delivers
/// nothing.
pub const PROACTIVE_CAPABLE_CHANNEL_TYPES: &[&str] = &["email", "feishu", "wecom_bot"];

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
    pub content: Option<Vec<u8>>,
    /// Path where the attachment was saved (set after saving to disk)
    pub saved_path: Option<PathBuf>,
    /// Absolute path a pipe adapter uploaded the bytes to *before* sending the
    /// message (`jyc-pipe` writes them through the hub's inbound endpoint,
    /// because the two processes need not share a filesystem). The topic
    /// worker moves the file into the resolved attachment directory, fills
    /// `saved_path`, and clears this; in-process adapters leave it `None`.
    #[serde(skip)]
    pub staged_path: Option<PathBuf>,
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
    /// Chat sequence number the adapter assigned to this reply's log row
    /// (`0` when the channel logs no chat row). The caller publishes the
    /// `ReplySent` event with it, so the live row and the logged row share
    /// one number — see `jyc_core::chat_seq`.
    pub chat_seq: u64,
}

/// A proactive (non-reply) message on its way out through a channel.
///
/// Built by the `jyc_send_message` tool, which is the only caller that knows
/// all three of the recipient, the channel it is addressed to, and where the
/// attachment files live. A channel relaying through a pipe process needs all
/// of them (the pipe has neither the files nor the channel routing); one
/// sending in-process needs only recipient/subject/body/attachments.
pub struct OutboundMessage<'a> {
    /// Channel-specific recipient identifier (a WeCom chat id, an email
    /// address, a Feishu chat id, …), as documented for `jyc_send_message`.
    pub recipient: &'a str,
    /// Channel config name the message is addressed to (e.g. `"wecom_bot"`).
    /// Empty when the caller only knows the hub channel the topic lives in —
    /// the adapter then resolves it from its own pipe routing table.
    pub channel: &'a str,
    /// Message subject: the mail subject for email, the bold lead-in for
    /// feishu, ignored by channels that have no notion of one.
    pub subject: &'a str,
    /// Message body.
    pub body: &'a str,
    /// Files to send along, when the channel supports attachments.
    pub attachments: Option<&'a [OutboundAttachment]>,
    /// Hub channel holding those files. The inspect server's file endpoint is
    /// addressed by channel and topic name, so a relay that does not share a
    /// filesystem with the hub needs both. Empty without attachments.
    pub origin_channel: &'a str,
    /// Topic under `origin_channel` holding those files.
    pub origin_topic: &'a str,
}

/// Close-event callback: upstream item number (GitHub issue/PR) plus its type
/// (`issue` / `pull_request`).
pub type CloseEventCallback = Box<dyn Fn(u64, &str) + Send + Sync>;

/// Options passed to an inbound adapter's `start()` method.
pub struct InboundAdapterOptions {
    /// Callback for each received message (fire-and-forget)
    pub on_message: Box<dyn Fn(InboundMessage) -> Result<()> + Send + Sync>,
    /// Callback for topic close events (e.g., chat disbanded)
    pub on_topic_close: Option<Box<dyn Fn(String) -> Result<()> + Send + Sync>>,
    /// Callback for close events identified by an upstream item number
    /// (GitHub issue/PR) and its type (`issue` / `pull_request`). Pipe-only
    /// adapters own no workspace, so they cannot resolve topic names by
    /// scanning directories — they re-render the configured pipe topic
    /// templates for that number and close those topics in the hub.
    pub on_close_event: Option<CloseEventCallback>,
    /// Callback for errors
    pub on_error: Box<dyn Fn(anyhow::Error) + Send + Sync>,
    /// Attachment download configuration
    pub attachment_config: Option<InboundAttachmentConfig>,
}

/// Match a message body against a pattern's keyword list.
///
/// Case-insensitive substring match, OR within the list: any hit satisfies
/// the rule. Returns the comma-joined keywords that actually hit — for
/// [`PatternMatch::matches`] details — or `None` when none did. An empty
/// list matches nothing (it is not "no constraint").
///
/// Callers pass the body in rather than an [`InboundMessage`] because
/// channels disagree on what counts as the body: some fall back to markdown,
/// some use plain text only, and WeCom treats an empty keyword list as
/// "not configured" (skip the pattern) rather than as a non-match.
pub fn match_keywords(keywords: &[String], body: &str) -> Option<String> {
    let body_lower = body.to_lowercase();
    let matched: Vec<&str> = keywords
        .iter()
        .filter(|kw| body_lower.contains(&kw.to_lowercase()))
        .map(String::as_str)
        .collect();

    if matched.is_empty() {
        None
    } else {
        Some(matched.join(","))
    }
}

/// Channel-specific message matching and topic name derivation.
///
/// Pure-logic trait used by MessageRouter. Every channel type implements this.
///
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

    /// Send a proactive message with the context that only pipe-relayed
    /// channels need: the channel it is addressed to and where the attachment
    /// files live.
    ///
    /// Defaults to the two methods above, so a channel that sends in-process
    /// keeps its behaviour unchanged. A channel that relays through a pipe
    /// overrides this, because the pipe holds the credentials and has no
    /// shared filesystem with the hub.
    async fn send_proactive(&self, message: &OutboundMessage<'_>) -> Result<SendResult> {
        match message.attachments.filter(|a| !a.is_empty()) {
            Some(attachments) => {
                self.send_message_with_attachments(
                    message.recipient,
                    message.subject,
                    message.body,
                    Some(attachments),
                )
                .await
            }
            None => {
                self.send_message(message.recipient, message.subject, message.body)
                    .await
            }
        }
    }

    /// Render an interactive question for the user.
    ///
    /// Only a channel with a question box overrides this — today the
    /// websocket channel, whose box is the TUI. The default fails so the
    /// `ask_user` tool asks in plain text instead. Nothing here waits for an
    /// answer: on every channel the answer comes back as the user's next
    /// message.
    ///
    /// `allow_multiple` is a rendering concern: a channel that cannot mark
    /// options may ignore it, and its answer then carries one option.
    async fn send_question(&self, _request: &QuestionRequest) -> Result<()> {
        Err(anyhow::anyhow!(
            "channel '{}' does not support interactive questions",
            self.channel_type()
        ))
    }
}

/// An interactive question pushed to a user through a channel.
///
/// Serialized as the websocket `question` payload; a channel without a UI for
/// it never receives one (see [`OutboundAdapter::send_question`]).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct QuestionRequest {
    /// Topic the question belongs to.
    pub topic: String,
    /// What the user needs in order to answer: the assistant's own text for
    /// the turn that asked. A box that shows only the question leaves the
    /// user guessing which answer is meant, and the text cannot be relied on
    /// to arrive separately, so it travels with the question. `None` when the
    /// model called the tool without writing anything first.
    #[serde(default)]
    pub context: Option<String>,
    /// Question text.
    pub question: String,
    /// Selectable options.
    pub options: Vec<String>,
    /// Whether the user may pick more than one option. Each channel renders
    /// this its own way (and is free to ignore it); the answer simply carries
    /// however many options were picked.
    #[serde(default)]
    pub allow_multiple: bool,
}

// --- Pattern Types ---

/// Message metadata key carrying a pipe's explicit target pattern name.
/// Written by the pipe retarget path (jyc-cli) and honored by the target
/// channel's matcher (e.g. `WebsocketMatcher`). Defined here so writer and
/// reader share one source of truth.
pub const PIPE_PATTERN_METADATA_KEY: &str = "pipe_pattern";

/// Message metadata key naming the channel a message actually arrived on,
/// before any pipe re-targeted it into another channel's topic.
///
/// Written by the pipe retarget path (jyc-cli) right before it overwrites
/// `InboundMessage::channel`, so the agent turn still knows who it is talking
/// to: an interactive question belongs to the channel the user's message came
/// from, not to the hub channel that happens to run the topic.
pub const ORIGIN_CHANNEL_METADATA_KEY: &str = "origin_channel";

/// Target of a `ChannelPattern.pipe`: which (channel, topic) to forward
/// matching messages into.
///
/// `pattern` names a pattern on the target channel whose config
/// (`topic_path`, template, skills, model) applies to the piped message.
/// `topic` is the target topic name and may embed the runtime placeholder
/// `${msg.chat_name}`. Resolution: `effective_topic = topic ?? pattern`;
/// at least one must be set. Legacy `topic`-only form keeps the implicit
/// "topic name == pattern name" selection of websocket channels.
///
/// **New form** (preferred): `pipe = { agent = "jyc", topic = "..." }` —
/// targets the agent by name. `agent` and `channel`/`pattern` are
/// mutually exclusive (config error if both set).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PipeTarget {
    /// New form: target agent name. Resolves to the agent's channel +
    /// topic dir via the synthesized "agents" channel.
    #[serde(default)]
    pub agent: Option<String>,

    /// Target channel name (typically a websocket-type channel).
    /// **Deprecated** alongside `pattern`: prefer `agent` for new configs.
    #[serde(default)]
    pub channel: Option<String>,

    /// Target pattern name whose config applies. When `topic` is omitted,
    /// the pattern name doubles as the topic name.
    /// **Deprecated** alongside `channel`: prefer `agent` for new configs.
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
    #[serde(default = "crate::default_true")]
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
    /// Template name to initialize topic (from data_dir/templates/)
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
    ///
    /// `None` means "not set" — the effective value is true. Keeping the
    /// explicit `None`/`Some` distinction lets config validation reject the
    /// field on pipe channels, where it would be dead configuration.
    #[serde(default)]
    pub live_injection: Option<bool>,
    /// Whether to auto-inject inbound `image/*` attachments into the first
    /// user turn of the agent loop as multimodal content blocks.
    ///
    /// Only takes effect when the active model has `supports_images = true`.
    /// When false (default), image attachments stay on disk and the agent
    /// must use the `read_image` built-in tool to load them on demand.
    ///
    /// `None` means "not set" — the effective value is false.
    #[serde(default)]
    pub inject_inbound_images: Option<bool>,
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
    /// Merged with channel-level `disabled_mcps`.
    ///
    /// Renamed from `disabled_mcp_servers`; the old key is still accepted
    /// via the serde alias.
    #[serde(default, alias = "disabled_mcp_servers")]
    pub disabled_mcps: Option<Vec<String>>,

    /// Per-pattern skills whitelist.
    ///
    /// When set, only skills whose names appear in this list are loaded
    /// for topics matching this pattern. Takes priority over channel-level
    /// `skills`. When both are unset, no skill is loaded — skills are
    /// opt-in.
    #[serde(default)]
    pub skills: Option<Vec<String>>,

    /// Per-pattern compression configuration for session reset.
    ///
    /// **Deprecated — ignored.** The auto-reset mechanism was removed: the
    /// sliding-window context strategy keeps the context inside the window,
    /// so there is nothing left to reset for. The key is still accepted so
    /// configs written before the removal keep loading.
    #[serde(default)]
    pub reset_compression: Option<ResetCompressionConfig>,

    /// Auto-reset threshold as a fraction of context window (0.0~1.0).
    ///
    /// **Deprecated — ignored**, for the same reason as `reset_compression`.
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

    /// Per-pattern context management strategy.
    ///
    /// Controls how prior conversation history is sent to the LLM. The
    /// on-disk `.jyc/agent-context.jsonl` always stores the full raw
    /// context unchanged; this field only shapes what is sent to the
    /// LLM on each request. Falls back to `[ai].context_strategy` when
    /// unset.
    #[serde(default)]
    pub context_strategy: Option<ContextStrategyConfig>,
}

impl ChannelPattern {
    /// Effective live-injection setting — `None` means the default, true.
    pub fn live_injection(&self) -> bool {
        self.live_injection.unwrap_or(true)
    }

    /// Effective inbound-image injection setting — `None` means the
    /// default, false.
    pub fn inject_inbound_images(&self) -> bool {
        self.inject_inbound_images.unwrap_or(false)
    }
}

/// Compression strategy for session reset.
///
/// **Deprecated — ignored.** A reset no longer compresses the context: it
/// clears it.
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
/// **Deprecated — ignored.** The auto-reset mechanism was removed; the
/// per-pattern `reset_compression` key is still accepted so configs written
/// before the removal keep loading, but nothing reads it.
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

/// Context management strategy controlling how prior conversation history is
/// sent to the LLM on each request. The on-disk `.jyc/agent-context.jsonl`
/// always stores the full raw context unchanged; only what is sent to the
/// LLM is shaped by this strategy.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ContextStrategy {
    /// Send the full persisted context (current behavior).
    #[default]
    Full,
    /// Send only the last N user+assistant turns from the prior context
    /// (tool calls/results are dropped), plus the full current turn
    /// kept verbatim.
    #[serde(alias = "sliding")]
    SlidingWindow,
}

/// Configuration for the context strategy.
///
/// `mode` selects which strategy to apply; `window` is the number of
/// latest user+assistant turns to keep in `sliding_window` mode.
// `Eq` is not derived: the `ext_rate` field is an `f64`, which has no total
// order. Nothing compares strategies by hashing, so `PartialEq` suffices.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ContextStrategyConfig {
    /// Context strategy mode. Default: `sliding_window`.
    #[serde(default = "default_context_strategy")]
    pub mode: ContextStrategy,
    /// Number of latest user+assistant turns to keep in `sliding_window`.
    /// Default: 10. Only meaningful when `mode = "sliding_window"`.
    #[serde(default = "default_context_window_size")]
    pub window: usize,
    /// Number of most recent windowed turns that keep their tool-call
    /// history note; turns older than that are text-only. `Some(3)` by
    /// default (sliding_window mode). Values above `window` clamp to
    /// `window`. Only meaningful when `mode = "sliding_window"`.
    /// Omitted from serialized override files when unset so existing
    /// readers stay compatible.
    #[serde(
        default = "default_note_window",
        skip_serializing_if = "Option::is_none"
    )]
    pub note_window: Option<usize>,
    /// Byte cap on each tool result kept in the verbatim region
    /// (`sliding_window` mode). When a tool result exceeds `cap` bytes,
    /// its content is truncated and suffixed with a marker noting the
    /// dropped length. `Some(2048)` by default (sliding_window mode);
    /// `Some(0)` disables capping — every tool result is sent in full.
    /// Only meaningful when `mode = "sliding_window"`. Omitted from
    /// serialized override files when unset so existing readers stay
    /// compatible.
    #[serde(
        default = "default_tool_result_cap",
        skip_serializing_if = "Option::is_none"
    )]
    pub tool_result_cap: Option<usize>,
    /// Byte cap on each tool result in the current turn
    /// (`sliding_window` mode). `None` (the default) sends the current
    /// turn uncapped — the model is reasoning over these results
    /// mid-loop, so a long one (a big file read, a verbose command)
    /// rides every remaining call of the turn. Setting a cap trims each
    /// oversized result to at most `Some(cap)` bytes — marker included —
    /// keeping its head and tail with the middle replaced by a paging
    /// marker, since what a result is usually read for (the command that
    /// ran, the error, the summary line) sits at the ends. Below ~128
    /// bytes there is no room for both ends, so the head-only cut takes
    /// over and its own marker sits on top of `cap`. `Some(0)` disables
    /// capping, same as `None`. Only meaningful when `mode = "sliding_window"`. Omitted
    /// from serialized override files when unset so existing readers stay
    /// compatible.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub turn_tool_result_cap: Option<usize>,
    /// Token budget for the two history regions — ① compacted text history
    /// plus ② verbatim prior turns — in `sliding_window` mode.
    /// `None` (the default) sends the whole windowed history, byte for byte
    /// what an unconfigured build sends; `Some(0)` means the same.
    ///
    /// `window` counts turns, so a window of 10 still costs whatever those
    /// 10 turns happen to weigh — a handful of small ones, or a few turns
    /// carrying megabyte-sized tool results. This budget caps that weight
    /// directly: when ①+② estimate over budget, the oldest content goes
    /// first — ① loses its oldest pairs, then ② loses its oldest whole
    /// turns. Region ③ (the current turn) is never touched; it has
    /// `turn_tool_result_cap` for that.
    ///
    /// The estimate is heuristic (CJK characters ×1, other bytes ÷4), not a
    /// tokenizer: it bounds history, and a few percent of drift is fine for
    /// that. Only meaningful when `mode = "sliding_window"`. Omitted from
    /// serialized override files when unset so existing readers stay
    /// compatible.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_history_tokens: Option<usize>,
    /// How far the retained history extends beyond `window` turns, as a
    /// fraction of `window`. With `note_window ≤ window` the retained span
    /// grows from `window` up to `window + ext_rate × window` turns — the
    /// compacted turns between the verbatim tail and the drop point form
    /// an anchored block whose wire prefix stays byte-stable across turns
    /// (prompt-cache friendly), so only the drop turn misses the cache.
    /// With `note_window ≥ window` there is no compact region and the
    /// extension is inert. Saturating at [`MAX_BLOCK_PAIRS`] caps the
    /// extension: once the cap binds, the span tops out at
    /// `MAX_BLOCK_PAIRS + note_window` turns and the jump period becomes
    /// `MAX_BLOCK_PAIRS − (window − note_window) + 1` turns. `ext_rate =
    /// 0` degenerates to the classic sliding window (constant `window`-
    /// turn span, the head advancing one turn per turn). Values are
    /// clamped into
    /// `0.0..=[MAX_EXT_RATE]` (1) with non-finite input falling back to
    /// [`DEFAULT_EXT_RATE`]; the useful range is `0..=1`, larger values
    /// only retain more history per turn at token cost and drop more than
    /// a full window at once. `None` (the default) resolves to
    /// [`DEFAULT_EXT_RATE`] (0.5). Only meaningful when
    /// `mode = "sliding_window"`. Omitted from serialized override files
    /// when unset so existing readers stay compatible.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ext_rate: Option<f64>,
}

/// Default for [`ContextStrategyConfig::ext_rate`]: the retained span
/// reaches `1.5 × window` turns before the oldest drop at once.
pub const DEFAULT_EXT_RATE: f64 = 0.5;

/// Upper bound for [`ContextStrategyConfig::ext_rate`]. Shared by the
/// config resolver ([`ContextStrategyConfig::ext_rate`]) and the `/context`
/// parser so both accept the same range. Values above 1 only retain more
/// history per turn at token cost and drop more than a full window at
/// once, so the range stops at 1.
pub const MAX_EXT_RATE: f64 = 1.0;

/// Upper bound on the region-① block capacity (in compact pairs): a large
/// window × ext_rate composition cannot inflate the per-turn payload and
/// reader scan back towards a full-journal read. Matches the largest
/// admissible sliding window.
pub const MAX_BLOCK_PAIRS: usize = 200;

impl ContextStrategyConfig {
    /// Resolve `ext_rate` to a usable fraction: the default when unset or
    /// non-finite, clamped into `0.0..=[MAX_EXT_RATE]` so degenerate config
    /// values (negative, NaN, huge) cannot produce an absurdly deep
    /// region ①. `0` is meaningful — it is the classic sliding window.
    pub fn ext_rate(&self) -> f64 {
        match self.ext_rate {
            Some(m) if m.is_finite() => m.clamp(0.0, MAX_EXT_RATE),
            _ => DEFAULT_EXT_RATE,
        }
    }
}

impl Default for ContextStrategyConfig {
    fn default() -> Self {
        Self {
            mode: default_context_strategy(),
            window: default_context_window_size(),
            note_window: default_note_window(),
            tool_result_cap: default_tool_result_cap(),
            // Current turn uncapped by default: the mid-loop reasoning
            // argument for region ②'s cap does not automatically extend
            // here, but silently trimming results the model is still
            // working through is the more damaging default — so it is opt-in.
            turn_tool_result_cap: None,
            // History weight is unbounded by default: `window` is the lever
            // users already know, and a token budget silently truncating
            // history is the more surprising default.
            max_history_tokens: None,
            // History extension uses the default (0.5): half a window of
            // extra retained history, a prefix-stable block between jumps.
            ext_rate: None,
        }
    }
}

fn default_context_window_size() -> usize {
    10
}

fn default_context_strategy() -> ContextStrategy {
    ContextStrategy::SlidingWindow
}

fn default_note_window() -> Option<usize> {
    Some(3)
}

fn default_tool_result_cap() -> Option<usize> {
    Some(2048)
}

/// Per-pattern filesystem access whitelist.
///
/// `read` paths widen the read boundary for tools like `read`, `bash` and
/// `grep`. `write` paths widen the write boundary for `write`, `edit`, and
/// `bash` — and are automatically readable too.
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
            live_injection: None,
            inject_inbound_images: None,
            model: None,
            plan_model: None,
            build_model: None,
            small_model: None,
            mode: None,
            mcps: None,
            disabled_tools: None,
            disabled_builtin_tools: None,
            disabled_mcps: None,
            skills: None,
            reset_compression: None,
            auto_reset_threshold: None,
            access: None,
            context_strategy: None,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_match_keywords() {
        let kws = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();

        // Hits keep configuration order, join with commas, and match
        // case-insensitively on both sides.
        assert_eq!(
            match_keywords(&kws(&["deploy", "Release"]), "time to RELEASE now"),
            Some("Release".to_string())
        );

        // No hit means no match and no details.
        assert_eq!(match_keywords(&kws(&["deploy"]), "nothing here"), None);

        // An empty list matches nothing — it is not "no constraint".
        assert_eq!(match_keywords(&[], "anything"), None);
    }

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

    #[test]
    fn test_ext_rate_resolves_degenerate_values() {
        // The only guard between a malformed config and an absurd region-①
        // block: unset and non-finite resolve to the default, finite values
        // clamp into 0.0..=MAX_EXT_RATE. 0 is meaningful (classic sliding
        // window), not degenerate.
        let cfg = |ext_rate: Option<f64>| ContextStrategyConfig {
            ext_rate,
            ..ContextStrategyConfig::default()
        };
        assert_eq!(cfg(None).ext_rate(), DEFAULT_EXT_RATE);
        assert_eq!(cfg(Some(f64::NAN)).ext_rate(), DEFAULT_EXT_RATE);
        assert_eq!(cfg(Some(f64::INFINITY)).ext_rate(), DEFAULT_EXT_RATE);
        assert_eq!(cfg(Some(f64::NEG_INFINITY)).ext_rate(), DEFAULT_EXT_RATE);
        assert_eq!(cfg(Some(-1.0)).ext_rate(), 0.0);
        assert_eq!(cfg(Some(0.0)).ext_rate(), 0.0);
        assert_eq!(cfg(Some(0.05)).ext_rate(), 0.05);
        assert_eq!(cfg(Some(MAX_EXT_RATE + 0.1)).ext_rate(), MAX_EXT_RATE);
        assert_eq!(cfg(Some(2.5)).ext_rate(), MAX_EXT_RATE);
        assert_eq!(cfg(Some(1.0)).ext_rate(), 1.0);
    }

    #[test]
    fn test_context_strategy_serde_aliases() {
        // The primary name and the `sliding` alias should both deserialize
        // to SlidingWindow.
        for s in ["sliding_window", "sliding"] {
            let cfg: ContextStrategyConfig = toml::from_str(&format!(r#"mode = "{s}""#)).unwrap();
            assert_eq!(cfg.mode, ContextStrategy::SlidingWindow);
            // window falls back to the default when omitted.
            assert_eq!(cfg.window, 10);
        }

        let cfg: ContextStrategyConfig = toml::from_str(r#"mode = "full""#).unwrap();
        assert_eq!(cfg.mode, ContextStrategy::Full);

        // Default for an empty table is sliding_window / window 10.
        let cfg: ContextStrategyConfig = toml::from_str("").unwrap();
        assert_eq!(cfg, ContextStrategyConfig::default());
    }

    #[test]
    fn test_context_strategy_default_values() {
        // Locks in the built-in defaults so a stray edit to
        // `ContextStrategyConfig::default()` surfaces immediately.
        let cfg = ContextStrategyConfig::default();
        assert_eq!(cfg.mode, ContextStrategy::SlidingWindow);
        assert_eq!(cfg.window, 10);
        assert_eq!(cfg.note_window, Some(3));
        assert_eq!(cfg.tool_result_cap, Some(2048));
    }

    #[test]
    fn test_context_strategy_tool_result_cap_missing_field_uses_default() {
        // Legacy JSON without the field still deserializes successfully
        // and picks up the struct default of `Some(2048)`. To explicitly
        // disable capping, write `null` in JSON (see the roundtrip test
        // below for `Some(0)` as the "off" sentinel).
        let toml_str = r#"
            mode = "sliding_window"
            window = 10
            note_window = 3
        "#;
        let cfg: ContextStrategyConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(cfg.tool_result_cap, Some(2048));
    }

    #[test]
    fn test_context_strategy_serde_partial_toml_uses_defaults() {
        // Regression guard for the partial-config bug: when a user writes
        // just `mode = "sliding_window"` in config.toml, the unset
        // `note_window` and `tool_result_cap` fields must pick up the
        // struct defaults (`Some(3)` / `Some(2048)`), not `None`. Without
        // the `default = "..."` serde attributes, `Option::default()`
        // would yield `None` and silently disable capping/truncation.
        let cfg: ContextStrategyConfig = toml::from_str(r#"mode = "sliding_window""#).unwrap();
        assert_eq!(cfg.mode, ContextStrategy::SlidingWindow);
        assert_eq!(cfg.window, 10);
        assert_eq!(cfg.note_window, Some(3));
        assert_eq!(cfg.tool_result_cap, Some(2048));
    }

    #[test]
    fn test_context_strategy_serde_empty_toml_equals_struct_default() {
        // Empty TOML must produce the same struct as `Default::default()`.
        // The mode field's serde default was `ContextStrategy::default() = Full`
        // (from `#[derive(Default)]` + `#[default]` on the enum variant), while
        // `ContextStrategyConfig::default()` sets `mode = SlidingWindow`. The
        // helper `default_context_strategy` closes that gap so empty configs
        // and `Default::default()` agree on `mode = SlidingWindow`.
        let cfg: ContextStrategyConfig = toml::from_str("").unwrap();
        assert_eq!(cfg, ContextStrategyConfig::default());
    }

    #[test]
    fn test_context_strategy_tool_result_cap_roundtrip() {
        // TOML round-trip.
        let cfg = ContextStrategyConfig {
            tool_result_cap: Some(4096),
            ..ContextStrategyConfig::default()
        };
        let serialized = toml::to_string(&cfg).unwrap();
        assert!(
            serialized.contains("tool_result_cap = 4096"),
            "tool_result_cap should serialize in TOML, got: {serialized}"
        );
        let deserialized: ContextStrategyConfig = toml::from_str(&serialized).unwrap();
        assert_eq!(deserialized, cfg);

        // JSON round-trip — the field must round-trip with snake_case.
        let json = serde_json::to_string(&cfg).unwrap();
        assert!(
            json.contains("\"tool_result_cap\":4096"),
            "tool_result_cap should serialize in JSON as snake_case, got: {json}"
        );
        let cfg2: ContextStrategyConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(cfg2, cfg);
    }

    #[test]
    fn test_context_strategy_tool_result_cap_zero_is_some_none_distinct() {
        // Some(0) is the explicit "off" sentinel and must survive a round-trip.
        let cfg = ContextStrategyConfig {
            tool_result_cap: Some(0),
            ..ContextStrategyConfig::default()
        };
        let json = serde_json::to_string(&cfg).unwrap();
        let cfg2: ContextStrategyConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(cfg2.tool_result_cap, Some(0));
        assert_ne!(cfg2.tool_result_cap, None);
    }

    #[test]
    fn test_context_strategy_turn_tool_result_cap_defaults_to_none() {
        // Unset by default: region ③ (the current turn) must stay
        // uncapped unless someone opts in.
        assert_eq!(ContextStrategyConfig::default().turn_tool_result_cap, None);

        // A config file written before this field existed must load with
        // None rather than failing — override files on disk are exactly
        // that shape.
        let cfg: ContextStrategyConfig =
            toml::from_str("mode = \"sliding_window\"\nwindow = 10\n").unwrap();
        assert_eq!(cfg.turn_tool_result_cap, None);

        // And it must stay out of the serialized form when unset, so a
        // round-trip through an old reader loses nothing.
        let serialized = toml::to_string(&ContextStrategyConfig::default()).unwrap();
        assert!(
            !serialized.contains("turn_tool_result_cap"),
            "unset turn_tool_result_cap must not serialize: {serialized}"
        );
    }

    #[test]
    fn test_context_strategy_turn_tool_result_cap_roundtrip() {
        let cfg = ContextStrategyConfig {
            turn_tool_result_cap: Some(32768),
            ..ContextStrategyConfig::default()
        };
        let serialized = toml::to_string(&cfg).unwrap();
        assert!(
            serialized.contains("turn_tool_result_cap = 32768"),
            "turn_tool_result_cap should serialize in TOML, got: {serialized}"
        );
        let deserialized: ContextStrategyConfig = toml::from_str(&serialized).unwrap();
        assert_eq!(deserialized, cfg);

        let json = serde_json::to_string(&cfg).unwrap();
        assert!(
            json.contains("\"turn_tool_result_cap\":32768"),
            "turn_tool_result_cap should serialize in JSON as snake_case, got: {json}"
        );
        let cfg2: ContextStrategyConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(cfg2, cfg);

        // Some(0) is the explicit-off sentinel, distinct from None.
        let cfg0 = ContextStrategyConfig {
            turn_tool_result_cap: Some(0),
            ..ContextStrategyConfig::default()
        };
        let cfg0_back: ContextStrategyConfig =
            serde_json::from_str(&serde_json::to_string(&cfg0).unwrap()).unwrap();
        assert_eq!(cfg0_back.turn_tool_result_cap, Some(0));
        assert_ne!(cfg0_back.turn_tool_result_cap, None);
    }

    #[test]
    fn test_context_strategy_max_history_tokens_defaults_to_none() {
        // Unset by default: history weight is bounded by `window` alone
        // until someone asks for a token budget.
        assert_eq!(ContextStrategyConfig::default().max_history_tokens, None);

        // A config file written before this field existed must load with
        // None rather than failing — override files on disk are exactly
        // that shape.
        let cfg: ContextStrategyConfig =
            toml::from_str("mode = \"sliding_window\"\nwindow = 10\n").unwrap();
        assert_eq!(cfg.max_history_tokens, None);

        // And it must stay out of the serialized form when unset, so a
        // round-trip through an old reader loses nothing.
        let serialized = toml::to_string(&ContextStrategyConfig::default()).unwrap();
        assert!(
            !serialized.contains("max_history_tokens"),
            "unset max_history_tokens must not serialize: {serialized}"
        );
    }

    #[test]
    fn test_context_strategy_max_history_tokens_roundtrip() {
        let cfg = ContextStrategyConfig {
            max_history_tokens: Some(30_000),
            ..ContextStrategyConfig::default()
        };
        let serialized = toml::to_string(&cfg).unwrap();
        assert!(
            serialized.contains("max_history_tokens = 30000"),
            "max_history_tokens should serialize in TOML, got: {serialized}"
        );
        let deserialized: ContextStrategyConfig = toml::from_str(&serialized).unwrap();
        assert_eq!(deserialized, cfg);

        let json = serde_json::to_string(&cfg).unwrap();
        assert!(
            json.contains("\"max_history_tokens\":30000"),
            "max_history_tokens should serialize in JSON as snake_case, got: {json}"
        );
        let cfg2: ContextStrategyConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(cfg2, cfg);

        // Some(0) is the explicit-off sentinel, distinct from None.
        let cfg0 = ContextStrategyConfig {
            max_history_tokens: Some(0),
            ..ContextStrategyConfig::default()
        };
        let cfg0_back: ContextStrategyConfig =
            serde_json::from_str(&serde_json::to_string(&cfg0).unwrap()).unwrap();
        assert_eq!(cfg0_back.max_history_tokens, Some(0));
        assert_ne!(cfg0_back.max_history_tokens, None);
    }
}
