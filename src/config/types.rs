use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use crate::channels::types::ChannelPattern;

/// MCP server configuration for template-driven MCP tool setup.
///
/// Supports both `local` (subprocess) and `remote` (HTTP) MCP server types.
/// Named MCPs are defined in `config.toml` `[[mcps]]` and referenced by
/// templates in `templates.toml` to determine which MCPs appear in each
/// thread's `opencode.json`.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct McpServerConfig {
    pub name: String,

    #[serde(flatten)]
    pub kind: McpServerKind,
}

/// Kind of MCP server — either `local` (subprocess) or `remote` (HTTP).
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum McpServerKind {
    Local {
        command: Vec<String>,
        #[serde(default)]
        environment: HashMap<String, String>,
        #[serde(default = "default_mcp_timeout")]
        timeout: u64,
    },
    Remote {
        url: String,
        #[serde(default = "default_true")]
        enabled: bool,
    },
}

fn default_mcp_timeout() -> u64 {
    300000
}

/// Top-level application configuration, deserialized from config.toml.
#[derive(Debug, Deserialize)]
pub struct AppConfig {
    /// General settings (concurrency, queue sizes)
    #[serde(default)]
    pub general: GeneralConfig,

    /// Named channels (e.g., "work", "personal")
    #[serde(default)]
    pub channels: HashMap<String, ChannelConfig>,

    /// Agent configuration (AI model, prompts, attachments)
    pub agent: AgentConfig,

    /// Inspect server configuration (exposes runtime state for dashboard)
    pub inspect: Option<InspectConfig>,

    /// Heartbeat configuration (progress updates during long AI processing)
    #[serde(default)]
    pub heartbeat: HeartbeatConfig,

    /// Unified attachment configuration (inbound downloading and outbound sending)
    #[serde(default)]
    pub attachments: Option<UnifiedAttachmentConfig>,

    /// Vision API configuration (image analysis via OpenAI-compatible API)
    pub vision: Option<VisionConfig>,

    /// Named MCP server configurations, referenced by templates.
    /// Each template in `templates.toml` can specify which MCPs it needs.
    #[serde(default)]
    pub mcps: Vec<McpServerConfig>,
}

/// General application settings.
#[derive(Debug, Deserialize)]
pub struct GeneralConfig {
    /// Max concurrent thread workers (default: 3)
    #[serde(default = "default_3")]
    pub max_concurrent_threads: usize,

    /// Max queued messages per thread (default: 10)
    #[serde(default = "default_10")]
    pub max_queue_size_per_thread: usize,
}

impl Default for GeneralConfig {
    fn default() -> Self {
        Self {
            max_concurrent_threads: 3,
            max_queue_size_per_thread: 10,
        }
    }
}

/// Configuration for a single channel (e.g., one email account).
#[derive(Debug, Deserialize)]
pub struct ChannelConfig {
    /// Channel type: "email", "feishu", etc.
    #[serde(rename = "type")]
    pub channel_type: String,

    /// IMAP configuration (for email channels)
    pub inbound: Option<ImapConfig>,

    /// SMTP configuration (for email channels)
    pub outbound: Option<SmtpConfig>,

    /// Feishu configuration (for feishu channels)
    pub feishu: Option<crate::channels::feishu::config::FeishuConfig>,

    /// GitHub configuration (for github channels)
    pub github: Option<crate::channels::github::config::GithubConfig>,

    /// Monitoring settings (IDLE vs poll, interval, etc.)
    pub monitor: Option<MonitorConfig>,

    /// Patterns for this channel
    pub patterns: Option<Vec<ChannelPattern>>,

    /// Per-channel heartbeat message template.
    /// Supports `{elapsed}` placeholder (e.g., "3m 20s").
    /// If not set, defaults to "Still working on your request... ({elapsed} elapsed)"
    pub heartbeat_template: Option<String>,

    /// Channel-specific agent config override
    pub agent: Option<AgentConfig>,
}

/// IMAP server configuration.
#[derive(Debug, Clone, Deserialize)]
pub struct ImapConfig {
    pub host: String,
    #[serde(default = "default_993")]
    pub port: u16,
    #[serde(default = "default_true")]
    pub tls: bool,
    pub auth_timeout_ms: Option<u64>,
    pub username: String,
    pub password: String,
}

/// SMTP server configuration.
#[derive(Debug, Clone, Deserialize)]
pub struct SmtpConfig {
    pub host: String,
    #[serde(default = "default_465")]
    pub port: u16,
    #[serde(default = "default_true")]
    pub secure: bool,
    pub username: String,
    pub password: String,
    /// Display name for the From header
    pub from_name: Option<String>,
    /// From email address (defaults to username)
    pub from_address: Option<String>,
}

/// Email monitoring settings.
#[derive(Debug, Clone, Deserialize)]
pub struct MonitorConfig {
    /// "idle" or "poll"
    #[serde(default = "default_idle")]
    pub mode: String,

    /// Polling interval in seconds (only used in poll mode)
    #[serde(default = "default_30")]
    pub poll_interval_secs: u64,

    /// Max consecutive failures before giving up
    #[serde(default = "default_5")]
    pub max_retries: usize,

    /// IMAP folder to monitor
    #[serde(default = "default_inbox")]
    pub folder: String,
}

impl Default for MonitorConfig {
    fn default() -> Self {
        Self {
            mode: "idle".to_string(),
            poll_interval_secs: 30,
            max_retries: 5,
            folder: "INBOX".to_string(),
        }
    }
}

/// Agent configuration — how the AI responds to messages.
#[derive(Debug, Clone, Deserialize)]
pub struct AgentConfig {
    /// Whether AI replies are enabled
    #[serde(default = "default_true")]
    pub enabled: bool,

    /// Reply mode: "opencode" or "static"
    #[serde(default = "default_opencode")]
    pub mode: String,

    /// Static reply text (used when mode = "static")
    pub text: Option<String>,

    /// OpenCode AI configuration
    pub opencode: Option<OpenCodeConfig>,

    /// Outbound attachment configuration
    pub attachments: Option<OutboundAttachmentConfig>,
}

/// OpenCode AI service configuration.
#[derive(Debug, Clone, Deserialize)]
pub struct OpenCodeConfig {
    /// Model identifier (e.g., "SiliconFlow/Pro/zai-org/GLM-4.7")
    pub model: Option<String>,

    /// Small model for lightweight tasks (title generation, compaction)
    pub small_model: Option<String>,

    /// System prompt for the AI
    pub system_prompt: Option<String>,

    /// Maximum input tokens per session before resetting.
    /// If not set, uses 95% of the model's context window, or 120K as fallback.
    pub max_input_tokens: Option<u64>,
}

/// Inspect server configuration — exposes runtime state via TCP for the dashboard.
#[derive(Debug, Clone, Deserialize)]
pub struct InspectConfig {
    /// Whether the inspect server is enabled (default: false)
    #[serde(default)]
    pub enabled: bool,

    /// TCP bind address (default: "127.0.0.1:9876")
    #[serde(default = "default_inspect_bind")]
    pub bind: String,
}

impl Default for InspectConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            bind: default_inspect_bind(),
        }
    }
}

fn default_inspect_bind() -> String {
    "127.0.0.1:9876".to_string()
}

/// Heartbeat configuration — controls progress updates sent during long-running AI processing.
///
/// When enabled, heartbeat emails/messages are sent periodically while the AI is working
/// on a message, so the sender knows their request is being processed.
#[derive(Debug, Clone, Deserialize)]
pub struct HeartbeatConfig {
    /// Whether heartbeat updates are enabled (default: true)
    #[serde(default = "default_true")]
    pub enabled: bool,

    /// Interval between heartbeat updates in seconds (default: 600 = 10 minutes)
    ///
    /// Controls both the timer tick rate and the minimum interval between
    /// consecutive heartbeat sends. Set higher to avoid SMTP rate limits.
    #[serde(default = "default_600")]
    pub interval_secs: u64,

    /// Minimum processing time before the first heartbeat is sent (default: 60)
    ///
    /// Prevents heartbeats for quick-to-process messages. The AI must have been
    /// processing for at least this many seconds before the first heartbeat fires.
    #[serde(default = "default_60")]
    pub min_elapsed_secs: u64,
}

impl Default for HeartbeatConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            interval_secs: 600,
            min_elapsed_secs: 60,
        }
    }
}

/// Vision API configuration for image analysis.
///
/// Uses any OpenAI-compatible vision API (Kimi, Volcengine/Ark, OpenAI, etc.)
/// Configures the MCP vision tool that the AI agent can call to analyze images.
#[derive(Debug, Clone, Deserialize)]
pub struct VisionConfig {
    /// Whether the vision tool is enabled
    #[serde(default)]
    pub enabled: bool,

    /// API key for the vision provider
    pub api_key: String,

    /// API endpoint URL (OpenAI-compatible chat completions endpoint)
    #[serde(default = "default_vision_api_url")]
    pub api_url: String,

    /// Model name to use for vision analysis
    #[serde(default = "default_vision_model")]
    pub model: String,
}

fn default_vision_api_url() -> String {
    "https://api.moonshot.cn/v1/chat/completions".to_string()
}

fn default_vision_model() -> String {
    "kimi-k2.5".to_string()
}

// --- Default value functions ---

fn default_true() -> bool {
    true
}
fn default_3() -> usize {
    3
}
fn default_5() -> usize {
    5
}
fn default_10() -> usize {
    10
}
fn default_30() -> u64 {
    30
}
fn default_993() -> u16 {
    993
}
fn default_465() -> u16 {
    465
}
fn default_idle() -> String {
    "idle".to_string()
}
fn default_inbox() -> String {
    "INBOX".to_string()
}
fn default_opencode() -> String {
    "opencode".to_string()
}

fn default_60() -> u64 {
    60
}

fn default_600() -> u64 {
    600
}

fn default_1_0() -> f64 {
    1.0
}

fn default_120_0() -> f64 {
    120.0
}

/// Unified attachment configuration with inbound and outbound sections.
#[derive(Debug, Clone, Deserialize)]
pub struct UnifiedAttachmentConfig {
    /// Inbound attachment configuration (downloading attachments from messages)
    pub inbound: Option<InboundAttachmentConfig>,

    /// Outbound attachment configuration (sending attachments with replies)
    pub outbound: Option<OutboundAttachmentConfig>,
}

/// Configuration for inbound attachment downloading.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InboundAttachmentConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,

    /// Allowed file extensions (e.g., [".pdf", ".docx"])
    #[serde(default)]
    pub allowed_extensions: Vec<String>,

    /// Max file size per attachment (human-readable: "25mb", "150kb")
    pub max_file_size: Option<String>,

    /// Max number of attachments to download per message
    pub max_per_message: Option<usize>,

    /// Path to save downloaded attachments (relative to workspace or absolute)
    /// If not set, attachments will be saved to thread directory
    pub save_path: Option<String>,
}

/// Configuration for outbound attachment sending.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutboundAttachmentConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,

    /// Allowed file extensions (e.g., [".pdf", ".docx"])
    #[serde(default)]
    pub allowed_extensions: Vec<String>,

    /// Max file size per attachment (human-readable: "10mb", "5mb")
    pub max_file_size: Option<String>,

    /// Max number of attachments to send per message
    pub max_per_message: Option<usize>,
}

/// Idle cleanup configuration for a channel pattern.
///
/// When enabled, automatically removes large subdirectories (e.g., cloned `repo/`)
/// from idle threads while preserving all metadata (`.jyc/`, chat history, sessions).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IdleCleanupConfig {
    /// Whether idle cleanup is enabled (default: false)
    #[serde(default)]
    pub enabled: bool,

    /// Idle timeout in seconds before cleaning (default: 86400 = 24 hours)
    #[serde(default = "default_86400")]
    pub timeout_secs: u64,

    /// Subdirectory paths to clean (e.g., ["repo"]) (default: [])
    #[serde(default)]
    pub clean_paths: Vec<String>,

    /// Scan interval in seconds (default: 300 = 5 minutes)
    #[serde(default = "default_300")]
    pub interval_secs: u64,
}

fn default_86400() -> u64 {
    86400
}

fn default_300() -> u64 {
    300
}
