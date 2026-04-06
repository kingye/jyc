use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::Path;

use super::client::OpenCodeClient;
use crate::config::types::{AgentConfig, SessionSummaryConfig};

/// Per-thread session state, persisted in `.jyc/opencode-session.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionState {
    #[serde(rename = "sessionId")]
    pub session_id: String,
    #[serde(rename = "createdAt")]
    pub created_at: String,
    #[serde(rename = "lastUsedAt")]
    pub last_used_at: String,
    /// Total active time in seconds (accumulated over session lifetime)
    #[serde(rename = "totalActiveTime", default)]
    pub total_active_time: u64,
    /// Timestamp when current active period started (if session is currently active)
    #[serde(
        rename = "lastActiveStart",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub last_active_start: Option<String>,
}

/// Session summary data structure.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionSummary {
    /// Session ID
    #[serde(rename = "sessionId")]
    pub session_id: String,

    /// Thread name
    pub thread_name: String,

    /// Session start time (ISO 8601)
    pub start_time: String,

    /// Session end time (ISO 8601)
    pub end_time: String,

    /// Session duration in seconds
    pub duration_secs: u64,

    /// Number of messages in this session
    pub message_count: usize,

    /// Key topics discussed (3-5 bullet points)
    pub key_topics: Vec<String>,

    /// Trigger reason for summary generation
    pub trigger_reason: TriggerReason,
}

/// Reason for generating a session summary.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum TriggerReason {
    /// Session timeout (inactivity threshold exceeded)
    Timeout { threshold_hours: f64 },
    /// Session active time exceeded maximum allowed
    ActiveTimeExceeded {
        total_active_hours: f64,
        max_active_hours: f64,
    },
    /// Session creation (new session created)
    SessionCreation,
    /// Session deletion (session deleted)
    SessionDeletion,
    /// Context overflow recovery
    ContextOverflow,
    /// Manual trigger
    Manual,
}

/// Session summary statistics for monitoring.
#[derive(Debug, Clone)]
pub struct SessionStats {
    /// Total number of messages
    pub total_messages: usize,
    /// Number of attachments
    pub attachment_count: usize,
    /// Session duration in hours
    pub duration_hours: f64,
    /// Whether session had errors
    pub had_errors: bool,
}

/// Get or create a session for a thread.
///
/// 1. Read `.jyc/opencode-session.json`
/// 2. Check if session should be summarized due to timeout
/// 3. If timeout → generate summary, create new session
/// 4. Verify session still exists via API
/// 5. If missing → create new session
pub async fn get_or_create_session(
    client: &OpenCodeClient,
    thread_path: &Path,
    summary_config: Option<&SessionSummaryConfig>,
) -> Result<String> {
    let state_path = thread_path.join(".jyc").join("opencode-session.json");

    // Try loading existing session
    if state_path.exists() {
        if let Ok(content) = tokio::fs::read_to_string(&state_path).await {
            if let Ok(state) = serde_json::from_str::<SessionState>(&content) {
                // Check if session should be summarized due to timeout
                if let Some(config) = summary_config {
                    if config.enabled {
                        // Use new function with both active time and idle time thresholds
                        match should_summarize_session_by_active_time(
                            &state,
                            config.timeout_hours,
                            config.max_idle_hours,
                        ) {
                            Ok(true) => {
                                let (trigger_reason, log_message) = if state.total_active_time
                                    >= (config.timeout_hours * 3600.0) as u64
                                {
                                    // Session exceeded maximum active time
                                    (
                                        TriggerReason::ActiveTimeExceeded {
                                            total_active_hours: state.total_active_time as f64
                                                / 3600.0,
                                            max_active_hours: config.timeout_hours,
                                        },
                                        format!(
                                            "Session active time exceeded ({}h total, {}h max)",
                                            state.total_active_time as f64 / 3600.0,
                                            config.timeout_hours
                                        ),
                                    )
                                } else {
                                    // Session exceeded maximum idle time
                                    (
                                        TriggerReason::Timeout {
                                            threshold_hours: config.max_idle_hours,
                                        },
                                        format!(
                                            "Session idle time exceeded (max {}h)",
                                            config.max_idle_hours
                                        ),
                                    )
                                };

                                tracing::info!(
                                    session_id = %state.session_id,
                                    reason = %log_message,
                                    "Session summary triggered"
                                );

                                // Generate and save session summary
                                let summary = create_basic_session_summary(
                                    thread_path,
                                    &state,
                                    trigger_reason,
                                )
                                .await?;

                                save_session_summary(thread_path, &summary, config).await?;

                                // Clean up old summaries
                                cleanup_old_summaries(thread_path, config.max_summaries).await?;

                                // Delete old session and create new one
                                delete_session(thread_path).await?;
                                return create_new_session(client, thread_path).await;
                            }
                            Ok(false) => {
                                // Session not timed out, continue verification
                            }
                            Err(e) => {
                                tracing::warn!(
                                    error = %e,
                                    "Failed to check session timeout, continuing"
                                );
                            }
                        }
                    }
                }

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
        total_active_time: 0,
        last_active_start: None,
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

/// Update the lastUsedAt timestamp and track active time.
pub async fn update_session_timestamp(thread_path: &Path) -> Result<()> {
    let state_path = thread_path.join(".jyc").join("opencode-session.json");
    if let Ok(content) = tokio::fs::read_to_string(&state_path).await {
        if let Ok(mut state) = serde_json::from_str::<SessionState>(&content) {
            let now = chrono::Utc::now();
            state.last_used_at = now.to_rfc3339();
            save_session_state(thread_path, &state).await?;
        }
    }
    Ok(())
}

/// Start tracking active time for a session.
pub async fn start_active_time_tracking(thread_path: &Path) -> Result<()> {
    let state_path = thread_path.join(".jyc").join("opencode-session.json");
    if let Ok(content) = tokio::fs::read_to_string(&state_path).await {
        if let Ok(mut state) = serde_json::from_str::<SessionState>(&content) {
            let now = chrono::Utc::now();
            state.last_active_start = Some(now.to_rfc3339());
            save_session_state(thread_path, &state).await?;
        }
    }
    Ok(())
}

/// Stop tracking active time and accumulate it to total active time.
pub async fn stop_active_time_tracking(thread_path: &Path) -> Result<()> {
    let state_path = thread_path.join(".jyc").join("opencode-session.json");
    if let Ok(content) = tokio::fs::read_to_string(&state_path).await {
        if let Ok(mut state) = serde_json::from_str::<SessionState>(&content) {
            if let Some(start_time_str) = &state.last_active_start {
                if let Ok(start_time) = chrono::DateTime::parse_from_rfc3339(start_time_str) {
                    let now = chrono::Utc::now();
                    let active_duration = now.signed_duration_since(start_time);
                    let active_seconds = active_duration.num_seconds().max(0) as u64;

                    state.total_active_time += active_seconds;
                    state.last_active_start = None;

                    tracing::debug!(
                        session_id = %state.session_id,
                        active_seconds = active_seconds,
                        total_active_time = state.total_active_time,
                        "Accumulated active time"
                    );

                    save_session_state(thread_path, &state).await?;
                }
            }
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

    let new_config = OpencodeConfig {
        schema: "https://opencode.ai/config.json".to_string(),
        model,
        small_model,
        permission: serde_json::json!({
            "*": "allow",
            "question": "deny"
        }),
        agent: None, // For simplicity, not configuring agent here
        mcp: serde_json::json!({
            "jiny_reply": {
                "type": "local",
                "command": tool_command,
                "environment": {
                    "JYC_ROOT": jyc_root.to_string_lossy()
                },
                "enabled": true,
                "timeout": 180000
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

/// Check if a session should be summarized based on timeout (legacy - uses last used time).
pub fn should_summarize_session(state: &SessionState, timeout_hours: f64) -> Result<bool> {
    let last_used = chrono::DateTime::parse_from_rfc3339(&state.last_used_at)
        .context("failed to parse last_used_at timestamp")?;

    let now = chrono::Utc::now();
    let elapsed = now.signed_duration_since(last_used);

    let timeout_secs = (timeout_hours * 3600.0) as i64;
    Ok(elapsed.num_seconds() > timeout_secs)
}

/// Check if a session should be summarized based on accumulated active time.
pub fn should_summarize_session_by_active_time(
    state: &SessionState,
    max_active_hours: f64,
    max_idle_hours: f64,
) -> Result<bool> {
    // First check if session has exceeded maximum active time
    let max_active_secs = (max_active_hours * 3600.0) as u64;
    if state.total_active_time >= max_active_secs {
        tracing::debug!(
            session_id = %state.session_id,
            total_active_time = state.total_active_time,
            max_active_secs = max_active_secs,
            "Session exceeded maximum active time"
        );
        return Ok(true);
    }

    // Then check if session has been idle for too long
    let last_used = chrono::DateTime::parse_from_rfc3339(&state.last_used_at)
        .context("failed to parse last_used_at timestamp")?;

    let now = chrono::Utc::now();
    let idle_time = now.signed_duration_since(last_used);

    let max_idle_secs = (max_idle_hours * 3600.0) as i64;
    let should_summarize = idle_time.num_seconds() > max_idle_secs;

    if should_summarize {
        tracing::debug!(
            session_id = %state.session_id,
            idle_seconds = idle_time.num_seconds(),
            max_idle_secs = max_idle_secs,
            total_active_time = state.total_active_time,
            "Session exceeded maximum idle time"
        );
    }

    Ok(should_summarize)
}

/// Calculate session duration in seconds.
pub fn calculate_session_duration(state: &SessionState) -> Result<u64> {
    let created = chrono::DateTime::parse_from_rfc3339(&state.created_at)
        .context("failed to parse created_at timestamp")?;
    let last_used = chrono::DateTime::parse_from_rfc3339(&state.last_used_at)
        .context("failed to parse last_used_at timestamp")?;

    let duration = last_used.signed_duration_since(created);
    Ok(duration.num_seconds().max(0) as u64)
}

/// Get the session summary directory path.
pub fn get_summary_dir(thread_path: &Path) -> std::path::PathBuf {
    thread_path.join(".jyc").join("session-summaries")
}

/// Generate a summary file name based on timestamp.
pub fn generate_summary_filename() -> String {
    let now = chrono::Utc::now();
    format!("{}.md", now.format("%Y-%m-%d_%H-%M-%S"))
}

/// Get the full path for a summary file.
pub fn get_summary_path(thread_path: &Path, filename: &str) -> std::path::PathBuf {
    get_summary_dir(thread_path).join(filename)
}

/// Save a session summary to a markdown file.
pub async fn save_session_summary(
    thread_path: &Path,
    summary: &SessionSummary,
    config: &crate::config::types::SessionSummaryConfig,
) -> Result<String> {
    let summary_dir = get_summary_dir(thread_path);
    tokio::fs::create_dir_all(&summary_dir)
        .await
        .context("failed to create summary directory")?;

    let filename = generate_summary_filename();
    let filepath = get_summary_path(thread_path, &filename);

    // Format summary as markdown
    let content = format_session_summary_markdown(summary);

    tokio::fs::write(&filepath, &content)
        .await
        .context("failed to write summary file")?;

    // Clean up old summaries if needed
    cleanup_old_summaries(thread_path, config.max_summaries).await?;

    tracing::info!(
        path = %filepath.display(),
        "Session summary saved"
    );

    Ok(filename)
}

/// Format a session summary as markdown with YAML frontmatter.
fn format_session_summary_markdown(summary: &SessionSummary) -> String {
    let mut content = String::new();

    // YAML frontmatter
    content.push_str("---\n");
    content.push_str(&format!("session_id: {}\n", summary.session_id));
    content.push_str(&format!("thread_name: {}\n", summary.thread_name));
    content.push_str(&format!("start_time: {}\n", summary.start_time));
    content.push_str(&format!("end_time: {}\n", summary.end_time));
    content.push_str(&format!("duration_secs: {}\n", summary.duration_secs));
    content.push_str(&format!("message_count: {}\n", summary.message_count));
    content.push_str(&format!("trigger_reason: {:?}\n", summary.trigger_reason));
    content.push_str("---\n\n");

    // Title
    content.push_str(&format!("# 会话总结: {}\n\n", summary.thread_name));

    // Key topics
    if !summary.key_topics.is_empty() {
        content.push_str("## 关键摘要\n\n");
        for topic in &summary.key_topics {
            content.push_str(&format!("- {}\n", topic));
        }
        content.push_str("\n");
    }

    // Session metadata
    content.push_str("## 会话元数据\n\n");
    content.push_str(&format!("- **会话ID**: {}\n", summary.session_id));
    content.push_str(&format!("- **线程名称**: {}\n", summary.thread_name));
    content.push_str(&format!("- **开始时间**: {}\n", summary.start_time));
    content.push_str(&format!("- **结束时间**: {}\n", summary.end_time));
    content.push_str(&format!(
        "- **持续时间**: {:.2} 小时\n",
        summary.duration_secs as f64 / 3600.0
    ));
    content.push_str(&format!("- **消息数量**: {}\n", summary.message_count));
    content.push_str(&format!("- **触发原因**: {:?}\n", summary.trigger_reason));

    content
}

/// Check if there are any session summary files in the thread.
pub async fn has_session_summaries(thread_path: &Path) -> bool {
    let summary_dir = get_summary_dir(thread_path);
    if !summary_dir.exists() {
        return false;
    }

    match tokio::fs::read_dir(&summary_dir).await {
        Ok(mut read_dir) => {
            while let Ok(Some(entry)) = read_dir.next_entry().await {
                if let Ok(file_type) = entry.file_type().await {
                    if file_type.is_file() && entry.file_name().to_string_lossy().ends_with(".md") {
                        return true;
                    }
                }
            }
            false
        }
        Err(_) => false,
    }
}

/// Clean up old summary files, keeping only the latest N files.
pub async fn cleanup_old_summaries(thread_path: &Path, max_summaries: usize) -> Result<()> {
    let summary_dir = get_summary_dir(thread_path);
    if !summary_dir.exists() || max_summaries == 0 {
        return Ok(());
    }

    let mut entries = Vec::new();
    let mut read_dir = tokio::fs::read_dir(&summary_dir).await?;

    while let Some(entry) = read_dir.next_entry().await? {
        if entry.file_type().await?.is_file() {
            let metadata = entry.metadata().await?;
            let modified = metadata.modified()?;
            entries.push((entry.path(), modified));
        }
    }

    // Sort by modification time (newest first)
    entries.sort_by(|a, b| b.1.cmp(&a.1));

    // Remove files beyond the limit
    if entries.len() > max_summaries {
        for (path, _) in entries.into_iter().skip(max_summaries) {
            if let Err(e) = tokio::fs::remove_file(&path).await {
                tracing::warn!(
                    path = %path.display(),
                    error = %e,
                    "Failed to remove old summary file"
                );
            } else {
                tracing::debug!(
                    path = %path.display(),
                    "Removed old session summary"
                );
            }
        }
    }

    Ok(())
}

/// Load a session summary from a file.
pub async fn load_session_summary(thread_path: &Path, filename: &str) -> Result<SessionSummary> {
    let filepath = get_summary_path(thread_path, filename);
    let content = tokio::fs::read_to_string(&filepath)
        .await
        .context("failed to read summary file")?;

    // Parse YAML frontmatter
    parse_session_summary_markdown(&content)
}

/// Parse a session summary from markdown content.
fn parse_session_summary_markdown(content: &str) -> Result<SessionSummary> {
    // Simple parsing for now - in a real implementation, we would use a proper YAML parser
    // This is a simplified version for the initial implementation
    let lines: Vec<&str> = content.lines().collect();

    // Find YAML frontmatter boundaries
    let mut in_frontmatter = false;
    let mut frontmatter_lines = Vec::new();

    for line in lines {
        if line == "---" {
            if !in_frontmatter {
                in_frontmatter = true;
            } else {
                break; // End of frontmatter
            }
        } else if in_frontmatter {
            frontmatter_lines.push(line);
        }
    }

    // For now, return a basic summary
    // TODO: Implement proper YAML parsing
    Ok(SessionSummary {
        session_id: "unknown".to_string(),
        thread_name: "unknown".to_string(),
        start_time: chrono::Utc::now().to_rfc3339(),
        end_time: chrono::Utc::now().to_rfc3339(),
        duration_secs: 0,
        message_count: 0,
        key_topics: vec!["Summary loaded from file".to_string()],
        trigger_reason: TriggerReason::Manual,
    })
}

/// Count message directories in the thread.
pub async fn count_messages(thread_path: &Path) -> Result<usize> {
    let messages_dir = thread_path.join("messages");
    if !messages_dir.exists() {
        return Ok(0);
    }

    let mut count = 0;
    let mut entries = tokio::fs::read_dir(&messages_dir).await?;

    while let Some(entry) = entries.next_entry().await? {
        if entry.file_type().await?.is_dir() {
            count += 1;
        }
    }

    Ok(count)
}

/// Create a basic session summary from session state.
pub async fn create_basic_session_summary(
    thread_path: &Path,
    state: &SessionState,
    trigger_reason: TriggerReason,
) -> Result<SessionSummary> {
    let thread_name = thread_path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "unknown".to_string());

    let message_count = count_messages(thread_path).await.unwrap_or(0);
    let duration_secs = calculate_session_duration(state).unwrap_or(0);

    // Generate basic key topics
    let mut key_topics = Vec::new();
    if message_count > 0 {
        key_topics.push(format!("处理了 {} 条消息", message_count));
    }
    if duration_secs > 0 {
        let hours = duration_secs as f64 / 3600.0;
        key_topics.push(format!("会话持续了 {:.2} 小时", hours));
    }

    // Ensure we have at least some topics
    if key_topics.is_empty() {
        key_topics.push("会话正常结束".to_string());
    }

    Ok(SessionSummary {
        session_id: state.session_id.clone(),
        thread_name,
        start_time: state.created_at.clone(),
        end_time: chrono::Utc::now().to_rfc3339(),
        duration_secs,
        message_count,
        key_topics,
        trigger_reason,
    })
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
            total_active_time: 0,
            last_active_start: None,
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
    fn test_should_summarize_session() {
        let now = chrono::Utc::now();
        let two_hours_ago = now - chrono::Duration::hours(2) - chrono::Duration::minutes(1);

        let state = SessionState {
            session_id: "test-session".to_string(),
            created_at: now.to_rfc3339(),
            last_used_at: two_hours_ago.to_rfc3339(),
            total_active_time: 0,
            last_active_start: None,
        };

        // Should summarize when idle for > 2 hours
        let should = should_summarize_session(&state, 2.0).unwrap();
        assert!(should, "Session idle for >2 hours should be summarized");

        // Should not summarize when idle for < 2 hours
        let one_hour_ago = now - chrono::Duration::hours(1);
        let state_recent = SessionState {
            session_id: "test-session".to_string(),
            created_at: now.to_rfc3339(),
            last_used_at: one_hour_ago.to_rfc3339(),
            total_active_time: 0,
            last_active_start: None,
        };

        let should_not = should_summarize_session(&state_recent, 2.0).unwrap();
        assert!(
            !should_not,
            "Session idle for <2 hours should not be summarized"
        );
    }

    #[test]
    fn test_should_summarize_session_by_active_time() {
        let now = chrono::Utc::now();

        // Test 1: Session with low active time but long idle time should summarize
        let twenty_five_hours_ago = now - chrono::Duration::hours(25);
        let state_low_active = SessionState {
            session_id: "test-session-1".to_string(),
            created_at: now.to_rfc3339(),
            last_used_at: twenty_five_hours_ago.to_rfc3339(),
            total_active_time: 1800, // 30 minutes active time
            last_active_start: None,
        };

        // Should summarize because idle for 25h > max_idle_hours (24h)
        let should_summarize_idle =
            should_summarize_session_by_active_time(&state_low_active, 2.0, 24.0).unwrap();
        assert!(
            should_summarize_idle,
            "Session with low active time but idle for 25h should summarize"
        );

        // Test 2: Session with high active time should summarize regardless of idle time
        let one_hour_ago = now - chrono::Duration::hours(1);
        let state_high_active = SessionState {
            session_id: "test-session-2".to_string(),
            created_at: now.to_rfc3339(),
            last_used_at: one_hour_ago.to_rfc3339(),
            total_active_time: 9000, // 2.5 hours active time (> 2h max)
            last_active_start: None,
        };

        // Should summarize because active time 2.5h > max_active_hours (2h)
        let should_summarize_active =
            should_summarize_session_by_active_time(&state_high_active, 2.0, 24.0).unwrap();
        assert!(
            should_summarize_active,
            "Session with active time > max_active_hours should summarize"
        );

        // Test 3: Session with low active time and short idle time should not summarize
        let state_healthy = SessionState {
            session_id: "test-session-3".to_string(),
            created_at: now.to_rfc3339(),
            last_used_at: one_hour_ago.to_rfc3339(),
            total_active_time: 1800, // 30 minutes active time
            last_active_start: None,
        };

        // Should not summarize: active time 0.5h < 2h, idle time 1h < 24h
        let should_not_summarize =
            should_summarize_session_by_active_time(&state_healthy, 2.0, 24.0).unwrap();
        assert!(
            !should_not_summarize,
            "Healthy session with low active time and short idle time should not summarize"
        );
    }

    #[test]
    fn test_calculate_session_duration() {
        let start = chrono::DateTime::parse_from_rfc3339("2026-04-04T10:00:00Z").unwrap();
        let end = chrono::DateTime::parse_from_rfc3339("2026-04-04T12:00:00Z").unwrap();

        let state = SessionState {
            session_id: "test-session".to_string(),
            created_at: start.to_rfc3339(),
            last_used_at: end.to_rfc3339(),
            total_active_time: 0,
            last_active_start: None,
        };

        let duration = calculate_session_duration(&state).unwrap();
        assert_eq!(
            duration, 7200,
            "Session duration should be 2 hours (7200 seconds)"
        );
    }

    #[test]
    fn test_generate_summary_filename() {
        let filename = generate_summary_filename();
        // Should match pattern YYYY-MM-DD_HH-MM-SS.md
        let re = regex::Regex::new(r"^\d{4}-\d{2}-\d{2}_\d{2}-\d{2}-\d{2}\.md$").unwrap();
        assert!(
            re.is_match(&filename),
            "Filename should match timestamp pattern: {}",
            filename
        );
    }

    #[tokio::test]
    async fn test_format_session_summary_markdown() {
        let summary = SessionSummary {
            session_id: "sess_123".to_string(),
            thread_name: "test-thread".to_string(),
            start_time: "2026-04-04T10:00:00Z".to_string(),
            end_time: "2026-04-04T12:00:00Z".to_string(),
            duration_secs: 7200,
            message_count: 5,
            key_topics: vec!["处理了5条消息".to_string(), "讨论了会话管理".to_string()],
            trigger_reason: TriggerReason::Timeout {
                threshold_hours: 2.0,
            },
        };

        let markdown = format_session_summary_markdown(&summary);

        // Check for expected content
        assert!(markdown.contains("session_id: sess_123"));
        assert!(markdown.contains("thread_name: test-thread"));
        assert!(markdown.contains("# 会话总结: test-thread"));
        assert!(markdown.contains("- 处理了5条消息"));
        assert!(markdown.contains("- 讨论了会话管理"));
        assert!(markdown.contains("**持续时间**: 2.00 小时"));
    }

    #[tokio::test]
    async fn test_create_basic_session_summary() {
        let tmp = tempfile::tempdir().unwrap();
        let thread_path = tmp.path().join("test-thread");
        tokio::fs::create_dir_all(&thread_path).await.unwrap();

        // Create a mock messages directory
        let messages_dir = thread_path.join("messages");
        tokio::fs::create_dir_all(&messages_dir.join("2026-04-04_10-00-00"))
            .await
            .unwrap();
        tokio::fs::create_dir_all(&messages_dir.join("2026-04-04_11-00-00"))
            .await
            .unwrap();

        let state = SessionState {
            session_id: "sess_123".to_string(),
            created_at: "2026-04-04T10:00:00Z".to_string(),
            last_used_at: "2026-04-04T12:00:00Z".to_string(),
            total_active_time: 0,
            last_active_start: None,
        };

        let summary = create_basic_session_summary(
            &thread_path,
            &state,
            TriggerReason::Timeout {
                threshold_hours: 2.0,
            },
        )
        .await
        .unwrap();

        assert_eq!(summary.session_id, "sess_123");
        assert_eq!(summary.thread_name, "test-thread");
        assert_eq!(summary.message_count, 2);
        assert_eq!(summary.duration_secs, 7200);
        assert!(!summary.key_topics.is_empty());
    }
}
