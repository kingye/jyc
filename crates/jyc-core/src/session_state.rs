use serde::Deserialize;
use std::path::Path;

/// Read input tokens from the agent session state file.
/// Returns (current_tokens, max_tokens).
pub async fn read_input_tokens(thread_path: &Path) -> (Option<u64>, Option<u64>) {
    let agent_path = thread_path.join(".jyc").join("agent-session.json");
    if let Ok(content) = tokio::fs::read_to_string(&agent_path).await {
        if let Ok(state) = serde_json::from_str::<AgentSessionState>(&content) {
            let current = if state.total_input_tokens > 0 { Some(state.total_input_tokens) } else { None };
            let max = if state.max_input_tokens > 0 { Some(state.max_input_tokens) } else { None };
            if current.is_some() || max.is_some() {
                return (current, max);
            }
        }
    }
    (None, None)
}

/// Agent session state format.
#[derive(Debug, Deserialize)]
struct AgentSessionState {
    #[serde(default)]
    total_input_tokens: u64,
    #[serde(default)]
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
