use serde::Deserialize;
use std::path::Path;

/// Read input tokens from the agent session state file.
/// Returns (current_tokens, max_tokens).
pub async fn read_input_tokens(thread_path: &Path) -> (Option<u64>, Option<u64>) {
    let agent_path = thread_path.join(".jyc").join("agent-session.json");
    if let Ok(content) = tokio::fs::read_to_string(&agent_path).await
        && let Ok(state) = serde_json::from_str::<AgentSessionState>(&content)
    {
        let current = if state.total_input_tokens > 0 {
            Some(state.total_input_tokens)
        } else {
            None
        };
        let max = if state.max_input_tokens > 0 {
            Some(state.max_input_tokens)
        } else {
            None
        };
        if current.is_some() || max.is_some() {
            return (current, max);
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
}
