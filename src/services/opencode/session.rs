use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::Path;

use super::client::OpenCodeClient;
use crate::config::types::{AgentConfig, AppConfig, McpServerConfig, McpServerKind};

/// Default maximum input tokens per session before resetting
pub const DEFAULT_MAX_INPUT_TOKENS: u64 = 120 * 1024; // 120K tokens

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





/// Get or create a session for a thread.
///
/// 1. Read `.jyc/opencode-session.json`
/// 2. Check if session has exceeded maximum input tokens (default: 108K)
/// 3. If exceeded → delete old session and create new one
/// 4. Verify session still exists via API
/// 5. If missing → create new session
///
/// Returns: (session_id, session_was_reset_due_to_token_limit)
pub async fn get_or_create_session(
    client: &OpenCodeClient,
    thread_path: &Path,
    max_input_tokens: Option<u64>,
) -> Result<(String, bool)> {
    let state_path = thread_path.join(".jyc").join("opencode-session.json");

    // Try loading existing session
    if state_path.exists() {
        if let Ok(content) = tokio::fs::read_to_string(&state_path).await {
            if let Ok(state) = serde_json::from_str::<SessionState>(&content) {
                // Check if session has exceeded maximum input tokens
                let max_tokens = max_input_tokens.unwrap_or(DEFAULT_MAX_INPUT_TOKENS);
                if state.total_input_tokens >= max_tokens {
                    tracing::info!(
                        session_id = %state.session_id,
                        total_input_tokens = state.total_input_tokens,
                        max_input_tokens = max_tokens,
                        "Session exceeded maximum input tokens, resetting"
                    );

                    // Delete old session and create new one
                    delete_session(thread_path).await?;
                    let new_session_id = create_new_session(client, thread_path).await?;
                    return Ok((new_session_id, true));
                }

                // Verify session still exists
                match client.get_session(&state.session_id, thread_path).await {
                    Ok(Some(_)) => {
                        tracing::debug!(
                            session_id = %state.session_id,
                            "Reusing existing session"
                        );
                        // Update last_used_at when session is reused
                        let mut updated_state = state.clone();
                        updated_state.last_used_at = chrono::Utc::now().to_rfc3339();
                        let _ = save_session_state(thread_path, &updated_state).await;
                        return Ok((state.session_id, false));
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
    let session_id = create_new_session(client, thread_path).await?;
    Ok((session_id, false))
}

/// Create a fresh session for a thread.
pub async fn create_new_session(client: &OpenCodeClient, thread_path: &Path) -> Result<String> {
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
        total_input_tokens: 0,
        max_input_tokens: 0,
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



/// Update input tokens in session state (write raw value from SSE, not accumulated).
pub async fn add_input_tokens(thread_path: &Path, input_tokens: u64) -> Result<()> {
    let state_path = thread_path.join(".jyc").join("opencode-session.json");
    tracing::debug!("add_input_tokens: path={}, tokens={}", state_path.display(), input_tokens);
    
    if !state_path.exists() {
        tracing::warn!("Session state file does not exist: {}", state_path.display());
        return Ok(());
    }
    
    match tokio::fs::read_to_string(&state_path).await {
        Ok(content) => {
            tracing::debug!("Read session file, length: {}", content.len());
            match serde_json::from_str::<SessionState>(&content) {
                Ok(mut state) => {
                    let old_tokens = state.total_input_tokens;
                    state.total_input_tokens = input_tokens;
                    state.last_used_at = chrono::Utc::now().to_rfc3339();
                    
                    tracing::info!(
                        session_id = %state.session_id,
                        input_tokens = input_tokens,
                        previous_input_tokens = old_tokens,
                        "Updated input tokens in session"
                    );
                    
                    match save_session_state(thread_path, &state).await {
                        Ok(_) => {
                            tracing::debug!("Successfully saved updated session state");
                            Ok(())
                        }
                        Err(e) => {
                            tracing::error!(error = %e, "Failed to save session state");
                            Err(e)
                        }
                    }
                }
                Err(e) => {
                    tracing::error!(error = %e, "Failed to parse session state JSON");
                    // Try to create a new session state
                    tracing::warn!("Creating new session state due to parse error");
                    Err(e.into())
                }
            }
        }
        Err(e) => {
            tracing::error!(error = %e, "Failed to read session state file");
            Err(e.into())
        }
    }
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
    #[serde(skip_serializing_if = "Option::is_none")]
    agent: Option<serde_json::Value>,
    mcp: serde_json::Value,
}

/// Ensure the thread has a properly configured `opencode.json`.
///
/// Returns `true` if the config was written (i.e., changed or new).
/// The caller should restart the OpenCode server if this returns true.
pub async fn ensure_thread_opencode_setup(
    thread_path: &Path,
    agent_config: &AgentConfig,
    app_config: &AppConfig,
    jyc_root: &Path,
) -> Result<bool> {
    // Read model override
    let model = read_model_override(thread_path)
        .await
        .or_else(|| agent_config.opencode.as_ref().and_then(|o| o.model.clone()));

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
    let question_command = get_question_tool_command();

    // Build MCP tools configuration
    let thread_dir_str = thread_path.to_string_lossy().to_string();
    let mut mcp_tools = serde_json::json!({
        "jyc_reply": {
            "type": "local",
            "command": tool_command,
            "environment": {
                "JYC_ROOT": jyc_root.to_string_lossy(),
                "JYC_THREAD_DIR": &thread_dir_str
            },
            "enabled": true,
            "timeout": 180000
        },
        "jyc_question": {
            "type": "local",
            "command": question_command,
            "environment": {
                "JYC_THREAD_DIR": &thread_dir_str
            },
            "enabled": true,
            "timeout": 360000
        }
    });

    // Read template MCPs from .jyc/mcps.json
    let mcps_path = thread_path.join(".jyc").join("mcps.json");
    let template_mcps: Vec<String> = if mcps_path.exists() {
        let content = tokio::fs::read_to_string(&mcps_path)
            .await
            .context("failed to read mcps.json")?;
        serde_json::from_str(&content)
            .inspect_err(|_| {
                tracing::warn!("Failed to parse {}", mcps_path.display());
            })
            .unwrap_or_default()
    } else {
        Vec::new()
    };

    // Read extra MCPs from .jyc/extra-mcps.json (runtime-injected)
    let extra_mcps_path = thread_path.join(".jyc").join("extra-mcps.json");
    let extra_mcps: Vec<String> = if extra_mcps_path.exists() {
        let content = tokio::fs::read_to_string(&extra_mcps_path)
            .await
            .context("failed to read extra-mcps.json")?;
        serde_json::from_str(&content)
            .inspect_err(|_| {
                tracing::warn!("Failed to parse {}", extra_mcps_path.display());
            })
            .unwrap_or_default()
    } else {
        Vec::new()
    };

    // Combine template MCPs and extra MCPs (extra takes precedence via later override)
    let all_mcp_names: Vec<String> = template_mcps
        .into_iter()
        .chain(extra_mcps)
        .collect();

    // Read deploy-time MCP definitions from .jyc/mcp-defs.json
    let mcp_defs_path = thread_path.join(".jyc").join("mcp-defs.json");
    let deploy_mcp_defs: Vec<McpServerConfig> = if mcp_defs_path.exists() {
        let content = tokio::fs::read_to_string(&mcp_defs_path)
            .await
            .context("failed to read mcp-defs.json")?;
        serde_json::from_str(&content)
            .inspect_err(|_| {
                tracing::warn!("Failed to parse {}", mcp_defs_path.display());
            })
            .unwrap_or_default()
    } else {
        Vec::new()
    };

    // Build a map of MCP name -> McpServerConfig for quick lookup.
    // Deploy-time definitions (mcp-defs.json) take precedence over app config.
    let mut mcp_config_map: std::collections::HashMap<&str, &McpServerConfig> =
        app_config.mcps.iter().map(|m| (m.name.as_str(), m)).collect();
    for def in &deploy_mcp_defs {
        mcp_config_map.insert(def.name.as_str(), def);
    }

    // Add template/extra MCPs to opencode.json
    for mcp_name in &all_mcp_names {
        if let Some(mcp_config) = mcp_config_map.get(mcp_name.as_str()) {
            let mcp_json = mcp_config_to_json(mcp_config, &thread_dir_str);
            mcp_tools[mcp_name] = mcp_json;
            tracing::debug!(mcp = %mcp_name, "Added MCP to opencode.json");
        } else {
            tracing::warn!(mcp = %mcp_name, "MCP not found in config, skipping");
        }
    }

    let new_config = OpencodeConfig {
        schema: "https://opencode.ai/config.json".to_string(),
        model,
        small_model,
        // Allow external_directory if the thread contains any symlinks pointing outside.
        // This prevents plan mode sub-agent deadlock (external_directory:ask + question:deny)
        // while keeping threads without external references restricted.
        permission: {
            let needs_external = has_external_symlinks(thread_path);
            if needs_external {
                tracing::debug!("Thread has external symlinks, allowing external_directory");
                serde_json::json!({
                    "question": "deny",
                    "external_directory": "allow"
                })
            } else {
                serde_json::json!({
                    "question": "deny"
                })
            }
        },
        agent: Some(serde_json::json!({
            "build": {
                "temperature": 0.1,
                "permission": {
                    "*": "allow",
                    "question": "deny"
                }
            }
        })),
        mcp: mcp_tools,
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

/// Convert an McpServerConfig into its opencode.json JSON representation.
fn mcp_config_to_json(config: &McpServerConfig, thread_dir: &str) -> serde_json::Value {
    match &config.kind {
        McpServerKind::Local { command, environment, timeout } => {
            let mut env_with_thread = environment.clone();
            env_with_thread.insert("JYC_THREAD_DIR".to_string(), thread_dir.to_string());
            serde_json::json!({
                "type": "local",
                "command": command,
                "environment": env_with_thread,
                "enabled": true,
                "timeout": timeout
            })
        }
        McpServerKind::Remote { url, enabled } => {
            serde_json::json!({
                "type": "remote",
                "url": url,
                "enabled": enabled
            })
        }
    }
}

/// Read the current and max input tokens from session state.
pub async fn read_input_tokens(thread_path: &Path) -> (Option<u64>, Option<u64>) {
    let state_path = thread_path.join(".jyc").join("opencode-session.json");
    let content = match tokio::fs::read_to_string(&state_path).await.ok() {
        Some(c) => c,
        None => return (None, None),
    };
    let state: SessionState = match serde_json::from_str(&content).ok() {
        Some(s) => s,
        None => return (None, None),
    };
    let current = if state.total_input_tokens > 0 { Some(state.total_input_tokens) } else { None };
    let max = if state.max_input_tokens > 0 { Some(state.max_input_tokens) } else { None };
    (current, max)
}

/// Save the resolved max_input_tokens to the session state.
pub async fn save_max_input_tokens(thread_path: &Path, max_tokens: u64) -> Result<()> {
    let state_path = thread_path.join(".jyc").join("opencode-session.json");
    if !state_path.exists() {
        return Ok(());
    }
    let content = tokio::fs::read_to_string(&state_path).await?;
    let mut state: SessionState = serde_json::from_str(&content)?;
    state.max_input_tokens = max_tokens;
    save_session_state(thread_path, &state).await
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
    get_mcp_tool_command("mcp-reply-tool")
}

/// Check if the thread directory contains any symlinks (up to 3 levels deep).
/// Symlinks typically point to external content (e.g., jyc repo, shared skills).
fn has_external_symlinks(thread_path: &Path) -> bool {
    fn scan_dir(dir: &Path, depth: u8) -> bool {
        if depth == 0 {
            return false;
        }
        let Ok(entries) = std::fs::read_dir(dir) else {
            return false;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_symlink() {
                return true;
            }
            if path.is_dir() && !path.file_name().is_some_and(|n| n == "messages" || n == "attachments" || n == "node_modules") {
                if scan_dir(&path, depth - 1) {
                    return true;
                }
            }
        }
        false
    }
    scan_dir(thread_path, 3)
}

fn get_question_tool_command() -> Vec<String> {
    get_mcp_tool_command("mcp-question-tool")
}

/// Resolve the command to invoke a jyc MCP tool subcommand.
fn get_mcp_tool_command(subcommand: &str) -> Vec<String> {
    // Try current executable path
    if let Ok(exe) = std::env::current_exe() {
        let exe_str = exe.to_string_lossy().to_string();
        return vec![exe_str, subcommand.to_string()];
    }

    // Fallback: check common paths
    for path in &["/usr/local/bin/jyc", "/usr/bin/jyc"] {
        if Path::new(path).exists() {
            return vec![path.to_string(), subcommand.to_string()];
        }
    }

    // Last resort
    vec!["jyc".to_string(), subcommand.to_string()]
}





/// Calculate session duration in seconds.
#[allow(dead_code)]
pub fn calculate_session_duration(state: &SessionState) -> Result<u64> {
    let created = chrono::DateTime::parse_from_rfc3339(&state.created_at)
        .context("failed to parse created_at timestamp")?;
    let last_used = chrono::DateTime::parse_from_rfc3339(&state.last_used_at)
        .context("failed to parse last_used_at timestamp")?;

    let duration = last_used.signed_duration_since(created);
    Ok(duration.num_seconds().max(0) as u64)
}













/// Count messages in the thread by scanning chat log files.
///
/// Counts `type:received` entries in `chat_history_*.md` files.
/// Falls back to counting `messages/` subdirectories for legacy threads.
#[allow(dead_code)]
pub async fn count_messages(thread_path: &Path) -> Result<usize> {
    // Primary: count entries in chat log files
    let pattern = thread_path.join("chat_history_*.md");
    let pattern_str = pattern.to_string_lossy();
    let mut count = 0;

    for entry in glob::glob(&pattern_str).into_iter().flatten().flatten() {
        if let Ok(content) = tokio::fs::read_to_string(&entry).await {
            count += content.matches("type:received").count();
        }
    }

    if count > 0 {
        return Ok(count);
    }

    // Fallback: count legacy messages/ subdirectories
    let messages_dir = thread_path.join("messages");
    if !messages_dir.exists() {
        return Ok(0);
    }

    let mut legacy_count = 0;
    let mut entries = tokio::fs::read_dir(&messages_dir).await?;

    while let Some(entry) = entries.next_entry().await? {
        if entry.file_type().await?.is_dir() {
            legacy_count += 1;
        }
    }

    if legacy_count > 0 {
        tracing::debug!(
            legacy_count,
            "count_messages: using legacy messages/ directory count"
        );
    }

    Ok(legacy_count)
}

/// Create a basic session summary from session state.


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
            total_input_tokens: 0,
            max_input_tokens: 0,
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

        tokio::fs::write(jyc_dir.join("reply-sent.flag"), "{}")
            .await
            .unwrap();
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





    #[test]
    fn test_calculate_session_duration() {
        let start = chrono::DateTime::parse_from_rfc3339("2026-04-04T10:00:00Z").unwrap();
        let end = chrono::DateTime::parse_from_rfc3339("2026-04-04T12:00:00Z").unwrap();

        let state = SessionState {
            session_id: "test-session".to_string(),
            created_at: start.to_rfc3339(),
            last_used_at: end.to_rfc3339(),
            total_input_tokens: 0,
            max_input_tokens: 0,
        };

        let duration = calculate_session_duration(&state).unwrap();
        assert_eq!(
            duration, 7200,
            "Session duration should be 2 hours (7200 seconds)"
        );
    }

    #[test]
    fn test_mcp_config_to_json_local() {
        use crate::config::types::{McpServerConfig, McpServerKind};
        use std::collections::HashMap;

        let config = McpServerConfig {
            name: "test_mcp".to_string(),
            kind: McpServerKind::Local {
                command: vec!["jyc".to_string(), "mcp-tool".to_string()],
                environment: HashMap::from([
                    ("VAR1".to_string(), "value1".to_string()),
                ]),
                timeout: 300000,
            },
        };

        let json = mcp_config_to_json(&config, "/thread/path");
        assert_eq!(json["type"], "local");
        assert_eq!(json["command"], serde_json::json!(["jyc", "mcp-tool"]));
        assert_eq!(json["environment"]["VAR1"], "value1");
        assert_eq!(json["environment"]["JYC_THREAD_DIR"], "/thread/path");
        assert_eq!(json["enabled"], true);
        assert_eq!(json["timeout"], 300000);
    }

    #[test]
    fn test_mcp_config_to_json_remote() {
        use crate::config::types::{McpServerConfig, McpServerKind};

        let config = McpServerConfig {
            name: "remote_mcp".to_string(),
            kind: McpServerKind::Remote {
                url: "https://mcp.example.com/handler".to_string(),
                enabled: true,
            },
        };

        let json = mcp_config_to_json(&config, "/thread/path");
        assert_eq!(json["type"], "remote");
        assert_eq!(json["url"], "https://mcp.example.com/handler");
        assert_eq!(json["enabled"], true);
    }

    #[tokio::test]
    async fn test_ensure_thread_opencode_setup_with_template_mcps() {
        use crate::config::types::{AgentConfig, AppConfig, McpServerConfig, McpServerKind, HeartbeatConfig};
        use std::collections::HashMap;

        let tmp = tempfile::tempdir().unwrap();
        let thread_path = tmp.path().join("test-thread");
        tokio::fs::create_dir_all(&thread_path).await.unwrap();
        tokio::fs::create_dir_all(thread_path.join(".jyc")).await.unwrap();

        // Write mcps.json
        tokio::fs::write(
            thread_path.join(".jyc").join("mcps.json"),
            r#"["jyc_vision"]"#,
        )
        .await
        .unwrap();

        // Create app_config with MCP definitions
        let app_config = AppConfig {
            general: Default::default(),
            channels: HashMap::new(),
            agent: AgentConfig {
                enabled: true,
                mode: "opencode".to_string(),
                text: None,
                opencode: None,
                attachments: None,
            },
            inspect: None,
            heartbeat: HeartbeatConfig::default(),
            attachments: None,
            vision: None,
            mcps: vec![McpServerConfig {
                name: "jyc_vision".to_string(),
                kind: McpServerKind::Local {
                    command: vec!["jyc".to_string(), "mcp-vision-tool".to_string()],
                    environment: HashMap::from([(
                        "VISION_API_KEY".to_string(),
                        "secret".to_string(),
                    )]),
                    timeout: 300000,
                },
            }],
        };

        let agent_config = &app_config.agent;
        let jyc_root = tmp.path();

        let changed = ensure_thread_opencode_setup(
            &thread_path,
            agent_config,
            &app_config,
            jyc_root,
        )
        .await
        .unwrap();

        assert!(changed, "opencode.json should be written");

        let config_content = tokio::fs::read_to_string(thread_path.join("opencode.json"))
            .await
            .unwrap();
        let config: serde_json::Value = serde_json::from_str(&config_content).unwrap();

        // Verify jyc_vision MCP is in the config
        let mcp = &config["mcp"];
        assert!(mcp["jyc_vision"].is_object(), "jyc_vision should be in mcp config");
        assert_eq!(mcp["jyc_vision"]["type"], "local");
        assert_eq!(
            mcp["jyc_vision"]["environment"]["VISION_API_KEY"],
            "secret"
        );

        // Verify jyc_reply and jyc_question are still present
        assert!(mcp["jyc_reply"].is_object(), "jyc_reply should be present");
        assert!(mcp["jyc_question"].is_object(), "jyc_question should be present");
    }

    #[tokio::test]
    async fn test_ensure_thread_opencode_setup_without_mcps_json() {
        use crate::config::types::{AgentConfig, AppConfig, HeartbeatConfig};
        use std::collections::HashMap;

        let tmp = tempfile::tempdir().unwrap();
        let thread_path = tmp.path().join("test-thread");
        tokio::fs::create_dir_all(&thread_path).await.unwrap();

        let app_config = AppConfig {
            general: Default::default(),
            channels: HashMap::new(),
            agent: AgentConfig {
                enabled: true,
                mode: "opencode".to_string(),
                text: None,
                opencode: None,
                attachments: None,
            },
            inspect: None,
            heartbeat: HeartbeatConfig::default(),
            attachments: None,
            vision: None,
            mcps: vec![],
        };

        let agent_config = &app_config.agent;
        let jyc_root = tmp.path();

        let changed = ensure_thread_opencode_setup(
            &thread_path,
            agent_config,
            &app_config,
            jyc_root,
        )
        .await
        .unwrap();

        assert!(changed, "opencode.json should be written");

        let config_content = tokio::fs::read_to_string(thread_path.join("opencode.json"))
            .await
            .unwrap();
        let config: serde_json::Value = serde_json::from_str(&config_content).unwrap();

        // Verify only jyc_reply and jyc_question are present
        let mcp = &config["mcp"];
        assert!(mcp["jyc_reply"].is_object());
        assert!(mcp["jyc_question"].is_object());
        assert!(mcp.get("jyc_vision").is_none());
    }
}
