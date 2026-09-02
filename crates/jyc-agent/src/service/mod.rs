//! AgentService implementation using the in-process agent loop.
//!
//! Uses direct LLM calls and tool execution instead of external server.

use anyhow::{Context, Result};
use arc_swap::ArcSwap;
use async_trait::async_trait;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use tokio::sync::{Mutex, mpsc};
use tokio_util::sync::CancellationToken;
use tracing;

use jyc_core::agent::{AgentResult, AgentService};
use jyc_core::topic_event_bus::TopicEventBusRef;
use jyc_types::{ChannelPattern, InboundMessage, QueueItem};

use crate::agent_loop::{self, AgentLoopConfig};
use crate::provider;
use crate::session;
use crate::tools::OutboundsMap;
use crate::tools::TopicManagersMap;
use crate::tools::registry::ToolRegistry;
use crate::vision::VisionClient;
use std::sync::Arc;

/// Metadata for a discovered skill.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillMeta {
    /// Skill name (e.g., "coding-principles")
    pub name: String,
    /// Human-readable description
    pub description: String,
    /// Path to the skill's directory (contains SKILL.md)
    pub source_path: PathBuf,
}

/// Parse frontmatter from a SKILL.md file.
pub struct JycAgentService {
    /// Live, swappable view of the full application config (shared with
    /// `MessageRouter` and the inspect server). On every config reload, the
    /// new `AppConfig` is atomically swapped in here, so the agent picks up
    /// new models, context_windows, params, etc. without a server restart.
    /// All per-model/agent fields are derived from this on demand via
    /// [`Self::agent_config`].
    config: Arc<ArcSwap<jyc_types::AppConfig>>,
    /// Per-topic event bus map.
    event_buses: Mutex<HashMap<String, TopicEventBusRef>>,
    /// JYC workdir (for discovering global skills).
    workdir: PathBuf,
    /// Channel patterns for the current channel (not cross-channel flattened).
    /// Used to look up per-pattern agent runtime flags (e.g.
    /// `inject_inbound_images`, model/small_model overrides, mcps,
    /// disabled_builtin_tools) by `InboundMessage.matched_pattern`.
    /// NOTE: only used by the prompt/model-resolution path; the tool
    /// registry derives MCP configs live from `config` on each build.
    patterns: Vec<ChannelPattern>,
    /// Global `[attachments.inbound]` config (used as fallback when a matched
    /// pattern does not specify its own `attachments`).
    global_inbound_attachments: Option<jyc_types::InboundAttachmentConfig>,
    /// Vision fallback client for text-only models to analyze images.
    vision_client: Option<Arc<VisionClient>>,
    /// Cache of MCP-enriched tool registries keyed by
    /// `(topic, config_snapshot_ptr)`. Bypasses the subprocess-spawn
    /// / HTTP-handshake cost on every inbound message when the config
    /// hasn't changed. A config swap (via `ArcSwap::store`) gives the
    /// new `Arc<AppConfig>` a fresh pointer, so the cache key
    /// invalidates automatically.
    registry_cache: Mutex<HashMap<(String, usize), Arc<ToolRegistry>>>,
    /// Outbound adapter for proactive messaging tools (e.g. `jyc_send_message`).
    outbound: Option<Arc<dyn jyc_types::channel::OutboundAdapter>>,
    /// Channel-level tools to disable (merged with pattern-level).
    channel_disabled_tools: Option<Vec<String>>,
    /// Channel-level skills whitelist.
    channel_skills: Option<Vec<String>>,
    /// Channel-level skills to disable (merged with pattern-level).
    channel_disabled_skills: Option<Vec<String>>,
    /// Cross-channel topic managers keyed by channel name.
    /// Passed through to `AgentLoopConfig` so the `jyc_send_to_topic` tool
    /// can inject messages into topics in other channels.
    /// Uses `std::sync::Mutex` for interior mutability (set after construction
    /// via `set_topic_managers()` on an `Arc<Self>`).
    topic_managers: std::sync::Mutex<Option<TopicManagersMap>>,
    /// Current channel name for source context in cross-topic tools.
    channel_name: String,
    /// Cross-channel outbound adapters keyed by channel name.
    /// Passed through to `AgentLoopConfig` so the `jyc_send_message` tool
    /// can send proactive messages through any channel's outbound adapter.
    /// Uses `std::sync::Mutex` for interior mutability (set after construction
    /// via `set_outbounds()` on an `Arc<Self>`).
    outbounds: std::sync::Mutex<Option<OutboundsMap>>,
}

impl JycAgentService {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        config: Arc<ArcSwap<jyc_types::AppConfig>>,
        workdir: PathBuf,
        patterns: Vec<ChannelPattern>,
        global_inbound_attachments: Option<jyc_types::InboundAttachmentConfig>,
        vision_client: Option<Arc<VisionClient>>,
        outbound: Option<Arc<dyn jyc_types::channel::OutboundAdapter>>,
        channel_disabled_tools: Option<Vec<String>>,
        channel_skills: Option<Vec<String>>,
        channel_disabled_skills: Option<Vec<String>>,
        channel_name: String,
    ) -> Self {
        Self {
            config,
            event_buses: Mutex::new(HashMap::new()),
            workdir,
            patterns,
            global_inbound_attachments,
            vision_client,
            outbound,
            channel_disabled_tools,
            channel_skills,
            channel_disabled_skills,
            registry_cache: Mutex::new(HashMap::new()),
            topic_managers: std::sync::Mutex::new(None),
            channel_name,
            outbounds: std::sync::Mutex::new(None),
        }
    }

    /// Derive the effective `jyc_types::AiConfig` for this service's
    /// channel from the live `AppConfig`.
    ///
    /// Reads the current `AppConfig` via the shared `ArcSwap` and applies
    /// the channel-level overrides (`channels.<name>.model`,
    /// `channels.<name>.small_model`) on top of the global `[agent]`
    /// section. Provider definitions, agent flags, vision, and reset
    /// compression pass through unchanged.
    ///
    /// Called on every agent request, so any reload of `config.toml` (via
    /// the TUI `reload config` action) takes effect immediately — no
    /// restart needed.
    pub fn agent_config(&self) -> jyc_types::AiConfig {
        derive_agent_config(&self.config.load(), &self.channel_name)
    }

    /// Set the cross-channel topic managers map.
    ///
    /// Called by the monitor during startup to inject the topic manager map
    /// into each agent service after all channels have been initialized.
    pub fn set_topic_managers(&self, tm: TopicManagersMap) {
        *self.topic_managers.lock().expect("topic_managers poisoned") = Some(tm);
    }

    /// Set the cross-channel outbound adapters map.
    ///
    /// Called by the monitor during startup to inject the outbound adapter map
    /// into each agent service after all channels have been initialized.
    pub fn set_outbounds(&self, outbounds: OutboundsMap) {
        *self.outbounds.lock().expect("outbounds poisoned") = Some(outbounds);
    }
}

#[async_trait]
impl AgentService for JycAgentService {
    async fn base_url(&self) -> Result<String> {
        // Not applicable for in-process agent
        Ok("in-process".to_string())
    }

    async fn process(
        &self,
        message: &InboundMessage,
        topic_name: &str,
        topic_path: &Path,
        message_dir: &str,
        _pending_rx: &mut mpsc::Receiver<QueueItem>,
        topic_cancel: CancellationToken,
    ) -> Result<AgentResult> {
        tracing::info!(
            topic = %topic_name,
            message_dir = %message_dir,
            "Processing message with in-process agent"
        );

        // 0. Snapshot the live agent config once for this request.
        //    Re-reads from the shared `ArcSwap` on every call, so any
        //    config reload (TUI `reload config`) takes effect immediately.
        let agent_cfg = self.agent_config();

        // 0b. Load topic-level (L3) `<topic>/.jyc/config.toml` once and
        //     share the result with both the [agent] model-resolution block
        //     below and `build_tool_registry` — avoids a duplicate disk read.
        let topic_cfg = jyc_types::load_topic_config(topic_path);

        // 1. Read mode override for this topic (used to select mode-specific model)
        let mode_override = jyc_core::session_state::read_mode_override(topic_path).await;

        // 1b. Model resolution priority:
        //     For plan/build mode:
        //       a) .jyc/<mode>-model-override (mode-specific runtime override)
        //       b) .jyc/model-override (legacy fallback, for migration)
        //       c) .jyc/config.toml [agent] (topic-level, L3)
        //       d) Pattern-level plan_model / build_model / model
        //       e) Config-level plan_model / build_model / model
        //     For default mode (no override):
        //       a) Pattern-level model
        //       b) Config-level model
        //     (Skip file overrides in default mode to avoid stale data from
        //      the old /model command which wrote model-override for all modes.)
        let file_override = {
            let mode_suffix = match mode_override.as_deref() {
                Some("plan") => "plan",
                _ => "build", // default = build mode
            };
            let mode_specific_path = topic_path
                .join(".jyc")
                .join(format!("{mode_suffix}-model-override"));
            let legacy_path = topic_path.join(".jyc").join("model-override");
            if mode_specific_path.exists() {
                tokio::fs::read_to_string(&mode_specific_path)
                    .await
                    .ok()
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
            } else if legacy_path.exists() {
                tokio::fs::read_to_string(&legacy_path)
                    .await
                    .ok()
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
            } else {
                None
            }
        };
        let pattern = message
            .matched_pattern
            .as_deref()
            .and_then(|name| self.patterns.iter().find(|p| p.name == name));
        // Mode resolution chain: .jyc/mode-override file > pattern.mode > default "build"
        let mode_override = mode_override.or_else(|| pattern.and_then(|p| p.mode.clone()));
        // Topic-level (L3) .jyc/config.toml — [agent] model overrides.
        // Priority: file overrides > topic config > pattern > config.
        // `topic_cfg` was loaded once at the top of `process` and shared with
        // `build_tool_registry` (avoids a duplicate disk read per message).
        let topic_agent_cfg = topic_cfg.as_ref().and_then(|c| c.ai.as_ref());
        let topic_cfg_override = topic_agent_cfg
            .as_ref()
            .and_then(|a| match mode_override.as_deref() {
                Some("plan") => a.plan_model.as_deref(),
                _ => a.build_model.as_deref(), // default = build
            })
            .or_else(|| topic_agent_cfg.as_ref().and_then(|a| a.model.as_deref()));
        // Pattern: try mode-specific field first, then generic model
        let pattern_override = pattern
            .and_then(|p| match mode_override.as_deref() {
                Some("plan") => p.plan_model.as_deref(),
                _ => p.build_model.as_deref(), // default = build
            })
            .or_else(|| pattern.and_then(|p| p.model.as_deref()));
        // Config: try mode-specific field first, then generic model
        let config_override = match mode_override.as_deref() {
            Some("plan") => agent_cfg.plan_model.as_deref(),
            _ => agent_cfg.build_model.as_deref(), // default = build
        }
        .or(agent_cfg.model.as_deref());
        let model_override = file_override
            .clone()
            .or_else(|| topic_cfg_override.map(|s| s.to_string()))
            .or_else(|| pattern_override.map(|s| s.to_string()))
            .or_else(|| config_override.map(|s| s.to_string()));

        // 2. Create provider
        let provider = self
            .create_provider(&agent_cfg, model_override.as_deref())
            .context("Failed to create LLM provider")?;

        tracing::info!(
            provider = %provider.name(),
            model = %provider.model(),
            "Using provider"
        );

        // 2b. Resolve small_model with priority:
        //     1. Topic-level .jyc/config.toml [agent] small_model (L3)
        //     2. Pattern-level small_model (from matched pattern config)
        //     3. Config-level small_model (from self.config.small_model, already
        //        channel-resolved or global fallback)
        //     Falls back to main model at call site if unset or construction fails.
        let pattern_small_model = message
            .matched_pattern
            .as_deref()
            .and_then(|name| self.patterns.iter().find(|p| p.name == name))
            .and_then(|p| p.small_model.as_deref());
        let small_model_resolved = topic_agent_cfg
            .as_ref()
            .and_then(|a| a.small_model.as_deref())
            .or(pattern_small_model)
            .or(agent_cfg.small_model.as_deref());

        // Resolve auto_reset_threshold: pattern-level > config-level > default 0.95
        let pattern_threshold = message
            .matched_pattern
            .as_deref()
            .and_then(|name| self.patterns.iter().find(|p| p.name == name))
            .and_then(|p| p.auto_reset_threshold);
        let auto_reset_threshold = pattern_threshold.unwrap_or(agent_cfg.auto_reset_threshold);
        let small_provider: Option<Box<dyn provider::Provider>> =
            small_model_resolved.and_then(|m| {
                match provider::create_provider(m, &agent_cfg.providers) {
                    Ok(p) => {
                        tracing::info!(
                            small_provider = %p.name(),
                            small_model = %p.model(),
                            "Using small model for ancillary LLM calls"
                        );
                        Some(p)
                    }
                    Err(e) => {
                        tracing::warn!(
                            error = %e,
                            small_model = m,
                            "Failed to construct small_model provider; falling back to main model"
                        );
                        None
                    }
                }
            });

        // 4. Build prompts (image-injection gated by per-pattern flag and
        //    per-model `supports_images`)
        // 3a. Build system prompt (available channels, skills, AGENTS.md, etc.)
        let system_prompt = self
            .build_system_prompt(topic_path, message.matched_pattern.as_deref())
            .await;
        tracing::debug!(
            topic = %topic_name,
            prompt_len = system_prompt.len(),
            has_plan_mode = system_prompt.contains("PLAN MODE"),
            "System prompt built"
        );
        tracing::trace!(
            topic = %topic_name,
            prompt = %system_prompt,
            "Full system prompt (enable RUST_LOG=trace to see)"
        );
        let current_mode = jyc_core::session_state::read_mode_override(topic_path).await;
        // Mode resolution chain: .jyc/mode-override file > pattern.mode > default "build"
        let current_mode = current_mode.or_else(|| pattern.and_then(|p| p.mode.clone()));
        let user_blocks =
            self.build_user_blocks(message, provider.supports_images(), current_mode.as_deref());

        // 5. Build tool registry (cached per (topic, config snapshot) so
        //    MCP subprocess spawn + handshake only runs when configs change).
        let tools_arc = self
            .get_or_build_tool_registry(
                topic_name,
                topic_path,
                topic_cfg.as_ref(),
                provider.supports_images(),
                message.matched_pattern.as_deref(),
            )
            .await;
        let tools = &*tools_arc;

        // 6. Get event bus for this topic
        let event_bus = self.get_event_bus(topic_name).await;

        // 6b. Determine per-pattern image injection flag for consistency
        // between `build_user_blocks` and the `read_image` tool's
        // vision-fallback decision.
        let pattern_inject = message
            .matched_pattern
            .as_deref()
            .and_then(|name| self.patterns.iter().find(|p| p.name == name))
            .map(|p| p.inject_inbound_images)
            .unwrap_or(false);

        // 7. Run agent loop
        // Resolve context_window: per-model override > provider default > fallback
        // (DEFAULT_CONTEXT_WINDOW lives in jyc-core::session_state).
        let model_str = model_override.as_deref().unwrap_or("");
        let context_window = if let Some((provider_name, model_id)) = model_str.split_once('/') {
            agent_cfg.providers.get(provider_name).and_then(|p| {
                // Check per-model override first, then provider default
                p.models
                    .get(model_id)
                    .and_then(|m| m.context_window)
                    .or(p.context_window)
            })
        } else {
            None
        }
        .or(Some(jyc_core::session_state::DEFAULT_CONTEXT_WINDOW));

        // Resolve billing rates for the same `provider/model` string that
        // `context_window` above resolves from. `None` when the model has no
        // configured pricing, which disables cost tracking for this round.
        // `providers` is untouched by `derive_agent_config` (it only overrides
        // model/small_model), so the global config is the right source.
        let pricing = jyc_types::pricing::lookup_pricing(&self.config.load(), model_str);
        // Same rates, in the shape the reset path needs so context-compression
        // calls land in the ledger too.
        let billing_ctx = pricing.as_ref().map(|p| session::BillingContext {
            pricing: p.clone(),
            model_label: model_str.to_string(),
        });

        // Resolve reset_compression using the matched pattern. This is the
        // single source of truth shared by manual `/reset`, this pre-loop
        // pre-check, and the post-loop auto-reset in `update_tokens`.
        let compression_config = jyc_core::session_state::resolve_reset_compression(
            &self.config.load(),
            &self.channel_name,
            message.matched_pattern.as_deref(),
        );

        // Resolve the context management strategy. Runtime override
        // (`.jyc/context-strategy.json` written by `/context`) wins over
        // configured defaults (matched pattern > first pattern > global
        // [ai] > full/window=10).
        let context_strategy = jyc_core::session_state::read_context_strategy_override(topic_path)
            .await
            .unwrap_or_else(|| {
                jyc_core::session_state::resolve_context_strategy(
                    &self.config.load(),
                    &self.channel_name,
                    message.matched_pattern.as_deref(),
                )
            });

        // Pre-loop pre-check: if the active model has a smaller context
        // window than the loaded session, reset the session BEFORE the
        // agent loop. Without this, the first LLM call rejects the
        // oversized context and the post-loop auto-reset never fires.
        // Uses the same `reset_compression` config as manual `/reset`.
        if let Some(cw) = context_window {
            let new_max = (cw as f64 * auto_reset_threshold) as u64;
            session::maybe_reset_for_new_context(
                topic_path,
                new_max,
                &compression_config,
                Some(provider.as_ref()),
                billing_ctx.as_ref(),
            )
            .await;
        }

        // Ensure the session file exists BEFORE loading context and running
        // the agent loop. Placed after the pre-loop pre-check because that
        // pre-check may have deleted the file via `reset_session`, and
        // `load_context` below returns empty when the session file is
        // missing — which would silently discard the compacted context the
        // pre-check just wrote. Without this, the dashboard and outbound
        // probes also see `(None, None, None)` for the window between "user
        // sends message" and "first LLM response arrives + persist_tokens
        // writes". The helper is a no-op when the file already exists, so
        // existing token data is preserved.
        session::ensure_session_file(topic_path, context_window, auto_reset_threshold).await;

        // Load session and prior raw context AFTER the pre-loop pre-check
        // above: if the pre-check just reset the session (e.g. the active
        // model switched to a smaller context window), this must read the
        // freshly compacted agent-context.json — not the stale pre-reset
        // contents, which would be sent verbatim on the first LLM call and
        // immediately re-inflate the wire context.
        let (prior_history, prior_raw_context) = session::load_context(topic_path).await;

        tracing::debug!(
            prior_messages = prior_history.len(),
            prior_raw_context = prior_raw_context.len(),
            "Loaded prior context"
        );

        let additional_read_roots = self.resolve_additional_read_roots(message, topic_path);
        let additional_write_roots = self.resolve_additional_write_roots(message);
        let topic_managers = self
            .topic_managers
            .lock()
            .expect("topic_managers poisoned")
            .clone();
        let outbounds = self.outbounds.lock().expect("outbounds poisoned").clone();
        let result = agent_loop::run(AgentLoopConfig {
            provider: provider.as_ref(),
            small_provider: small_provider
                .as_deref()
                .map(|p| p as &dyn provider::Provider),
            tools,
            system_prompt: &system_prompt,
            user_blocks,
            working_dir: topic_path,
            topic_path,
            cancel: topic_cancel,
            topic_name,
            event_bus: event_bus.as_ref(),
            prior_history,
            prior_raw_context,
            max_iterations: Some(agent_cfg.max_iterations),
            sse_read_timeout: std::time::Duration::from_secs(agent_cfg.sse_read_timeout_secs),
            additional_read_roots,
            additional_write_roots,
            pattern_inject_images: pattern_inject,
            outbound: self.outbound.clone(),
            topic_managers: topic_managers.clone(),
            current_channel: Some(self.channel_name.clone()),
            outbounds,
            context_window,
            auto_reset_threshold,
            thinking_enabled: read_thinking_enabled(topic_path),
            pricing,
            model_label: model_str,
            context_strategy,
            reply_target: Some(crate::tools::ReplyTarget {
                original: message.clone(),
                message_dir: message_dir.to_string(),
            }),
        })
        .await?;

        tracing::info!(
            reply_sent_by_tool = result.reply_sent_by_tool,
            text_len = result.text.len(),
            input_tokens = result.input_tokens,
            output_tokens = result.output_tokens,
            "Agent loop completed"
        );

        // 8. Save raw context (preserves provider-specific fields for round-tripping)
        session::save_raw_context(topic_path, &result.raw_context).await;

        // 9. Update session token tracking
        // Provider used for the between-message context-reset summary (when
        // input_tokens crosses the 95 % auto-reset threshold). Same fallback
        // rule as the cycle-boundary summary: small_model if configured,
        // else the main model.
        let summary_provider: &dyn provider::Provider = small_provider
            .as_deref()
            .map(|p| p as &dyn provider::Provider)
            .unwrap_or(provider.as_ref());
        session::update_tokens(
            topic_path,
            result.input_tokens,
            result.total_input_tokens,
            result.output_tokens,
            result.total_cache_hit_tokens,
            result.total_cache_creation_tokens,
            context_window,
            summary_provider,
            auto_reset_threshold,
            &compression_config,
            billing_ctx.as_ref(),
        )
        .await;

        // 9. Return result
        if result.reply_sent_by_tool {
            Ok(AgentResult {
                reply_sent_by_tool: true,
                reply_auto_delivered: result.reply_auto_delivered,
                reply_text: result.reply_text_from_tool,
            })
        } else {
            Ok(AgentResult {
                reply_sent_by_tool: false,
                reply_auto_delivered: false,
                reply_text: if result.text.is_empty() {
                    None
                } else {
                    Some(result.text)
                },
            })
        }
    }

    async fn set_topic_event_bus(&self, topic_name: &str, event_bus: Option<TopicEventBusRef>) {
        let mut buses = self.event_buses.lock().await;
        match event_bus {
            Some(bus) => {
                buses.insert(topic_name.to_string(), bus);
            }
            None => {
                buses.remove(topic_name);
            }
        }
    }

    async fn reset_session(
        &self,
        topic_path: &Path,
        topic_name: &str,
        config: &jyc_types::channel::ResetCompressionConfig,
    ) -> Result<()> {
        // Read the live agent config so reload of small_model / providers
        // takes effect without a server restart.
        let agent_cfg = self.agent_config();
        // Use the agent config's small_model as the compression provider if available
        let small_model = agent_cfg.small_model.as_deref();
        let provider: Option<Box<dyn provider::Provider>> =
            small_model.and_then(|m| provider::create_provider(m, &agent_cfg.providers).ok());

        // Resolve compression config: pattern (not available here) -> agent config
        let resolved_config = config.clone();

        // Bill the compression call against the topic's effective model so
        // a manual `/reset` still lands in the ledger. Resolved from the
        // configured model since no per-message override is in scope here.
        let billing_ctx = agent_cfg.model.as_deref().and_then(|m| {
            jyc_types::pricing::lookup_pricing(&self.config.load(), m).map(|p| {
                session::BillingContext {
                    pricing: p,
                    model_label: m.to_string(),
                }
            })
        });

        session::reset_session(
            topic_path,
            &resolved_config,
            provider.as_deref().map(|p| p as &dyn provider::Provider),
            billing_ctx.as_ref(),
        )
        .await;

        // Publish SessionStatus event for dashboard visibility
        let mode_str = match config.mode {
            jyc_types::channel::CompressionMode::None => "none",
            jyc_types::channel::CompressionMode::Heuristic => "heuristic",
            jyc_types::channel::CompressionMode::Llm => "llm",
        };
        let event_bus = self.get_event_bus(topic_name).await;
        if let Some(bus) = event_bus {
            let _ = bus
                .publish(jyc_core::topic_event::TopicEvent::SessionStatus {
                    topic_name: topic_name.to_string(),
                    status_type: "session_reset".to_string(),
                    attempt: None,
                    message: Some(format!("mode={mode_str}")),
                    timestamp: chrono::Utc::now(),
                })
                .await;
        }

        Ok(())
    }
}

/// Derive the effective [`jyc_types::AiConfig`] for a given channel from
/// the full [`jyc_types::AppConfig`].
///
/// Applies the channel-level overrides (`channels.<name>.model`,
/// `channels.<name>.small_model`) on top of the global `[agent]` settings.
/// All other fields — providers, agent flags, vision, reset compression —
/// pass through unchanged from `app.agent`.
///
/// Channel-level `[channels.<name>.agent]` overrides (a nested
/// `AiConfig` per channel) are intentionally **not** applied here; the
/// current top-level fields `model` and `small_model` are the only
/// per-channel knobs `JycAgentService` reads.
pub fn derive_agent_config(app: &jyc_types::AppConfig, channel_name: &str) -> jyc_types::AiConfig {
    let mut agent = app.ai.clone();
    if let Some(ch) = app.channels.get(channel_name) {
        if ch.model.is_some() {
            agent.model = ch.model.clone();
        }
        if ch.small_model.is_some() {
            agent.small_model = ch.small_model.clone();
        }
    }
    agent
}

#[cfg(test)]
mod mcp_load_tests;
mod prompt;
mod skills;
#[cfg(test)]
mod tests;
mod tools;

use prompt::read_thinking_enabled;

pub use skills::{format_skills_section, parse_skill_frontmatter};
