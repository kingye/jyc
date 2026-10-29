use jyc_types::channel::ContextStrategyConfig;
use jyc_types::state_dir::resolve_state_dir;
use jyc_types::task::TaskList;
use jyc_types::{AppConfig, ContextRegionInfo};
use serde::Deserialize;
use std::path::Path;

/// Fallback context window when neither the per-model nor the per-provider
/// setting is configured. Mirrors the constant in `jyc-agent::service`.
pub const DEFAULT_CONTEXT_WINDOW: u64 = 128000;

/// Raw provider context store inside a topic's state dir: an append-only
/// journal, one message per line. This is the authoritative format;
/// `jyc-agent::session` appends a turn's messages at the end of the file
/// and tolerates a torn tail line.
pub const AGENT_CONTEXT_FILE: &str = "agent-context.jsonl";

/// Best-effort removal of the raw context journal. A missing file is
/// ignored; used by reset paths where absence means "no context".
pub async fn remove_raw_context_files(state_dir: &Path) {
    tokio::fs::remove_file(state_dir.join(AGENT_CONTEXT_FILE))
        .await
        .ok();
}

/// Read input tokens from the agent session state file.
/// Returns (current input tokens, context window).
pub async fn read_input_tokens(topic_name: &str, topic_path: &Path) -> (Option<u64>, Option<u64>) {
    let state = read_token_state(topic_name, topic_path).await;
    (state.context_input_tokens, state.context_window)
}

/// Read accumulated output tokens from the agent session state file.
/// Returns `None` when the file is missing, malformed, or the value is zero.
/// The session file already deserializes `total_output_tokens` — this just
/// surfaces it. Output tokens accumulate across LLM calls in a round.
pub async fn read_output_tokens(topic_name: &str, topic_path: &Path) -> Option<u64> {
    read_token_state(topic_name, topic_path)
        .await
        .total_output_tokens
}

/// Read accumulated total input tokens from the agent session state file.
/// Returns `None` when the file is missing, malformed, or the value is zero.
/// Distinct from `read_input_tokens` (which returns the current context
/// size); this is the running sum across all LLM calls in the session.
pub async fn read_total_input_tokens(topic_name: &str, topic_path: &Path) -> Option<u64> {
    read_token_state(topic_name, topic_path)
        .await
        .total_input_tokens
}

/// Read accumulated prompt-cache-hit tokens from the agent session state
/// file. Returns `None` when the file is missing, malformed, or the
/// accumulated value is zero. Mirrors `read_total_input_tokens`; zero
/// covers both "no calls yet" and "provider didn't surface cache hits".
pub async fn read_total_cache_hit_tokens(topic_name: &str, topic_path: &Path) -> Option<u64> {
    read_token_state(topic_name, topic_path)
        .await
        .total_cache_hit_tokens
}

/// Read accumulated prompt-cache-**creation** (write) tokens from the
/// agent session state file. Returns `None` when the file is missing,
/// malformed, the value is zero, or the field is absent (sessions
/// written before the field existed deserialize as `None` via
/// `serde(default)`).
///
/// Anthropic is the only provider that reports writes separately from
/// reads; for every other vendor this is always `None` unless the
/// caller actively fills it.
pub async fn read_total_cache_creation_tokens(topic_name: &str, topic_path: &Path) -> Option<u64> {
    read_token_state(topic_name, topic_path)
        .await
        .total_cache_creation_tokens
}

/// Read the accumulated cost of the current session.
///
/// Returns `None` when the file is missing, malformed, or the cost is
/// zero — zero covers both "no calls yet" and "model has no configured
/// pricing", and in either case there is nothing meaningful to show.
///
/// Kept separate from `read_token_state` rather than widening its
/// tuple: cost has a single caller, and References a sixth element
/// through four existing helpers would be a much larger change than
/// one extra read of an already page-cached file.
pub async fn read_session_cost(topic_name: &str, topic_path: &Path) -> Option<f64> {
    let agent_path = resolve_state_dir(topic_name, topic_path).join("agent-session.json");
    let content = tokio::fs::read_to_string(&agent_path).await.ok()?;
    let state = serde_json::from_str::<AgentSessionState>(&content).ok()?;
    (state.session_cost > 0.0).then_some(state.session_cost)
}

/// Call-latency figures for a topic: the session-wide average and the
/// latest turn's average, each as `(average_ms, call_count)`.
///
/// A pair is `None` when there is nothing to average in that scope — no
/// measured call yet (fresh session, a session file written before the
/// fields existed), or the turn counters were just cleared at the start
/// of a turn. Pairing the average with its count structurally keeps a
/// figure drawn from a single call from being rendered as a settled one,
/// and keeps the count guard (`count > 0`, which is what prevents an
/// average of `0ms`) attached to the figure it protects.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CallLatency {
    /// Average wall-clock time per LLM call across the session, with the
    /// number of calls it covers.
    pub session: Option<(u64, u64)>,
    /// Average wall-clock time per LLM call in the latest turn, with the
    /// number of calls it covers.
    pub turn: Option<(u64, u64)>,
}

/// Read the session-wide and latest-turn call latency from the topic's
/// session file.
///
/// Returns default (no figures) when the file is missing or malformed.
/// Kept separate from `read_token_state` rather than widening its tuple,
/// mirroring `read_session_cost`: one extra read of an already
/// page-cached file is cheaper than threading more elements through
/// every tuple consumer.
pub async fn read_call_latency(topic_name: &str, topic_path: &Path) -> CallLatency {
    let agent_path = resolve_state_dir(topic_name, topic_path).join("agent-session.json");
    let Ok(content) = tokio::fs::read_to_string(&agent_path).await else {
        return CallLatency::default();
    };
    let Ok(state) = serde_json::from_str::<AgentSessionState>(&content) else {
        return CallLatency::default();
    };
    // `count` alone is the guard: the total only ever grows alongside a
    // count bump, so a non-zero count implies an average of at least 1ms,
    // which is what keeps the row from ever reading `0ms/call`.
    let average = |total: u64, count: u64| (count > 0).then(|| (total / count, count));
    CallLatency {
        session: average(state.total_call_duration_ms, state.call_count),
        turn: average(state.turn_total_call_duration_ms, state.turn_call_count),
    }
}

/// The token figures of one agent session, read from
/// `.jyc/agent-session.json` in a single file read.
///
/// Every field is `None` when the file is missing, malformed, or the
/// underlying value is zero — a session that never made an LLM call has no
/// token figures at all.
#[derive(Debug, Clone, Default)]
pub struct TokenState {
    /// The input the most recent LLM call was sent — the context it
    /// carried.
    pub context_input_tokens: Option<u64>,
    /// The output tokens of that same call — the reply that gets appended
    /// to the context. Together with `context_input_tokens` this is the
    /// size of the context the next call will carry.
    pub context_output_tokens: Option<u64>,
    /// The model's context window — the whole budget the next call has to
    /// fit into.
    pub context_window: Option<u64>,
    /// Accumulated output tokens across the session.
    pub total_output_tokens: Option<u64>,
    /// Accumulated input tokens across the session.
    pub total_input_tokens: Option<u64>,
    /// Accumulated prompt-cache-hit tokens across the session.
    pub total_cache_hit_tokens: Option<u64>,
    /// Accumulated prompt-cache-creation tokens across the session.
    pub total_cache_creation_tokens: Option<u64>,
    /// The prompt-cache-hit tokens of the LLM calls in the latest turn —
    /// the turn-scoped twin of `total_cache_hit_tokens`.
    pub turn_cache_hit_tokens: Option<u64>,
    /// The input tokens of the LLM calls in the latest turn — the
    /// turn-scoped twin of `total_input_tokens`.
    pub turn_input_tokens: Option<u64>,
    /// The output tokens of the LLM calls in the latest turn — the
    /// turn-scoped twin of `total_output_tokens`.
    pub turn_output_tokens: Option<u64>,
    /// Accumulated reasoning (thinking) tokens across the session. Zero
    /// (hence `None`) both when no call has reported any and when the
    /// provider does not break reasoning tokens out.
    pub total_reasoning_tokens: Option<u64>,
    /// The reasoning tokens of the LLM calls in the latest turn — the
    /// turn-scoped twin of `total_reasoning_tokens`.
    pub turn_reasoning_tokens: Option<u64>,
    /// How the last completed call's wire messages split into context
    /// regions. Empty when no call has completed or the session file
    /// predates the field.
    pub context_composition: Vec<ContextRegionInfo>,
}

/// Read every token field of a session in a single file read.
///
/// Callers that need more than one field (e.g. `topic_manager::list_topics`)
/// should use this rather than calling the single-purpose helpers above
/// separately — saves one file open + JSON parse per call.
pub async fn read_token_state(topic_name: &str, topic_path: &Path) -> TokenState {
    let agent_path = resolve_state_dir(topic_name, topic_path).join("agent-session.json");
    let Ok(content) = tokio::fs::read_to_string(&agent_path).await else {
        return TokenState::default();
    };
    let Ok(state) = serde_json::from_str::<AgentSessionState>(&content) else {
        return TokenState::default();
    };
    TokenState {
        context_input_tokens: (state.context_input_tokens > 0)
            .then_some(state.context_input_tokens),
        context_output_tokens: (state.context_output_tokens > 0)
            .then_some(state.context_output_tokens),
        context_window: (state.context_window > 0).then_some(state.context_window),
        total_output_tokens: (state.total_output_tokens > 0).then_some(state.total_output_tokens),
        total_input_tokens: (state.total_input_tokens > 0).then_some(state.total_input_tokens),
        total_cache_hit_tokens: (state.total_cache_hit_tokens > 0)
            .then_some(state.total_cache_hit_tokens),
        total_cache_creation_tokens: (state.total_cache_creation_tokens > 0)
            .then_some(state.total_cache_creation_tokens),
        turn_cache_hit_tokens: (state.turn_cache_hit_tokens > 0)
            .then_some(state.turn_cache_hit_tokens),
        turn_input_tokens: (state.turn_input_tokens > 0).then_some(state.turn_input_tokens),
        turn_output_tokens: (state.turn_output_tokens > 0).then_some(state.turn_output_tokens),
        total_reasoning_tokens: (state.total_reasoning_tokens > 0)
            .then_some(state.total_reasoning_tokens),
        turn_reasoning_tokens: (state.turn_reasoning_tokens > 0)
            .then_some(state.turn_reasoning_tokens),
        context_composition: state.context_composition,
    }
}

/// Agent session state format.
#[derive(Debug, Deserialize)]
struct AgentSessionState {
    #[serde(default)]
    context_input_tokens: u64,
    #[serde(default)]
    context_output_tokens: u64,
    #[serde(default, alias = "max_input_tokens")]
    context_window: u64,
    #[serde(default)]
    context_composition: Vec<ContextRegionInfo>,
    #[serde(default)]
    total_input_tokens: u64,
    #[serde(default)]
    total_output_tokens: u64,
    #[serde(default)]
    total_cache_hit_tokens: u64,
    #[serde(default)]
    total_cache_creation_tokens: u64,
    #[serde(default)]
    total_reasoning_tokens: u64,
    #[serde(default)]
    turn_reasoning_tokens: u64,
    #[serde(default)]
    session_cost: f64,
    #[serde(default)]
    total_call_duration_ms: u64,
    #[serde(default)]
    call_count: u64,
    #[serde(default)]
    turn_total_call_duration_ms: u64,
    #[serde(default)]
    turn_call_count: u64,
    #[serde(default)]
    turn_cache_hit_tokens: u64,
    #[serde(default)]
    turn_input_tokens: u64,
    #[serde(default)]
    turn_output_tokens: u64,
}

/// Read the mode override file if it exists.
pub async fn read_mode_override(topic_name: &str, topic_path: &Path) -> Option<String> {
    let override_path = resolve_state_dir(topic_name, topic_path).join("mode-override");
    tokio::fs::read_to_string(override_path)
        .await
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Read the matched pattern file if it exists.
pub async fn read_pattern(topic_name: &str, topic_path: &Path) -> Option<String> {
    let pattern_path = resolve_state_dir(topic_name, topic_path).join("pattern");
    tokio::fs::read_to_string(pattern_path)
        .await
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// File name of the per-topic `/skill` toggle override.
pub const SKILL_OVERRIDE_FILE: &str = "skill-override.json";
/// File name of the per-topic `/mcp` toggle override.
pub const MCP_OVERRIDE_FILE: &str = "mcp-override.json";

/// Runtime name toggles written by the `/skill` and `/mcp` slash commands,
/// persisted in the topic `.jyc/` dir until an explicit `reset`.
///
/// `on` forces a name enabled (removes it from the effective disabled /
/// exclusion list, adds it to a skill whitelist); `off` forces it disabled
/// (appends to the effective exclusion list). A name must never appear in
/// both lists — [`ToggleOverride::set`] maintains that invariant.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ToggleOverride {
    #[serde(default)]
    pub on: Vec<String>,
    #[serde(default)]
    pub off: Vec<String>,
}

impl ToggleOverride {
    /// Whether the override carries no entries (file can be removed).
    pub fn is_empty(&self) -> bool {
        self.on.is_empty() && self.off.is_empty()
    }

    /// Record `name` as toggled on (`on=true`) or off (`on=false`),
    /// removing it from the opposite list.
    pub fn set(&mut self, name: &str, on: bool) {
        self.on.retain(|n| n != name);
        self.off.retain(|n| n != name);
        if on {
            self.on.push(name.to_string());
        } else {
            self.off.push(name.to_string());
        }
    }

    /// Apply to a set of disabled names: drop everything in `on`,
    /// add everything in `off`.
    pub fn apply(&self, disabled: &[String]) -> Vec<String> {
        let mut out: Vec<String> = disabled
            .iter()
            .filter(|d| !self.on.contains(d))
            .cloned()
            .collect();
        for name in &self.off {
            if !out.contains(name) {
                out.push(name.clone());
            }
        }
        out
    }
}

/// Read a toggle override file (`SKILL_OVERRIDE_FILE` / `MCP_OVERRIDE_FILE`).
pub async fn read_toggle_override(
    topic_name: &str,
    topic_path: &Path,
    file: &str,
) -> Option<ToggleOverride> {
    let path = resolve_state_dir(topic_name, topic_path).join(file);
    let content = tokio::fs::read_to_string(&path).await.ok()?;
    match serde_json::from_str(&content) {
        Ok(ovr) => Some(ovr),
        Err(e) => {
            tracing::warn!(
                path = %path.display(),
                error = %e,
                "Corrupt toggle override, ignoring"
            );
            None
        }
    }
}

/// Read the `/skill` override for a topic.
pub async fn read_skill_override(topic_name: &str, topic_path: &Path) -> Option<ToggleOverride> {
    read_toggle_override(topic_name, topic_path, SKILL_OVERRIDE_FILE).await
}

/// Read the `/mcp` override for a topic.
pub async fn read_mcp_override(topic_name: &str, topic_path: &Path) -> Option<ToggleOverride> {
    read_toggle_override(topic_name, topic_path, MCP_OVERRIDE_FILE).await
}

/// Write a toggle override; removes the file when empty so the absence of
/// the file always means "no override".
///
/// A closed topic writes nothing: a `/model`, `/mode`, `/plan`, `/skill` or
/// `/mcp` stacked behind a `/close` in the same message still runs (commands
/// are executed one after another), and recreating the state dir here would
/// leave a topic behind that nothing owns.
pub async fn write_toggle_override(
    topic_name: &str,
    topic_path: &Path,
    file: &str,
    ovr: &ToggleOverride,
) -> anyhow::Result<()> {
    if jyc_types::state_dir::is_closed(topic_name) {
        return Ok(());
    }
    let dir = resolve_state_dir(topic_name, topic_path);
    let path = dir.join(file);
    if ovr.is_empty() {
        tokio::fs::remove_file(&path).await.ok();
        return Ok(());
    }
    tokio::fs::create_dir_all(&dir).await?;
    tokio::fs::write(&path, serde_json::to_string(ovr)?).await?;
    Ok(())
}

/// Delete a toggle override file (explicit `reset`).
pub async fn clear_toggle_override(topic_name: &str, topic_path: &Path, file: &str) {
    tokio::fs::remove_file(resolve_state_dir(topic_name, topic_path).join(file))
        .await
        .ok();
}

/// File name of the agent-maintained task list (types in `jyc_types::task`).
///
/// The helpers below take the topic's `.jyc` dir (from
/// `jyc_types::state_dir::state_dir`) rather than `(topic_name, topic_path)`, so
/// the agent's tools — which only see `ToolContext::working_dir` — and the
/// topic-info readers share one implementation.
pub const TASKS_FILE: &str = "tasks.json";

/// Read a topic's task list. `None` when the file is missing, malformed, or
/// holds no items — an empty list has nothing to show, and the renderers omit
/// the section entirely ([`write_tasks_at`] keeps that invariant by deleting
/// instead of writing `[]`).
pub async fn read_tasks_at(state_dir: &Path) -> Option<TaskList> {
    let path = state_dir.join(TASKS_FILE);
    let content = tokio::fs::read_to_string(&path).await.ok()?;
    match serde_json::from_str::<TaskList>(&content) {
        Ok(tasks) if !tasks.is_empty() => Some(tasks),
        Ok(_) => None,
        Err(e) => {
            tracing::warn!(
                path = %path.display(),
                error = %e,
                "Corrupt task list, ignoring"
            );
            None
        }
    }
}

/// Write a topic's task list (creating the `.jyc` dir); deletes the file when
/// the list is empty so a missing file always means "no list".
///
/// A closed topic writes nothing — see [`write_toggle_override`]: a tool call
/// that lands after the turn closed its own topic must not recreate the dir.
pub async fn write_tasks_at(
    topic_name: &str,
    state_dir: &Path,
    tasks: &TaskList,
) -> anyhow::Result<()> {
    if jyc_types::state_dir::is_closed(topic_name) {
        return Ok(());
    }
    let path = state_dir.join(TASKS_FILE);
    if tasks.is_empty() {
        tokio::fs::remove_file(&path).await.ok();
        return Ok(());
    }
    tokio::fs::create_dir_all(state_dir).await?;
    tokio::fs::write(&path, serde_json::to_string(tasks)?).await?;
    Ok(())
}

/// Delete a topic's task list (`/reset`, `/new`).
pub async fn clear_tasks_at(state_dir: &Path) {
    tokio::fs::remove_file(state_dir.join(TASKS_FILE))
        .await
        .ok();
}

/// Resolve the effective mode for a topic:
/// `.jyc/mode-override` > topic config `mode` > pattern `mode` from channel
/// config (looked up via `.jyc/pattern`) > `None` (= build default).
///
/// Mirrors the runtime chain in `jyc-agent::service::process()` so that
/// non-runtime paths (dashboard topic list, `/model` command) agree with the
/// mode the agent actually runs in. The runtime states the same precedence once
/// more, as the pure `resolve_turn_mode`. Changing one means changing the
/// other; they are small enough that moving them together would cost the runtime
/// a fresh topic-config read per turn.
///
/// One difference that follows from where each side reads its pattern: this
/// looks it up in the *live* config, while the runtime's pattern layer comes
/// from `JycAgentService::patterns`, the channel's list as captured at
/// construction. A hot reload that changes a pattern's `mode` therefore moves
/// this answer and not the runtime's, until the service is rebuilt. (The model
/// layer no longer has that split: `resolve_model` reads the live config for
/// both.)
///
/// Config-derived mode is never persisted
/// to `.jyc/mode-override`; that file remains reserved for explicit user
/// overrides via `/mode`, which is why a subagent type's `mode = "plan"` is
/// read as a config layer here instead.
pub async fn resolve_effective_mode(
    topic_name: &str,
    topic_path: &Path,
    config: &AppConfig,
    channel_name: &str,
) -> Option<String> {
    if let Some(mode) = read_mode_override(topic_name, topic_path).await {
        return Some(mode);
    }
    if let Some(mode) = jyc_types::load_topic_config(topic_name, topic_path).and_then(|c| c.mode) {
        return Some(mode);
    }
    let pattern_name = read_pattern(topic_name, topic_path).await?;
    config
        .channels
        .get(channel_name)?
        .patterns
        .as_ref()?
        .iter()
        .find(|p| p.name == pattern_name)
        .and_then(|p| p.mode.clone())
}

/// Resolve the context window of the model currently active on this topic.
///
/// Returns `None` if no model can be resolved (no config, no providers, etc.).
/// Callers should treat `None` as a no-op.
pub async fn resolve_active_context_window(
    topic_name: &str,
    topic_path: &Path,
    config: &AppConfig,
    channel: &str,
) -> Option<u64> {
    let model_override = resolve_effective_model(topic_name, topic_path, config, channel).await?;
    context_window_for_model(&model_override, config)
}

/// Idempotently write `context_window` to `.jyc/agent-session.json`.
/// No-op if the value would not change (avoids disk churn).
///
/// The legacy `max_input_tokens` key is dropped in the same write: it held
/// the auto-reset cap and is an alias of `context_window` on the read side,
/// so leaving both in the file would make it fail to deserialize. The key
/// goes even when the window itself does not change — otherwise a file
/// carrying both keys would keep failing to deserialize forever.
///
/// A closed topic is skipped: its state dir is gone on purpose, and
/// recreating it here would leave a topic behind that nothing owns.
pub async fn write_context_window(topic_name: &str, topic_path: &Path, new_window: u64) {
    if jyc_types::state_dir::is_closed(topic_name) {
        return;
    }
    let session_path = resolve_state_dir(topic_name, topic_path).join("agent-session.json");
    let content = tokio::fs::read_to_string(&session_path)
        .await
        .unwrap_or_default();
    let mut state: serde_json::Map<String, serde_json::Value> =
        serde_json::from_str(&content).unwrap_or_default();
    let current = state
        .get("context_window")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let legacy_present = state.remove("max_input_tokens").is_some();
    if current == new_window && !legacy_present {
        return;
    }
    state.insert("context_window".to_string(), serde_json::json!(new_window));
    if let Some(parent) = session_path.parent() {
        tokio::fs::create_dir_all(parent).await.ok();
    }
    if let Ok(json) = serde_json::to_string_pretty(&state) {
        tokio::fs::write(&session_path, json).await.ok();
    }
}

/// The model a topic runs in `mode` — the single place this chain lives.
///
/// Every step is keyed by `mode` (the value [`resolve_effective_mode`]
/// resolved): `.jyc/<mode>-model-override` > the legacy `.jyc/model-override`
/// (migration) > the topic's `.jyc/config.toml` `[agent]` > the pattern the
/// message matched > the channel-resolved global `[agent]`.
///
/// Consumers: the agent runtime (the model it builds the provider with), the
/// display source ([`crate::topic_manager::TopicManager::topic_display_state`])
/// and everything that renders it — dashboard, Feishu status card, `/info` —
/// the context-window resolver below, and `/spawn`, which hands a fresh child
/// the model its parent is running ([`seed_start_model`] in
/// `command::spawn_common`).
///
/// `pattern_name` is the pattern that applies to this turn — the runtime passes
/// what the message matched, so `None` there means no pattern layer at all
/// (injected turns have none, and the pre-unification runtime consulted no
/// other source for them); callers that mean "the pattern this
/// topic runs" read `.jyc/pattern` themselves — [`resolve_effective_model`] does
/// it for the spawn triggers, the display passes its own `topic_pattern`. There
/// is deliberately no fallback to the channel's first pattern: the runtime has no
/// such layer. `topic_agent_cfg` lets a caller that already parsed
/// `.jyc/config.toml` hand the `[agent]` table in instead of paying for a second
/// read.
///
/// Whatever layer the value comes from may be a reference to the global `[ai]`
/// value it would otherwise repeat (`"${ai.small_model}"` — see
/// [`jyc_types::resolve_model_reference`]); it is resolved last, here. A
/// reference that cannot be resolved is reported at error level and treated as
/// no override, rather than surviving as a model id that fails much later.
///
/// [`seed_start_model`]: crate::command::spawn_common
pub async fn resolve_model(
    topic_name: &str,
    topic_path: &Path,
    config: &AppConfig,
    channel: &str,
    mode: Option<&str>,
    pattern_name: Option<&str>,
    topic_agent_cfg: Option<&jyc_types::TopicAiConfig>,
) -> Option<String> {
    let state_dir = resolve_state_dir(topic_name, topic_path);
    let plan = matches!(mode, Some("plan"));

    // 1. Mode-specific file override, then the legacy fallback.
    let file_override = {
        let mode_specific = state_dir.join(format!(
            "{}-model-override",
            if plan { "plan" } else { "build" }
        ));
        let path = if mode_specific.exists() {
            Some(mode_specific)
        } else {
            let legacy = state_dir.join("model-override");
            legacy.exists().then_some(legacy)
        };
        match path {
            Some(p) => tokio::fs::read_to_string(&p)
                .await
                .ok()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty()),
            None => None,
        }
    };

    // 2. Topic config (`.jyc/config.toml` `[agent]`), unless handed in.
    let loaded;
    let topic_agent_cfg = match topic_agent_cfg {
        Some(cfg) => Some(cfg),
        None => {
            loaded = jyc_types::load_topic_config(topic_name, topic_path).and_then(|c| c.ai);
            loaded.as_ref()
        }
    };
    let topic_cfg_override = topic_agent_cfg
        .and_then(|a| {
            if plan {
                a.plan_model.clone()
            } else {
                a.build_model.clone()
            }
        })
        .or_else(|| topic_agent_cfg.and_then(|a| a.model.clone()));

    // 3. The pattern the caller says applies to this turn (`None`: none does).
    let pattern_override = {
        let patterns = config
            .channels
            .get(channel)
            .and_then(|c| c.patterns.as_ref());
        let pat =
            pattern_name.and_then(|name| patterns.and_then(|p| p.iter().find(|p| p.name == name)));
        pat.and_then(|p| {
            if plan {
                p.plan_model.clone()
            } else {
                p.build_model.clone()
            }
        })
        .or_else(|| pat.and_then(|p| p.model.clone()))
    };

    // 4. Channel-resolved global `[agent]` — the same derivation the runtime
    //    builds its provider config from.
    let effective_agent = jyc_types::derive_agent_config(config, channel);
    let config_override = if plan {
        effective_agent.plan_model.clone()
    } else {
        effective_agent.build_model.clone()
    }
    .or(effective_agent.model);

    // A model may be written as a reference to the global `[ai]` value it would
    // otherwise repeat (`"${ai.small_model}"`), wherever in the chain it sits —
    // so the value the chain picked is resolved last, in this one place both the
    // runtime and the display route through.
    let resolved = file_override
        .or(topic_cfg_override)
        .or(pattern_override)
        .or(config_override);
    match resolved {
        Some(value) => {
            // The two names mean the `[ai]` values in effect for this topic's
            // channel — the same base `create_provider` resolves its fallback
            // against, so the same text cannot mean two different ids.
            let ai = jyc_types::derive_agent_config(config, channel);
            match jyc_types::resolve_model_reference(&value, &ai) {
                Ok(model) => Some(model),
                Err(err) => {
                    // Loud, then no override — the stance the runtime already
                    // takes for a model id it cannot build a provider from.
                    tracing::error!(topic = %topic_name, "{err}");
                    None
                }
            }
        }
        None => None,
    }
}

/// The model this topic runs right now: the effective mode, then
/// [`resolve_model`] for it. Used where a caller wants "the current model"
/// rather than a particular mode's: the two spawn triggers and the
/// context-window resolver.
pub async fn resolve_effective_model(
    topic_name: &str,
    topic_path: &Path,
    config: &AppConfig,
    channel: &str,
) -> Option<String> {
    let mode = resolve_effective_mode(topic_name, topic_path, config, channel).await;
    // These callers have no matched pattern in hand: they mean the one this
    // topic runs, which `.jyc/pattern` records (the worker writes it on every
    // processed message).
    let pattern = read_pattern(topic_name, topic_path).await;
    resolve_model(
        topic_name,
        topic_path,
        config,
        channel,
        mode.as_deref(),
        pattern.as_deref(),
        None,
    )
    .await
}

/// Resolve the `context_window` for a model string (`<provider>/<model-id>`).
/// Per-model override > provider default > `DEFAULT_CONTEXT_WINDOW`.
fn context_window_for_model(model_str: &str, config: &AppConfig) -> Option<u64> {
    if let Some((provider_name, model_id)) = model_str.split_once('/')
        && let Some(provider) = config.ai.providers.get(provider_name)
    {
        let per_model = provider.models.get(model_id).and_then(|m| m.context_window);
        if let Some(cw) = per_model.or(provider.context_window) {
            return Some(cw);
        }
    }
    Some(DEFAULT_CONTEXT_WINDOW)
}

/// File name for the runtime context-strategy override written by the
/// `/context` command. Persisted as JSON (mode + optional window).
pub const CONTEXT_STRATEGY_FILE: &str = "context-strategy.json";

/// File names for the wire-payload debug dump toggled by
/// `/context dump on|off`. The flag is a tiny JSON object
/// (`{"enabled":true}`); the dump itself is a JSONL file capped at
/// `WIRE_PAYLOAD_DUMP_MAX_LINES` lines (oldest dropped).
///
/// Shared between `jyc-core` (the command handler) and `jyc-agent`
/// (the dump appender) — keeping them in one place prevents silent
/// drift between the writer and the reader.
pub const WIRE_PAYLOAD_DUMP_FLAG_FILE: &str = "wire-payload-dump.json";
pub const WIRE_PAYLOAD_DUMP_FILE: &str = "wire-payload.jsonl";
pub const WIRE_PAYLOAD_DUMP_MAX_LINES: usize = 50;

/// Read the runtime context-strategy override if it exists.
///
/// Returns `None` when the file is missing, malformed, or `window == 0`.
/// A zero-window prior context would degrade to a single fallback user
/// message (see `extract_user_assistant_pairs`), which is almost
/// certainly not what the user wants; we silently fall back to the
/// configured default rather than persist a broken strategy.
pub async fn read_context_strategy_override(
    topic_name: &str,
    topic_path: &Path,
) -> Option<ContextStrategyConfig> {
    let path = resolve_state_dir(topic_name, topic_path).join(CONTEXT_STRATEGY_FILE);
    let content = tokio::fs::read_to_string(&path).await.ok()?;
    let cfg: ContextStrategyConfig = serde_json::from_str(&content).ok()?;
    if cfg.window == 0 {
        return None;
    }
    Some(cfg)
}

/// Resolve the `ContextStrategyConfig` for a topic from config alone.
///
/// Priority: the topic's own config (`[context_strategy]`, whether the topic
/// wrote it or a subagent type expanded into it) > matched pattern > first
/// pattern > global `[ai].context_strategy` >
/// `ContextStrategyConfig::default()` (sliding_window / window=10 /
/// extend_rate=0.5).
///
/// The topic layer is a *config* layer, not an override: callers must consult
/// the runtime `.jyc/context-strategy.json` (the user's `/context`) *before*
/// this function, and what they find there wins — the same shape as `mode`.
///
/// Pass `matched_pattern = Some(name)` when the caller has access to the
/// pattern that created the topic (e.g. `process()` with
/// `message.matched_pattern`). Pass `None` when the caller doesn't (e.g.
/// `/context` command handler without topic context) — the first pattern
/// on the channel is used as the best available signal.
pub fn resolve_context_strategy(
    config: &AppConfig,
    channel: &str,
    matched_pattern: Option<&str>,
    topic_strategy: Option<&ContextStrategyConfig>,
) -> ContextStrategyConfig {
    // Hand the topic's `[context_strategy]` in rather than reading the topic's
    // config here: the callers already parse it (`resolve_model` takes its `[ai]`
    // table the same way), a second per-turn read buys nothing, and this keeps
    // the function clear of `load_topic_config`'s "registered topic or panic"
    // contract — an unregistered name must not be a way to abort a turn.
    if let Some(from_topic) = topic_strategy {
        return from_topic.clone();
    }

    let channel_cfg = config.channels.get(channel);
    let patterns = channel_cfg.and_then(|c| c.patterns.as_ref());

    let from_matched = matched_pattern
        .and_then(|name| patterns.and_then(|p| p.iter().find(|p| p.name == name)))
        .and_then(|p| p.context_strategy.clone());

    let from_first = from_matched.or_else(|| {
        patterns
            .and_then(|p| p.first())
            .and_then(|p| p.context_strategy.clone())
    });

    from_first
        .or_else(|| config.ai.context_strategy.clone())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use jyc_types::task::{TaskItem, TaskStatus};

    #[tokio::test]
    async fn read_input_tokens_from_file() {
        let tmp = tempfile::tempdir().unwrap();
        let topic = "read_input_tokens_from_file";
        jyc_types::state_dir::register(topic, &tmp.path().join(".jyc"));
        let state_dir = tmp.path().join(".jyc");
        tokio::fs::create_dir_all(&state_dir).await.unwrap();
        tokio::fs::write(
            state_dir.join("agent-session.json"),
            r#"{"context_input_tokens": 1000, "max_input_tokens": 2000}"#,
        )
        .await
        .unwrap();
        let (current, max) = read_input_tokens(topic, tmp.path()).await;
        assert_eq!(current, Some(1000));
        assert_eq!(max, Some(2000));
    }

    #[tokio::test]
    async fn read_input_tokens_no_file() {
        let tmp = tempfile::tempdir().unwrap();
        let topic = "read_input_tokens_no_file";
        jyc_types::state_dir::register(topic, &tmp.path().join(".jyc"));
        let (current, max) = read_input_tokens(topic, tmp.path()).await;
        assert_eq!(current, None);
        assert_eq!(max, None);
    }

    #[tokio::test]
    async fn read_input_tokens_zero_values() {
        let tmp = tempfile::tempdir().unwrap();
        let topic = "read_input_tokens_zero_values";
        jyc_types::state_dir::register(topic, &tmp.path().join(".jyc"));
        let state_dir = tmp.path().join(".jyc");
        tokio::fs::create_dir_all(&state_dir).await.unwrap();
        tokio::fs::write(
            state_dir.join("agent-session.json"),
            r#"{"context_input_tokens": 0, "max_input_tokens": 0}"#,
        )
        .await
        .unwrap();
        let (current, max) = read_input_tokens(topic, tmp.path()).await;
        assert_eq!(current, None);
        assert_eq!(max, None);
    }

    #[tokio::test]
    async fn read_input_tokens_invalid_json() {
        let tmp = tempfile::tempdir().unwrap();
        let topic = "read_input_tokens_invalid_json";
        jyc_types::state_dir::register(topic, &tmp.path().join(".jyc"));
        let state_dir = tmp.path().join(".jyc");
        tokio::fs::create_dir_all(&state_dir).await.unwrap();
        tokio::fs::write(state_dir.join("agent-session.json"), "not json")
            .await
            .unwrap();
        let (current, max) = read_input_tokens(topic, tmp.path()).await;
        assert_eq!(current, None);
        assert_eq!(max, None);
    }

    #[tokio::test]
    async fn read_output_tokens_from_file() {
        let tmp = tempfile::tempdir().unwrap();
        let topic = "read_output_tokens_from_file";
        jyc_types::state_dir::register(topic, &tmp.path().join(".jyc"));
        let state_dir = tmp.path().join(".jyc");
        tokio::fs::create_dir_all(&state_dir).await.unwrap();
        tokio::fs::write(
            state_dir.join("agent-session.json"),
            r#"{"context_input_tokens":1000,"total_output_tokens":250,"max_input_tokens":2000}"#,
        )
        .await
        .unwrap();
        assert_eq!(read_output_tokens(topic, tmp.path()).await, Some(250));
    }

    #[tokio::test]
    async fn read_output_tokens_no_file() {
        let tmp = tempfile::tempdir().unwrap();
        let topic = "read_output_tokens_no_file";
        jyc_types::state_dir::register(topic, &tmp.path().join(".jyc"));
        assert_eq!(read_output_tokens(topic, tmp.path()).await, None);
    }

    #[tokio::test]
    async fn read_output_tokens_zero_value() {
        let tmp = tempfile::tempdir().unwrap();
        let topic = "read_output_tokens_zero_value";
        jyc_types::state_dir::register(topic, &tmp.path().join(".jyc"));
        let state_dir = tmp.path().join(".jyc");
        tokio::fs::create_dir_all(&state_dir).await.unwrap();
        tokio::fs::write(
            state_dir.join("agent-session.json"),
            r#"{"total_output_tokens":0}"#,
        )
        .await
        .unwrap();
        assert_eq!(read_output_tokens(topic, tmp.path()).await, None);
    }

    #[tokio::test]
    async fn read_total_input_tokens_from_file() {
        let tmp = tempfile::tempdir().unwrap();
        let topic = "read_total_input_tokens_from_file";
        jyc_types::state_dir::register(topic, &tmp.path().join(".jyc"));
        let state_dir = tmp.path().join(".jyc");
        tokio::fs::create_dir_all(&state_dir).await.unwrap();
        tokio::fs::write(
            state_dir.join("agent-session.json"),
            r#"{"context_input_tokens":1500,"total_input_tokens":4800,"total_output_tokens":250,"max_input_tokens":10000}"#,
        )
        .await
        .unwrap();
        assert_eq!(read_total_input_tokens(topic, tmp.path()).await, Some(4800));
    }

    #[tokio::test]
    async fn read_total_input_tokens_missing_field_returns_none() {
        // Old session files (pre-#490 second commit) don't have
        // total_input_tokens; serde(default) on the field makes it 0,
        // and the helper converts 0 to None.
        let tmp = tempfile::tempdir().unwrap();
        let topic = "read_total_input_tokens_missing_field_returns_none";
        jyc_types::state_dir::register(topic, &tmp.path().join(".jyc"));
        let state_dir = tmp.path().join(".jyc");
        tokio::fs::create_dir_all(&state_dir).await.unwrap();
        tokio::fs::write(
            state_dir.join("agent-session.json"),
            r#"{"context_input_tokens":1000,"max_input_tokens":1000}"#,
        )
        .await
        .unwrap();
        assert_eq!(read_total_input_tokens(topic, tmp.path()).await, None);
    }

    #[tokio::test]
    async fn read_token_state_reads_every_field() {
        let tmp = tempfile::tempdir().unwrap();
        let topic = "read_token_state_reads_every_field";
        jyc_types::state_dir::register(topic, &tmp.path().join(".jyc"));
        let state_dir = tmp.path().join(".jyc");
        tokio::fs::create_dir_all(&state_dir).await.unwrap();
        tokio::fs::write(
            state_dir.join("agent-session.json"),
            r#"{"context_input_tokens":1500,"context_output_tokens":250,"total_output_tokens":400,"total_input_tokens":9000,"total_cache_hit_tokens":3500,"context_window":10000}"#,
        )
        .await
        .unwrap();
        let state = read_token_state(topic, tmp.path()).await;
        assert_eq!(state.context_input_tokens, Some(1500));
        assert_eq!(state.context_output_tokens, Some(250));
        assert_eq!(state.context_window, Some(10000));
        assert_eq!(state.total_output_tokens, Some(400));
        assert_eq!(state.total_input_tokens, Some(9000));
        assert_eq!(state.total_cache_hit_tokens, Some(3500));
        assert_eq!(state.total_cache_creation_tokens, None);
    }

    #[tokio::test]
    async fn read_token_state_legacy_file_missing_cache_hit() {
        // Pre-#493 session files don't carry `total_cache_hit_tokens`;
        // `#[serde(default)]` fills it with 0, which the helper maps to
        // `None` like the other zero-valued fields.
        let tmp = tempfile::tempdir().unwrap();
        let topic = "read_token_state_legacy_file_missing_cache_hit";
        jyc_types::state_dir::register(topic, &tmp.path().join(".jyc"));
        let state_dir = tmp.path().join(".jyc");
        tokio::fs::create_dir_all(&state_dir).await.unwrap();
        tokio::fs::write(
            state_dir.join("agent-session.json"),
            r#"{"context_input_tokens":1500,"total_output_tokens":400,"max_input_tokens":10000}"#,
        )
        .await
        .unwrap();
        let state = read_token_state(topic, tmp.path()).await;
        assert_eq!(state.context_input_tokens, Some(1500));
        // The old file has no per-call output tokens.
        assert_eq!(state.context_output_tokens, None);
        // `max_input_tokens` is the old name of `context_window` — read
        // through the alias.
        assert_eq!(state.context_window, Some(10000));
        assert_eq!(state.total_output_tokens, Some(400));
        assert_eq!(state.total_input_tokens, None);
        assert_eq!(state.total_cache_hit_tokens, None);
    }

    #[tokio::test]
    async fn read_token_state_no_file() {
        let tmp = tempfile::tempdir().unwrap();
        let topic = "read_token_state_no_file";
        jyc_types::state_dir::register(topic, &tmp.path().join(".jyc"));
        let state = read_token_state(topic, tmp.path()).await;
        assert_eq!(state.context_input_tokens, None);
        assert_eq!(state.context_output_tokens, None);
        assert_eq!(state.context_window, None);
        assert_eq!(state.total_output_tokens, None);
        assert_eq!(state.total_input_tokens, None);
        assert_eq!(state.total_cache_hit_tokens, None);
        assert_eq!(state.total_cache_creation_tokens, None);
    }

    #[tokio::test]
    async fn read_total_cache_hit_tokens_from_file() {
        let tmp = tempfile::tempdir().unwrap();
        let topic = "read_total_cache_hit_tokens_from_file";
        jyc_types::state_dir::register(topic, &tmp.path().join(".jyc"));
        let state_dir = tmp.path().join(".jyc");
        tokio::fs::create_dir_all(&state_dir).await.unwrap();
        tokio::fs::write(
            state_dir.join("agent-session.json"),
            r#"{"context_input_tokens":1500,"total_input_tokens":9000,"total_output_tokens":250,"total_cache_hit_tokens":4200,"max_input_tokens":10000}"#,
        )
        .await
        .unwrap();
        assert_eq!(
            read_total_cache_hit_tokens(topic, tmp.path()).await,
            Some(4200)
        );
    }

    #[tokio::test]
    async fn read_total_cache_hit_tokens_zero_value_returns_none() {
        // Zero is treated identically to "missing" — providers that don't
        // surface cache hits will leave this at 0 forever; the helper
        // surfaces None so the dashboard's `if let Some(...)` idiom
        // hides the row entirely.
        let tmp = tempfile::tempdir().unwrap();
        let topic = "read_total_cache_hit_tokens_zero_value_returns_none";
        jyc_types::state_dir::register(topic, &tmp.path().join(".jyc"));
        let state_dir = tmp.path().join(".jyc");
        tokio::fs::create_dir_all(&state_dir).await.unwrap();
        tokio::fs::write(
            state_dir.join("agent-session.json"),
            r#"{"total_cache_hit_tokens":0}"#,
        )
        .await
        .unwrap();
        assert_eq!(read_total_cache_hit_tokens(topic, tmp.path()).await, None);
    }

    #[tokio::test]
    async fn read_total_cache_hit_tokens_missing_field_returns_none() {
        let tmp = tempfile::tempdir().unwrap();
        let topic = "read_total_cache_hit_tokens_missing_field_returns_none";
        jyc_types::state_dir::register(topic, &tmp.path().join(".jyc"));
        let state_dir = tmp.path().join(".jyc");
        tokio::fs::create_dir_all(&state_dir).await.unwrap();
        tokio::fs::write(
            state_dir.join("agent-session.json"),
            r#"{"context_input_tokens":1000,"max_input_tokens":1000}"#,
        )
        .await
        .unwrap();
        assert_eq!(read_total_cache_hit_tokens(topic, tmp.path()).await, None);
    }

    #[tokio::test]
    async fn read_mode_override_existing() {
        let tmp = tempfile::tempdir().unwrap();
        let topic = "read_mode_override_existing";
        jyc_types::state_dir::register(topic, &tmp.path().join(".jyc"));
        let state_dir = tmp.path().join(".jyc");
        tokio::fs::create_dir_all(&state_dir).await.unwrap();
        tokio::fs::write(state_dir.join("mode-override"), "static\n")
            .await
            .unwrap();
        let result = read_mode_override(topic, tmp.path()).await;
        assert_eq!(result, Some("static".to_string()));
    }

    #[tokio::test]
    async fn read_mode_override_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let topic = "read_mode_override_missing";
        jyc_types::state_dir::register(topic, &tmp.path().join(".jyc"));
        let result = read_mode_override(topic, tmp.path()).await;
        assert_eq!(result, None);
    }

    // ── resolve_effective_mode ──────────────────────────────────────

    fn config_with_pattern_mode(pattern_name: &str, mode: Option<&str>) -> AppConfig {
        let mut toml = String::from(
            r#"
[general]
[channels.c]
type = "email"
[channels.c.inbound]
host = "h"
port = 993
username = "u"
password = "p"
[channels.c.outbound]
host = "h"
port = 465
username = "u"
password = "p"
[agent]
enabled = true
mode = "agent"
"#,
        );
        toml.push_str(&format!(
            "\n[[channels.c.patterns]]\nname = \"{pattern_name}\"\n"
        ));
        if let Some(m) = mode {
            toml.push_str(&format!("mode = \"{m}\"\n"));
        }
        jyc_types::load_config_from_str(&toml).expect("config should parse")
    }

    #[tokio::test]
    async fn resolve_effective_mode_override_file_wins() {
        let tmp = tempfile::tempdir().unwrap();
        let topic = "resolve_effective_mode_override_file_wins";
        jyc_types::state_dir::register(topic, &tmp.path().join(".jyc"));
        let state_dir = tmp.path().join(".jyc");
        tokio::fs::create_dir_all(&state_dir).await.unwrap();
        tokio::fs::write(state_dir.join("mode-override"), "build\n")
            .await
            .unwrap();
        tokio::fs::write(state_dir.join("pattern"), "p1\n")
            .await
            .unwrap();
        let config = config_with_pattern_mode("p1", Some("plan"));
        let result = resolve_effective_mode(topic, tmp.path(), &config, "c").await;
        assert_eq!(result, Some("build".to_string()));
    }

    #[tokio::test]
    async fn resolve_effective_mode_uses_pattern_config_mode() {
        let tmp = tempfile::tempdir().unwrap();
        let topic = "resolve_effective_mode_uses_pattern_config_mode";
        jyc_types::state_dir::register(topic, &tmp.path().join(".jyc"));
        let state_dir = tmp.path().join(".jyc");
        tokio::fs::create_dir_all(&state_dir).await.unwrap();
        tokio::fs::write(state_dir.join("pattern"), "p1\n")
            .await
            .unwrap();
        let config = config_with_pattern_mode("p1", Some("plan"));
        let result = resolve_effective_mode(topic, tmp.path(), &config, "c").await;
        assert_eq!(result, Some("plan".to_string()));
    }

    #[tokio::test]
    async fn resolve_effective_mode_none_when_neither_source() {
        let tmp = tempfile::tempdir().unwrap();
        let topic = "resolve_effective_mode_none_when_neither_source";
        jyc_types::state_dir::register(topic, &tmp.path().join(".jyc"));
        let state_dir = tmp.path().join(".jyc");
        tokio::fs::create_dir_all(&state_dir).await.unwrap();
        let config = config_with_pattern_mode("p1", Some("plan"));
        let result = resolve_effective_mode(topic, tmp.path(), &config, "c").await;
        assert_eq!(result, None);
    }

    #[tokio::test]
    async fn resolve_effective_mode_empty_pattern_file() {
        let tmp = tempfile::tempdir().unwrap();
        let topic = "resolve_effective_mode_empty_pattern_file";
        jyc_types::state_dir::register(topic, &tmp.path().join(".jyc"));
        let state_dir = tmp.path().join(".jyc");
        tokio::fs::create_dir_all(&state_dir).await.unwrap();
        tokio::fs::write(state_dir.join("pattern"), "  \n")
            .await
            .unwrap();
        let config = config_with_pattern_mode("p1", Some("plan"));
        let result = resolve_effective_mode(topic, tmp.path(), &config, "c").await;
        assert_eq!(result, None);
    }

    #[tokio::test]
    async fn resolve_effective_mode_pattern_without_mode() {
        let tmp = tempfile::tempdir().unwrap();
        let topic = "resolve_effective_mode_pattern_without_mode";
        jyc_types::state_dir::register(topic, &tmp.path().join(".jyc"));
        let state_dir = tmp.path().join(".jyc");
        tokio::fs::create_dir_all(&state_dir).await.unwrap();
        tokio::fs::write(state_dir.join("pattern"), "p1\n")
            .await
            .unwrap();
        let config = config_with_pattern_mode("p1", None);
        let result = resolve_effective_mode(topic, tmp.path(), &config, "c").await;
        assert_eq!(result, None);
    }

    #[tokio::test]
    async fn resolve_effective_mode_unknown_channel() {
        let tmp = tempfile::tempdir().unwrap();
        let topic = "resolve_effective_mode_unknown_channel";
        jyc_types::state_dir::register(topic, &tmp.path().join(".jyc"));
        let state_dir = tmp.path().join(".jyc");
        tokio::fs::create_dir_all(&state_dir).await.unwrap();
        tokio::fs::write(state_dir.join("pattern"), "p1\n")
            .await
            .unwrap();
        let config = config_with_pattern_mode("p1", Some("plan"));
        let result = resolve_effective_mode(topic, tmp.path(), &config, "other").await;
        assert_eq!(result, None);
    }

    // ── resolve_model ─────────────────────────────────────────────────

    /// A model may name the global value it would otherwise repeat, so changing
    /// `[ai].small_model` in one place moves every agent and role that asked for
    /// it. The reference is resolved at the end of the chain, whichever layer
    /// wrote it.
    #[tokio::test]
    async fn resolve_model_resolves_a_reference_to_the_global_ai_value() {
        let tmp = tempfile::tempdir().unwrap();
        let topic = "resolve_model_resolves_a_reference_to_the_global_ai_value";
        jyc_types::state_dir::register(topic, &tmp.path().join(".jyc"));
        let state_dir = tmp.path().join(".jyc");
        tokio::fs::create_dir_all(&state_dir).await.unwrap();
        tokio::fs::write(
            state_dir.join("config.toml"),
            "[ai]\nmodel = \"${ai.small_model}\"\n",
        )
        .await
        .unwrap();

        let mut config = AppConfig::default();
        config.ai.model = Some("p/main".to_string());
        config.ai.small_model = Some("p/small".to_string());

        assert_eq!(
            resolve_model(topic, tmp.path(), &config, "x", None, None, None)
                .await
                .as_deref(),
            Some("p/small"),
            "the topic's `[ai].model` asks for whatever the global small model is"
        );

        // The failure stance is part of the promise: a reference that cannot be
        // resolved yields no override (logged at error level), never the literal
        // text — and the fallback that fills `None` resolves too (see
        // `create_provider`), so the text cannot reach a provider.
        tokio::fs::write(
            state_dir.join("config.toml"),
            "[ai]\nmodel = \"${ai.nope}\"\n",
        )
        .await
        .unwrap();
        assert_eq!(
            resolve_model(topic, tmp.path(), &config, "x", None, None, None).await,
            None,
            "an unresolvable reference is no override, not a literal model id"
        );
    }

    /// The chain is keyed by the mode the caller asks for, not by the topic's
    /// own mode and not by a stale file for the other one: a topic in plan mode
    /// that carries a leftover `build-model-override` runs the plan side.
    #[tokio::test]
    async fn resolve_model_is_keyed_by_the_mode_it_is_asked_for() {
        let tmp = tempfile::tempdir().unwrap();
        let topic = "resolve_model_is_keyed_by_the_mode_it_is_asked_for";
        jyc_types::state_dir::register(topic, &tmp.path().join(".jyc"));
        let state_dir = tmp.path().join(".jyc");
        tokio::fs::create_dir_all(&state_dir).await.unwrap();
        tokio::fs::write(
            state_dir.join("build-model-override"),
            "provider/build-file\n",
        )
        .await
        .unwrap();
        tokio::fs::write(
            state_dir.join("config.toml"),
            "[ai]\nplan_model = \"provider/plan-config\"\n",
        )
        .await
        .unwrap();

        let config = AppConfig::default();
        assert_eq!(
            resolve_model(topic, tmp.path(), &config, "x", Some("plan"), None, None)
                .await
                .as_deref(),
            Some("provider/plan-config"),
            "plan mode never reads the build file"
        );
        assert_eq!(
            resolve_model(topic, tmp.path(), &config, "x", Some("build"), None, None)
                .await
                .as_deref(),
            Some("provider/build-file"),
            "build mode reads it"
        );
        assert_eq!(
            resolve_model(topic, tmp.path(), &config, "x", None, None, None)
                .await
                .as_deref(),
            Some("provider/build-file"),
            "no mode means build"
        );

        // The legacy file answers for either mode once its own file is gone.
        tokio::fs::write(state_dir.join("model-override"), "provider/legacy\n")
            .await
            .unwrap();
        assert_eq!(
            resolve_model(topic, tmp.path(), &config, "x", Some("plan"), None, None)
                .await
                .as_deref(),
            Some("provider/legacy")
        );

        // An existing-but-empty file still blocks the legacy one: the rule is
        // existence, not content — the runtime always read it that way, and the
        // old display's fall-through was the side that had to move.
        tokio::fs::write(state_dir.join("plan-model-override"), "")
            .await
            .unwrap();
        assert_eq!(
            resolve_model(topic, tmp.path(), &config, "x", Some("plan"), None, None)
                .await
                .as_deref(),
            Some("provider/plan-config"),
            "an empty plan file is this mode's file; the legacy one is not consulted"
        );
    }

    /// Every config layer is keyed by the requested mode too — which is how a
    /// topic whose mode comes from its own config (a typed subagent) finds the
    /// model that mode names.
    #[tokio::test]
    async fn resolve_model_asks_each_layer_for_the_requested_mode() {
        let tmp = tempfile::tempdir().unwrap();
        let topic = "resolve_model_asks_each_layer_for_the_requested_mode";
        jyc_types::state_dir::register(topic, &tmp.path().join(".jyc"));
        let state_dir = tmp.path().join(".jyc");
        tokio::fs::create_dir_all(&state_dir).await.unwrap();
        tokio::fs::write(
            state_dir.join("config.toml"),
            "[ai]\nplan_model = \"provider/plan-config\"\nbuild_model = \"provider/build-config\"\n",
        )
        .await
        .unwrap();

        let config = AppConfig::default();
        for (mode, expected) in [
            (Some("plan"), "provider/plan-config"),
            (Some("build"), "provider/build-config"),
            (None, "provider/build-config"),
        ] {
            assert_eq!(
                resolve_model(topic, tmp.path(), &config, "x", mode, None, None)
                    .await
                    .as_deref(),
                Some(expected),
                "mode {mode:?}"
            );
        }
    }

    /// No first-pattern fallback: the runtime has no such layer, so a topic that
    /// matched no pattern must not pick up the channel's first one. Naming the
    /// pattern is what reaches it.
    #[tokio::test]
    async fn resolve_model_has_no_first_pattern_fallback() {
        let tmp = tempfile::tempdir().unwrap();
        let topic = "resolve_model_has_no_first_pattern_fallback";
        jyc_types::state_dir::register(topic, &tmp.path().join(".jyc"));
        tokio::fs::create_dir_all(&tmp.path().join(".jyc"))
            .await
            .unwrap();

        let config = jyc_types::load_config_from_str(
            r#"
[general]
[channels.c]
type = "email"
[channels.c.inbound]
host = "h"
port = 993
username = "u"
password = "p"
[channels.c.outbound]
host = "h"
port = 465
username = "u"
password = "p"
[agent]
enabled = true
mode = "agent"
[[channels.c.patterns]]
name = "p1"
model = "provider/first-pattern"
"#,
        )
        .expect("config should parse");

        assert_eq!(
            resolve_model(topic, tmp.path(), &config, "c", Some("build"), None, None).await,
            None,
            "the channel's first pattern is not a layer"
        );
        assert_eq!(
            resolve_model(
                topic,
                tmp.path(),
                &config,
                "c",
                Some("build"),
                Some("p1"),
                None
            )
            .await
            .as_deref(),
            Some("provider/first-pattern"),
            "naming the pattern reaches it"
        );
    }

    // ── write_context_window ───────────────────────────────────────────

    #[tokio::test]
    async fn write_context_window_creates_file_when_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let topic = "write_context_window_creates_file_when_missing";
        jyc_types::state_dir::register(topic, &tmp.path().join(".jyc"));
        write_context_window(topic, tmp.path(), 12345).await;
        let content = tokio::fs::read_to_string(tmp.path().join(".jyc/agent-session.json"))
            .await
            .unwrap();
        let state: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert_eq!(state["context_window"], 12345);
    }

    #[tokio::test]
    async fn write_context_window_updates_existing() {
        let tmp = tempfile::tempdir().unwrap();
        let topic = "write_context_window_updates_existing";
        jyc_types::state_dir::register(topic, &tmp.path().join(".jyc"));
        let state_dir = tmp.path().join(".jyc");
        tokio::fs::create_dir_all(&state_dir).await.unwrap();
        tokio::fs::write(
            state_dir.join("agent-session.json"),
            r#"{"context_input_tokens":5000,"context_window":100000}"#,
        )
        .await
        .unwrap();
        write_context_window(topic, tmp.path(), 250000).await;
        let content = tokio::fs::read_to_string(state_dir.join("agent-session.json"))
            .await
            .unwrap();
        let state: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert_eq!(state["context_window"], 250000);
        // Other fields preserved
        assert_eq!(state["context_input_tokens"], 5000);
    }

    /// The legacy `max_input_tokens` key is an alias of `context_window` on
    /// the read side — a file carrying both would fail to deserialize, so
    /// the write drops it.
    #[tokio::test]
    async fn write_context_window_drops_the_legacy_key() {
        let tmp = tempfile::tempdir().unwrap();
        let topic = "write_context_window_drops_the_legacy_key";
        jyc_types::state_dir::register(topic, &tmp.path().join(".jyc"));
        let state_dir = tmp.path().join(".jyc");
        tokio::fs::create_dir_all(&state_dir).await.unwrap();
        tokio::fs::write(
            state_dir.join("agent-session.json"),
            r#"{"context_input_tokens":5000,"max_input_tokens":100000}"#,
        )
        .await
        .unwrap();
        write_context_window(topic, tmp.path(), 250_000).await;
        let content = tokio::fs::read_to_string(state_dir.join("agent-session.json"))
            .await
            .unwrap();
        let state: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert_eq!(state["context_window"], 250_000);
        assert!(state.get("max_input_tokens").is_none());
        // The file still deserializes into the session state.
        let parsed: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert_eq!(parsed["context_input_tokens"], 5000);
    }

    /// A file carrying BOTH `context_window` and its legacy alias
    /// `max_input_tokens` fails to deserialize (serde sees the alias twice),
    /// so every reader silently falls back to the default state. The
    /// idempotent path must drop the legacy key too — otherwise the file
    /// stays broken forever.
    #[tokio::test]
    async fn write_context_window_drops_the_legacy_key_even_when_unchanged() {
        let tmp = tempfile::tempdir().unwrap();
        let topic = "write_context_window_drops_the_legacy_key_even_when_unchanged";
        jyc_types::state_dir::register(topic, &tmp.path().join(".jyc"));
        let state_dir = tmp.path().join(".jyc");
        tokio::fs::create_dir_all(&state_dir).await.unwrap();
        tokio::fs::write(
            state_dir.join("agent-session.json"),
            r#"{"context_input_tokens":5000,"context_window":100000,"max_input_tokens":100000}"#,
        )
        .await
        .unwrap();

        // Before the write the file does not deserialize at all.
        assert_eq!(
            read_token_state(topic, tmp.path()).await.context_window,
            None
        );

        // Same window as the file already carries — the window itself does
        // not change, but the legacy key still has to go.
        write_context_window(topic, tmp.path(), 100_000).await;

        let content = tokio::fs::read_to_string(state_dir.join("agent-session.json"))
            .await
            .unwrap();
        let state: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert_eq!(state["context_window"], 100_000);
        assert!(state.get("max_input_tokens").is_none());
        // The file deserializes again.
        let parsed = read_token_state(topic, tmp.path()).await;
        assert_eq!(parsed.context_window, Some(100_000));
        assert_eq!(parsed.context_input_tokens, Some(5000));
    }

    #[tokio::test]
    async fn write_context_window_is_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let topic = "write_context_window_is_idempotent";
        jyc_types::state_dir::register(topic, &tmp.path().join(".jyc"));
        write_context_window(topic, tmp.path(), 12345).await;
        let first_mtime = tokio::fs::metadata(tmp.path().join(".jyc/agent-session.json"))
            .await
            .unwrap()
            .modified()
            .unwrap();
        // Sleep a beat so mtime would change if we rewrote
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        write_context_window(topic, tmp.path(), 12345).await;
        let second_mtime = tokio::fs::metadata(tmp.path().join(".jyc/agent-session.json"))
            .await
            .unwrap()
            .modified()
            .unwrap();
        assert_eq!(
            first_mtime, second_mtime,
            "file should not be rewritten when value unchanged"
        );
    }

    /// A closed topic's state dir is gone on purpose; wiring the window
    /// (which happens mid-turn, when the agent learns the model's limit)
    /// must not bring it back.
    #[tokio::test]
    async fn write_context_window_skips_a_closed_topic() {
        let tmp = tempfile::tempdir().unwrap();
        let topic = "write_context_window_skips_a_closed_topic";
        let state_dir = tmp.path().join(".jyc");
        jyc_types::state_dir::register(topic, &state_dir);
        jyc_types::state_dir::mark_closed(topic);

        write_context_window(topic, tmp.path(), 12345).await;

        assert!(!state_dir.exists(), "a closed topic's state dir stays gone");
        jyc_types::state_dir::clear_closed(topic);
    }

    // ── resolve_context_strategy ──────────────────────────────────────

    use jyc_types::channel::{ContextStrategy, ContextStrategyConfig};

    fn config_with_strategies(
        patterns: Vec<(&str, Option<ContextStrategyConfig>)>,
        global: Option<ContextStrategyConfig>,
    ) -> AppConfig {
        // Base ends with `[agent]` (alias for `[ai]` via
        // `#[serde(rename = "ai", alias = "agent")]`). Global context_strategy
        // is added right after `[agent]` so it lands at `[ai]` level — the
        // global fallback `resolve_context_strategy` reads from.
        let mut toml = String::from(
            r#"
[general]
[channels.c]
type = "email"
[channels.c.inbound]
host = "h"
port = 993
username = "u"
password = "p"
[channels.c.outbound]
host = "h"
port = 465
username = "u"
password = "p"
[agent]
enabled = true
mode = "agent"
"#,
        );
        if let Some(g) = &global {
            let mode = match g.mode {
                ContextStrategy::Full => "full",
                ContextStrategy::SlidingWindow => "sliding_window",
            };
            let parts = format!(
                "mode = \"{mode}\", window = {}, extend_rate = {}",
                g.window, g.extend_rate
            );
            toml.push_str(&format!("context_strategy = {{ {parts} }}\n"));
        }
        // Patterns loop — opens [[channels.c.patterns]] which leaves the
        // current scope at the latest pattern element. Keep global BEFORE
        // this loop so it always lands at [agent]/[ai] level.
        for (name, s) in &patterns {
            toml.push_str(&format!("\n[[channels.c.patterns]]\nname = \"{name}\"\n"));
            if let Some(c) = s {
                let mode = match c.mode {
                    ContextStrategy::Full => "full",
                    ContextStrategy::SlidingWindow => "sliding_window",
                };
                let parts = format!(
                    "mode = \"{mode}\", window = {}, extend_rate = {}",
                    c.window, c.extend_rate
                );
                toml.push_str(&format!("context_strategy = {{ {parts} }}\n"));
            }
        }
        jyc_types::load_config_from_str(&toml).expect("config should parse")
    }

    #[test]
    fn resolve_context_strategy_uses_matched_pattern() {
        let slide = ContextStrategyConfig {
            mode: ContextStrategy::SlidingWindow,
            window: 4,
            extend_rate: 0.5,
            turn_tool_result_cap: None,
            max_history_tokens: None,
        };
        let app = config_with_strategies(vec![("a", None), ("b", Some(slide.clone()))], None);
        let resolved = resolve_context_strategy(&app, "c", Some("b"), None);
        assert_eq!(resolved.mode, ContextStrategy::SlidingWindow);
        assert_eq!(resolved.window, 4);
    }

    #[test]
    fn resolve_context_strategy_falls_back_to_first_pattern() {
        let slide = ContextStrategyConfig {
            mode: ContextStrategy::SlidingWindow,
            window: 6,
            extend_rate: 0.5,
            turn_tool_result_cap: None,
            max_history_tokens: None,
        };
        let app = config_with_strategies(vec![("a", Some(slide.clone()))], None);
        let resolved = resolve_context_strategy(&app, "c", None, None);
        assert_eq!(resolved.mode, ContextStrategy::SlidingWindow);
        assert_eq!(resolved.window, 6);
    }

    #[test]
    fn resolve_context_strategy_falls_back_to_global() {
        let slide = ContextStrategyConfig {
            mode: ContextStrategy::SlidingWindow,
            window: 8,
            extend_rate: 0.5,
            turn_tool_result_cap: None,
            max_history_tokens: None,
        };
        let app = config_with_strategies(vec![], Some(slide.clone()));
        let resolved = resolve_context_strategy(&app, "c", Some("a"), None);
        assert_eq!(resolved.mode, ContextStrategy::SlidingWindow);
        assert_eq!(resolved.window, 8);
    }

    #[test]
    fn resolve_context_strategy_default_is_sliding_window_10_0() {
        let app = config_with_strategies(vec![], None);
        let resolved = resolve_context_strategy(&app, "c", Some("a"), None);
        assert_eq!(resolved.mode, ContextStrategy::SlidingWindow);
        assert_eq!(resolved.window, 10);
    }

    /// The topic's own strategy — what a subagent type hands its child — is a
    /// layer above the pattern/channel layers and below the runtime override
    /// file, which callers read first: the same shape as `mode`.
    #[test]
    fn resolve_context_strategy_topic_strategy_wins_over_pattern() {
        let pattern = ContextStrategyConfig {
            mode: ContextStrategy::SlidingWindow,
            window: 10,
            extend_rate: 0.5,
            turn_tool_result_cap: None,
            max_history_tokens: None,
        };
        let app = config_with_strategies(vec![("a", Some(pattern))], None);
        let topic_strategy = ContextStrategyConfig {
            mode: ContextStrategy::SlidingWindow,
            window: 3,
            extend_rate: 0.5,
            turn_tool_result_cap: None,
            max_history_tokens: None,
        };

        let resolved = resolve_context_strategy(&app, "c", Some("a"), Some(&topic_strategy));

        assert_eq!(
            resolved.window, 3,
            "the topic's strategy wins over the pattern"
        );
    }

    #[tokio::test]
    async fn read_context_strategy_override_roundtrip() {
        let tmp = tempfile::tempdir().unwrap();
        let topic = "resolve_context_strategy_uses_matched_pattern";
        jyc_types::state_dir::register(topic, &tmp.path().join(".jyc"));
        let state_dir = tmp.path().join(".jyc");
        tokio::fs::create_dir_all(&state_dir).await.unwrap();
        tokio::fs::write(
            state_dir.join(CONTEXT_STRATEGY_FILE),
            r#"{"mode":"sliding_window","window":3}"#,
        )
        .await
        .unwrap();
        let cfg = read_context_strategy_override(topic, tmp.path())
            .await
            .unwrap();
        assert_eq!(cfg.mode, ContextStrategy::SlidingWindow);
        assert_eq!(cfg.window, 3);
    }

    #[tokio::test]
    async fn read_context_strategy_override_ignores_removed_keys() {
        // A persisted override written before `note_window` was removed
        // must still load: the key is unknown to the struct and ignored.
        let tmp = tempfile::tempdir().unwrap();
        let topic = "read_context_strategy_override_ignores_removed_keys";
        jyc_types::state_dir::register(topic, &tmp.path().join(".jyc"));
        let state_dir = tmp.path().join(".jyc");
        tokio::fs::create_dir_all(&state_dir).await.unwrap();
        tokio::fs::write(
            state_dir.join(CONTEXT_STRATEGY_FILE),
            r#"{"mode":"sliding_window","window":5,"note_window":2}"#,
        )
        .await
        .unwrap();
        let cfg = read_context_strategy_override(topic, tmp.path())
            .await
            .unwrap();
        assert_eq!(cfg.window, 5);
    }

    #[tokio::test]
    async fn read_context_strategy_override_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let topic = "read_context_strategy_override_missing";
        jyc_types::state_dir::register(topic, &tmp.path().join(".jyc"));
        assert!(
            read_context_strategy_override(topic, tmp.path())
                .await
                .is_none()
        );
    }

    #[tokio::test]
    async fn read_context_strategy_override_window_zero_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let topic = "read_context_strategy_override_window_zero_rejected";
        jyc_types::state_dir::register(topic, &tmp.path().join(".jyc"));
        let state_dir = tmp.path().join(".jyc");
        tokio::fs::create_dir_all(&state_dir).await.unwrap();
        tokio::fs::write(
            state_dir.join(CONTEXT_STRATEGY_FILE),
            r#"{"mode":"sliding_window","window":0}"#,
        )
        .await
        .unwrap();
        assert!(
            read_context_strategy_override(topic, tmp.path())
                .await
                .is_none()
        );
    }

    // ── extend_rate resolution ────────────────────────────────────────

    #[test]
    fn resolve_context_strategy_extend_rate_from_global() {
        let slide = ContextStrategyConfig {
            mode: ContextStrategy::SlidingWindow,
            window: 10,
            extend_rate: 1.0,
            turn_tool_result_cap: None,
            max_history_tokens: None,
        };
        let app = config_with_strategies(vec![], Some(slide));
        let resolved = resolve_context_strategy(&app, "c", Some("a"), None);
        assert_eq!(resolved.extend_rate, 1.0);
        assert_eq!(resolved.max_window(), 20);
    }

    #[test]
    fn resolve_context_strategy_extend_rate_pattern_wins_over_global() {
        let pattern = ContextStrategyConfig {
            mode: ContextStrategy::SlidingWindow,
            window: 10,
            extend_rate: 2.0,
            turn_tool_result_cap: None,
            max_history_tokens: None,
        };
        let global = ContextStrategyConfig {
            mode: ContextStrategy::SlidingWindow,
            window: 20,
            extend_rate: 0.25,
            turn_tool_result_cap: None,
            max_history_tokens: None,
        };
        let app = config_with_strategies(vec![("a", Some(pattern))], Some(global));
        let resolved = resolve_context_strategy(&app, "c", Some("a"), None);
        assert_eq!(resolved.window, 10, "pattern window wins");
        assert_eq!(resolved.extend_rate, 2.0, "pattern extend_rate wins");
    }

    #[tokio::test]
    async fn read_context_strategy_override_with_extend_rate() {
        let tmp = tempfile::tempdir().unwrap();
        let topic = "read_context_strategy_override_with_extend_rate";
        jyc_types::state_dir::register(topic, &tmp.path().join(".jyc"));
        let state_dir = tmp.path().join(".jyc");
        tokio::fs::create_dir_all(&state_dir).await.unwrap();
        tokio::fs::write(
            state_dir.join(CONTEXT_STRATEGY_FILE),
            r#"{"mode":"sliding_window","window":10,"extend_rate":1.5}"#,
        )
        .await
        .unwrap();
        let cfg = read_context_strategy_override(topic, tmp.path())
            .await
            .unwrap();
        assert_eq!(cfg.extend_rate, 1.5);
    }

    #[tokio::test]
    async fn read_context_strategy_override_extend_rate_missing_uses_default() {
        // A legacy override file without the field still parses, picking up
        // the struct default of `0.5`.
        let tmp = tempfile::tempdir().unwrap();
        let topic = "read_context_strategy_override_extend_rate_missing_uses_default";
        jyc_types::state_dir::register(topic, &tmp.path().join(".jyc"));
        let state_dir = tmp.path().join(".jyc");
        tokio::fs::create_dir_all(&state_dir).await.unwrap();
        tokio::fs::write(
            state_dir.join(CONTEXT_STRATEGY_FILE),
            r#"{"mode":"sliding_window","window":10}"#,
        )
        .await
        .unwrap();
        let cfg = read_context_strategy_override(topic, tmp.path())
            .await
            .unwrap();
        assert_eq!(cfg.extend_rate, 0.5);
    }

    // ── task list ─────────────────────────────────────────────────────

    fn jyc_of(tmp: &tempfile::TempDir) -> std::path::PathBuf {
        tmp.path().join(".jyc")
    }

    fn tasks(a: &[(&str, TaskStatus)]) -> TaskList {
        TaskList {
            items: a
                .iter()
                .enumerate()
                .map(|(i, (text, status))| TaskItem {
                    id: (i + 1) as u32,
                    text: (*text).to_string(),
                    status: *status,
                })
                .collect(),
        }
    }

    #[tokio::test]
    async fn write_then_read_tasks_roundtrips() {
        let tmp = tempfile::tempdir().unwrap();
        let topic = "write_then_read_tasks_roundtrips";
        jyc_types::state_dir::register(topic, &tmp.path().join(".jyc"));
        let list = tasks(&[("a", TaskStatus::Pending), ("b", TaskStatus::Completed)]);
        write_tasks_at(topic, &jyc_of(&tmp), &list).await.unwrap();
        assert_eq!(read_tasks_at(&jyc_of(&tmp)).await, Some(list));
    }

    #[tokio::test]
    async fn read_tasks_missing_file_is_none() {
        let tmp = tempfile::tempdir().unwrap();
        let topic = "read_tasks_missing_file_is_none";
        jyc_types::state_dir::register(topic, &tmp.path().join(".jyc"));
        assert_eq!(read_tasks_at(&jyc_of(&tmp)).await, None);
    }

    #[tokio::test]
    async fn read_tasks_corrupt_file_is_none() {
        let tmp = tempfile::tempdir().unwrap();
        let topic = "read_tasks_corrupt_file_is_none";
        jyc_types::state_dir::register(topic, &tmp.path().join(".jyc"));
        let dir = jyc_of(&tmp);
        tokio::fs::create_dir_all(&dir).await.unwrap();
        tokio::fs::write(dir.join(TASKS_FILE), "not json")
            .await
            .unwrap();
        assert_eq!(read_tasks_at(&jyc_of(&tmp)).await, None);
    }

    #[tokio::test]
    async fn write_empty_tasks_deletes_file() {
        // A missing file and an empty list must mean the same thing, so the
        // renderer can treat `None` as "no section".
        let tmp = tempfile::tempdir().unwrap();
        let topic = "write_empty_tasks_deletes_file";
        jyc_types::state_dir::register(topic, &tmp.path().join(".jyc"));
        write_tasks_at(topic, &jyc_of(&tmp), &tasks(&[("a", TaskStatus::Pending)]))
            .await
            .unwrap();
        write_tasks_at(topic, &jyc_of(&tmp), &TaskList::default())
            .await
            .unwrap();
        assert!(
            !tokio::fs::try_exists(jyc_of(&tmp).join(TASKS_FILE))
                .await
                .unwrap()
        );
        assert_eq!(read_tasks_at(&jyc_of(&tmp)).await, None);
    }

    #[tokio::test]
    async fn clear_tasks_removes_file_and_is_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let topic = "clear_tasks_removes_file_and_is_idempotent";
        jyc_types::state_dir::register(topic, &tmp.path().join(".jyc"));
        write_tasks_at(topic, &jyc_of(&tmp), &tasks(&[("a", TaskStatus::Pending)]))
            .await
            .unwrap();
        clear_tasks_at(&jyc_of(&tmp)).await;
        assert_eq!(read_tasks_at(&jyc_of(&tmp)).await, None);
        clear_tasks_at(&jyc_of(&tmp)).await;
    }

    /// A tool call or a stacked command that lands after the turn closed its
    /// own topic must not recreate the state dir — the same skeleton the
    /// session writers were fixed to stop producing.
    #[tokio::test]
    async fn closed_topic_writes_no_task_list() {
        let tmp = tempfile::tempdir().unwrap();
        let topic = "closed_topic_writes_no_task_list";
        let state_dir = tmp.path().join(".jyc");
        jyc_types::state_dir::register(topic, &state_dir);
        jyc_types::state_dir::mark_closed(topic);
        write_tasks_at(topic, &state_dir, &tasks(&[("a", TaskStatus::Pending)]))
            .await
            .unwrap();
        assert!(!state_dir.exists(), "a closed topic's state dir stays gone");
        jyc_types::state_dir::clear_closed(topic);
    }

    #[tokio::test]
    async fn closed_topic_writes_no_toggle_override() {
        let tmp = tempfile::tempdir().unwrap();
        let topic = "closed_topic_writes_no_toggle_override";
        let state_dir = tmp.path().join(".jyc");
        jyc_types::state_dir::register(topic, &state_dir);
        jyc_types::state_dir::mark_closed(topic);
        write_toggle_override(
            topic,
            tmp.path(),
            "model-override",
            &ToggleOverride {
                on: vec!["x".to_string()],
                off: Vec::new(),
            },
        )
        .await
        .unwrap();
        assert!(!state_dir.exists(), "a closed topic's state dir stays gone");
        jyc_types::state_dir::clear_closed(topic);
    }
}
