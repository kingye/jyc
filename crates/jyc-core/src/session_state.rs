use jyc_types::AppConfig;
use jyc_types::channel::ResetCompressionConfig;
use serde::Deserialize;
use std::path::Path;

/// Fallback context window when neither the per-model nor the per-provider
/// setting is configured. Mirrors the constant in `jyc-agent::service`.
pub const DEFAULT_CONTEXT_WINDOW: u64 = 128000;

/// Read input tokens from the agent session state file.
/// Returns (current_tokens, max_tokens).
pub async fn read_input_tokens(thread_path: &Path) -> (Option<u64>, Option<u64>) {
    let (cur, max, _) = read_token_state(thread_path).await;
    (cur, max)
}

/// Read accumulated output tokens from the agent session state file.
/// Returns `None` when the file is missing, malformed, or the value is zero.
/// The session file already deserializes `total_output_tokens` — this just
/// surfaces it. Output tokens accumulate across LLM calls in a round.
pub async fn read_output_tokens(thread_path: &Path) -> Option<u64> {
    let (_, _, out) = read_token_state(thread_path).await;
    out
}

/// Read all three token fields in a single file read.
/// Returns (current_input_tokens, max_input_tokens, output_tokens). Any
/// individual field is `None` when the file is missing, malformed, or the
/// underlying value is zero.
///
/// Callers that need all three fields (e.g. `thread_manager::list_threads`)
/// should use this rather than calling `read_input_tokens` and
/// `read_output_tokens` separately — saves one file open + JSON parse per
/// call. The two single-purpose helpers above are thin wrappers retained
/// for callers that only need one field.
pub async fn read_token_state(thread_path: &Path) -> (Option<u64>, Option<u64>, Option<u64>) {
    let agent_path = thread_path.join(".jyc").join("agent-session.json");
    let Ok(content) = tokio::fs::read_to_string(&agent_path).await else {
        return (None, None, None);
    };
    let Ok(state) = serde_json::from_str::<AgentSessionState>(&content) else {
        return (None, None, None);
    };
    let current = (state.total_input_tokens > 0).then_some(state.total_input_tokens);
    let max = (state.max_input_tokens > 0).then_some(state.max_input_tokens);
    let output = (state.total_output_tokens > 0).then_some(state.total_output_tokens);
    (current, max, output)
}

/// Agent session state format.
#[derive(Debug, Deserialize)]
struct AgentSessionState {
    #[serde(default)]
    total_input_tokens: u64,
    #[serde(default)]
    #[allow(dead_code)]
    total_output_tokens: u64,
    #[serde(default)]
    max_input_tokens: u64,
}

/// Read the model override file if it exists.
pub async fn read_model_override(thread_path: &Path) -> Option<String> {
    let override_path = thread_path.join(".jyc").join("model-override");
    tokio::fs::read_to_string(override_path)
        .await
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Read the mode override file if it exists.
pub async fn read_mode_override(thread_path: &Path) -> Option<String> {
    let override_path = thread_path.join(".jyc").join("mode-override");
    tokio::fs::read_to_string(override_path)
        .await
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Read the matched pattern file if it exists.
pub async fn read_pattern(thread_path: &Path) -> Option<String> {
    let pattern_path = thread_path.join(".jyc").join("pattern");
    tokio::fs::read_to_string(pattern_path)
        .await
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Resolve the `ResetCompressionConfig` for a thread.
///
/// Priority: matched pattern > first pattern > global `[agent].reset_compression` > default.
///
/// Pass `matched_pattern = Some(name)` when the caller has access to the
/// pattern that created the thread (e.g. `process()` with
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
        .or_else(|| config.agent.reset_compression.clone())
        .unwrap_or_default()
}

/// Resolve the `max_input_tokens` value the session should have for the
/// currently active model on this thread.
///
/// Returns `None` if no model can be resolved (no config, no providers, etc.).
/// Callers should treat `None` as a no-op.
pub async fn resolve_active_context_window(
    thread_path: &Path,
    config: &AppConfig,
    channel: &str,
    auto_reset_threshold: f64,
) -> Option<u64> {
    let model_override = resolve_active_model(thread_path, config, channel).await?;
    let context_window = context_window_for_model(&model_override, config)?;
    Some((context_window as f64 * auto_reset_threshold) as u64)
}

/// Idempotently write `max_input_tokens` to `.jyc/agent-session.json`.
/// No-op if the value would not change (avoids disk churn).
pub async fn write_max_input_tokens(thread_path: &Path, new_max: u64) {
    let session_path = thread_path.join(".jyc").join("agent-session.json");
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

/// Resolve the active model string (`<provider>/<model-id>`) for this thread.
/// Mirrors the chain in `jyc-agent::service::process()` (lines 1191-1263).
async fn resolve_active_model(
    thread_path: &Path,
    config: &AppConfig,
    channel: &str,
) -> Option<String> {
    let mode_override = read_mode_override(thread_path).await;
    let pattern_name = read_pattern(thread_path).await;

    // 1. Mode-specific file override
    let file_override = {
        let mode_suffix = match mode_override.as_deref() {
            Some("plan") => "plan",
            _ => "build",
        };
        let mode_specific = thread_path
            .join(".jyc")
            .join(format!("{mode_suffix}-model-override"));
        let legacy = thread_path.join(".jyc").join("model-override");
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

    // 2. Thread config (`.jyc/config.toml`) — read once, then check both
    //    the mode-specific and the generic `model` field against the same
    //    loaded value (avoids re-reading the file in the common no-config case).
    let thread_cfg = jyc_types::load_thread_config(thread_path).and_then(|c| c.agent);
    let thread_cfg_override = thread_cfg
        .as_ref()
        .and_then(|a| match mode_override.as_deref() {
            Some("plan") => a.plan_model.clone(),
            _ => a.build_model.clone(),
        })
        .or_else(|| thread_cfg.as_ref().and_then(|a| a.model.clone()));

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
    let mut effective_agent = config.agent.clone();
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
        .or(thread_cfg_override)
        .or(pattern_override)
        .or(config_override)
}

/// Resolve the `context_window` for a model string (`<provider>/<model-id>`).
/// Per-model override > provider default > `DEFAULT_CONTEXT_WINDOW`.
fn context_window_for_model(model_str: &str, config: &AppConfig) -> Option<u64> {
    if let Some((provider_name, model_id)) = model_str.split_once('/')
        && let Some(provider) = config.agent.providers.get(provider_name)
    {
        let per_model = provider.models.get(model_id).and_then(|m| m.context_window);
        if let Some(cw) = per_model.or(provider.context_window) {
            return Some(cw);
        }
    }
    Some(DEFAULT_CONTEXT_WINDOW)
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
            r#"{"total_input_tokens": 1000, "max_input_tokens": 2000}"#,
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
            r#"{"total_input_tokens": 0, "max_input_tokens": 0}"#,
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
            r#"{"total_input_tokens":1000,"total_output_tokens":250,"max_input_tokens":2000}"#,
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
    async fn read_token_state_all_three_fields() {
        let tmp = tempfile::tempdir().unwrap();
        let jyc_dir = tmp.path().join(".jyc");
        tokio::fs::create_dir_all(&jyc_dir).await.unwrap();
        tokio::fs::write(
            jyc_dir.join("agent-session.json"),
            r#"{"total_input_tokens":1500,"total_output_tokens":400,"max_input_tokens":10000}"#,
        )
        .await
        .unwrap();
        assert_eq!(
            read_token_state(tmp.path()).await,
            (Some(1500), Some(10000), Some(400))
        );
    }

    #[tokio::test]
    async fn read_token_state_no_file() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(read_token_state(tmp.path()).await, (None, None, None));
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
            r#"{"total_input_tokens":5000,"max_input_tokens":100000}"#,
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
        assert_eq!(state["total_input_tokens"], 5000);
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
}
