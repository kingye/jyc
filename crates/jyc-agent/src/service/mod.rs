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
use jyc_core::command::send_handler::{
    SendOutcome, SendRequest, SendSource, send_to_topic_with_target,
};
use jyc_core::topic_event_bus::TopicEventBusRef;
use jyc_core::topic_manager::TopicManagers;
use jyc_types::{ChannelPattern, InboundMessage, QueueItem};

use crate::agent_loop::{self, AgentLoopConfig};
use crate::provider;
use crate::session;
use crate::tools::OutboundsMap;
use crate::tools::registry::ToolRegistry;
use crate::vision::VisionClient;
use std::sync::Arc;

/// Metadata for a discovered skill. Re-exported from `jyc-types` so the
/// command layer and the inspect payload can name the same type.
pub use jyc_types::SkillMeta;

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
    /// JYC data_dir (for discovering global skills).
    data_dir: PathBuf,
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
    /// `(topic, config_snapshot_ptr, mcp_override_mtime)`. Bypasses the
    /// subprocess-spawn / HTTP-handshake cost on every inbound message
    /// when neither the config nor the `/mcp` toggle override has
    /// changed. A config swap (via `ArcSwap::store`) gives the new
    /// `Arc<AppConfig>` a fresh pointer, and a toggle rewrites
    /// `mcp-override.json` (new mtime), so either change invalidates the
    /// cache key automatically.
    registry_cache: Mutex<HashMap<(String, usize, u128), Arc<ToolRegistry>>>,
    /// Outbound adapter for proactive messaging tools (e.g. `jyc_send_message`).
    outbound: Option<Arc<dyn jyc_types::channel::OutboundAdapter>>,
    /// Channel-level skills whitelist.
    channel_skills: Option<Vec<String>>,
    /// Cross-channel topic managers keyed by channel name.
    /// Passed through to `AgentLoopConfig` so the `jyc_send_to_topic` tool
    /// can inject messages into topics in other channels.
    /// Uses `std::sync::Mutex` for interior mutability (set after construction
    /// via `set_topic_managers()` on an `Arc<Self>`).
    topic_managers: std::sync::Mutex<Option<Arc<TopicManagers>>>,
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
        data_dir: PathBuf,
        patterns: Vec<ChannelPattern>,
        global_inbound_attachments: Option<jyc_types::InboundAttachmentConfig>,
        vision_client: Option<Arc<VisionClient>>,
        outbound: Option<Arc<dyn jyc_types::channel::OutboundAdapter>>,
        channel_skills: Option<Vec<String>>,
        channel_name: String,
    ) -> Self {
        Self {
            config,
            event_buses: Mutex::new(HashMap::new()),
            data_dir,
            patterns,
            global_inbound_attachments,
            vision_client,
            outbound,
            channel_skills,
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
    pub fn set_topic_managers(&self, tm: Arc<TopicManagers>) {
        *self.topic_managers.lock().expect("topic_managers poisoned") = Some(tm);
    }

    /// Set the cross-channel outbound adapters map.
    ///
    /// Called by the monitor during startup to inject the outbound adapter map
    /// into each agent service after all channels have been initialized.
    pub fn set_outbounds(&self, outbounds: OutboundsMap) {
        *self.outbounds.lock().expect("outbounds poisoned") = Some(outbounds);
    }

    /// Answer the `require_reply` requests this turn owes.
    ///
    /// The requests are the ones the message this turn handled carried,
    /// plus the ones that arrived while the turn was running — the turn
    /// owes an answer to both.
    ///
    /// `outbound_targets` is what this turn sent to, as `(channel, topic)`
    /// pairs; `ending` says how the turn ended. Returns what was forwarded.
    async fn guard_and_forward(
        &self,
        message: &InboundMessage,
        topic_name: &str,
        topic_path: &Path,
        ending: reply_guard::TurnEnding<'_>,
        arrived: &[reply_guard::ReplyRequest],
        outbound_targets: &[(String, String)],
    ) -> Vec<SendOutcome> {
        let requests = reply_guard::reply_requests(&message.metadata, arrived);
        let managers = self
            .topic_managers
            .lock()
            .expect("topic_managers poisoned")
            .clone();
        self.forward_unanswered_replies(
            managers.as_ref(),
            &requests,
            topic_name,
            topic_path,
            ending,
            outbound_targets,
        )
        .await
    }

    /// Answer the `require_reply` requests this turn left unanswered.
    ///
    /// A request asked for an answer and the turn did not send one, so the
    /// turn's own last words go to the asking topic in its place. Without
    /// this the requester waits forever: `require_reply` is a request, not a
    /// promise, and nothing else checked that it was kept.
    ///
    /// A turn that already sent to an asking topic has answered it, and
    /// nothing is forwarded on top of that. What the turn sent is what tells
    /// the two apart.
    ///
    /// `outbound_targets` is what this turn sent to, as `(channel, topic)`
    /// pairs; `ending` says how the turn ended. Returns what was forwarded.
    async fn forward_unanswered_replies(
        &self,
        managers: Option<&Arc<TopicManagers>>,
        requests: &[reply_guard::ReplyRequest],
        topic_name: &str,
        topic_path: &Path,
        ending: reply_guard::TurnEnding<'_>,
        outbound_targets: &[(String, String)],
    ) -> Vec<SendOutcome> {
        let mut forwarded = Vec::new();
        for source in reply_guard::unanswered_requests(requests, outbound_targets) {
            let Some(managers) = managers else {
                tracing::warn!(
                    source_topic = %source.topic,
                    "No topic managers available to forward an unanswered reply request"
                );
                break;
            };
            let text = reply_guard::forward_message(&source, topic_name, ending);
            let from = SendSource {
                channel: Some(self.channel_name.as_str()),
                topic: Some(topic_name),
                working_dir: topic_path,
            };
            let req = SendRequest {
                agent: &source.topic,
                topic: Some(&source.topic),
                message: &text,
                attachments: &[],
                recipient: None,
                require_reply: false,
            };
            match send_to_topic_with_target(managers, from, req).await {
                Ok(outcome) => {
                    tracing::info!(
                        source_topic = %source.topic,
                        target_channel = %outcome.channel,
                        target_topic = %outcome.topic,
                        "Forwarded an unanswered reply request to the topic that asked for it"
                    );
                    forwarded.push(outcome);
                }
                Err(e) => tracing::warn!(
                    source_topic = %source.topic,
                    error = %e,
                    "Could not forward an unanswered reply request to the topic that asked for it"
                ),
            }
        }
        forwarded
    }
}

/// Mode resolution chain: `.jyc/mode-override` > topic config `mode` >
/// `pattern.mode` > default "build" downstream.
///
/// The topic-config layer sits here rather than in `.jyc/mode-override` on
/// purpose: that file means "the user chose this via `/mode`", while a
/// config-derived mode (a subagent type's `mode = "plan"`) has to stay in a
/// config layer to keep the two distinguishable (see
/// `session_state::resolve_effective_mode`).
fn resolve_turn_mode(
    file_mode: Option<String>,
    topic_mode: Option<String>,
    pattern_mode: Option<String>,
) -> Option<String> {
    file_mode.or(topic_mode).or(pattern_mode)
}

#[async_trait]
impl AgentService for JycAgentService {
    async fn base_url(&self) -> Result<String> {
        // Not applicable for in-process agent
        Ok("in-process".to_string())
    }

    fn available_skills(&self, topic_name: &str, topic_path: &Path) -> Vec<SkillMeta> {
        // Every discovered skill is invocable, whatever the topic's
        // allow-list says: `skills` decides what the *system prompt*
        // lists (context cost), not what a message may ask for. A skill
        // that is not listed is still reachable via `/skill:<name>`.
        self.discover_skills(topic_name, topic_path, None, None)
    }

    async fn process(
        &self,
        message: &InboundMessage,
        topic_name: &str,
        topic_path: &Path,
        message_dir: &str,
        pending_rx: &mut mpsc::Receiver<QueueItem>,
        topic_cancel: CancellationToken,
    ) -> Result<AgentResult> {
        tracing::info!(
            topic = %topic_name,
            message_dir = %message_dir,
            "Processing message with in-process agent"
        );

        // Marks for the turn setup: the same reason as the dispatch marks in
        // the worker — the steps between the calls below log nothing, so the
        // summary line at the end of the block is the only place the split
        // shows. Drop them once the breakdown is known.
        let t_turn = std::time::Instant::now();

        // 0. Snapshot the live agent config once for this request.
        //    Re-reads from the shared `ArcSwap` on every call, so any
        //    config reload (TUI `reload config`) takes effect immediately.
        let mut agent_cfg = self.agent_config();
        // Expand `{channel}`/`{topic}` placeholders in provider `params`
        // (e.g. OpenAI `prompt_cache_key`) so this session's requests get
        // their own cache-affinity bucket.
        provider::expand_params_placeholders(
            &mut agent_cfg.providers,
            &self.channel_name,
            topic_name,
        );

        // 0b. Load topic-level (L3) `<topic>/.jyc/config.toml` once and
        //     share the result with both the [agent] model-resolution block
        //     below and `build_tool_registry` — avoids a duplicate disk read.
        let topic_cfg = jyc_types::load_topic_config(topic_name, topic_path);

        // 1. Read mode override for this topic (used to select mode-specific model)
        let mode_override =
            jyc_core::session_state::read_mode_override(topic_name, topic_path).await;

        // 1b. The pattern the message matched — it carries the mode and the
        //     model this turn runs with.
        let pattern = message
            .matched_pattern
            .as_deref()
            .and_then(|name| self.patterns.iter().find(|p| p.name == name));
        // Mode resolution chain: .jyc/mode-override file > topic config > pattern.mode > default "build"
        let mode_override = resolve_turn_mode(
            mode_override,
            topic_cfg.as_ref().and_then(|c| c.mode.clone()),
            pattern.and_then(|p| p.mode.clone()),
        );
        // 1c. The model this turn runs. `jyc_core::session_state::resolve_model` is
        //     the single place that chain lives: the display source
        //     (`TopicManager::topic_display_state`) and the `/spawn` and spawn-tool
        //     triggers call the same function. `topic_cfg` was loaded once at the
        //     top of `process` (shared with `build_tool_registry`), so it is
        //     handed in rather than read a second time.
        let topic_agent_cfg = topic_cfg.as_ref().and_then(|c| c.ai.as_ref());
        let model_override = jyc_core::session_state::resolve_model(
            topic_name,
            topic_path,
            &self.config.load(),
            &self.channel_name,
            mode_override.as_deref(),
            message.matched_pattern.as_deref(),
            topic_agent_cfg,
        )
        .await;

        let t_before_provider = std::time::Instant::now();

        // 2. Create provider
        let provider = self
            .create_provider(&agent_cfg, model_override.as_deref())
            .context("Failed to create LLM provider")?;
        let t_provider = std::time::Instant::now();

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
        // A `small_model` may be a reference to an `[ai]` value like any other
        // model in the chain; only `tools.rs::create_provider` resolved it
        // before, and this site builds the provider directly, so without this
        // the literal text `${ai.models.review}` reached the provider as a model
        // id — a warning every turn instead of the model asked for.
        let small_model_resolved = small_model_resolved.and_then(|m| {
            match jyc_types::resolve_model_reference(m, &agent_cfg) {
                Ok(m) => Some(m),
                Err(err) => {
                    tracing::warn!(
                        error = %err,
                        small_model = m,
                        "small_model is a reference that cannot be resolved; using the main model"
                    );
                    None
                }
            }
        });

        let small_provider: Option<Box<dyn provider::Provider>> =
            small_model_resolved.and_then(|m| {
                match provider::create_provider(&m, &agent_cfg.providers) {
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

        let t_small = std::time::Instant::now();

        // 4. Build prompts (image-injection gated by per-pattern flag and
        //    per-model `supports_images`)
        // Resolve access roots first: the system prompt must describe exactly
        // the boundaries the tool layer enforces (same Vecs, single source).
        let additional_read_roots = self.resolve_additional_read_roots(message, topic_path);
        let additional_write_roots = self.resolve_additional_write_roots(message, topic_path);
        // 3a. Build system prompt (boundaries, skills, AGENTS.md, static
        //     cross-topic prose). Everything in it is unchanged by runtime
        //     churn on purpose — the boundary's grant lines excepted, since
        //     those track enforcement: providers that cache by prefix reuse the
        //     whole entry for as long as these bytes do not move.
        let system_prompt = self
            .build_system_prompt(
                topic_name,
                topic_path,
                message.matched_pattern.as_deref(),
                topic_cfg.as_ref(),
                &additional_read_roots,
                &additional_write_roots,
            )
            .await;
        // 3b. The volatile half — subagents and the channel/topic roster —
        //     rides the turn's user message instead of the system prompt, so
        //     a topic appearing or closing costs no cached prefix.
        let turn_reference = self.build_turn_reference(topic_name).await;
        let t_prompt = std::time::Instant::now();
        let current_mode = resolve_turn_mode(
            jyc_core::session_state::read_mode_override(topic_name, topic_path).await,
            topic_cfg.as_ref().and_then(|c| c.mode.clone()),
            pattern.and_then(|p| p.mode.clone()),
        );
        // `prompt_hash` is the tripwire for the byte-stability property above:
        // a hash that changes between turns of an otherwise idle topic means
        // something volatile crept back into the system prompt. Two movers
        // are legitimate and expected, and the reader has to exclude them by
        // context rather than by this line: a `/grant`, and a spawn/close
        // (the child's directory is an access grant, and the boundary section
        // must list exactly the roots the tool layer enforces).
        tracing::debug!(
            topic = %topic_name,
            prompt_len = system_prompt.len(),
            prompt_hash = prompt_hash(&system_prompt),
            reference_len = turn_reference.len(),
            mode = %current_mode.as_deref().unwrap_or("-"),
            "System prompt built"
        );
        tracing::trace!(
            topic = %topic_name,
            prompt = %system_prompt,
            "Full system prompt (enable RUST_LOG=trace to see)"
        );
        let user_blocks = self.build_user_blocks(
            message,
            provider.supports_images(),
            current_mode.as_deref(),
            &turn_reference,
        );

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
        let t_tools = std::time::Instant::now();

        tracing::info!(
            topic = %topic_name,
            // Spans: cfg_model_ms = topic config + mode override + model
            // resolution, provider_ms / small_ms = the two create_provider
            // calls, prompt_ms = build_user_blocks + system prompt,
            // tools_ms = tool registry (MCP spawn + handshake on a cache
            // miss), setup_ms = the whole turn setup so far.
            cfg_model_ms = t_before_provider.duration_since(t_turn).as_millis() as u64,
            provider_ms = t_provider.duration_since(t_before_provider).as_millis() as u64,
            small_ms = t_small.duration_since(t_provider).as_millis() as u64,
            small = small_provider.is_some(),
            prompt_ms = t_prompt.duration_since(t_small).as_millis() as u64,
            tools_ms = t_tools.duration_since(t_prompt).as_millis() as u64,
            setup_ms = t_tools.duration_since(t_turn).as_millis() as u64,
            "Turn setup"
        );

        // 6. Get event bus for this topic
        let event_bus = self.get_event_bus(topic_name).await;

        // 6b. Determine per-pattern image injection flag for consistency
        // between `build_user_blocks` and the `read_image` tool's
        // vision-fallback decision.
        let pattern_inject = message
            .matched_pattern
            .as_deref()
            .and_then(|name| self.patterns.iter().find(|p| p.name == name))
            .map(|p| p.inject_inbound_images())
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
        let billing_mode = jyc_types::pricing::lookup_billing_mode(&self.config.load(), model_str);

        // Resolve the context management strategy. Runtime override
        // (`.jyc/context-strategy.json` written by `/context`) wins over
        // configured defaults (matched pattern > first pattern > global
        // [ai] > full/window=10).
        let context_strategy =
            jyc_core::session_state::read_context_strategy_override(topic_name, topic_path)
                .await
                .unwrap_or_else(|| {
                    jyc_core::session_state::resolve_context_strategy(
                        &self.config.load(),
                        &self.channel_name,
                        message.matched_pattern.as_deref(),
                        topic_cfg.as_ref().and_then(|c| c.context_strategy.as_ref()),
                    )
                });

        // Ensure the session file exists BEFORE loading context and running
        // the agent loop. Without this, the dashboard and outbound probes
        // see no token data for the window between "user sends message" and
        // "first LLM response arrives + persist_tokens writes". The helper
        // is a no-op when the file already exists, so existing token data is
        // preserved.
        session::ensure_session_file(topic_name, topic_path, context_window).await;

        // Start this turn's counters: the dashboard's "latest turn"
        // figures must describe this turn, not the previous one. Placed
        // after `ensure_session_file` so the file (and a reset session
        // rebuilt by the pre-check) is already settled.
        session::begin_turn_counters(topic_name, topic_path).await;

        // Load session and prior raw context. The strategy says how much
        // of the journal it reads — the reader parses only that much.
        let (prior_history, prior_raw_context) = session::load_context(
            topic_name,
            topic_path,
            crate::agent_loop::context::prior_depth(&context_strategy),
        )
        .await;

        tracing::debug!(
            prior_messages = prior_history.len(),
            prior_raw_context = prior_raw_context.len(),
            "Loaded prior context"
        );

        let topic_managers = self
            .topic_managers
            .lock()
            .expect("topic_managers poisoned")
            .clone();
        let outbounds = self.outbounds.lock().expect("outbounds poisoned").clone();
        let result = match agent_loop::run(AgentLoopConfig {
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
            // The queue the worker hands live messages to: the loop folds
            // them into the running turn (see `live_injection`).
            injection_rx: Some(&mut *pending_rx),
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
            thinking_enabled: read_thinking_enabled(topic_name, topic_path),
            pricing,
            billing_mode,
            billing_dir: jyc_core::billing_log_store::BillingLogStore::billing_dir(),
            model_label: model_str,
            context_strategy,
            reply_target: Some(crate::tools::ReplyTarget {
                original: message.clone(),
                message_dir: message_dir.to_string(),
            }),
        })
        .await
        {
            Ok(result) => result,
            Err(e) => {
                // The turn died. Whoever asked for a reply has to hear
                // that, or it waits forever — a turn that had already
                // answered may still get a second, redundant notice. The
                // requests that arrived mid-turn died with the turn: the
                // loop hands them back with the error.
                let failure = e.error.to_string();
                self.guard_and_forward(
                    message,
                    topic_name,
                    topic_path,
                    reply_guard::TurnEnding::Failed(&failure),
                    &e.pending_replies,
                    &[],
                )
                .await;
                return Err(e.error);
            }
        };

        // The reply guard: a message asked for a reply, and this turn did
        // not send one. The requester hears the turn's last words — or the
        // fact that there were none — instead of silence. A request that
        // arrived while the turn was running counts too: the model saw it
        // in the conversation, so the turn owes it an answer as well.
        self.guard_and_forward(
            message,
            topic_name,
            topic_path,
            reply_guard::TurnEnding::Finished(&result.text),
            &result.pending_replies,
            &result.outbound_targets,
        )
        .await;

        tracing::info!(
            reply_delivered = result.reply_delivered,
            text_len = result.text.len(),
            input_tokens = result.input_tokens,
            output_tokens = result.output_tokens,
            "Agent loop completed"
        );

        // 8. Save raw context (preserves provider-specific fields for round-tripping).
        // The journal already holds the prior turns; only this turn's
        // messages are new, and the journal is append-only.
        let mut raw_context = result.raw_context;
        let current = raw_context.split_off(result.prior_len);
        session::save_raw_context(topic_name, topic_path, current).await;

        // 9. Update session token tracking. The loop already persisted
        // these figures after every LLM call; this is the turn's final
        // write, with no cost, duration, cache or output figure to add
        // (all were banked per call).
        session::persist_tokens(
            topic_name,
            topic_path,
            result.input_tokens,
            result.context_output_tokens,
            result.total_input_tokens,
            result.output_tokens,
            result.total_cache_hit_tokens,
            result.total_cache_creation_tokens,
            result.total_reasoning_tokens,
            context_window,
            0.0,
            0,
            0,
            0,
            0,
            0,
        )
        .await;

        // 9. Return result
        Ok(AgentResult {
            reply_delivered: result.reply_delivered,
            reply_text: if result.text.is_empty() {
                None
            } else {
                Some(result.text)
            },
        })
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

    async fn reset_session(&self, topic_path: &Path, topic_name: &str) -> Result<()> {
        session::reset_session(topic_name, topic_path).await;

        // Publish SessionStatus event for dashboard visibility
        let event_bus = self.get_event_bus(topic_name).await;
        if let Some(bus) = event_bus {
            let _ = bus
                .publish(jyc_core::topic_event::TopicEvent::SessionStatus {
                    topic_name: topic_name.to_string(),
                    status_type: "session_reset".to_string(),
                    attempt: None,
                    message: Some("session cleared".to_string()),
                    timestamp: chrono::Utc::now(),
                })
                .await;
        }

        Ok(())
    }
}

/// Derive the effective [`jyc_types::AiConfig`] for a channel — re-exported
/// from `jyc_types`, where it now lives because `jyc_core`'s model resolution
/// needs the same derivation.
pub use jyc_types::derive_agent_config;

/// Content hash of the system prompt, logged per turn so its byte-stability is
/// observable: a hash that moves between two turns of an idle topic means
/// volatile state leaked back into the prompt and is costing prefix-cache
/// hits. Not a security primitive and not stable across Rust releases —
/// compare values within one running instance only.
fn prompt_hash(prompt: &str) -> u64 {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut hasher = DefaultHasher::new();
    prompt.hash(&mut hasher);
    hasher.finish()
}

#[cfg(test)]
mod guard_tests;
#[cfg(test)]
mod mcp_load_tests;
pub(crate) mod prompt;
pub(crate) mod reply_guard;
mod skills;
#[cfg(test)]
mod tests;
mod tools;

use prompt::read_thinking_enabled;

pub use jyc_types::skill::format_skills_section;
pub use skills::parse_skill_frontmatter;
