use serde::Deserialize;
use std::collections::HashMap;

use crate::channels::types::{AttachmentConfig, ChannelPattern};

/// Top-level application configuration, deserialized from config.toml.
#[derive(Debug, Deserialize)]
pub struct AppConfig {
    /// General settings (concurrency, queue sizes)
    #[serde(default)]
    pub general: GeneralConfig,

    /// Named channels (e.g., "work", "personal")
    #[serde(default)]
    pub channels: HashMap<String, ChannelConfig>,

    /// Agent configuration (AI model, prompts, progress, attachments)
    pub agent: AgentConfig,

    /// Alerting configuration (error digests, health checks)
    pub alerting: Option<AlertingConfig>,
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

    /// Workspace directory path (relative to workdir)
    pub workspace: Option<String>,

    /// IMAP configuration (for email channels)
    pub inbound: Option<ImapConfig>,

    /// SMTP configuration (for email channels)
    pub outbound: Option<SmtpConfig>,

    /// Monitoring settings (IDLE vs poll, interval, etc.)
    pub monitor: Option<MonitorConfig>,

    /// Patterns for this channel
    pub patterns: Option<Vec<ChannelPattern>>,

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

    /// Progress update configuration
    pub progress: Option<ProgressConfig>,

    /// Outbound attachment configuration
    pub attachments: Option<AttachmentConfig>,
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

    /// Whether to include thread history in prompts
    #[serde(default = "default_true")]
    pub include_thread_history: bool,
}

/// Progress notification configuration.
#[derive(Debug, Clone, Deserialize)]
pub struct ProgressConfig {
    #[serde(default)]
    pub enabled: bool,

    /// Delay before first progress update (seconds)
    #[serde(default = "default_180")]
    pub initial_delay_secs: u64,

    /// Interval between progress updates (seconds)
    #[serde(default = "default_180")]
    pub interval_secs: u64,

    /// Maximum number of progress updates per message
    #[serde(default = "default_5")]
    pub max_messages: usize,
}

/// Alerting configuration — error digests and health checks.
#[derive(Debug, Deserialize)]
pub struct AlertingConfig {
    pub enabled: bool,

    /// Email address to send alerts to
    pub recipient: String,

    /// How often to flush error buffer (minutes)
    #[serde(default = "default_5_u64")]
    pub batch_interval_minutes: u64,

    /// Max errors per digest email
    #[serde(default = "default_50")]
    pub max_errors_per_batch: usize,

    /// Subject line prefix for alert emails
    pub subject_prefix: Option<String>,

    /// Whether to include reply-tool.log tail in error digests
    #[serde(default = "default_true")]
    pub include_reply_tool_log: bool,

    /// Number of lines to include from reply-tool.log
    #[serde(default = "default_50")]
    pub reply_tool_log_tail_lines: usize,

    /// Health check report configuration
    pub health_check: Option<HealthCheckConfig>,
}

/// Health check report configuration.
#[derive(Debug, Deserialize)]
pub struct HealthCheckConfig {
    pub enabled: bool,

    /// How often to send health check reports (hours, supports decimals)
    #[serde(default = "default_24f")]
    pub interval_hours: f64,

    /// Override recipient (falls back to alerting.recipient)
    pub recipient: Option<String>,
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
fn default_5_u64() -> u64 {
    5
}
fn default_10() -> usize {
    10
}
fn default_30() -> u64 {
    30
}
fn default_50() -> usize {
    50
}
fn default_180() -> u64 {
    180
}
fn default_993() -> u16 {
    993
}
fn default_465() -> u16 {
    465
}
fn default_24f() -> f64 {
    24.0
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
