use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use crate::channel::ChannelPattern;
use crate::feishu_config::FeishuConfig;
use crate::github_config::GithubConfig;
use crate::openilink_config::OpenilinkConfig;

/// MCP server configuration for agent dynamic tool loading.
///
/// Supports both `local` (subprocess) and `remote` (HTTP) MCP server types.
/// Named MCPs are defined in `config.toml` `[[mcps]]` and loaded by the
/// agent at startup. Each MCP server's tools are dynamically discovered
/// via `list_tools()` and registered in the agent's tool registry.
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
    },
    Remote {
        url: String,
        #[serde(default = "default_true")]
        enabled: bool,
    },
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

    /// Unified attachment configuration (inbound downloading and outbound sending)
    #[serde(default)]
    pub attachments: Option<UnifiedAttachmentConfig>,

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

/// Footer display configuration for a channel.
///
/// Controls whether the model/mode/tokens footer is appended to AI replies.
/// Default is `enabled = true` for backward compatibility.
#[derive(Debug, Clone, Deserialize)]
pub struct FooterConfig {
    /// Whether the footer is appended to replies (default: true)
    #[serde(default = "default_true")]
    pub enabled: bool,
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
    pub feishu: Option<FeishuConfig>,

    /// GitHub configuration (for github channels)
    pub github: Option<GithubConfig>,

    /// OpeniLink configuration (for openilink/wechat channels)
    pub openilink: Option<OpenilinkConfig>,

    /// Monitoring settings (IDLE vs poll, interval, etc.)
    pub monitor: Option<MonitorConfig>,

    /// Patterns for this channel
    pub patterns: Option<Vec<ChannelPattern>>,

    /// Channel-specific agent config override
    pub agent: Option<AgentConfig>,

    /// Footer display configuration (omit for default: footer enabled)
    pub footer: Option<FooterConfig>,
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

    /// Reply mode: "agent" or "static"
    #[serde(default = "default_agent_mode")]
    pub mode: String,

    /// Model identifier in "provider/model-id" format (e.g., "anthropic/claude-opus-4-6")
    pub model: Option<String>,

    /// Optional small/fast model used for ancillary LLM work (cycle-boundary
    /// progress summary and between-message context-reset summary). Falls
    /// back to the main `model` if unset or if provider construction fails
    /// (logged as a warning, the agent continues).
    #[serde(default)]
    pub small_model: Option<String>,

    /// System prompt for the AI
    pub system_prompt: Option<String>,

    /// Maximum agent loop iterations per cycle. When exceeded, the agent sends a
    /// progress reply, resets the iteration counter, and continues working.
    /// There is no upper bound on cycles — the agent runs until it produces a
    /// final reply or the user resets the session.
    /// Default: 200.
    #[serde(default = "default_max_iterations")]
    pub max_iterations: usize,

    /// Static reply text (used when mode = "static")
    pub text: Option<String>,

    /// Outbound attachment configuration
    pub attachments: Option<OutboundAttachmentConfig>,

    /// Provider definitions for the in-process agent
    #[serde(default)]
    pub providers: std::collections::HashMap<String, ProviderDef>,

}

/// Provider definition for the in-process agent.
#[derive(Debug, Clone, Deserialize)]
pub struct ProviderDef {
    /// Provider type: "anthropic" or "openai-compatible"
    #[serde(rename = "type")]
    pub provider_type: String,
    /// API base URL
    pub base_url: Option<String>,
    /// Environment variable name containing the API key
    pub api_key_env: Option<String>,
    /// Default context window size in tokens (used if model-specific not set)
    pub context_window: Option<u64>,
    /// Extra parameters merged into every API request for this provider
    #[serde(default)]
    pub params: Option<serde_json::Value>,
    /// Per-model context window overrides
    #[serde(default)]
    pub models: std::collections::HashMap<String, ModelDef>,
}

/// Per-model configuration within a provider.
#[derive(Debug, Clone, Deserialize)]
pub struct ModelDef {
    /// Context window size in tokens for this specific model
    pub context_window: Option<u64>,
    /// Extra parameters merged into API request when using this model (overrides provider params)
    #[serde(default)]
    pub params: Option<serde_json::Value>,
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
fn default_agent_mode() -> String {
    "agent".to_string()
}

fn default_max_iterations() -> usize {
    200
}

#[allow(dead_code)]
fn default_1_0() -> f64 {
    1.0
}

#[allow(dead_code)]
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

use anyhow::{Context, Result};
use regex::Regex;
use std::path::Path;

/// Load configuration from a TOML file.
///
/// Reads the file, expands `${VAR}` environment variable references,
/// then deserializes into `AppConfig`.
pub fn load_config(path: &Path) -> Result<AppConfig> {
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read config file: {}", path.display()))?;

    load_config_from_str(&content)
}

/// Load configuration from a TOML string.
///
/// Expands `${VAR}` environment variable references, then deserializes.
pub fn load_config_from_str(content: &str) -> Result<AppConfig> {
    // First parse as raw TOML Value so we can expand env vars
    let mut value: toml::Value = toml::from_str(content).context("failed to parse TOML")?;

    expand_env_vars(&mut value);

    // Now deserialize the expanded TOML into our config struct
    let config: AppConfig = value.try_into().context("failed to deserialize config")?;

    Ok(config)
}

/// Recursively expand `${VAR}` patterns in TOML string values
/// with values from environment variables.
///
/// Missing env vars are replaced with empty strings.
fn expand_env_vars(value: &mut toml::Value) {
    let re = Regex::new(r"\$\{(\w+)\}").unwrap();

    match value {
        toml::Value::String(s) => {
            if s.contains("${") {
                *s = re
                    .replace_all(s, |caps: &regex::Captures| {
                        std::env::var(&caps[1]).unwrap_or_default()
                    })
                    .to_string();
            }
        }
        toml::Value::Table(t) => {
            for (_, v) in t.iter_mut() {
                expand_env_vars(v);
            }
        }
        toml::Value::Array(a) => {
            for v in a.iter_mut() {
                expand_env_vars(v);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod config_loader_tests {
    use super::*;

    #[test]
    fn test_expand_env_vars() {
        // SAFETY: This test runs in isolation (cargo test runs single-threaded by default for unit tests)
        unsafe {
            std::env::set_var("JYC_TEST_HOST", "imap.example.com");
            std::env::set_var("JYC_TEST_PORT", "993");
        }

        let mut value = toml::Value::Table({
            let mut t = toml::map::Map::new();
            t.insert(
                "host".into(),
                toml::Value::String("${JYC_TEST_HOST}".into()),
            );
            t.insert(
                "port".into(),
                toml::Value::String("${JYC_TEST_PORT}".into()),
            );
            t.insert(
                "missing".into(),
                toml::Value::String("${JYC_NONEXISTENT}".into()),
            );
            t.insert("plain".into(), toml::Value::String("no vars here".into()));
            t
        });

        expand_env_vars(&mut value);

        let table = value.as_table().unwrap();
        assert_eq!(table["host"].as_str().unwrap(), "imap.example.com");
        assert_eq!(table["port"].as_str().unwrap(), "993");
        assert_eq!(table["missing"].as_str().unwrap(), "");
        assert_eq!(table["plain"].as_str().unwrap(), "no vars here");

        // Cleanup
        unsafe {
            std::env::remove_var("JYC_TEST_HOST");
            std::env::remove_var("JYC_TEST_PORT");
        }
    }

    #[test]
    fn test_load_minimal_config() {
        let toml = r#"
[general]

[channels.work]
type = "email"

[channels.work.inbound]
host = "imap.example.com"
port = 993
username = "user"
password = "pass"

[channels.work.outbound]
host = "smtp.example.com"
port = 465
username = "user"
password = "pass"

[agent]
enabled = true
mode = "agent"
"#;

        let config = load_config_from_str(toml).unwrap();
        assert_eq!(config.channels.len(), 1);
        assert!(config.channels.contains_key("work"));
        assert_eq!(config.channels["work"].channel_type, "email");
        assert!(config.agent.enabled);
        assert_eq!(config.agent.mode, "agent");
    }

    #[test]
    fn test_load_config_with_defaults() {
        let toml = r#"
[general]

[channels.work]
type = "email"

[channels.work.inbound]
host = "imap.example.com"
port = 993
username = "user"
password = "pass"

[channels.work.outbound]
host = "smtp.example.com"
port = 465
username = "user"
password = "pass"

[agent]
enabled = true
mode = "agent"
"#;

        let config = load_config_from_str(toml).unwrap();
        assert_eq!(config.general.max_concurrent_threads, 3);
        assert_eq!(config.general.max_queue_size_per_thread, 10);
    }

    #[test]
    fn test_load_config_with_mcps() {
        let toml = r#"
[general]

[channels.work]
type = "email"

[channels.work.inbound]
host = "imap.example.com"
port = 993
username = "user"
password = "pass"

[channels.work.outbound]
host = "smtp.example.com"
port = 465
username = "user"
password = "pass"

[agent]
enabled = true
mode = "agent"

[[mcps]]
name = "jyc_vision"
type = "local"
command = ["jyc", "mcp-vision-tool"]
environment = { "VISION_API_KEY" = "secret", "VISION_API_URL" = "https://api.example.com" }

[[mcps]]
name = "remote_mcp"
type = "remote"
url = "https://mcp.example.com/handler"
enabled = true
"#;

        let config = load_config_from_str(toml).unwrap();
        assert_eq!(config.mcps.len(), 2);

        let vision = &config.mcps[0];
        assert_eq!(vision.name, "jyc_vision");
        match &vision.kind {
            super::McpServerKind::Local {
                command,
                environment,
            } => {
                assert_eq!(command, &["jyc", "mcp-vision-tool"]);
                assert_eq!(environment.get("VISION_API_KEY").unwrap(), "secret");
            }
            _ => panic!("Expected Local variant for jyc_vision"),
        }

        let remote = &config.mcps[1];
        assert_eq!(remote.name, "remote_mcp");
        match &remote.kind {
            super::McpServerKind::Remote { url, enabled } => {
                assert_eq!(url, "https://mcp.example.com/handler");
                assert!(*enabled);
            }
            _ => panic!("Expected Remote variant for remote_mcp"),
        }
    }
}
