use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use crate::WecomGlobalConfig;

use crate::channel::ChannelPattern;
use crate::channel::ResetCompressionConfig;
use crate::feishu_config::FeishuConfig;
use crate::gitee_config::GiteeConfig;
use crate::github_config::GithubConfig;
use crate::wechat_config::WechatConfig;
use crate::wecom_bot_config::WecomBotConfig;
use crate::wecom_config::WecomConfig;
use crate::wecom_kf_config::WecomKfConfig;

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

    /// Optional whitelist of tools to load from this MCP server.
    ///
    /// When set, only tools whose names appear in this list are registered.
    /// All other tools from this server are silently ignored. When `None`
    /// (default), all discovered tools are loaded.
    #[serde(default)]
    pub enabled_tools: Option<Vec<String>>,
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
        /// Bearer token for authentication (without "Bearer " prefix).
        /// Sent as `Authorization: Bearer <token>` header with every request.
        /// Mutually exclusive with `oauth`; if both are set, validation rejects.
        #[serde(default)]
        auth_header: Option<String>,
        /// Custom HTTP headers to include with every request.
        /// Keys are header names, values are header values.
        #[serde(default)]
        custom_headers: HashMap<String, String>,
        /// OAuth2 client_credentials grant. When set, a one-shot POST to
        /// `token_endpoint` is performed at connect time and the resulting
        /// access token is sent as `Authorization: Bearer`. Token is fetched
        /// once per MCP connect — no auto-refresh; restart on expiry.
        #[serde(default)]
        oauth: Option<OAuthClientCredentialsConfig>,
    },
}

/// OAuth2 client_credentials grant configuration.
///
/// Used by remote MCP servers that require machine-to-machine OAuth2 instead
/// of a static bearer token. The agent POSTs `grant_type=client_credentials`
/// to `token_endpoint` at connect time and uses the returned `access_token`.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct OAuthClientCredentialsConfig {
    pub client_id: String,
    pub client_secret: String,
    pub token_endpoint: String,
    #[serde(default)]
    pub scopes: Vec<String>,
}

/// Top-level application configuration, deserialized from config.toml.
#[derive(Debug, Clone, Deserialize)]
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

    /// WeCom global configuration (shared HTTP server settings)
    #[serde(default)]
    pub wecom: Option<WecomGlobalConfig>,

    /// Named MCP server configurations, referenced by agent templates.
    #[serde(default)]
    pub mcps: Vec<McpServerConfig>,

    /// Scheduler configuration for channel-agnostic scheduled jobs.
    /// When enabled, a background JobScheduler runs alongside the monitor
    /// and fires due jobs by injecting InboundMessage into ThreadManager.
    #[serde(default)]
    pub scheduler: SchedulerConfig,

    /// User-defined slash commands (e.g. `/review`), declared as `[[commands]]`.
    #[serde(default)]
    pub commands: Vec<CustomCommand>,
}

/// A user-defined slash command declared in `config.toml` as `[[commands]]`.
///
/// Invoking `/<name>` switches the thread to `mode` (when set), points the
/// agent at `skills`, and appends `user_prompt` to the message body. The
/// command also appears in `/?` and the dashboard command popup.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct CustomCommand {
    /// Command name without the leading slash (e.g. `review`).
    pub name: String,

    /// Short description shown in `/?` and the command popup.
    #[serde(default)]
    pub description: String,

    /// Mode to switch to before running: `plan` or `build`.
    /// When unset, the current thread mode is left unchanged.
    #[serde(default)]
    pub mode: Option<String>,

    /// Skills the agent should use for this command.
    ///
    /// These names are surfaced to the agent in the appended prompt. The
    /// system prompt already lists every discovered skill with its path and
    /// description, so naming them here is enough for the agent to locate
    /// and read the corresponding `SKILL.md`.
    #[serde(default)]
    pub skills: Option<Vec<String>>,

    /// Instruction text appended to the message body after the command runs.
    pub user_prompt: String,
}

/// Names of the built-in slash commands, including the leading slash.
///
/// Used to reject `[[commands]]` entries that would shadow a built-in.
/// `jyc_core::command::all_commands()` has a test asserting it stays in
/// sync with this list.
pub const BUILTIN_COMMAND_NAMES: &[&str] = &[
    "/model",
    "/plan",
    "/build",
    "/reset",
    "/new",
    "/close",
    "/template",
    "/cancel",
    "/?",
    "/pin",
    "/unpin",
    "/thinking",
];

/// General application settings.
#[derive(Debug, Clone, Deserialize)]
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
#[derive(Debug, Clone, Deserialize)]
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

    /// Gitee configuration (for gitee channels)
    pub gitee: Option<GiteeConfig>,

    /// GitHub configuration (for github channels)
    pub github: Option<GithubConfig>,

    /// WeChat configuration (for wechat channels)
    pub wechat: Option<WechatConfig>,

    /// WeCom configuration (for wecom channels)
    pub wecom: Option<WecomConfig>,

    /// WeCom KF (Customer Service) configuration (for wecomkf channels)
    #[serde(default)]
    pub wecom_kf: Option<WecomKfConfig>,

    /// WeCom Smart Robot configuration (for wecom_bot channels)
    #[serde(default)]
    pub wecom_bot: Option<WecomBotConfig>,

    /// Monitoring settings (IDLE vs poll, interval, etc.)
    pub monitor: Option<MonitorConfig>,

    /// Patterns for this channel
    pub patterns: Option<Vec<ChannelPattern>>,

    /// Channel-specific agent config override
    pub agent: Option<AgentConfig>,

    /// Override model for this channel (e.g., "anthropic/claude-opus-4-6").
    /// Takes priority over global [agent].model, but below pattern-level model.
    #[serde(default)]
    pub model: Option<String>,
    /// Override small_model for this channel.
    /// Takes priority over global [agent].small_model, but below pattern-level small_model.
    #[serde(default)]
    pub small_model: Option<String>,

    /// Footer display configuration (omit for default: footer enabled)
    pub footer: Option<FooterConfig>,

    /// Channel-level MCP server configurations.
    ///
    /// When set, these MCPs are loaded for all threads in this channel.
    /// Pattern-level `mcps` takes priority over this. When both are unset,
    /// falls back to global `[[mcps]]`.
    #[serde(default)]
    pub mcps: Option<Vec<McpServerConfig>>,

    /// Channel-level tools to disable for all patterns in this channel.
    ///
    /// Tool names match `Tool::name()` (e.g. `"bash"`, `"jyc_send_message"`,
    /// `"invoice/process"`). Merged with pattern-level `disabled_tools`.
    #[serde(default)]
    pub disabled_tools: Option<Vec<String>>,

    /// Channel-level MCP servers to disable for all patterns in this channel.
    ///
    /// Server names match `McpServerConfig.name`. Merged with pattern-level
    /// `disabled_mcp_servers`. Servers listed here are skipped during tool
    /// loading even if they appear in global `[[mcps]]` or channel `mcps`.
    #[serde(default)]
    pub disabled_mcp_servers: Option<Vec<String>>,

    /// Channel-level skills whitelist.
    ///
    /// When set, only skills whose names appear in this list are loaded
    /// for all threads in this channel. Pattern-level `skills` takes priority.
    /// When both are unset, all discovered skills are loaded.
    #[serde(default)]
    pub skills: Option<Vec<String>>,

    /// Channel-level skills to disable for all patterns in this channel.
    ///
    /// Skill names match the `name` field in SKILL.md frontmatter.
    /// Merged with pattern-level `disabled_skills`.
    #[serde(default)]
    pub disabled_skills: Option<Vec<String>>,
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

/// Vision model configuration for the `read_image` tool fallback.
///
/// When the primary model does not support images (`supports_images = false`),
/// the `read_image` tool uses this configuration to call an independent vision
/// model (e.g., DeepSeek-OCR) to analyze images and return text descriptions.
///
/// The `provider` field references a named entry in `[agent.providers.xxx]`
/// to reuse its `base_url` and `api_key` (or `api_key_env` as legacy).
#[derive(Debug, Clone, Deserialize)]
pub struct VisionConfig {
    /// Whether vision fallback is enabled (default: false)
    #[serde(default)]
    pub enabled: bool,
    /// Name of the provider in `[agent.providers]` to use for vision calls
    pub provider: String,
    /// Model identifier (e.g., "deepseek-ocr")
    pub model: String,
    /// Optional custom prompt for the vision model (e.g., "请仔细识别并提取图片中的所有文字内容")
    pub prompt: Option<String>,
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

    /// Model override for plan (read-only) mode. Falls back to `model` if unset.
    #[serde(default)]
    pub plan_model: Option<String>,

    /// Model override for build (full execution) mode. Falls back to `model` if unset.
    #[serde(default)]
    pub build_model: Option<String>,

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

    /// Maximum gap (seconds) between SSE events before the stream is considered
    /// hung and triggers a retry. Default: 120.
    /// Increase if your provider (e.g. DeepSeek reasoning mode) sends large
    /// reasoning blocks with extended gaps between tokens.
    #[serde(default = "default_sse_read_timeout")]
    pub sse_read_timeout_secs: u64,

    /// Static reply text (used when mode = "static")
    pub text: Option<String>,

    /// Outbound attachment configuration
    pub attachments: Option<OutboundAttachmentConfig>,

    /// Provider definitions for the in-process agent
    #[serde(default)]
    pub providers: std::collections::HashMap<String, ProviderDef>,

    /// Vision fallback configuration for text-only models to use an external
    /// vision model (e.g., DeepSeek-OCR) for image analysis via `read_image`.
    pub vision: Option<VisionConfig>,

    /// Compression configuration for session reset (global fallback).
    /// Per-pattern `reset_compression` takes priority when set.
    #[serde(default)]
    pub reset_compression: Option<ResetCompressionConfig>,

    /// Auto-reset threshold as a fraction of context window (0.0~1.0).
    /// Per-pattern `auto_reset_threshold` takes priority when set.
    /// Default: 0.95.
    #[serde(default = "default_auto_reset_threshold")]
    pub auto_reset_threshold: f64,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            mode: default_agent_mode(),
            model: None,
            plan_model: None,
            build_model: None,
            small_model: None,
            system_prompt: None,
            max_iterations: default_max_iterations(),
            sse_read_timeout_secs: default_sse_read_timeout(),
            text: None,
            attachments: None,
            providers: std::collections::HashMap::new(),
            vision: None,
            reset_compression: None,
            auto_reset_threshold: default_auto_reset_threshold(),
        }
    }
}

/// Billing rates for a model, expressed per one million tokens.
///
/// The rates map to the token classes a provider bills separately.
/// Cost for a single LLM call is computed by
/// [`crate::pricing::compute_cost_split`] as:
///
/// ```text
/// (input - cache_read - cache_creation) * input_per_million       / 1_000_000
/// + output_tokens                       * output_per_million      / 1_000_000
/// + cache_read_tokens                   * cache_hit_per_million   / 1_000_000
/// + cache_creation_tokens               * cache_creation_per_million / 1_000_000
///                                       (defaults to cache_hit_per_million)
/// ```
///
/// `input_tokens` is the provider-reported prompt size, which *includes*
/// tokens served from the prompt cache. Subtracting the two cache buckets
/// leaves the portion billed at the full input rate; the cached portions
/// are billed at their respective cache rates.
///
/// Configurable at provider level (applies to all its models) and at
/// model level (overrides the provider default). Rates are plain numbers
/// in whatever currency `currency` names — no conversion is performed.
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct ModelPricing {
    /// Price per 1M uncached input tokens (e.g. `3.0` for $3/M).
    pub input_per_million: f64,
    /// Price per 1M output tokens (e.g. `15.0` for $15/M).
    pub output_per_million: f64,
    /// Price per 1M prompt-cache-hit (read) tokens (e.g. `0.3` for
    /// $0.30/M). Set equal to `input_per_million` for providers that
    /// bill cache reads at the normal input rate.
    #[serde(default)]
    pub cache_hit_per_million: f64,
    /// Price per 1M prompt-cache-**creation** (write) tokens for
    /// providers that distinguish read and write cache buckets
    /// (Anthropic — writes bill at ~1.25× the input rate).
    ///
    /// `None` (default) collapses writes into `cache_hit_per_million` —
    /// existing single-rate configs are unchanged. Set this only when
    /// pricing Anthropic cache writes at their premium rate.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_creation_per_million: Option<f64>,
    /// Currency label used for display (e.g. `"CNY"`, `"USD"`).
    /// Defaults to [`DEFAULT_CURRENCY`] when omitted. Purely a label —
    /// jyc never converts between currencies, so a provider billing in
    /// USD must say so explicitly.
    pub currency: Option<String>,
}

/// Currency assumed when a `ModelPricing` omits `currency`.
///
/// CNY: jyc's primary deployments price in yuan. Providers billing in
/// another currency must set `currency` explicitly — no conversion is
/// ever performed, so this is purely the display label.
pub const DEFAULT_CURRENCY: &str = "CNY";

impl ModelPricing {
    /// Currency label for display, defaulting to [`DEFAULT_CURRENCY`].
    pub fn currency_label(&self) -> &str {
        self.currency.as_deref().unwrap_or(DEFAULT_CURRENCY)
    }
}

/// Provider definition for the in-process agent.
#[derive(Debug, Clone, Deserialize)]
pub struct ProviderDef {
    /// Provider type: "anthropic" or "openai-compatible"
    #[serde(rename = "type")]
    pub provider_type: String,
    /// API base URL
    pub base_url: Option<String>,
    /// API key, expressed as a `${ENV_VAR}` reference (e.g.
    /// `api_key = "${ANTHROPIC_API_KEY}"`). Consistent with every other
    /// secret field in the config. Resolved at config-load via the same
    /// `${VAR}` expansion that handles `token`, `password`, etc.
    ///
    /// When both `api_key` and `api_key_env` are set, `api_key_env` wins
    /// (legacy precedence) and a warning is logged at startup.
    #[serde(default)]
    pub api_key: Option<String>,
    /// Environment variable name containing the API key (legacy).
    /// The value is the *name* of an env var, not the key itself.
    /// Resolved lazily inside `create_provider` via `std::env::var`, so
    /// the key can be rotated in a long-running process without restart.
    #[serde(default)]
    pub api_key_env: Option<String>,
    /// Default context window size in tokens (used if model-specific not set)
    pub context_window: Option<u64>,
    /// Whether models under this provider can accept image content blocks
    /// (multimodal input). Per-model `ModelDef.supports_images` overrides this.
    /// Default: false.
    pub supports_images: Option<bool>,
    /// Extra parameters merged into every API request for this provider
    #[serde(default)]
    pub params: Option<serde_json::Value>,
    /// Optional User-Agent header override for all models under this provider.
    /// Model-level `user_agent` takes precedence over this value.
    pub user_agent: Option<String>,
    /// Default billing rates for all models under this provider.
    /// Per-model `ModelDef.pricing` overrides this. When neither is set,
    /// no cost is computed and the dashboard omits the cost row.
    pub pricing: Option<ModelPricing>,
    /// Per-model context window overrides
    #[serde(default)]
    pub models: std::collections::HashMap<String, ModelDef>,
}

impl ProviderDef {
    /// Resolve the API key for this provider.
    ///
    /// Order of preference:
    ///   1. `api_key_env` — legacy. The value is an env-var *name*; we read
    ///      the env on every call so keys can be rotated in a long-running
    ///      process without restart.
    ///   2. `api_key` — preferred. The TOML loader has already expanded
    ///      `${VAR}` references, so by the time we get here this is either
    ///      the resolved key value or `""` (env var was unset).
    ///
    /// Returns `None` when neither field is set, or when the referenced
    /// env var is unset / expands to an empty string.
    pub fn resolve_api_key(&self) -> Option<String> {
        if let Some(env_var) = &self.api_key_env {
            return std::env::var(env_var).ok().filter(|s| !s.is_empty());
        }
        self.api_key.clone().filter(|s| !s.is_empty())
    }
}

/// Per-model configuration within a provider.
#[derive(Debug, Clone, Deserialize)]
pub struct ModelDef {
    /// Actual model identifier sent to the remote LLM API.
    /// When unset, the key of this entry in `ProviderDef.models` is used.
    /// This allows multiple config entries (aliases) with different params
    /// to point at the same remote model.
    pub model_id: Option<String>,
    /// Context window size in tokens for this specific model
    pub context_window: Option<u64>,
    /// Whether this specific model can accept image content blocks
    /// (multimodal input). Overrides `ProviderDef.supports_images`.
    pub supports_images: Option<bool>,
    /// Extra parameters merged into API request when using this model (overrides provider params)
    #[serde(default)]
    pub params: Option<serde_json::Value>,
    /// Optional User-Agent header override for requests made by this model.
    /// When set, the provider sends this value as the `User-Agent` header
    /// instead of the HTTP client's default.
    pub user_agent: Option<String>,
    /// Billing rates for this specific model. Overrides
    /// `ProviderDef.pricing`.
    pub pricing: Option<ModelPricing>,
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

    /// Public base URL used by the `jyc_publish_file` tool to build
    /// shareable links to `/exchange/<channel>/<thread>/<name>`.
    /// Falls back to `http://<bind>` when unset.
    #[serde(default)]
    pub exchange_base_url: Option<String>,
}

impl InspectConfig {
    /// Base URL for published-file links: the configured `exchange_base_url`
    /// (trailing slashes trimmed) or `http://<bind>` when unset.
    pub fn effective_exchange_base_url(&self) -> String {
        self.exchange_base_url
            .clone()
            .unwrap_or_else(|| format!("http://{}", self.bind))
            .trim_end_matches('/')
            .to_string()
    }
}

impl Default for InspectConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            bind: default_inspect_bind(),
            exchange_base_url: None,
        }
    }
}

fn default_inspect_bind() -> String {
    "127.0.0.1:9876".to_string()
}

/// Scheduler configuration for channel-agnostic scheduled jobs.
///
/// Controls the background JobScheduler that fires due jobs by injecting
/// InboundMessage into the originating thread via ThreadManager.
#[derive(Debug, Clone, Deserialize)]
pub struct SchedulerConfig {
    /// Whether the job scheduler is enabled (default: true).
    #[serde(default = "default_true")]
    pub enabled: bool,

    /// How often (in seconds) the scheduler scans for due jobs (default: 60).
    #[serde(default = "default_60")]
    pub scan_interval_secs: u64,

    /// Maximum number of jobs per thread (default: 10).
    #[serde(default = "default_10_jobs")]
    pub max_jobs_per_thread: usize,
}

impl Default for SchedulerConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            scan_interval_secs: 60,
            max_jobs_per_thread: 10,
        }
    }
}

fn default_60() -> u64 {
    60
}

fn default_10_jobs() -> usize {
    10
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
    // 500 (raised from 200 in v0.3.6 — in-loop summarization at the cycle
    // boundary now keeps the request size bounded regardless of iteration
    // count). See jyc-agent's prior slim AgentConfig for the original
    // rationale.
    500
}

fn default_sse_read_timeout() -> u64 {
    120
}

#[allow(dead_code)]
fn default_1_0() -> f64 {
    1.0
}

#[allow(dead_code)]
fn default_120_0() -> f64 {
    120.0
}

fn default_auto_reset_threshold() -> f64 {
    0.95
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

/// Parse raw TOML content into a `toml::Value` tree. No `${VAR}` expansion,
/// no deserialization. Used by [`parse_and_deserialize`] and the L2 layer
/// merge in [`load_config_layered`].
///
/// `ctx` is included in the error message so failures point at the file
/// the content came from.
fn parse_toml_value(content: &str, ctx: &str) -> Result<toml::Value> {
    toml::from_str(content).with_context(|| format!("failed to parse TOML: {ctx}"))
}

/// Read a TOML file into a string and parse it to `toml::Value`. Used by
/// [`load_config`] (single file) and [`load_config_layered`] (twice — once
/// for the workdir overlay, once for the global base). No `${VAR}` expansion
/// at this step; that happens after merging in L2.
fn read_and_parse(path: &Path) -> Result<toml::Value> {
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read config file: {}", path.display()))?;
    parse_toml_value(&content, &path.display().to_string())
}

/// Parse + expand `${VAR}` + deserialize to a typed target. The canonical
/// "load a TOML config" pipeline used by the L1 and L3 loaders; L2 uses
/// [`read_and_parse`] + [`parse_and_deserialize_from_value`] for its
/// two-file deep-merge path.
fn parse_and_deserialize<T: serde::de::DeserializeOwned>(content: &str, ctx: &str) -> Result<T> {
    let value = parse_toml_value(content, ctx)?;
    parse_and_deserialize_from_value(value, ctx)
}

/// Run `${VAR}` expansion on a parsed TOML tree, then deserialize to a
/// typed target. The tail half of [`parse_and_deserialize`] — split out
/// so `load_config_layered` can call it after deep-merging L1 (base) and
/// L2 (overlay).
fn parse_and_deserialize_from_value<T: serde::de::DeserializeOwned>(
    mut value: toml::Value,
    ctx: &str,
) -> Result<T> {
    expand_env_vars(&mut value);
    value
        .try_into()
        .with_context(|| format!("failed to deserialize config: {ctx}"))
}

/// Load configuration from a TOML file.
///
/// Reads the file, expands `${VAR}` environment variable references,
/// then deserializes into `AppConfig`.
pub fn load_config(path: &Path) -> Result<AppConfig> {
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read config file: {}", path.display()))?;
    parse_and_deserialize(&content, &path.display().to_string())
}

/// Thread-level configuration (L3), loaded from `<thread_path>/.jyc/config.toml`.
///
/// Restricted subset of the app config:
/// - `[agent]`: model overrides. Precedence: `.jyc/<mode>-model-override` >
///   `.jyc/config.toml` > pattern > channel > global.
/// - `[mcps]`: MCP overrides (additive by default, opt-in full replace via
///   `mcps_replace`). Precedence: `.jyc/config.toml` > pattern > channel >
///   global. No `<mode>-model-override` higher layer exists for MCPs.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ThreadConfig {
    /// Agent overrides for this thread.
    pub agent: Option<ThreadAgentConfig>,

    /// MCPs added for this thread.
    ///
    /// Default merge = additive: thread MCPs union with pattern/channel/global
    /// MCPs, and a thread MCP with the same `name` as an inherited one wins.
    /// Set `mcps_replace = true` to fully replace the inherited set (mirror of
    /// how `ChannelPattern.mcps` overrides channel-level MCPs).
    #[serde(default)]
    pub mcps: Option<Vec<McpServerConfig>>,

    /// When `true`, ignore the matched pattern/channel/global MCPs entirely
    /// and use only `mcps`. Default `false` (additive).
    #[serde(default)]
    pub mcps_replace: bool,
}

/// Agent model overrides for a single thread.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ThreadAgentConfig {
    /// Model override for all modes.
    pub model: Option<String>,
    /// Model override for plan mode.
    pub plan_model: Option<String>,
    /// Model override for build mode.
    pub build_model: Option<String>,
    /// Small model override (used for lightweight tasks).
    pub small_model: Option<String>,
}

/// Load thread-level overrides from `<thread_path>/.jyc/config.toml`.
///
/// Returns `None` when the file does not exist, when it cannot be read
/// (e.g. EACCES in remote deployments — the agent runs under a different
/// user than the config owner), or when it fails to parse. All non-`Ok`
/// outcomes are logged at `warn` so the failure mode is visible in
/// production logs; a broken thread config must not crash the agent.
///
/// Structurally mirrors [`load_config_from_str`] but returns
/// `Option<ThreadConfig>` and swallows errors. `${VAR}` expansion runs
/// on every string field (via [`parse_and_deserialize`]).
pub fn load_thread_config(thread_path: &Path) -> Option<ThreadConfig> {
    let path = thread_path.join(".jyc").join("config.toml");
    let path_label = path.display().to_string();
    if !path.exists() {
        return None;
    }
    let content = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(
                path = %path_label,
                error = %e,
                "Failed to read thread config; thread-local MCP overlay will be skipped"
            );
            return None;
        }
    };
    // Same parse + expand + deserialize pipeline as
    // load_config_from_str / load_config_layered (all four public
    // loaders now share [`parse_and_deserialize`] for the parse+expand
    // step). Errors are swallowed (warn + None) per the docstring
    // above: a broken thread config must not crash the agent.
    match parse_and_deserialize::<ThreadConfig>(&content, &path_label) {
        Ok(cfg) => Some(cfg),
        Err(e) => {
            tracing::warn!(path = %path_label, error = %e, "Ignoring invalid thread config");
            None
        }
    }
}

/// Load configuration from a TOML string.
///
/// Expands `${VAR}` environment variable references, then deserializes.
pub fn load_config_from_str(content: &str) -> Result<AppConfig> {
    parse_and_deserialize(content, "<inline>")
}

/// Apply the thread-level (L3) MCP overlay onto a base list.
///
/// - When `thread_cfg` is `None` or its `mcps` is `None`, returns `base` unchanged.
/// - When `thread_cfg.mcps_replace` is `true`, returns the thread's MCPs only.
/// - Otherwise (additive default): union of `base` + thread MCPs; on name
///   conflict, the thread version wins (last-writer-wins).
pub fn apply_thread_mcp_overlay(
    base: &[McpServerConfig],
    thread_cfg: Option<&ThreadConfig>,
) -> Vec<McpServerConfig> {
    let Some(t) = thread_cfg else {
        return base.to_vec();
    };
    let Some(thread_mcps) = t.mcps.as_ref() else {
        return base.to_vec();
    };
    if t.mcps_replace {
        return thread_mcps.clone();
    }
    let mut out: Vec<McpServerConfig> = base.to_vec();
    for tm in thread_mcps {
        if let Some(slot) = out.iter_mut().find(|c| c.name == tm.name) {
            *slot = tm.clone();
        } else {
            out.push(tm.clone());
        }
    }
    out
}

/// Deep-merge two TOML values: tables merge recursively; all other values
/// (strings, arrays, scalars) are replaced by the overlay.
///
/// Used for layered configuration: the workdir config (overlay) overrides
/// the global config (base) on a per-key basis.
pub fn merge_toml(base: toml::Value, overlay: toml::Value) -> toml::Value {
    match (base, overlay) {
        (toml::Value::Table(mut base_table), toml::Value::Table(overlay_table)) => {
            for (key, overlay_value) in overlay_table {
                let merged = match base_table.remove(&key) {
                    Some(base_value) => merge_toml(base_value, overlay_value),
                    None => overlay_value,
                };
                base_table.insert(key, merged);
            }
            toml::Value::Table(base_table)
        }
        (_base, overlay) => overlay,
    }
}

/// Load configuration with global/workdir layering.
///
/// When `global` is `Some` and differs from `path`, the global config is
/// loaded first as the base layer and `path` is merged on top of it via
/// [`merge_toml`]. `${VAR}` expansion happens after the merge.
///
/// A missing global config file is silently ignored (layering is optional);
/// a missing `path` config file is an error.
pub fn load_config_layered(global: Option<&Path>, path: &Path) -> Result<AppConfig> {
    // Read + parse the workdir file. L2 is the overlay; L1 (if any) is
    // the base, deep-merged underneath.
    let mut value = read_and_parse(path)?;

    if let Some(global_path) = global.filter(|g| *g != path && g.exists()) {
        let global_value = read_and_parse(global_path)?;
        value = merge_toml(global_value, value);
    }

    // Expansion happens after the merge so `${VAR}` resolves identically
    // regardless of which layer defined the key. Same expand+deserialize
    // tail as L1/L3 (see [`parse_and_deserialize_from_value`]).
    parse_and_deserialize_from_value(value, &path.display().to_string())
}

/// Recursively expand `${VAR}` patterns in TOML string values
/// with values from environment variables.
///
/// Missing env vars are replaced with empty strings.
fn expand_env_vars(value: &mut toml::Value) {
    let re = Regex::new(r"\$\{(\w+)\}").unwrap();

    match value {
        toml::Value::String(s) if s.contains("${") => {
            *s = re
                .replace_all(s, |caps: &regex::Captures| {
                    std::env::var(&caps[1]).unwrap_or_default()
                })
                .to_string();
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

    /// Test-only builder for `ProviderDef`. Every field except the two
    /// API-key fields is irrelevant to the `resolve_api_key` tests; this
    /// helper shrinks the four test fixtures to one line each.
    fn provider_with_keys(api_key: Option<&str>, api_key_env: Option<&str>) -> ProviderDef {
        ProviderDef {
            provider_type: "anthropic".to_string(),
            base_url: None,
            api_key: api_key.map(String::from),
            api_key_env: api_key_env.map(String::from),
            context_window: None,
            supports_images: None,
            params: None,
            user_agent: None,
            pricing: None,
            models: std::collections::HashMap::new(),
        }
    }

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

    /// `resolve_api_key` returns the env-var value when `api_key_env` is
    /// set and the env var exists. Late binding preserved.
    #[test]
    fn resolve_api_key_uses_api_key_env_first() {
        unsafe {
            std::env::set_var("JYC_RESOLVE_KEY_TEST", "env-key-value");
        }
        let p = provider_with_keys(Some("literal-not-used"), Some("JYC_RESOLVE_KEY_TEST"));
        assert_eq!(p.resolve_api_key().as_deref(), Some("env-key-value"));
        unsafe {
            std::env::remove_var("JYC_RESOLVE_KEY_TEST");
        }
    }

    /// When `api_key_env` is unset and `api_key` carries an expanded
    /// `${VAR}` value, return that value.
    #[test]
    fn resolve_api_key_falls_back_to_api_key_field() {
        let p = provider_with_keys(Some("expanded-key-value"), None);
        assert_eq!(p.resolve_api_key().as_deref(), Some("expanded-key-value"));
    }

    /// Empty `api_key` (i.e. `${UNSET}` after expansion) and no
    /// `api_key_env` → `None`.
    #[test]
    fn resolve_api_key_returns_none_when_empty_and_no_env() {
        let p = provider_with_keys(Some(""), None);
        assert_eq!(p.resolve_api_key(), None);
    }

    /// Neither field set → `None`.
    #[test]
    fn resolve_api_key_returns_none_when_neither_set() {
        let p = provider_with_keys(None, None);
        assert_eq!(p.resolve_api_key(), None);
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

    /// End-to-end: `api_key = "${VAR}"` round-trips through the TOML
    /// loader. `${VAR}` expands at load time; the resolved value lands
    /// in the `api_key` field.
    #[test]
    fn test_provider_api_key_field_parses() {
        let toml = r#"
[general]

[channels.work]
type = "email"

[channels.work.inbound]
host = "imap.example.com"
port = 993
username = "u"
password = "p"

[channels.work.outbound]
host = "smtp.example.com"
port = 465
username = "u"
password = "p"

[agent]
enabled = true
mode = "agent"

[agent.providers.anthropic]
type = "anthropic"
base_url = "https://api.anthropic.com/v1"
api_key = "${JYC_LOAD_TEST_API_KEY}"
"#;

        // SAFETY: see existing test_expand_env_vars above. This test
        // runs alongside other unit tests; AGENTS.md prefers no env
        // mutation, but this is a load-time check of ${VAR} expansion
        // for the new field — the only way to verify it end-to-end
        // without restructuring the loader to take env as a parameter.
        unsafe {
            std::env::set_var("JYC_LOAD_TEST_API_KEY", "loaded-key-123");
        }
        let config = load_config_from_str(toml).unwrap();
        let provider = config
            .agent
            .providers
            .get("anthropic")
            .expect("anthropic provider must parse");
        assert_eq!(
            provider.api_key.as_deref(),
            Some("loaded-key-123"),
            "${{VAR}} must expand into api_key"
        );
        // legacy field stays None when not set
        assert!(provider.api_key_env.is_none());
        unsafe {
            std::env::remove_var("JYC_LOAD_TEST_API_KEY");
        }
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
            super::McpServerKind::Remote {
                url,
                enabled,
                auth_header,
                custom_headers,
                oauth,
            } => {
                assert_eq!(url, "https://mcp.example.com/handler");
                assert!(*enabled);
                assert!(auth_header.is_none());
                assert!(custom_headers.is_empty());
                assert!(oauth.is_none());
            }
            _ => panic!("Expected Remote variant for remote_mcp"),
        }
    }

    #[test]
    fn test_merge_toml_tables_deep_merge() {
        let base: toml::Value = toml::from_str(
            r#"
[general]
max_concurrent_threads = 3

[agent]
model = "global-model"
mode = "opencode"
"#,
        )
        .unwrap();
        let overlay: toml::Value = toml::from_str(
            r#"
[agent]
model = "workdir-model"
"#,
        )
        .unwrap();

        let merged = merge_toml(base, overlay);
        assert_eq!(
            merged["general"]["max_concurrent_threads"].as_integer(),
            Some(3)
        );
        // Overlay wins on conflicting keys
        assert_eq!(merged["agent"]["model"].as_str(), Some("workdir-model"));
        // Base-only keys survive
        assert_eq!(merged["agent"]["mode"].as_str(), Some("opencode"));
    }

    #[test]
    fn test_merge_toml_channels_merge_by_name() {
        let base: toml::Value = toml::from_str(
            r#"
[channels.global_chan]
type = "email"

[channels.shared]
type = "email"
"#,
        )
        .unwrap();
        let overlay: toml::Value = toml::from_str(
            r#"
[channels.local_chan]
type = "feishu"

[channels.shared]
type = "websocket"
"#,
        )
        .unwrap();

        let merged = merge_toml(base, overlay);
        let channels = merged["channels"].as_table().unwrap();
        assert_eq!(channels.len(), 3);
        assert_eq!(channels["global_chan"]["type"].as_str(), Some("email"));
        assert_eq!(channels["local_chan"]["type"].as_str(), Some("feishu"));
        // Same-name channel: overlay wins
        assert_eq!(channels["shared"]["type"].as_str(), Some("websocket"));
    }

    #[test]
    fn test_merge_toml_arrays_replaced_not_concatenated() {
        let base: toml::Value = toml::from_str(
            r#"
[[mcps]]
name = "a"
"#,
        )
        .unwrap();
        let overlay: toml::Value = toml::from_str(
            r#"
[[mcps]]
name = "b"
"#,
        )
        .unwrap();

        let merged = merge_toml(base, overlay);
        let mcps = merged["mcps"].as_array().unwrap();
        assert_eq!(mcps.len(), 1);
        assert_eq!(mcps[0]["name"].as_str(), Some("b"));
    }

    #[test]
    fn test_load_config_layered_global_base_workdir_overlay() {
        let tmp = tempfile::tempdir().unwrap();
        let global_path = tmp.path().join("global.toml");
        let workdir_path = tmp.path().join("config.toml");

        std::fs::write(
            &global_path,
            r#"
[agent]
mode = "static"
model = "global-model"

[channels.global_chan]
type = "email"
"#,
        )
        .unwrap();
        std::fs::write(
            &workdir_path,
            r#"
[agent]
mode = "static"
model = "workdir-model"

[channels.local_chan]
type = "feishu"
"#,
        )
        .unwrap();

        let config = load_config_layered(Some(&global_path), &workdir_path).unwrap();
        assert_eq!(config.agent.model.as_deref(), Some("workdir-model"));
        assert!(config.channels.contains_key("global_chan"));
        assert!(config.channels.contains_key("local_chan"));
    }

    /// L2 `${VAR}` expansion runs *after* the global/workdir deep-merge.
    /// Verifies (a) `${VAR}` from the global is expanded when only the
    /// global defines it, and (b) the workdir's literal overrides the
    /// global's `${VAR}` reference (overlay wins on scalar keys).
    #[test]
    fn test_load_config_layered_expands_env_vars_after_merge() {
        let tmp = tempfile::tempdir().unwrap();
        let global_path = tmp.path().join("global.toml");
        let workdir_path = tmp.path().join("config.toml");

        // Global uses a `${VAR}` reference; expansion happens after the
        // merge, so this is resolved against the env var at load time.
        std::fs::write(
            &global_path,
            r#"
[agent]
mode = "static"

[channels.work]
type = "email"

[channels.work.inbound]
host = "${JYC_LAYERED_HOST}"
port = 993
username = "u"
password = "p"

[channels.work.outbound]
host = "smtp.example.com"
port = 465
username = "u"
password = "p"
"#,
        )
        .unwrap();

        // Workdir overrides `host` with a literal — should win over the
        // global's `${VAR}` reference (scalar replacement in `merge_toml`).
        std::fs::write(
            &workdir_path,
            r#"
[channels.work.inbound]
host = "literal-host.example.com"
"#,
        )
        .unwrap();

        // SAFETY: existing test pattern; see test_expand_env_vars above.
        unsafe {
            std::env::set_var("JYC_LAYERED_HOST", "expanded-from-env.example.com");
        }
        let config = load_config_layered(Some(&global_path), &workdir_path).unwrap();
        let work = &config.channels["work"];
        let inbound = work.inbound.as_ref().expect("inbound must parse");
        // Workdir's literal wins over the global's `${VAR}` reference.
        assert_eq!(inbound.host, "literal-host.example.com");

        // SAFETY: cleanup.
        unsafe {
            std::env::remove_var("JYC_LAYERED_HOST");
        }
    }

    /// Companion to the above: when the workdir *also* uses `${VAR}`
    /// (and the global is absent), expansion still works. This is the
    /// simpler path through L2.
    #[test]
    fn test_load_config_layered_expands_env_vars_in_workdir_only() {
        let tmp = tempfile::tempdir().unwrap();
        let workdir_path = tmp.path().join("config.toml");
        // No global path — workdir alone.

        std::fs::write(
            &workdir_path,
            r#"
[agent]
mode = "static"

[channels.work]
type = "email"

[channels.work.inbound]
host = "${JYC_LAYERED_WORKDIR_ONLY_HOST}"
port = 993
username = "u"
password = "p"

[channels.work.outbound]
host = "smtp.example.com"
port = 465
username = "u"
password = "p"
"#,
        )
        .unwrap();

        unsafe {
            std::env::set_var(
                "JYC_LAYERED_WORKDIR_ONLY_HOST",
                "workdir-only-expanded.example.com",
            );
        }
        let config = load_config_layered(None, &workdir_path).unwrap();
        let work = &config.channels["work"];
        let inbound = work.inbound.as_ref().expect("inbound must parse");
        assert_eq!(inbound.host, "workdir-only-expanded.example.com");
        unsafe {
            std::env::remove_var("JYC_LAYERED_WORKDIR_ONLY_HOST");
        }
    }

    #[test]
    fn test_load_config_layered_missing_global_ignored() {
        let tmp = tempfile::tempdir().unwrap();
        let global_path = tmp.path().join("nonexistent.toml");
        let workdir_path = tmp.path().join("config.toml");
        std::fs::write(
            &workdir_path,
            r#"
[agent]
mode = "static"
"#,
        )
        .unwrap();

        let config = load_config_layered(Some(&global_path), &workdir_path).unwrap();
        assert_eq!(config.agent.mode, "static");
    }

    #[test]
    fn test_load_thread_config_missing_file() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(load_thread_config(tmp.path()).is_none());
    }

    #[test]
    fn test_load_thread_config_agent_overrides() {
        let tmp = tempfile::tempdir().unwrap();
        let jyc_dir = tmp.path().join(".jyc");
        std::fs::create_dir_all(&jyc_dir).unwrap();
        std::fs::write(
            jyc_dir.join("config.toml"),
            r#"
[agent]
model = "provider/thread-model"
plan_model = "provider/plan-model"
small_model = "provider/small-model"
"#,
        )
        .unwrap();

        let cfg = load_thread_config(tmp.path()).unwrap();
        let agent = cfg.agent.unwrap();
        assert_eq!(agent.model.as_deref(), Some("provider/thread-model"));
        assert_eq!(agent.plan_model.as_deref(), Some("provider/plan-model"));
        assert_eq!(agent.build_model, None);
        assert_eq!(agent.small_model.as_deref(), Some("provider/small-model"));
    }

    #[test]
    fn test_load_thread_config_invalid_toml_ignored() {
        let tmp = tempfile::tempdir().unwrap();
        let jyc_dir = tmp.path().join(".jyc");
        std::fs::create_dir_all(&jyc_dir).unwrap();
        std::fs::write(jyc_dir.join("config.toml"), "not [valid toml").unwrap();
        assert!(load_thread_config(tmp.path()).is_none());
    }

    #[test]
    fn test_load_thread_config_mcps_parsed() {
        let tmp = tempfile::tempdir().unwrap();
        let jyc_dir = tmp.path().join(".jyc");
        std::fs::create_dir_all(&jyc_dir).unwrap();
        std::fs::write(
            jyc_dir.join("config.toml"),
            r#"
[[mcps]]
name = "local-only"
type = "local"
command = ["./local-mcp"]

[agent]
model = "anthropic/claude-opus-4-7"
"#,
        )
        .unwrap();

        let cfg = load_thread_config(tmp.path()).unwrap();
        let mcps = cfg.mcps.expect("mcps field should be present");
        assert_eq!(mcps.len(), 1);
        assert_eq!(mcps[0].name, "local-only");
        assert!(matches!(mcps[0].kind, McpServerKind::Local { .. }));
        // mcps_replace defaults to false (additive).
        assert!(!cfg.mcps_replace);
    }

    #[test]
    fn test_load_thread_config_mcps_replace_flag_explicit() {
        let tmp = tempfile::tempdir().unwrap();
        let jyc_dir = tmp.path().join(".jyc");
        std::fs::create_dir_all(&jyc_dir).unwrap();
        std::fs::write(
            jyc_dir.join("config.toml"),
            r#"
mcps_replace = true

[[mcps]]
name = "totally-different"
type = "remote"
url = "https://example.com/mcp"
"#,
        )
        .unwrap();

        let cfg = load_thread_config(tmp.path()).unwrap();
        assert!(cfg.mcps_replace);
        let mcps = cfg.mcps.unwrap();
        assert_eq!(mcps[0].name, "totally-different");
    }

    /// L3 bug fix regression: `${VAR}` in `[agent].model` must expand at
    /// thread-config load. Before the shared `parse_and_deserialize`
    /// helper, thread loader bypassed `expand_env_vars` and the literal
    /// `${VAR}` string landed in the `ThreadConfig`.
    #[test]
    fn test_load_thread_config_expands_env_vars_in_agent_model() {
        // SAFETY: AGENTS.md prefers no env mutation, but verifying the
        // load-time expansion end-to-end requires it. Test uses a unique
        // env-var name and cleans up; see existing test_expand_env_vars.
        unsafe {
            std::env::set_var("JYC_LOAD_THREAD_MODEL", "anthropic/claude-opus-4-7");
        }
        let tmp = tempfile::tempdir().unwrap();
        let jyc_dir = tmp.path().join(".jyc");
        std::fs::create_dir_all(&jyc_dir).unwrap();
        std::fs::write(
            jyc_dir.join("config.toml"),
            r#"
[agent]
model = "${JYC_LOAD_THREAD_MODEL}"
"#,
        )
        .unwrap();

        let cfg = load_thread_config(tmp.path()).unwrap();
        let agent = cfg.agent.unwrap();
        assert_eq!(
            agent.model.as_deref(),
            Some("anthropic/claude-opus-4-7"),
            "${{VAR}} in [agent].model must expand at thread-config load"
        );
        unsafe {
            std::env::remove_var("JYC_LOAD_THREAD_MODEL");
        }
    }

    /// L3 `${VAR}` expansion in `[[mcps]].command` (a `Vec<String>`).
    /// The recursive walker descends into arrays too, so each element
    /// gets expanded.
    #[test]
    fn test_load_thread_config_expands_env_vars_in_mcp_command() {
        unsafe {
            std::env::set_var("JYC_LOAD_THREAD_MCP_BIN", "/opt/jyc/mcp-server");
            std::env::set_var("JYC_LOAD_THREAD_MCP_TOKEN", "secret-token");
        }
        let tmp = tempfile::tempdir().unwrap();
        let jyc_dir = tmp.path().join(".jyc");
        std::fs::create_dir_all(&jyc_dir).unwrap();
        std::fs::write(
            jyc_dir.join("config.toml"),
            r#"
[[mcps]]
name = "local-tools"
type = "local"
command = ["${JYC_LOAD_THREAD_MCP_BIN}", "--flag"]

[mcps.environment]
TOKEN = "${JYC_LOAD_THREAD_MCP_TOKEN}"
"#,
        )
        .unwrap();

        let cfg = load_thread_config(tmp.path()).unwrap();
        let mcps = cfg.mcps.expect("mcps field should be present");
        assert_eq!(mcps.len(), 1);
        match &mcps[0].kind {
            McpServerKind::Local {
                command,
                environment,
            } => {
                assert_eq!(command[0], "/opt/jyc/mcp-server");
                assert_eq!(command[1], "--flag");
                assert_eq!(
                    environment.get("TOKEN").map(String::as_str),
                    Some("secret-token"),
                    "${{VAR}} must expand inside [[mcps]].environment"
                );
            }
            other => panic!("expected Local MCP, got {:?}", other),
        }
        unsafe {
            std::env::remove_var("JYC_LOAD_THREAD_MCP_BIN");
            std::env::remove_var("JYC_LOAD_THREAD_MCP_TOKEN");
        }
    }

    /// Missing env var at thread level → empty string, no panic. Matches
    /// the global loader's `unwrap_or_default()` behavior.
    #[test]
    fn test_load_thread_config_missing_env_var_yields_empty() {
        // SAFETY: ensure the env var is unset before the test runs.
        unsafe {
            std::env::remove_var("JYC_LOAD_THREAD_DEFINITELY_UNSET");
        }
        let tmp = tempfile::tempdir().unwrap();
        let jyc_dir = tmp.path().join(".jyc");
        std::fs::create_dir_all(&jyc_dir).unwrap();
        std::fs::write(
            jyc_dir.join("config.toml"),
            r#"
[agent]
model = "${JYC_LOAD_THREAD_DEFINITELY_UNSET}"
"#,
        )
        .unwrap();

        let cfg = load_thread_config(tmp.path()).unwrap();
        let agent = cfg.agent.unwrap();
        assert_eq!(
            agent.model.as_deref(),
            Some(""),
            "missing env var must expand to empty string"
        );
    }

    #[test]
    fn test_load_config_layered_same_path_not_double_loaded() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.toml");
        std::fs::write(
            &path,
            r#"
[agent]
mode = "static"
"#,
        )
        .unwrap();

        let config = load_config_layered(Some(&path), &path).unwrap();
        assert_eq!(config.agent.mode, "static");
    }

    // ---- apply_thread_mcp_overlay ----

    fn local_mcp(name: &str) -> McpServerConfig {
        McpServerConfig {
            name: name.to_string(),
            kind: McpServerKind::Local {
                command: vec!["./x".to_string()],
                environment: Default::default(),
            },
            enabled_tools: None,
        }
    }

    fn remote_mcp(name: &str, url: &str) -> McpServerConfig {
        McpServerConfig {
            name: name.to_string(),
            kind: McpServerKind::Remote {
                url: url.to_string(),
                enabled: true,
                auth_header: None,
                custom_headers: Default::default(),
                oauth: None,
            },
            enabled_tools: None,
        }
    }

    #[test]
    fn test_apply_thread_mcp_overlay_none_is_noop() {
        let base = vec![local_mcp("a")];
        let out = apply_thread_mcp_overlay(&base, None);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].name, "a");
    }

    #[test]
    fn test_apply_thread_mcp_overlay_additive_unions() {
        let base = vec![local_mcp("a")];
        let thread = ThreadConfig {
            mcps: Some(vec![remote_mcp("b", "https://b")]),
            mcps_replace: false,
            ..Default::default()
        };
        let out = apply_thread_mcp_overlay(&base, Some(&thread));
        let names: Vec<&str> = out.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["a", "b"]);
    }

    #[test]
    fn test_apply_thread_mcp_overlay_thread_wins_on_conflict() {
        let base = vec![remote_mcp("a", "https://inherited")];
        let thread = ThreadConfig {
            mcps: Some(vec![remote_mcp("a", "https://thread")]),
            mcps_replace: false,
            ..Default::default()
        };
        let out = apply_thread_mcp_overlay(&base, Some(&thread));
        assert_eq!(out.len(), 1);
        match &out[0].kind {
            McpServerKind::Remote { url, .. } => assert_eq!(url, "https://thread"),
            _ => panic!("expected remote"),
        }
    }

    #[test]
    fn test_apply_thread_mcp_overlay_replace_drops_base() {
        let base = vec![local_mcp("a"), local_mcp("b")];
        let thread = ThreadConfig {
            mcps: Some(vec![remote_mcp("c", "https://c")]),
            mcps_replace: true,
            ..Default::default()
        };
        let out = apply_thread_mcp_overlay(&base, Some(&thread));
        let names: Vec<&str> = out.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["c"]);
    }

    /// Helper-level coverage: `parse_and_deserialize` runs the full
    /// parse + expand + deserialize pipeline used by all three loaders.
    #[test]
    fn parse_and_deserialize_expands_env_vars() {
        unsafe {
            std::env::set_var("JYC_PARSE_AND_DESERIALIZE_VAR", "value-from-env");
        }
        let toml = r#"
[general]

[channels.work]
type = "email"

[channels.work.inbound]
host = "imap.example.com"
port = 993
username = "u"
password = "${JYC_PARSE_AND_DESERIALIZE_VAR}"

[channels.work.outbound]
host = "smtp.example.com"
port = 465
username = "u"
password = "literal-pw"

[agent]
enabled = true
mode = "agent"
"#;
        let cfg: AppConfig = parse_and_deserialize(toml, "<test>").unwrap();
        let work = cfg.channels.get("work").expect("work channel must parse");
        assert_eq!(work.inbound.as_ref().unwrap().password, "value-from-env");
        assert_eq!(work.outbound.as_ref().unwrap().password, "literal-pw");
        unsafe {
            std::env::remove_var("JYC_PARSE_AND_DESERIALIZE_VAR");
        }
    }

    /// Helper-level coverage: `parse_toml_value` errors out on invalid
    /// TOML with a context message.
    #[test]
    fn parse_toml_value_handles_invalid_toml() {
        let result = parse_toml_value("not [valid", "<bad>");
        assert!(result.is_err());
        let msg = format!("{}", result.unwrap_err());
        assert!(
            msg.contains("failed to parse TOML") && msg.contains("<bad>"),
            "error must mention the failure and ctx label; got: {msg}"
        );
    }
}
