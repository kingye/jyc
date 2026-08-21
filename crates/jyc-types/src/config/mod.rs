use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use crate::WecomGlobalConfig;

use crate::channel::ChannelPattern;
use crate::channel::{ContextStrategyConfig, ResetCompressionConfig};
use crate::feishu_config::FeishuConfig;
use crate::gitee_config::GiteeConfig;
use crate::github_config::GithubConfig;
use crate::wechat_config::WechatConfig;
use crate::wecom_bot_config::WecomBotConfig;
use crate::wecom_config::WecomConfig;
use crate::wecom_kf_config::WecomKfConfig;

pub mod agent;
pub use agent::AgentConfig;

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

    /// Named agents — websocket-based endpoints with behavior but no
    /// matching rules. Each `[agents.<name>]` becomes one pattern
    /// inside the synthesized channel "agents" (channel_type =
    /// "websocket"). Supersedes the legacy
    /// `[channels.<name>] type = "websocket"` + `[[patterns]]` form.
    #[serde(default)]
    pub agents: HashMap<String, AgentConfig>,

    /// AI configuration (model, prompts, providers) — the shared brain.
    /// Legacy key `[agent]` is accepted as an alias (deprecated).
    #[serde(rename = "ai", alias = "agent")]
    pub ai: AiConfig,

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
    /// and fires due jobs by injecting InboundMessage into TopicManager.
    #[serde(default)]
    pub scheduler: SchedulerConfig,

    /// User-defined slash commands (e.g. `/review`), declared as `[[commands]]`.
    #[serde(default)]
    pub commands: Vec<CustomCommand>,
}

/// A user-defined slash command declared in `config.toml` as `[[commands]]`.
///
/// Invoking `/<name>` switches the topic to `mode` (when set), points the
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
    /// When unset, the current topic mode is left unchanged.
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
    "/exchange",
    "/context",
];

/// General application settings.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GeneralConfig {
    /// Max concurrent topic workers (default: 3)
    #[serde(default = "default_3")]
    pub max_concurrent_topics: usize,

    /// Max queued messages per topic (default: 10)
    #[serde(default = "default_10")]
    pub max_queue_size_per_topic: usize,
}

impl Default for GeneralConfig {
    fn default() -> Self {
        Self {
            max_concurrent_topics: 3,
            max_queue_size_per_topic: 10,
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
#[derive(Debug, Clone, Default, Deserialize)]
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

    /// Channel-specific AI config override.
    /// Legacy key `agent` is accepted as an alias (deprecated).
    #[serde(rename = "ai", alias = "agent", default)]
    pub ai: Option<AiConfig>,

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
    /// When set, these MCPs are loaded for all topics in this channel.
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
    /// for all topics in this channel. Pattern-level `skills` takes priority.
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
pub struct AiConfig {
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

    /// Default context management strategy. Per-pattern `context_strategy`
    /// takes priority when set.
    #[serde(default)]
    pub context_strategy: Option<ContextStrategyConfig>,
}

impl Default for AiConfig {
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
            context_strategy: None,
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
    /// Time-of-day rate overrides. Each window supplies its own rates for
    /// the hours between `start` and `end`; the flat fields above act as
    /// the default for any time outside every window. First matching
    /// window wins (windows are expected to be non-overlapping).
    /// Empty by default — flat rates always apply.
    #[serde(default)]
    pub time_windows: Vec<TimeWindowPricing>,
    /// Fixed UTC offset used to interpret `time_windows` `start`/`end`
    /// times, e.g. `"+08:00"` for Beijing time (DeepSeek's off-peak
    /// discount schedule). Defaults to UTC when omitted or unparseable.
    #[serde(default)]
    pub utc_offset: Option<String>,
}

/// Rates for one time-of-day window within [`ModelPricing`].
///
/// `start`/`end` are `"HH:MM"` or `"HH:MM:SS"` local times (in the
/// pricing's `utc_offset`). A window whose `start > end` wraps past
/// midnight (e.g. `16:30` → `00:30`). The interval is
/// start-inclusive / end-exclusive. Rates omitted on a window inherit
/// the flat [`ModelPricing`] values, so a window that only varies
/// input/output keeps the flat cache rates.
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct TimeWindowPricing {
    /// Window start, `"HH:MM"` or `"HH:MM:SS"` (inclusive).
    pub start: String,
    /// Window end, `"HH:MM"` or `"HH:MM:SS"` (exclusive; `<= start`
    /// wraps past midnight).
    pub end: String,
    /// Price per 1M uncached input tokens during this window.
    pub input_per_million: f64,
    /// Price per 1M output tokens during this window.
    pub output_per_million: f64,
    /// Price per 1M prompt-cache-hit (read) tokens during this window.
    /// `None` inherits `ModelPricing::cache_hit_per_million`.
    #[serde(default)]
    pub cache_hit_per_million: Option<f64>,
    /// Price per 1M prompt-cache-creation (write) tokens during this
    /// window. `None` inherits `ModelPricing::cache_creation_per_million`
    /// and then falls back to the (resolved) cache-hit rate.
    #[serde(default)]
    pub cache_creation_per_million: Option<f64>,
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

    /// Externally-reachable base URL of the inspect server (scheme + host,
    /// optionally port and subpath), used to build links that leave the
    /// server — currently `/exchange/<channel>/<topic>/<name>` share links.
    /// Required behind a reverse proxy; falls back to `http://<bind>`
    /// (wildcard host replaced by the primary LAN IP) when unset.
    #[serde(default)]
    pub base_url: Option<String>,
}

impl InspectConfig {
    /// Base URL for outbound links: the configured `base_url` (trailing
    /// slashes trimmed) or `http://<bind>` when unset.
    ///
    /// A wildcard bind host (`0.0.0.0`, `[::]`) is never a reachable
    /// destination, so it is replaced with this host's primary LAN IP —
    /// otherwise every generated link would be dead off-machine. That
    /// substitution opens a throwaway UDP socket (see [`primary_lan_ip`]),
    /// so this method is not pure. Behind a reverse proxy the guess cannot
    /// be right; set `base_url` explicitly.
    pub fn effective_base_url(&self) -> String {
        self.base_url
            .clone()
            .unwrap_or_else(|| format!("http://{}", self.reachable_bind()))
            .trim_end_matches('/')
            .to_string()
    }

    /// `bind` with a wildcard IP swapped for the primary LAN IP, port kept.
    /// Unparseable or already-concrete binds are returned unchanged.
    fn reachable_bind(&self) -> String {
        match self.bind.parse::<std::net::SocketAddr>() {
            Ok(addr) if addr.ip().is_unspecified() => {
                let ip = primary_lan_ip();
                tracing::warn!(
                    bind = %self.bind,
                    guessed_host = %ip,
                    "[inspect] bind is a wildcard address, which is not reachable from \
                     clients; guessing {ip} for generated links. Set \
                     [inspect] base_url to the URL clients actually use \
                     (required behind a reverse proxy)."
                );
                format!("{}:{}", ip, addr.port())
            }
            _ => self.bind.clone(),
        }
    }
}

/// Best-effort primary LAN IPv4 of this host, falling back to `127.0.0.1`.
///
/// Asks the OS which local interface it would route from by `connect`ing a
/// UDP socket to a public address; UDP `connect` only sets the socket's peer,
/// so no packet is sent and the address need not be reachable.
fn primary_lan_ip() -> std::net::Ipv4Addr {
    let fallback = std::net::Ipv4Addr::LOCALHOST;
    let Ok(socket) = std::net::UdpSocket::bind("0.0.0.0:0") else {
        return fallback;
    };
    if socket.connect("8.8.8.8:80").is_err() {
        return fallback;
    }
    match socket.local_addr() {
        // Bound to 0.0.0.0, so the local address is always IPv4.
        Ok(std::net::SocketAddr::V4(addr)) => *addr.ip(),
        _ => fallback,
    }
}

impl Default for InspectConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            bind: default_inspect_bind(),
            base_url: None,
        }
    }
}

fn default_inspect_bind() -> String {
    "127.0.0.1:9876".to_string()
}

/// Scheduler configuration for channel-agnostic scheduled jobs.
///
/// Controls the background JobScheduler that fires due jobs by injecting
/// InboundMessage into the originating topic via TopicManager.
#[derive(Debug, Clone, Deserialize)]
pub struct SchedulerConfig {
    /// Whether the job scheduler is enabled (default: true).
    #[serde(default = "default_true")]
    pub enabled: bool,

    /// How often (in seconds) the scheduler scans for due jobs (default: 60).
    #[serde(default = "default_60")]
    pub scan_interval_secs: u64,

    /// Maximum number of jobs per topic (default: 10).
    #[serde(default = "default_10_jobs")]
    pub max_jobs_per_topic: usize,
}

impl Default for SchedulerConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            scan_interval_secs: 60,
            max_jobs_per_topic: 10,
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
    // count). See jyc-agent's prior slim AiConfig for the original
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
    /// If not set, attachments will be saved to topic directory
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
pub(crate) fn read_and_parse(path: &Path) -> Result<toml::Value> {
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
    // Deprecation warning for the legacy `[agent]` table (renamed to `[ai]`).
    if let toml::Value::Table(t) = &value
        && t.contains_key("agent")
    {
        tracing::warn!(
            "config {ctx}: the [agent] table is deprecated; rename it to [ai] \
             (legacy key still accepted)"
        );
    }
    // Resolve `[agents.<name>] extends = "<base>"` inheritance before
    // expansion (so `${VAR}` resolves uniformly in base and child) and
    // before deserialization (so the `extends` key never reaches the
    // `deny_unknown_fields` `AgentConfig` struct).
    resolve_agent_extends(&mut value, ctx)?;
    expand_env_vars(&mut value);
    value
        .try_into()
        .with_context(|| format!("failed to deserialize config: {ctx}"))
}

/// Load configuration from a TOML file.
///
/// Reads the file, expands `${VAR}` environment variable references,
/// then deserializes into `AppConfig`.
mod loader;
#[cfg(test)]
mod tests;

pub use loader::*;

use loader::{expand_env_vars, resolve_agent_extends};
