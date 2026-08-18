use jyc_types::AppConfig;
use jyc_types::channel::{ContextStrategyConfig, ResetCompressionConfig};
use serde::Deserialize;
use std::path::Path;

/// Fallback context window when neither the per-model nor the per-provider
/// setting is configured. Mirrors the constant in `jyc-agent::service`.
pub const DEFAULT_CONTEXT_WINDOW: u64 = 128000;

/// Read input tokens from the agent session state file.
/// Returns (current_tokens, max_tokens).
pub async fn read_input_tokens(topic_path: &Path) -> (Option<u64>, Option<u64>) {
    let (cur, max, _, _, _, _) = read_token_state(topic_path).await;
    (cur, max)
}

/// Read accumulated output tokens from the agent session state file.
/// Returns `None` when the file is missing, malformed, or the value is zero.
/// The session file already deserializes `total_output_tokens` — this just
/// surfaces it. Output tokens accumulate across LLM calls in a round.
pub async fn read_output_tokens(topic_path: &Path) -> Option<u64> {
    let (_, _, out, _, _, _) = read_token_state(topic_path).await;
    out
}

/// Read accumulated total input tokens from the agent session state file.
/// Returns `None` when the file is missing, malformed, or the value is zero.
/// Distinct from `read_input_tokens` (which returns the current context
/// size); this is the running sum across all LLM calls in the session.
pub async fn read_total_input_tokens(topic_path: &Path) -> Option<u64> {
    let (_, _, _, total, _, _) = read_token_state(topic_path).await;
    total
}

/// Read accumulated prompt-cache-hit tokens from the agent session state
/// file. Returns `None` when the file is missing, malformed, or the
/// accumulated value is zero. Mirrors `read_total_input_tokens`; zero
/// covers both "no calls yet" and "provider didn't surface cache hits".
pub async fn read_total_cache_hit_tokens(topic_path: &Path) -> Option<u64> {
    let (_, _, _, _, cache_hit, _) = read_token_state(topic_path).await;
    cache_hit
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
pub async fn read_total_cache_creation_tokens(topic_path: &Path) -> Option<u64> {
    let (_, _, _, _, _, cache_creation) = read_token_state(topic_path).await;
    cache_creation
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
pub async fn read_session_cost(topic_path: &Path) -> Option<f64> {
    let agent_path = topic_path.join(".jyc").join("agent-session.json");
    let content = tokio::fs::read_to_string(&agent_path).await.ok()?;
    let state = serde_json::from_str::<AgentSessionState>(&content).ok()?;
    (state.session_cost > 0.0).then_some(state.session_cost)
}

/// Read all six token fields in a single file read.
/// Returns (current_input_tokens, max_input_tokens, output_tokens,
/// total_input_tokens, total_cache_hit_tokens,
/// total_cache_creation_tokens). Any individual field is `None` when
/// the file is missing, malformed, or the underlying value is zero
/// (`total_cache_creation_tokens` is also `None` when the field is
/// absent — sessions written before it existed deserialize as `None`).
///
/// Callers that need all six fields (e.g. `topic_manager::list_topics`)
/// should use this rather than calling the single-purpose helpers above
/// separately — saves one file open + JSON parse per call.
pub async fn read_token_state(
    topic_path: &Path,
) -> (
    Option<u64>,
    Option<u64>,
    Option<u64>,
    Option<u64>,
    Option<u64>,
    Option<u64>,
) {
    let agent_path = topic_path.join(".jyc").join("agent-session.json");
    let Ok(content) = tokio::fs::read_to_string(&agent_path).await else {
        return (None, None, None, None, None, None);
    };
    let Ok(state) = serde_json::from_str::<AgentSessionState>(&content) else {
        return (None, None, None, None, None, None);
    };
    let current = (state.context_input_tokens > 0).then_some(state.context_input_tokens);
    let max = (state.max_input_tokens > 0).then_some(state.max_input_tokens);
    let output = (state.total_output_tokens > 0).then_some(state.total_output_tokens);
    let total_input = (state.total_input_tokens > 0).then_some(state.total_input_tokens);
    let total_cache_hit =
        (state.total_cache_hit_tokens > 0).then_some(state.total_cache_hit_tokens);
    let total_cache_creation =
        (state.total_cache_creation_tokens > 0).then_some(state.total_cache_creation_tokens);
    (
        current,
        max,
        output,
        total_input,
        total_cache_hit,
        total_cache_creation,
    )
}

/// Agent session state format.
#[derive(Debug, Deserialize)]
struct AgentSessionState {
    #[serde(default)]
    context_input_tokens: u64,
    #[serde(default)]
    #[allow(dead_code)]
    total_input_tokens: u64,
    #[serde(default)]
    #[allow(dead_code)]
    total_output_tokens: u64,
    #[serde(default)]
    #[allow(dead_code)]
    total_cache_hit_tokens: u64,
    #[serde(default)]
    #[allow(dead_code)]
    total_cache_creation_tokens: u64,
    #[serde(default)]
    max_input_tokens: u64,
    #[serde(default)]
    session_cost: f64,
}

/// Read the model override file if it exists.
pub async fn read_model_override(topic_path: &Path) -> Option<String> {
    let override_path = topic_path.join(".jyc").join("model-override");
    tokio::fs::read_to_string(override_path)
        .await
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Read the mode override file if it exists.
pub async fn read_mode_override(topic_path: &Path) -> Option<String> {
    let override_path = topic_path.join(".jyc").join("mode-override");
    tokio::fs::read_to_string(override_path)
        .await
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Read the matched pattern file if it exists.
pub async fn read_pattern(topic_path: &Path) -> Option<String> {
    let pattern_path = topic_path.join(".jyc").join("pattern");
    tokio::fs::read_to_string(pattern_path)
        .await
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Resolve the `ResetCompressionConfig` for a topic.
///
/// Priority: matched pattern > first pattern > global `[agent].reset_compression` > default.
///
/// Pass `matched_pattern = Some(name)` when the caller has access to the
/// pattern that created the topic (e.g. `process()` with
/// `message.matched_pattern`). Pass `None` when the caller doesn't (e.g.
/// `/reset` command handler) — in that case the first pattern on the
/// channel is used as the best available signal.
pub fn resolve_reset_compression(
    config: &AppConfig,
    channel: &str,
    matched_pattern: Option<&str>,
) -> ResetCompressionConfig {
    let channel_cfg = config.channels.get(channel);
    let patterns = channel_cfg.and_then(|c| c.patterns.as_ref());

    // 1. Matched pattern's reset_compression
    let from_matched = matched_pattern
        .and_then(|name| patterns.and_then(|p| p.iter().find(|p| p.name == name)))
        .and_then(|p| p.reset_compression.clone());

    // 2. First pattern (fallback when matched pattern is unknown)
    let from_first = from_matched.or_else(|| {
        patterns
            .and_then(|p| p.first())
            .and_then(|p| p.reset_compression.clone())
    });

    // 3. Global [agent].reset_compression
    // 4. Default
    from_first
        .or_else(|| config.ai.reset_compression.clone())
        .unwrap_or_default()
}

/// Resolve the `max_input_tokens` value the session should have for the
/// currently active model on this topic.
///
/// Returns `None` if no model can be resolved (no config, no providers, etc.).
/// Callers should treat `None` as a no-op.
pub async fn resolve_active_context_window(
    topic_path: &Path,
    config: &AppConfig,
    channel: &str,
    auto_reset_threshold: f64,
) -> Option<u64> {
    let model_override = resolve_active_model(topic_path, config, channel).await?;
    let context_window = context_window_for_model(&model_override, config)?;
    Some((context_window as f64 * auto_reset_threshold) as u64)
}

/// Idempotently write `max_input_tokens` to `.jyc/agent-session.json`.
/// No-op if the value would not change (avoids disk churn).
pub async fn write_max_input_tokens(topic_path: &Path, new_max: u64) {
    let session_path = topic_path.join(".jyc").join("agent-session.json");
    let content = tokio::fs::read_to_string(&session_path)
        .await
        .unwrap_or_default();
    let mut state: serde_json::Map<String, serde_json::Value> =
        serde_json::from_str(&content).unwrap_or_default();
    let current = state
        .get("max_input_tokens")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    if current == new_max {
        return;
    }
    state.insert("max_input_tokens".to_string(), serde_json::json!(new_max));
    if let Some(parent) = session_path.parent() {
        tokio::fs::create_dir_all(parent).await.ok();
    }
    if let Ok(json) = serde_json::to_string_pretty(&state) {
        tokio::fs::write(&session_path, json).await.ok();
    }
}

/// Resolve the active model string (`<provider>/<model-id>`) for this topic.
/// Mirrors the chain in `jyc-agent::service::process()` (lines 1191-1263).
async fn resolve_active_model(
    topic_path: &Path,
    config: &AppConfig,
    channel: &str,
) -> Option<String> {
    let mode_override = read_mode_override(topic_path).await;
    let pattern_name = read_pattern(topic_path).await;

    // 1. Mode-specific file override
    let file_override = {
        let mode_suffix = match mode_override.as_deref() {
            Some("plan") => "plan",
            _ => "build",
        };
        let mode_specific = topic_path
            .join(".jyc")
            .join(format!("{mode_suffix}-model-override"));
        let legacy = topic_path.join(".jyc").join("model-override");
        let path = if mode_specific.exists() {
            Some(mode_specific)
        } else if legacy.exists() {
            Some(legacy)
        } else {
            None
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

    // 2. Topic config (`.jyc/config.toml`) — read once, then check both
    //    the mode-specific and the generic `model` field against the same
    //    loaded value (avoids re-reading the file in the common no-config case).
    let topic_cfg = jyc_types::load_topic_config(topic_path).and_then(|c| c.ai);
    let topic_cfg_override = topic_cfg
        .as_ref()
        .and_then(|a| match mode_override.as_deref() {
            Some("plan") => a.plan_model.clone(),
            _ => a.build_model.clone(),
        })
        .or_else(|| topic_cfg.as_ref().and_then(|a| a.model.clone()));

    // 3. Pattern (matched or first) — resolved from `config.channels[channel].patterns`
    let pattern_override = {
        let channel_cfg = config.channels.get(channel);
        let patterns = channel_cfg.and_then(|c| c.patterns.as_ref());
        let pat = pattern_name
            .as_deref()
            .and_then(|name| patterns.and_then(|p| p.iter().find(|p| p.name == name)))
            .or_else(|| patterns.and_then(|p| p.first()));
        pat.and_then(|p| match mode_override.as_deref() {
            Some("plan") => p.plan_model.clone(),
            _ => p.build_model.clone(),
        })
        .or_else(|| pat.and_then(|p| p.model.clone()))
    };

    // 4. Global [agent] (apply channel-level model override on top of global)
    let mut effective_agent = config.ai.clone();
    if let Some(ch) = config.channels.get(channel)
        && ch.model.is_some()
    {
        effective_agent.model = ch.model.clone();
    }
    let config_override = match mode_override.as_deref() {
        Some("plan") => effective_agent.plan_model.clone(),
        _ => effective_agent.build_model.clone(),
    }
    .or(effective_agent.model);

    file_override
        .or(topic_cfg_override)
        .or(pattern_override)
        .or(config_override)
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

/// Read the runtime context-strategy override if it exists.
///
/// Returns `None` when the file is missing, malformed, or `window == 0`.
/// A zero-window prior context would degrade to a single fallback user
/// message (see `extract_user_assistant_pairs`), which is almost
/// certainly not what the user wants; we silently fall back to the
/// configured default rather than persist a broken strategy.
pub async fn read_context_strategy_override(topic_path: &Path) -> Option<ContextStrategyConfig> {
    let path = topic_path.join(".jyc").join(CONTEXT_STRATEGY_FILE);
    let content = tokio::fs::read_to_string(&path).await.ok()?;
    let cfg: ContextStrategyConfig = serde_json::from_str(&content).ok()?;
    if cfg.window == 0 {
        return None;
    }
    Some(cfg)
}

/// Resolve the `ContextStrategyConfig` for a topic from config alone.
///
/// Priority: matched pattern > first pattern > global `[ai].context_strategy` >
/// `ContextStrategyConfig::default()` (full / window=10).
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
) -> ContextStrategyConfig {
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

    #[tokio::test]
    async fn read_input_tokens_from_file() {
        let tmp = tempfile::tempdir().unwrap();
        let jyc_dir = tmp.path().join(".jyc");
        tokio::fs::create_dir_all(&jyc_dir).await.unwrap();
        tokio::fs::write(
            jyc_dir.join("agent-session.json"),
            r#"{"context_input_tokens": 1000, "max_input_tokens": 2000}"#,
        )
        .await
        .unwrap();
        let (current, max) = read_input_tokens(tmp.path()).await;
        assert_eq!(current, Some(1000));
        assert_eq!(max, Some(2000));
    }

    #[tokio::test]
    async fn read_input_tokens_no_file() {
        let tmp = tempfile::tempdir().unwrap();
        let (current, max) = read_input_tokens(tmp.path()).await;
        assert_eq!(current, None);
        assert_eq!(max, None);
    }

    #[tokio::test]
    async fn read_input_tokens_zero_values() {
        let tmp = tempfile::tempdir().unwrap();
        let jyc_dir = tmp.path().join(".jyc");
        tokio::fs::create_dir_all(&jyc_dir).await.unwrap();
        tokio::fs::write(
            jyc_dir.join("agent-session.json"),
            r#"{"context_input_tokens": 0, "max_input_tokens": 0}"#,
        )
        .await
        .unwrap();
        let (current, max) = read_input_tokens(tmp.path()).await;
        assert_eq!(current, None);
        assert_eq!(max, None);
    }

    #[tokio::test]
    async fn read_input_tokens_invalid_json() {
        let tmp = tempfile::tempdir().unwrap();
        let jyc_dir = tmp.path().join(".jyc");
        tokio::fs::create_dir_all(&jyc_dir).await.unwrap();
        tokio::fs::write(jyc_dir.join("agent-session.json"), "not json")
            .await
            .unwrap();
        let (current, max) = read_input_tokens(tmp.path()).await;
        assert_eq!(current, None);
        assert_eq!(max, None);
    }

    #[tokio::test]
    async fn read_output_tokens_from_file() {
        let tmp = tempfile::tempdir().unwrap();
        let jyc_dir = tmp.path().join(".jyc");
        tokio::fs::create_dir_all(&jyc_dir).await.unwrap();
        tokio::fs::write(
            jyc_dir.join("agent-session.json"),
            r#"{"context_input_tokens":1000,"total_output_tokens":250,"max_input_tokens":2000}"#,
        )
        .await
        .unwrap();
        assert_eq!(read_output_tokens(tmp.path()).await, Some(250));
    }

    #[tokio::test]
    async fn read_output_tokens_no_file() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(read_output_tokens(tmp.path()).await, None);
    }

    #[tokio::test]
    async fn read_output_tokens_zero_value() {
        let tmp = tempfile::tempdir().unwrap();
        let jyc_dir = tmp.path().join(".jyc");
        tokio::fs::create_dir_all(&jyc_dir).await.unwrap();
        tokio::fs::write(
            jyc_dir.join("agent-session.json"),
            r#"{"total_output_tokens":0}"#,
        )
        .await
        .unwrap();
        assert_eq!(read_output_tokens(tmp.path()).await, None);
    }

    #[tokio::test]
    async fn read_total_input_tokens_from_file() {
        let tmp = tempfile::tempdir().unwrap();
        let jyc_dir = tmp.path().join(".jyc");
        tokio::fs::create_dir_all(&jyc_dir).await.unwrap();
        tokio::fs::write(
            jyc_dir.join("agent-session.json"),
            r#"{"context_input_tokens":1500,"total_input_tokens":4800,"total_output_tokens":250,"max_input_tokens":10000}"#,
        )
        .await
        .unwrap();
        assert_eq!(read_total_input_tokens(tmp.path()).await, Some(4800));
    }

    #[tokio::test]
    async fn read_total_input_tokens_missing_field_returns_none() {
        // Old session files (pre-#490 second commit) don't have
        // total_input_tokens; serde(default) on the field makes it 0,
        // and the helper converts 0 to None.
        let tmp = tempfile::tempdir().unwrap();
        let jyc_dir = tmp.path().join(".jyc");
        tokio::fs::create_dir_all(&jyc_dir).await.unwrap();
        tokio::fs::write(
            jyc_dir.join("agent-session.json"),
            r#"{"context_input_tokens":1000,"max_input_tokens":1000}"#,
        )
        .await
        .unwrap();
        assert_eq!(read_total_input_tokens(tmp.path()).await, None);
    }

    #[tokio::test]
    async fn read_token_state_all_six_fields() {
        let tmp = tempfile::tempdir().unwrap();
        let jyc_dir = tmp.path().join(".jyc");
        tokio::fs::create_dir_all(&jyc_dir).await.unwrap();
        tokio::fs::write(
            jyc_dir.join("agent-session.json"),
            r#"{"context_input_tokens":1500,"total_output_tokens":400,"total_input_tokens":9000,"total_cache_hit_tokens":3500,"max_input_tokens":10000}"#,
        )
        .await
        .unwrap();
        assert_eq!(
            read_token_state(tmp.path()).await,
            (
                Some(1500),
                Some(10000),
                Some(400),
                Some(9000),
                Some(3500),
                None
            )
        );
    }

    #[tokio::test]
    async fn read_token_state_legacy_file_missing_cache_hit() {
        // Pre-#493 session files don't carry `total_cache_hit_tokens`;
        // `#[serde(default)]` fills it with 0, which the helper maps to
        // `None` like the other zero-valued fields.
        let tmp = tempfile::tempdir().unwrap();
        let jyc_dir = tmp.path().join(".jyc");
        tokio::fs::create_dir_all(&jyc_dir).await.unwrap();
        tokio::fs::write(
            jyc_dir.join("agent-session.json"),
            r#"{"context_input_tokens":1500,"total_output_tokens":400,"max_input_tokens":10000}"#,
        )
        .await
        .unwrap();
        assert_eq!(
            read_token_state(tmp.path()).await,
            (Some(1500), Some(10000), Some(400), None, None, None)
        );
    }

    #[tokio::test]
    async fn read_token_state_no_file() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(
            read_token_state(tmp.path()).await,
            (None, None, None, None, None, None)
        );
    }

    #[tokio::test]
    async fn read_total_cache_hit_tokens_from_file() {
        let tmp = tempfile::tempdir().unwrap();
        let jyc_dir = tmp.path().join(".jyc");
        tokio::fs::create_dir_all(&jyc_dir).await.unwrap();
        tokio::fs::write(
            jyc_dir.join("agent-session.json"),
            r#"{"context_input_tokens":1500,"total_input_tokens":9000,"total_output_tokens":250,"total_cache_hit_tokens":4200,"max_input_tokens":10000}"#,
        )
        .await
        .unwrap();
        assert_eq!(read_total_cache_hit_tokens(tmp.path()).await, Some(4200));
    }

    #[tokio::test]
    async fn read_total_cache_hit_tokens_zero_value_returns_none() {
        // Zero is treated identically to "missing" — providers that don't
        // surface cache hits will leave this at 0 forever; the helper
        // surfaces None so the dashboard's `if let Some(...)` idiom
        // hides the row entirely.
        let tmp = tempfile::tempdir().unwrap();
        let jyc_dir = tmp.path().join(".jyc");
        tokio::fs::create_dir_all(&jyc_dir).await.unwrap();
        tokio::fs::write(
            jyc_dir.join("agent-session.json"),
            r#"{"total_cache_hit_tokens":0}"#,
        )
        .await
        .unwrap();
        assert_eq!(read_total_cache_hit_tokens(tmp.path()).await, None);
    }

    #[tokio::test]
    async fn read_total_cache_hit_tokens_missing_field_returns_none() {
        let tmp = tempfile::tempdir().unwrap();
        let jyc_dir = tmp.path().join(".jyc");
        tokio::fs::create_dir_all(&jyc_dir).await.unwrap();
        tokio::fs::write(
            jyc_dir.join("agent-session.json"),
            r#"{"context_input_tokens":1000,"max_input_tokens":1000}"#,
        )
        .await
        .unwrap();
        assert_eq!(read_total_cache_hit_tokens(tmp.path()).await, None);
    }

    #[tokio::test]
    async fn read_model_override_existing() {
        let tmp = tempfile::tempdir().unwrap();
        let jyc_dir = tmp.path().join(".jyc");
        tokio::fs::create_dir_all(&jyc_dir).await.unwrap();
        tokio::fs::write(jyc_dir.join("model-override"), "anthropic/claude-3.5\n")
            .await
            .unwrap();
        let result = read_model_override(tmp.path()).await;
        assert_eq!(result, Some("anthropic/claude-3.5".to_string()));
    }

    #[tokio::test]
    async fn read_model_override_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let jyc_dir = tmp.path().join(".jyc");
        tokio::fs::create_dir_all(&jyc_dir).await.unwrap();
        tokio::fs::write(jyc_dir.join("model-override"), "  \n")
            .await
            .unwrap();
        let result = read_model_override(tmp.path()).await;
        assert_eq!(result, None);
    }

    #[tokio::test]
    async fn read_model_override_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let result = read_model_override(tmp.path()).await;
        assert_eq!(result, None);
    }

    #[tokio::test]
    async fn read_mode_override_existing() {
        let tmp = tempfile::tempdir().unwrap();
        let jyc_dir = tmp.path().join(".jyc");
        tokio::fs::create_dir_all(&jyc_dir).await.unwrap();
        tokio::fs::write(jyc_dir.join("mode-override"), "static\n")
            .await
            .unwrap();
        let result = read_mode_override(tmp.path()).await;
        assert_eq!(result, Some("static".to_string()));
    }

    #[tokio::test]
    async fn read_mode_override_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let result = read_mode_override(tmp.path()).await;
        assert_eq!(result, None);
    }

    // ── resolve_reset_compression ─────────────────────────────────────

    use jyc_types::channel::{CompressionMode, ResetCompressionConfig};

    fn config_with_patterns(
        patterns: Vec<(&str, Option<ResetCompressionConfig>)>,
        global: Option<ResetCompressionConfig>,
    ) -> AppConfig {
        // Build a minimal TOML string. Mode values are the serde rename
        // values: "none" | "heuristic" | "llm". keep_pairs is required
        // when mode is set.
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
auto_reset_threshold = 0.95
"#,
        );
        for (name, rc) in &patterns {
            toml.push_str(&format!("\n[[channels.c.patterns]]\nname = \"{name}\"\n"));
            if let Some(c) = rc {
                let mode = match c.mode {
                    CompressionMode::None => "none",
                    CompressionMode::Heuristic => "heuristic",
                    CompressionMode::Llm => "llm",
                };
                toml.push_str(&format!(
                    "reset_compression = {{ mode = \"{mode}\", keep_pairs = {} }}\n",
                    c.keep_pairs
                ));
            }
        }
        if let Some(g) = &global {
            let mode = match g.mode {
                CompressionMode::None => "none",
                CompressionMode::Heuristic => "heuristic",
                CompressionMode::Llm => "llm",
            };
            toml.push_str(&format!(
                "reset_compression = {{ mode = \"{mode}\", keep_pairs = {} }}\n",
                g.keep_pairs
            ));
        }
        jyc_types::load_config_from_str(&toml).expect("config should parse")
    }

    #[test]
    fn resolve_reset_compression_uses_matched_pattern() {
        let none_cfg = ResetCompressionConfig {
            mode: CompressionMode::None,
            keep_pairs: 3,
        };
        let llm_cfg = ResetCompressionConfig {
            mode: CompressionMode::Llm,
            keep_pairs: 5,
        };
        let app = config_with_patterns(
            vec![("a", Some(none_cfg)), ("b", Some(llm_cfg.clone()))],
            None,
        );
        let resolved = resolve_reset_compression(&app, "c", Some("b"));
        assert_eq!(resolved.mode, CompressionMode::Llm);
        assert_eq!(resolved.keep_pairs, 5);
    }

    #[test]
    fn resolve_reset_compression_falls_back_to_first_pattern() {
        let none_cfg = ResetCompressionConfig {
            mode: CompressionMode::None,
            keep_pairs: 3,
        };
        let app = config_with_patterns(vec![("a", Some(none_cfg))], None);
        let resolved = resolve_reset_compression(&app, "c", None);
        assert_eq!(resolved.mode, CompressionMode::None);
    }

    #[test]
    fn resolve_reset_compression_falls_back_to_global() {
        let app = config_with_patterns(
            vec![],
            Some(ResetCompressionConfig {
                mode: CompressionMode::Llm,
                keep_pairs: 7,
            }),
        );
        let resolved = resolve_reset_compression(&app, "c", Some("a"));
        assert_eq!(resolved.mode, CompressionMode::Llm);
        assert_eq!(resolved.keep_pairs, 7);
    }

    #[test]
    fn resolve_reset_compression_falls_back_to_default() {
        let app = config_with_patterns(vec![], None);
        let resolved = resolve_reset_compression(&app, "c", Some("a"));
        // Default is Heuristic per #[derive(Default)] on CompressionMode
        assert_eq!(resolved.mode, CompressionMode::Heuristic);
        assert_eq!(resolved.keep_pairs, 3);
    }

    // ── write_max_input_tokens ────────────────────────────────────────

    #[tokio::test]
    async fn write_max_input_tokens_creates_file_when_missing() {
        let tmp = tempfile::tempdir().unwrap();
        write_max_input_tokens(tmp.path(), 12345).await;
        let content = tokio::fs::read_to_string(tmp.path().join(".jyc/agent-session.json"))
            .await
            .unwrap();
        let state: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert_eq!(state["max_input_tokens"], 12345);
    }

    #[tokio::test]
    async fn write_max_input_tokens_updates_existing() {
        let tmp = tempfile::tempdir().unwrap();
        let jyc_dir = tmp.path().join(".jyc");
        tokio::fs::create_dir_all(&jyc_dir).await.unwrap();
        tokio::fs::write(
            jyc_dir.join("agent-session.json"),
            r#"{"context_input_tokens":5000,"max_input_tokens":100000}"#,
        )
        .await
        .unwrap();
        write_max_input_tokens(tmp.path(), 250000).await;
        let content = tokio::fs::read_to_string(jyc_dir.join("agent-session.json"))
            .await
            .unwrap();
        let state: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert_eq!(state["max_input_tokens"], 250000);
        // Other fields preserved
        assert_eq!(state["context_input_tokens"], 5000);
    }

    #[tokio::test]
    async fn write_max_input_tokens_is_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        write_max_input_tokens(tmp.path(), 12345).await;
        let first_mtime = tokio::fs::metadata(tmp.path().join(".jyc/agent-session.json"))
            .await
            .unwrap()
            .modified()
            .unwrap();
        // Sleep a beat so mtime would change if we rewrote
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        write_max_input_tokens(tmp.path(), 12345).await;
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

    // ── resolve_context_strategy ──────────────────────────────────────

    use jyc_types::channel::{ContextStrategy, ContextStrategyConfig};

    fn config_with_strategies(
        patterns: Vec<(&str, Option<ContextStrategyConfig>)>,
        global: Option<ContextStrategyConfig>,
    ) -> AppConfig {
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
        for (name, s) in &patterns {
            toml.push_str(&format!("\n[[channels.c.patterns]]\nname = \"{name}\"\n"));
            if let Some(c) = s {
                let mode = match c.mode {
                    ContextStrategy::Full => "full",
                    ContextStrategy::SlidingWindow => "sliding_window",
                };
                toml.push_str(&format!(
                    "context_strategy = {{ mode = \"{mode}\", window = {} }}\n",
                    c.window
                ));
            }
        }
        if let Some(g) = &global {
            let mode = match g.mode {
                ContextStrategy::Full => "full",
                ContextStrategy::SlidingWindow => "sliding_window",
            };
            toml.push_str(&format!(
                "context_strategy = {{ mode = \"{mode}\", window = {} }}\n",
                g.window
            ));
        }
        jyc_types::load_config_from_str(&toml).expect("config should parse")
    }

    #[test]
    fn resolve_context_strategy_uses_matched_pattern() {
        let slide = ContextStrategyConfig {
            mode: ContextStrategy::SlidingWindow,
            window: 4,
        };
        let app = config_with_strategies(vec![("a", None), ("b", Some(slide.clone()))], None);
        let resolved = resolve_context_strategy(&app, "c", Some("b"));
        assert_eq!(resolved.mode, ContextStrategy::SlidingWindow);
        assert_eq!(resolved.window, 4);
    }

    #[test]
    fn resolve_context_strategy_falls_back_to_first_pattern() {
        let slide = ContextStrategyConfig {
            mode: ContextStrategy::SlidingWindow,
            window: 6,
        };
        let app = config_with_strategies(vec![("a", Some(slide.clone()))], None);
        let resolved = resolve_context_strategy(&app, "c", None);
        assert_eq!(resolved.mode, ContextStrategy::SlidingWindow);
        assert_eq!(resolved.window, 6);
    }

    #[test]
    fn resolve_context_strategy_falls_back_to_global() {
        let slide = ContextStrategyConfig {
            mode: ContextStrategy::SlidingWindow,
            window: 8,
        };
        let app = config_with_strategies(vec![], Some(slide.clone()));
        let resolved = resolve_context_strategy(&app, "c", Some("a"));
        assert_eq!(resolved.mode, ContextStrategy::SlidingWindow);
        assert_eq!(resolved.window, 8);
    }

    #[test]
    fn resolve_context_strategy_default_is_full_window10() {
        let app = config_with_strategies(vec![], None);
        let resolved = resolve_context_strategy(&app, "c", Some("a"));
        assert_eq!(resolved.mode, ContextStrategy::Full);
        assert_eq!(resolved.window, 10);
    }

    #[tokio::test]
    async fn read_context_strategy_override_roundtrip() {
        let tmp = tempfile::tempdir().unwrap();
        let jyc_dir = tmp.path().join(".jyc");
        tokio::fs::create_dir_all(&jyc_dir).await.unwrap();
        tokio::fs::write(
            jyc_dir.join(CONTEXT_STRATEGY_FILE),
            r#"{"mode":"sliding_window","window":3}"#,
        )
        .await
        .unwrap();
        let cfg = read_context_strategy_override(tmp.path()).await.unwrap();
        assert_eq!(cfg.mode, ContextStrategy::SlidingWindow);
        assert_eq!(cfg.window, 3);
    }

    #[tokio::test]
    async fn read_context_strategy_override_missing() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(read_context_strategy_override(tmp.path()).await.is_none());
    }

    #[tokio::test]
    async fn read_context_strategy_override_window_zero_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let jyc_dir = tmp.path().join(".jyc");
        tokio::fs::create_dir_all(&jyc_dir).await.unwrap();
        tokio::fs::write(
            jyc_dir.join(CONTEXT_STRATEGY_FILE),
            r#"{"mode":"sliding_window","window":0}"#,
        )
        .await
        .unwrap();
        assert!(read_context_strategy_override(tmp.path()).await.is_none());
    }
}
