//! Shared agent service builder for CLI commands.
//!
//! Extracts common agent initialization logic used by both `serve` and `local`
//! commands to avoid code duplication.

use std::path::Path;
use std::sync::Arc;

use anyhow::Result;
use arc_swap::ArcSwap;

use jyc_agent::JycAgentService;
use jyc_agent::service::derive_agent_config;
use jyc_core::agent::AgentService;
use jyc_core::static_agent::StaticAgentService;
use jyc_types::{
    ChannelConfig, ChannelPattern, InboundAttachmentConfig, McpServerConfig, OutboundAdapter,
};

/// Result of building an agent service.
///
/// The `jyc_agent` field is `Some` only when the configured mode is `"agent"`,
/// allowing callers (e.g. `serve.rs`) that need the concrete type for
/// cross-channel wiring to collect it separately.
pub struct AgentServiceResult {
    /// The generic agent service trait object.
    pub agent: Arc<dyn AgentService>,
    /// Concrete `JycAgentService` when mode == `"agent"`.
    pub jyc_agent: Option<Arc<JycAgentService>>,
}

/// Build an `AgentServiceResult` from configuration.
///
/// This helper centralises the common agent setup shared between `serve.rs`
/// and the in-process command-palette path (provider mapping, vision-client
/// building, `JycAgentService::new` call).
///
/// # Parameters
/// - `live_config` – shared `Arc<ArcSwap<AppConfig>>` (single source of truth,
///   same handle the inspect server, `MessageRouter`, and `TopicManager` use).
///   Reloading the config in the TUI atomically swaps in a new `AppConfig`,
///   and the agent service reads the new values on each request — no restart
///   needed.
/// - `agent_config` – global `[agent]` table from `config.toml` (a snapshot;
///   passed for vision-client wiring which still needs a stable handle at
///   construction time).
/// - `channel_config` – per-channel configuration
/// - `workdir` – JYC working directory
/// - `outbound` – outbound adapter for the channel
/// - `patterns` – channel patterns (used for per-pattern overrides)
/// - `global_mcp_configs` – global MCP server configurations
/// - `inbound_attachment_config` – optional inbound attachment config (`None` for local)
/// - `channel_name` – channel name (for logging / context)
#[allow(clippy::too_many_arguments)]
pub fn build_agent_service(
    live_config: Arc<ArcSwap<jyc_types::AppConfig>>,
    agent_config: &jyc_types::AgentConfig,
    channel_config: &ChannelConfig,
    workdir: &Path,
    outbound: Arc<dyn OutboundAdapter>,
    patterns: Vec<ChannelPattern>,
    global_mcp_configs: Vec<McpServerConfig>,
    inbound_attachment_config: Option<InboundAttachmentConfig>,
    channel_name: &str,
) -> Result<AgentServiceResult> {
    match agent_config.mode.as_str() {
        "agent" => {
            // Derive the effective agent config once for the log line. The
            // service itself re-derives it live on every call via
            // `JycAgentService::agent_config()`.
            let initial = derive_agent_config(&live_config.load(), channel_name);
            let model = initial.model.clone();
            tracing::info!(channel = %channel_name, model = ?model, "Using agent: jyc-agent (in-process)");

            let vision_client: Option<std::sync::Arc<jyc_agent::vision::VisionClient>> = {
                agent_config
                    .vision
                    .as_ref()
                    .filter(|v| v.enabled)
                    .and_then(|v| {
                        let provider_def = agent_config.providers.get(&v.provider)?;
                        let base_url = provider_def
                            .base_url
                            .clone()
                            .unwrap_or_else(|| "https://api.deepseek.com".to_string());
                        // Same precedence as the main provider path:
                        // `api_key_env` (legacy) wins when both are set,
                        // else fall back to `api_key`. If the provider
                        // has neither, fall back to the historical
                        // DEEPSEEK_API_KEY env var (vision callers who
                        // never customized their config rely on this).
                        let api_key = provider_def
                            .resolve_api_key()
                            .or_else(|| std::env::var("DEEPSEEK_API_KEY").ok())
                            .unwrap_or_default();
                        if api_key.is_empty() {
                            tracing::warn!(
                                provider = %v.provider,
                                "Vision fallback: API key not found (set api_key or api_key_env in [agent.providers.{}])",
                                v.provider,
                            );
                            return None;
                        }
                        Some(std::sync::Arc::new(jyc_agent::vision::VisionClient::new(
                            base_url,
                            api_key,
                            v.model.clone(),
                            v.prompt.clone(),
                        )))
                    })
            };

            let jyc_agent_svc = Arc::new(JycAgentService::new(
                live_config,
                workdir.to_path_buf(),
                global_mcp_configs,
                channel_config.mcps.clone(),
                patterns,
                inbound_attachment_config,
                vision_client,
                Some(outbound),
                channel_config.disabled_tools.clone(),
                channel_config.disabled_mcp_servers.clone(),
                channel_config.skills.clone(),
                channel_config.disabled_skills.clone(),
                channel_name.to_string(),
            ));

            Ok(AgentServiceResult {
                agent: jyc_agent_svc.clone(),
                jyc_agent: Some(jyc_agent_svc),
            })
        }
        "static" => {
            let text = agent_config
                .text
                .as_deref()
                .unwrap_or("Thank you for your message.");
            let agent = Arc::new(StaticAgentService::new(text));
            Ok(AgentServiceResult {
                agent,
                jyc_agent: None,
            })
        }
        other => {
            anyhow::bail!("unsupported agent mode: '{other}'");
        }
    }
}
