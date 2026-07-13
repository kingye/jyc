//! Shared agent service builder for CLI commands.
//!
//! Extracts common agent initialization logic used by both `monitor` and `local`
//! commands to avoid code duplication.

use std::path::Path;
use std::sync::Arc;

use anyhow::Result;

use jyc_agent::JycAgentService;
use jyc_core::agent::AgentService;
use jyc_core::static_agent::StaticAgentService;
use jyc_types::{
    AgentConfig, ChannelConfig, ChannelPattern, InboundAttachmentConfig, McpServerConfig,
    OutboundAdapter,
};

/// Result of building an agent service.
///
/// The `jyc_agent` field is `Some` only when the configured mode is `"agent"`,
/// allowing callers (e.g. `monitor.rs`) that need the concrete type for
/// cross-channel wiring to collect it separately.
pub struct AgentServiceResult {
    /// The generic agent service trait object.
    pub agent: Arc<dyn AgentService>,
    /// Concrete `JycAgentService` when mode == `"agent"`.
    pub jyc_agent: Option<Arc<JycAgentService>>,
}

/// Build an `AgentServiceResult` from configuration.
///
/// This helper centralises the ~110 lines of agent setup that are identical
/// between `monitor.rs` and `local.rs` (provider mapping, `AgentConfig`
/// construction, `VisionClient` building, `JycAgentService::new` call).
///
/// # Parameters
/// - `agent_config` – global `[agent]` table from `config.toml`
/// - `channel_config` – per-channel configuration
/// - `workdir` – JYC working directory
/// - `outbound` – outbound adapter for the channel
/// - `patterns` – channel patterns (used for per-pattern overrides)
/// - `global_mcp_configs` – global MCP server configurations
/// - `inbound_attachment_config` – optional inbound attachment config (`None` for local)
/// - `channel_name` – channel name (for logging / context)
#[allow(clippy::too_many_arguments)]
pub fn build_agent_service(
    agent_config: &AgentConfig,
    channel_config: &ChannelConfig,
    workdir: &Path,
    outbound: Arc<dyn OutboundAdapter>,
    patterns: Vec<ChannelPattern>,
    global_mcp_configs: Vec<McpServerConfig>,
    inbound_attachment_config: Option<InboundAttachmentConfig>,
    channel_name: &str,
) -> Result<AgentServiceResult> {
    let effective_model = channel_config
        .model
        .clone()
        .or_else(|| agent_config.model.clone());
    let effective_small_model = channel_config
        .small_model
        .clone()
        .or_else(|| agent_config.small_model.clone());

    match agent_config.mode.as_str() {
        "agent" => {
            let model = effective_model;
            tracing::info!(channel = %channel_name, model = ?model, "Using agent: jyc-agent (in-process)");

            let providers = agent_config
                .providers
                .iter()
                .map(|(name, def)| {
                    let models = def
                        .models
                        .iter()
                        .map(|(model_name, model_def)| {
                            (
                                model_name.clone(),
                                jyc_agent::types::ModelConfig {
                                    context_window: model_def.context_window,
                                    supports_images: model_def.supports_images,
                                    params: model_def.params.clone(),
                                    user_agent: model_def.user_agent.clone(),
                                },
                            )
                        })
                        .collect();
                    (
                        name.clone(),
                        jyc_agent::types::ProviderConfig {
                            provider_type: def.provider_type.clone(),
                            base_url: def.base_url.clone(),
                            api_key_env: def.api_key_env.clone(),
                            context_window: def.context_window,
                            supports_images: def.supports_images,
                            params: def.params.clone(),
                            user_agent: def.user_agent.clone(),
                            models,
                        },
                    )
                })
                .collect();

            let agent_cfg = jyc_agent::types::AgentConfig {
                plan_model: None,
                build_model: None,
                model,
                small_model: effective_small_model.clone(),
                providers,
                max_iterations: agent_config.max_iterations,
                sse_read_timeout_secs: agent_config.sse_read_timeout_secs,
                vision: agent_config
                    .vision
                    .clone()
                    .map(|v| jyc_agent::types::VisionConfig {
                        enabled: v.enabled,
                        provider: v.provider,
                        model: v.model,
                        prompt: v.prompt,
                    }),
                reset_compression: agent_config.reset_compression.clone(),
                auto_reset_threshold: agent_config.auto_reset_threshold,
            };

            let channel_patterns = patterns;

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
                        let api_key_env = provider_def
                            .api_key_env
                            .clone()
                            .unwrap_or_else(|| "DEEPSEEK_API_KEY".to_string());
                        let api_key = std::env::var(&api_key_env).unwrap_or_default();
                        if api_key.is_empty() {
                            tracing::warn!(
                                provider = %v.provider,
                                api_key_env = %api_key_env,
                                "Vision fallback: API key not found in environment"
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
                agent_cfg,
                workdir.to_path_buf(),
                global_mcp_configs,
                channel_config.mcps.clone(),
                channel_patterns,
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
