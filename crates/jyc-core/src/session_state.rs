use serde::{Deserialize, Serialize};
use std::path::Path;

/// Per-thread session state, persisted in `.jyc/opencode-session.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionState {
    #[serde(rename = "sessionId")]
    pub session_id: String,
    #[serde(rename = "createdAt")]
    pub created_at: String,
    #[serde(rename = "lastUsedAt")]
    pub last_used_at: String,
    /// Current input tokens (from latest step-finish SSE event)
    #[serde(rename = "totalInputTokens", default)]
    pub total_input_tokens: u64,
    /// Resolved max input tokens for this session
    #[serde(rename = "maxInputTokens", default)]
    pub max_input_tokens: u64,
}

/// Read input tokens from the session state file.
/// Returns (current_tokens, max_tokens).
/// Checks both opencode-session.json and agent-session.json.
pub async fn read_input_tokens(thread_path: &Path) -> (Option<u64>, Option<u64>) {
    // Try opencode-session.json first (OpenCode mode)
    let opencode_path = thread_path.join(".jyc").join("opencode-session.json");
    if let Some(result) = read_tokens_from_file(&opencode_path).await {
        return result;
    }

    // Try agent-session.json (jyc-agent mode)
    let agent_path = thread_path.join(".jyc").join("agent-session.json");
    if let Some(result) = read_agent_tokens_from_file(&agent_path).await {
        return result;
    }

    (None, None)
}

async fn read_tokens_from_file(path: &Path) -> Option<(Option<u64>, Option<u64>)> {
    let content = tokio::fs::read_to_string(path).await.ok()?;
    let state: SessionState = serde_json::from_str(&content).ok()?;
    let current = if state.total_input_tokens > 0 { Some(state.total_input_tokens) } else { None };
    let max = if state.max_input_tokens > 0 { Some(state.max_input_tokens) } else { None };
    if current.is_some() || max.is_some() {
        Some((current, max))
    } else {
        None
    }
}

/// Agent session state format (from jyc-agent crate).
#[derive(Debug, Deserialize)]
struct AgentSessionState {
    #[serde(default)]
    total_input_tokens: u64,
    #[serde(default)]
    total_output_tokens: u64,
    #[serde(default)]
    max_input_tokens: u64,
}

async fn read_agent_tokens_from_file(path: &Path) -> Option<(Option<u64>, Option<u64>)> {
    let content = tokio::fs::read_to_string(path).await.ok()?;
    let state: AgentSessionState = serde_json::from_str(&content).ok()?;
    let current = if state.total_input_tokens > 0 { Some(state.total_input_tokens) } else { None };
    let max = if state.max_input_tokens > 0 { Some(state.max_input_tokens) } else { None };
    if current.is_some() || max.is_some() {
        Some((current, max))
    } else {
        None
    }
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
