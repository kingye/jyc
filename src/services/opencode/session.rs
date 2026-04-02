use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::Path;

use super::client::OpenCodeClient;
use crate::config::types::AgentConfig;

/// Per-thread session state, persisted in `.jyc/opencode-session.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionState {
    #[serde(rename = "sessionId")]
    pub session_id: String,
    #[serde(rename = "createdAt")]
    pub created_at: String,
    #[serde(rename = "lastUsedAt")]
    pub last_used_at: String,
}

/// Get or create a session for a thread.
///
/// 1. Read `.jyc/opencode-session.json`
/// 2. Verify session still exists via API
/// 3. If missing → create new session
pub async fn get_or_create_session(
    client: &OpenCodeClient,
    thread_path: &Path,
) -> Result<String> {
    let state_path = thread_path.join(".jyc").join("opencode-session.json");

    // Try loading existing session
    if state_path.exists() {
        if let Ok(content) = tokio::fs::read_to_string(&state_path).await {
            if let Ok(state) = serde_json::from_str::<SessionState>(&content) {
                // Verify session still exists
                match client.get_session(&state.session_id, thread_path).await {
                    Ok(Some(_)) => {
                        tracing::debug!(
                            session_id = %state.session_id,
                            "Reusing existing session"
                        );
                        return Ok(state.session_id);
                    }
                    Ok(None) => {
                        tracing::info!(
                            session_id = %state.session_id,
                            "Session no longer exists, creating new one"
                        );
                    }
                    Err(e) => {
                        tracing::warn!(
                            error = %e,
                            "Failed to verify session, creating new one"
                        );
                    }
                }
            }
        }
    }

    // Create new session
    create_new_session(client, thread_path).await
}

/// Create a fresh session for a thread.
pub async fn create_new_session(
    client: &OpenCodeClient,
    thread_path: &Path,
) -> Result<String> {
    let title = thread_path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "unnamed".to_string());

    let session = client
        .create_session(thread_path, &title)
        .await
        .context("failed to create session")?;

    // Persist session state
    let state = SessionState {
        session_id: session.id.clone(),
        created_at: chrono::Utc::now().to_rfc3339(),
        last_used_at: chrono::Utc::now().to_rfc3339(),
    };

    save_session_state(thread_path, &state).await?;

    tracing::info!(session_id = %session.id, "New session created");
    Ok(session.id)
}

/// Delete the session state file (for stale session recovery).
pub async fn delete_session(thread_path: &Path) -> Result<()> {
    let state_path = thread_path.join(".jyc").join("opencode-session.json");
    if state_path.exists() {
        tokio::fs::remove_file(&state_path).await.ok();
        tracing::debug!("Session state deleted");
    }
    Ok(())
}

/// Update the lastUsedAt timestamp.
pub async fn update_session_timestamp(thread_path: &Path) -> Result<()> {
    let state_path = thread_path.join(".jyc").join("opencode-session.json");
    if let Ok(content) = tokio::fs::read_to_string(&state_path).await {
        if let Ok(mut state) = serde_json::from_str::<SessionState>(&content) {
            state.last_used_at = chrono::Utc::now().to_rfc3339();
            save_session_state(thread_path, &state).await?;
        }
    }
    Ok(())
}

/// Save session state to `.jyc/opencode-session.json`.
async fn save_session_state(thread_path: &Path, state: &SessionState) -> Result<()> {
    let jyc_dir = thread_path.join(".jyc");
    tokio::fs::create_dir_all(&jyc_dir).await?;

    let state_path = jyc_dir.join("opencode-session.json");
    let content = serde_json::to_string_pretty(state)?;
    tokio::fs::write(&state_path, content).await?;
    Ok(())
}

// --- opencode.json management ---

/// Per-thread OpenCode configuration.
#[derive(Debug, Serialize, Deserialize)]
struct OpencodeConfig {
    #[serde(rename = "$schema")]
    schema: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    small_model: Option<String>,
    permission: serde_json::Value,
    mcp: serde_json::Value,
}

/// Ensure the thread has a properly configured `opencode.json`.
///
/// Returns `true` if the config was written (i.e., changed or new).
/// The caller should restart the OpenCode server if this returns true.
pub async fn ensure_thread_opencode_setup(
    thread_path: &Path,
    agent_config: &AgentConfig,
    jyc_root: &Path,
) -> Result<bool> {
    // Read model override
    let model = read_model_override(thread_path)
        .await
        .or_else(|| {
            agent_config
                .opencode
                .as_ref()
                .and_then(|o| o.model.clone())
        });

    let small_model = if read_model_override(thread_path).await.is_some() {
        None // Model override disables small_model
    } else {
        agent_config
            .opencode
            .as_ref()
            .and_then(|o| o.small_model.clone())
    };

    // Build the reply tool command
    let tool_command = get_reply_tool_command();

    let new_config = OpencodeConfig {
        schema: "https://opencode.ai/config.json".to_string(),
        model,
        small_model,
        permission: serde_json::json!({
            "*": "allow",
            "question": "deny"
        }),
        mcp: serde_json::json!({
            "jiny_reply": {
                "type": "local",
                "command": tool_command,
                "environment": {
                    "JYC_ROOT": jyc_root.to_string_lossy()
                },
                "enabled": true,
                "timeout": 60000
            }
        }),
    };

    let config_path = thread_path.join("opencode.json");
    let new_content = serde_json::to_string_pretty(&new_config)?;

    // Staleness check: skip write if unchanged
    if config_path.exists() {
        if let Ok(existing) = tokio::fs::read_to_string(&config_path).await {
            if existing.trim() == new_content.trim() {
                return Ok(false); // No change
            }
        }
    }

    // Create .opencode/ directory
    tokio::fs::create_dir_all(thread_path.join(".opencode")).await?;

    // Write new config
    tokio::fs::write(&config_path, &new_content).await?;
    tracing::info!(
        path = %config_path.display(),
        "opencode.json written"
    );

    Ok(true)
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

/// Get the reply tool command for opencode.json MCP config.
///
/// Resolves the jyc binary path and returns `["/path/to/jyc", "mcp-reply-tool"]`.
fn get_reply_tool_command() -> Vec<String> {
    // Try current executable path
    if let Ok(exe) = std::env::current_exe() {
        let exe_str = exe.to_string_lossy().to_string();
        return vec![exe_str, "mcp-reply-tool".to_string()];
    }

    // Fallback: check common paths
    for path in &["/usr/local/bin/jyc", "/usr/bin/jyc"] {
        if Path::new(path).exists() {
            return vec![path.to_string(), "mcp-reply-tool".to_string()];
        }
    }

    // Last resort
    vec!["jyc".to_string(), "mcp-reply-tool".to_string()]
}

// --- Signal file ---

/// Clean up stale signal file before starting a new prompt.
pub async fn cleanup_signal_file(thread_path: &Path) {
    let flag_path = thread_path.join(".jyc").join("reply-sent.flag");
    if flag_path.exists() {
        tokio::fs::remove_file(&flag_path).await.ok();
    }
}

/// Check if the signal file exists (reply sent by MCP tool).
pub async fn check_signal_file(thread_path: &Path) -> bool {
    let flag_path = thread_path.join(".jyc").join("reply-sent.flag");
    flag_path.exists()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_session_state_round_trip() {
        let tmp = tempfile::tempdir().unwrap();
        let thread_path = tmp.path().join("test-thread");
        tokio::fs::create_dir_all(&thread_path).await.unwrap();

        let state = SessionState {
            session_id: "sess_123".to_string(),
            created_at: "2026-03-27T10:00:00Z".to_string(),
            last_used_at: "2026-03-27T10:00:00Z".to_string(),
        };

        save_session_state(&thread_path, &state).await.unwrap();

        let state_path = thread_path.join(".jyc").join("opencode-session.json");
        assert!(state_path.exists());

        let content = tokio::fs::read_to_string(&state_path).await.unwrap();
        let loaded: SessionState = serde_json::from_str(&content).unwrap();
        assert_eq!(loaded.session_id, "sess_123");
    }

    #[tokio::test]
    async fn test_signal_file() {
        let tmp = tempfile::tempdir().unwrap();
        let thread_path = tmp.path().join("test-thread");
        let jyc_dir = thread_path.join(".jyc");
        tokio::fs::create_dir_all(&jyc_dir).await.unwrap();

        assert!(!check_signal_file(&thread_path).await);

        tokio::fs::write(jyc_dir.join("reply-sent.flag"), "{}").await.unwrap();
        assert!(check_signal_file(&thread_path).await);

        cleanup_signal_file(&thread_path).await;
        assert!(!check_signal_file(&thread_path).await);
    }

    #[test]
    fn test_get_reply_tool_command() {
        let cmd = get_reply_tool_command();
        assert!(cmd.len() >= 2);
        assert_eq!(cmd.last().unwrap(), "mcp-reply-tool");
    }
}
