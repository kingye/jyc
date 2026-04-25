use anyhow::Result;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use tokio_util::sync::CancellationToken;

use crate::config::types::{IdleCleanupConfig, InboundAttachmentConfig};

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
    /// Email: References header values; FeiShu: thread ID
    pub thread_refs: Option<Vec<String>>,
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
    /// Callback for thread close events (e.g., chat disbanded)
    pub on_thread_close: Option<Box<dyn Fn(String) -> Result<()> + Send + Sync>>,
    /// Callback for errors
    #[allow(dead_code)]
    pub on_error: Box<dyn Fn(anyhow::Error) + Send + Sync>,
    /// Attachment download configuration
    pub attachment_config: Option<crate::config::types::InboundAttachmentConfig>,
}

/// Channel-specific message matching and thread name derivation.
///
/// Pure-logic trait used by MessageRouter. Every channel type implements this.
/// Separated from InboundAdapter to allow use without the lifecycle (start/stop) —
/// e.g., email uses ImapMonitor for the connection lifecycle but EmailMatcher
/// for pattern matching and thread name derivation.
pub trait ChannelMatcher: Send + Sync {
    /// The channel type this matcher handles (e.g., "email", "feishu")
    #[allow(dead_code)]
    fn channel_type(&self) -> &str;

    /// Derive a thread name from the message and patterns.
    ///
    /// Each channel type has its own thread naming strategy:
    /// - Email: strips Re:/Fwd: prefixes and subject pattern prefixes, sanitizes
    /// - Feishu: uses chat_id or user_id from metadata
    fn derive_thread_name(
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
    async fn start(
        &self,
        options: InboundAdapterOptions,
        cancel: CancellationToken,
    ) -> Result<()>;
}

/// Outbound adapter trait — one implementation per channel type.
///
/// Responsible for:
/// - Sending replies through the channel (including full-lifecycle: format + send + store)
/// - Format conversion (e.g., markdown → HTML for email, markdown for feishu)
/// - Adding channel-specific headers (threading, etc.)
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
    /// This is the primary method called by ThreadManager and process_message.
    /// Each channel implementation handles:
    /// - Channel-specific formatting (quoted history for email, etc.)
    /// - Sending via the channel's transport (SMTP, HTTP API, etc.)
    /// - Storing the reply to the chat log
    async fn send_reply(
        &self,
        original: &InboundMessage,
        reply_text: &str,
        thread_path: &Path,
        message_dir: &str,
        attachments: Option<&[OutboundAttachment]>,
    ) -> Result<SendResult>;

    /// Send a fresh (non-reply) alert/notification.
    async fn send_alert(
        &self,
        recipient: &str,
        subject: &str,
        body: &str,
    ) -> Result<SendResult>;

    /// Send a heartbeat/progress update to the user.
    ///
    /// Called periodically by the Thread Event system during long-running AI
    /// processing. The `message` parameter is pre-formatted from the per-channel
    /// heartbeat_template config (e.g., "正在处理中，请稍候... (已用时 3m 20s)").
    async fn send_heartbeat(
        &self,
        original: &InboundMessage,
        message: &str,
    ) -> Result<SendResult>;
}

// --- Pattern Types ---

/// A channel pattern defines matching rules for a specific channel.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChannelPattern {
    /// Pattern name (used as identifier)
    pub name: String,
    /// Channel this pattern applies to
    #[serde(default)]
    pub channel: ChannelType,
    /// Whether this pattern is active
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Matching rules (channel-specific)
    pub rules: PatternRules,
    /// Attachment download configuration for messages matching this pattern
    pub attachments: Option<InboundAttachmentConfig>,
    /// Template name to initialize thread (from workdir/templates/)
    /// If not specified, no template is applied
    #[serde(default)]
    pub template: Option<String>,
    /// Fixed thread name override.
    /// If set, all messages matching this pattern are routed to this thread
    /// instead of deriving the thread name from the message content.
    /// Channel-agnostic: works for email, Feishu, or any channel.
    #[serde(default)]
    pub thread_name: Option<String>,
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
    /// Idle cleanup configuration for this pattern.
    /// When enabled, automatically removes specified subdirectories from idle threads.
    #[serde(default)]
    pub idle_cleanup: Option<IdleCleanupConfig>,
    /// Repo group key for shared repo directories among GitHub threads.
    /// When set, threads matching this pattern share a single repo clone
    /// via symlinks, saving disk space. The group key is `"{repo_group}-{github_number}"`.
    /// Patterns without `repo_group` keep existing behavior (no symlink, no sharing).
    #[serde(default)]
    pub repo_group: Option<String>,
}

impl Default for ChannelPattern {
    fn default() -> Self {
        Self {
            name: String::new(),
            channel: String::new(),
            enabled: true,
            rules: PatternRules::default(),
            attachments: None,
            template: None,
            thread_name: None,
            role: None,
            live_injection: true,
            idle_cleanup: None,
            repo_group: None,
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
#[derive(Debug, Clone, Serialize, Deserialize)]
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

impl Default for PatternRules {
    fn default() -> Self {
        Self {
            sender: None,
            subject: None,
            mentions: None,
            keywords: None,
            chat_name: None,
            github_type: None,
            labels: None,
            assignees: None,
            exclude_labels: None,
        }
    }
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
    /// Subject prefixes to match (also stripped from thread name)
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
            LabelRule::Flat(labels) => {
                labels.iter().any(|l| msg_labels.contains(&l.to_lowercase()))
            }
            LabelRule::Nested(groups) => {
                groups.iter().all(|group| {
                    group.is_empty()
                        || group.iter().any(|l| msg_labels.contains(&l.to_lowercase()))
                })
            }
        }
    }
}

fn default_true() -> bool {
    true
}
