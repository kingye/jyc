use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::Path;

use super::client::OpenCodeClient;
use crate::config::types::AgentConfig;

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
/// Writes the config only if content has changed (staleness check).
/// Returns `true` if the file was (re)written.
pub async fn ensure_thread_opencode_setup(
    thread_path: &Path,
    agent_config: &AgentConfig,
    jyc_root: &Path,
    vision_config: Option<&crate::config::types::VisionConfig>,
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

    // Start with user's global MCP tools (from ~/.config/opencode/ and workdir opencode.json)
    let mut mcp_tools = load_user_mcp_configs(jyc_root).await;

    // Add JYC's own MCP tools (these always take precedence over user tools)
    let thread_dir_str = thread_path.to_string_lossy().to_string();
    mcp_tools["jyc_reply"] = serde_json::json!({
        "type": "local",
        "command": tool_command,
        "environment": {
            "JYC_ROOT": jyc_root.to_string_lossy(),
            "JYC_THREAD_DIR": &thread_dir_str
        },
        "enabled": true,
        "timeout": 180000
    });
    mcp_tools["jyc_question"] = serde_json::json!({
        "type": "local",
        "command": question_command,
        "environment": {
            "JYC_THREAD_DIR": &thread_dir_str
        },
        "enabled": true,
        "timeout": 360000
    });

    // Register vision MCP tool if configured and enabled
    if let Some(vision) = vision_config {
        if vision.enabled {
            let vision_command = get_vision_tool_command();
            mcp_tools["jyc_vision"] = serde_json::json!({
                "type": "local",
                "command": vision_command,
                "environment": {
                    "VISION_API_KEY": vision.api_key,
                    "VISION_API_URL": vision.api_url,
                    "VISION_MODEL": vision.model,
                    "JYC_THREAD_DIR": &thread_dir_str
                },
                "enabled": true,
                "timeout": 300000
            });
            tracing::debug!("Vision MCP tool registered in opencode.json");
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

/// Load MCP tool configurations from the user's global OpenCode config.
///
/// Reads MCP entries from two locations (in order, later entries override earlier):
/// 1. `~/.config/opencode/opencode.json` (or `opencode.jsonc`) — user's global config
/// 2. `<jyc_root>/opencode.json` — workdir-level config
///
/// Returns a merged JSON object of all discovered MCP tools.
/// JYC's own tools (jyc_reply, jyc_question, jyc_vision) should be added AFTER
/// calling this function so they always take precedence.
async fn load_user_mcp_configs(jyc_root: &Path) -> serde_json::Value {
    let mut merged = serde_json::json!({});

    // 1. Try global config: ~/.config/opencode/opencode.json or opencode.jsonc
    if let Some(config_dir) = dirs_next_config_dir() {
        for filename in &["opencode.json", "opencode.jsonc"] {
            let global_path = config_dir.join("opencode").join(filename);
            if let Some(mcp) = read_mcp_from_config(&global_path).await {
                merge_mcp(&mut merged, &mcp);
                tracing::debug!(
                    path = %global_path.display(),
                    count = mcp.as_object().map(|m| m.len()).unwrap_or(0),
                    "Loaded user MCP tools from global config"
                );
                break; // Use first found
            }
        }
    }

    // 2. Try workdir-level config: <jyc_root>/opencode.json
    let workdir_path = jyc_root.join("opencode.json");
    if let Some(mcp) = read_mcp_from_config(&workdir_path).await {
        merge_mcp(&mut merged, &mcp);
        tracing::debug!(
            path = %workdir_path.display(),
            count = mcp.as_object().map(|m| m.len()).unwrap_or(0),
            "Loaded user MCP tools from workdir config"
        );
    }

    merged
}

/// Read the `mcp` field from an OpenCode config file.
/// Supports both JSON and JSONC (strips // comments and trailing commas).
async fn read_mcp_from_config(path: &Path) -> Option<serde_json::Value> {
    let content = tokio::fs::read_to_string(path).await.ok()?;

    // Strip single-line comments (// ...) and trailing commas for JSONC support
    let cleaned: String = content
        .lines()
        .map(|line| {
            // Remove // comments (but not inside strings — simple heuristic)
            if let Some(idx) = line.find("//") {
                // Only strip if the // is not inside a quoted string
                let before = &line[..idx];
                let quote_count = before.matches('"').count();
                if quote_count % 2 == 0 {
                    return before.to_string();
                }
            }
            line.to_string()
        })
        .collect::<Vec<_>>()
        .join("\n");

    // Remove trailing commas before } or ]
    let re_trailing_comma = regex::Regex::new(r",\s*([}\]])").ok()?;
    let cleaned = re_trailing_comma.replace_all(&cleaned, "$1");

    let config: serde_json::Value = serde_json::from_str(&cleaned).ok()?;
    config.get("mcp").cloned()
}

/// Merge source MCP entries into target. Source entries override target on key conflict.
fn merge_mcp(target: &mut serde_json::Value, source: &serde_json::Value) {
    if let (Some(target_obj), Some(source_obj)) = (target.as_object_mut(), source.as_object()) {
        for (key, value) in source_obj {
            target_obj.insert(key.clone(), value.clone());
        }
    }
}

/// Get the user's config directory (~/.config on macOS/Linux).
fn dirs_next_config_dir() -> Option<std::path::PathBuf> {
    // Use XDG_CONFIG_HOME if set, otherwise ~/.config
    if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
        let path = std::path::PathBuf::from(xdg);
        if path.is_absolute() {
            return Some(path);
        }
    }
    // Fall back to $HOME/.config
    std::env::var("HOME")
        .ok()
        .map(|home| std::path::PathBuf::from(home).join(".config"))
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

fn get_vision_tool_command() -> Vec<String> {
    get_mcp_tool_command("mcp-vision-tool")
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






}
