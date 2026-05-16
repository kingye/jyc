//! Session management for the in-process agent.
//!
//! Manages:
//! - Full conversation log (`.jyc/agent-conversation.json`) — complete LLM history
//!   including tool calls and results for multi-turn context
//! - Session state (`.jyc/agent-session.json`) — token tracking, auto-reset
//!
//! On reset: session state is cleared, conversation is summarized (last few turns kept).

use serde::{Deserialize, Serialize};
use std::path::Path;
use tracing;

use crate::types::{ContentBlock, Message, Role};

/// Number of recent user+assistant pairs to keep as summary on reset.
const SUMMARY_KEEP_PAIRS: usize = 3;

const CONTEXT_FILE: &str = "agent-context.json";
const SESSION_FILE: &str = "agent-session.json";

/// Session state persisted to `.jyc/agent-session.json`.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SessionState {
    /// When this session was created (ISO 8601).
    pub created_at: String,
    pub total_input_tokens: u64,
    pub total_output_tokens: u64,
    /// Max tokens (context window) for the model.
    #[serde(default)]
    pub max_input_tokens: u64,
}

// ─── Conversation Persistence ────────────────────────────────────────

/// Save the raw provider-formatted context to disk.
///
/// Called after each agent_loop::run() completes. Stores the raw API messages
/// exactly as they were sent/received (preserves provider-specific fields like
/// DeepSeek's reasoning_content).
pub async fn save_raw_context(thread_path: &Path, raw_context: &[serde_json::Value]) {
    let jyc_dir = thread_path.join(".jyc");
    tokio::fs::create_dir_all(&jyc_dir).await.ok();
    let path = jyc_dir.join(CONTEXT_FILE);

    match serde_json::to_string(raw_context) {
        Ok(json) => {
            if let Err(e) = tokio::fs::write(&path, json).await {
                tracing::warn!(error = %e, "Failed to save raw context");
            }
        }
        Err(e) => {
            tracing::warn!(error = %e, "Failed to serialize raw context");
        }
    }
}

/// Load prior raw context from agent-context.json.
///
/// Returns (internal_messages, raw_context):
/// - internal_messages: for logic (reply detection, text extraction)
/// - raw_context: for sending to the API (preserves provider-specific fields)
///
/// If no session file exists (fresh or after reset), returns empty.
pub async fn load_context(thread_path: &Path) -> (Vec<Message>, Vec<serde_json::Value>) {
    let jyc_dir = thread_path.join(".jyc");
    let session_path = jyc_dir.join(SESSION_FILE);
    let context_path = jyc_dir.join(CONTEXT_FILE);

    // No session file = fresh start. No prior context.
    if !session_path.exists() {
        return (Vec::new(), Vec::new());
    }

    // Load raw context (provider-formatted JSON)
    if context_path.exists() {
        if let Ok(content) = tokio::fs::read_to_string(&context_path).await {
            if let Ok(raw_context) = serde_json::from_str::<Vec<serde_json::Value>>(&content) {
                // Filter out invalid assistant messages:
                // DeepSeek requires content OR tool_calls (reasoning_content alone is not accepted)
                let raw_context: Vec<serde_json::Value> = raw_context.into_iter()
                    .filter(|m| {
                        if m.get("role").and_then(|r| r.as_str()) == Some("assistant") {
                            let has_content = m.get("content")
                                .and_then(|c| c.as_str())
                                .is_some_and(|s| !s.is_empty());
                            let has_tool_calls = m.get("tool_calls")
                                .and_then(|t| t.as_array())
                                .is_some_and(|a| !a.is_empty());
                            has_content || has_tool_calls
                        } else {
                            true
                        }
                    })
                    .collect();

                if !raw_context.is_empty() {
                    // Validate: must contain at least one assistant message
                    let has_assistant = raw_context.iter().any(|m| {
                        m.get("role").and_then(|r| r.as_str()) == Some("assistant")
                    });
                    if has_assistant {
                        tracing::debug!(
                            context_messages = raw_context.len(),
                            "Loaded raw context from agent-context.json"
                        );
                        // Build internal messages from raw context (for reply detection logic)
                        let internal = raw_context_to_messages(&raw_context);
                        return (internal, raw_context);
                    } else {
                        tracing::warn!("Context file has no assistant messages (corrupted), ignoring");
                        tokio::fs::remove_file(&context_path).await.ok();
                    }
                }
            }
        }
    }

    // Fallback: no raw context available, start fresh
    (Vec::new(), Vec::new())
}

/// Convert raw provider JSON context to internal Messages (best-effort).
/// Used for internal logic only (reply detection, etc.).
fn raw_context_to_messages(raw: &[serde_json::Value]) -> Vec<Message> {
    raw.iter().filter_map(|m| {
        let role = m.get("role")?.as_str()?;
        match role {
            "user" => {
                let content = m.get("content")?.as_str()?;
                Some(Message::user(content.to_string()))
            }
            "assistant" => {
                let content = m.get("content").and_then(|c| c.as_str()).unwrap_or("");
                if content.is_empty() {
                    // Check for tool_calls
                    if m.get("tool_calls").is_some() {
                        Some(Message {
                            role: Role::Assistant,
                            content: vec![], // Will be populated if needed
                        })
                    } else {
                        None
                    }
                } else {
                    Some(Message::assistant(content.to_string()))
                }
            }
            "tool" => {
                let tool_call_id = m.get("tool_call_id")?.as_str()?;
                let content = m.get("content").and_then(|c| c.as_str()).unwrap_or("");
                Some(Message::tool_result(tool_call_id.to_string(), content.to_string(), false))
            }
            _ => None,
        }
    }).collect()
}

// ─── Token Tracking ──────────────────────────────────────────────────

/// Update token tracking in the session state.
/// Creates the session file if it doesn't exist.
/// Auto-resets (with summary) if total tokens exceed max_input_tokens.
///
/// `input_tokens` is the tokens reported by the last API call — this already
/// includes all prior context, so we store it directly (not accumulated).
pub async fn update_tokens(thread_path: &Path, input_tokens: u64, output_tokens: u64, context_window: Option<u64>) {
    let session_path = thread_path.join(".jyc").join(SESSION_FILE);
    let mut state = load_session_state(&session_path).await;

    // Store the latest input tokens (not accumulated — each API call includes full context)
    state.total_input_tokens = input_tokens;
    state.total_output_tokens += output_tokens;

    if let Some(cw) = context_window {
        // Use 95% of context window as max input tokens (reserve 5% for output)
        state.max_input_tokens = (cw as f64 * 0.95) as u64;
    }

    // Set created_at on first creation
    if state.created_at.is_empty() {
        state.created_at = chrono::Utc::now().to_rfc3339();
    }

    // Auto-reset if tokens exceed max context window
    if state.max_input_tokens > 0 && state.total_input_tokens >= state.max_input_tokens {
        tracing::info!(
            total_input_tokens = state.total_input_tokens,
            max_input_tokens = state.max_input_tokens,
            "Session exceeded max input tokens, auto-resetting with summary"
        );

        // Summarize the context (keep last few turns)
        summarize_context(thread_path).await;

        // Reset token counters
        state.total_input_tokens = 0;
        state.total_output_tokens = 0;
        state.created_at = chrono::Utc::now().to_rfc3339();
    }

    save_session_state(&session_path, &state).await;
}

// ─── Reset ───────────────────────────────────────────────────────────

/// Reset the session with summary.
///
/// Called when user triggers a session reset (e.g., from dashboard or /reset command).
/// - Deletes `agent-session.json` (resets token tracking)
/// - Summarizes `agent-context.json` (keeps last few user+reply pairs, removes tool calls)
pub async fn reset_session(thread_path: &Path) {
    let jyc_dir = thread_path.join(".jyc");

    // Summarize context before deleting session
    summarize_context(thread_path).await;

    // Delete session state (triggers fresh start on next invocation)
    let session_path = jyc_dir.join(SESSION_FILE);
    tokio::fs::remove_file(&session_path).await.ok();

    tracing::info!("Agent session reset (context summarized)");
}

/// Summarize the raw context: keep only the last N user+assistant text pairs.
///
/// Removes tool calls, tool results, and reasoning_content.
/// Keeps only user messages and assistant text responses for compact context.
async fn summarize_context(thread_path: &Path) {
    let context_path = thread_path.join(".jyc").join(CONTEXT_FILE);

    if !context_path.exists() {
        return;
    }

    let content = match tokio::fs::read_to_string(&context_path).await {
        Ok(c) => c,
        Err(_) => return,
    };

    let raw_context: Vec<serde_json::Value> = match serde_json::from_str(&content) {
        Ok(m) => m,
        Err(_) => return,
    };

    // Extract user+assistant text pairs (skip tool messages)
    let mut pairs: Vec<(serde_json::Value, serde_json::Value)> = Vec::new();
    let mut last_user: Option<serde_json::Value> = None;

    for msg in &raw_context {
        let role = msg.get("role").and_then(|r| r.as_str()).unwrap_or("");
        match role {
            "user" => {
                last_user = Some(msg.clone());
            }
            "assistant" => {
                let content = msg.get("content").and_then(|c| c.as_str()).unwrap_or("");
                if !content.is_empty() {
                    if let Some(user_msg) = last_user.take() {
                        // Keep only role + content (strip reasoning_content, tool_calls)
                        let clean_assistant = serde_json::json!({
                            "role": "assistant",
                            "content": content,
                        });
                        pairs.push((user_msg, clean_assistant));
                    }
                }
            }
            _ => {} // Skip tool messages
        }
    }

    // Keep only the last N pairs
    let summary: Vec<serde_json::Value> = pairs
        .into_iter()
        .rev()
        .take(SUMMARY_KEEP_PAIRS)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .flat_map(|(user, assistant)| vec![user, assistant])
        .collect();

    tracing::debug!(
        original_messages = raw_context.len(),
        summary_messages = summary.len(),
        "Context summarized"
    );

    // Write summary back
    if summary.is_empty() {
        tokio::fs::remove_file(&context_path).await.ok();
    } else {
        match serde_json::to_string(&summary) {
            Ok(json) => { tokio::fs::write(&context_path, json).await.ok(); }
            Err(_) => { tokio::fs::remove_file(&context_path).await.ok(); }
        }
    }
}

// ─── Internal helpers ────────────────────────────────────────────────

/// Load session state from disk.
async fn load_session_state(path: &Path) -> SessionState {
    if path.exists() {
        if let Ok(content) = tokio::fs::read_to_string(path).await {
            if let Ok(state) = serde_json::from_str(&content) {
                return state;
            }
        }
    }
    SessionState::default()
}

/// Save session state to disk.
async fn save_session_state(path: &Path, state: &SessionState) {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await.ok();
    }
    if let Ok(json) = serde_json::to_string_pretty(state) {
        tokio::fs::write(path, json).await.ok();
    }
}

/// Fallback: Load context from chat_history_*.md files (text-only).
async fn load_from_chat_history(thread_path: &Path, cutoff: Option<&chrono::DateTime<chrono::Utc>>) -> Vec<Message> {
    let mut history_files: Vec<_> = match std::fs::read_dir(thread_path) {
        Ok(entries) => entries
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.file_name()
                    .to_str()
                    .is_some_and(|n| n.starts_with("chat_history_") && n.ends_with(".md"))
            })
            .map(|e| e.path())
            .collect(),
        Err(_) => return Vec::new(),
    };

    history_files.sort();

    let mut messages: Vec<Message> = Vec::new();

    for file in history_files.iter().rev() {
        let content = match std::fs::read_to_string(file) {
            Ok(c) => c,
            Err(_) => continue,
        };

        let entries = parse_chat_entries(&content, cutoff);
        for entry in entries.into_iter().rev() {
            if messages.len() >= 20 {
                break;
            }
            messages.push(entry);
        }

        if messages.len() >= 20 {
            break;
        }
    }

    messages.reverse();

    if !messages.is_empty() {
        tracing::debug!(
            context_messages = messages.len(),
            "Loaded conversation context from chat_history (fallback)"
        );
    }

    messages
}

/// Parse chat history markdown entries into Messages.
fn parse_chat_entries(content: &str, cutoff: Option<&chrono::DateTime<chrono::Utc>>) -> Vec<Message> {
    let mut messages = Vec::new();
    let mut current_type: Option<&str> = None;
    let mut current_text = String::new();
    let mut current_after_cutoff = cutoff.is_none();

    for line in content.lines() {
        if line.starts_with("<!-- ") && line.ends_with(" -->") {
            if let Some(msg_type) = current_type {
                if current_after_cutoff && !current_text.trim().is_empty() {
                    let msg = if msg_type == "received" {
                        Message::user(current_text.trim().to_string())
                    } else {
                        Message::assistant(current_text.trim().to_string())
                    };
                    messages.push(msg);
                }
            }

            current_type = if line.contains("type:received") {
                Some("received")
            } else if line.contains("type:reply") {
                Some("reply")
            } else {
                None
            };
            current_text.clear();

            if let Some(cutoff_ts) = cutoff {
                current_after_cutoff = extract_timestamp(line)
                    .map(|ts| ts >= *cutoff_ts)
                    .unwrap_or(false);
            }
        } else if line == "---" {
            if let Some(msg_type) = current_type {
                if current_after_cutoff && !current_text.trim().is_empty() {
                    let msg = if msg_type == "received" {
                        Message::user(current_text.trim().to_string())
                    } else {
                        Message::assistant(current_text.trim().to_string())
                    };
                    messages.push(msg);
                }
            }
            current_type = None;
            current_text.clear();
        } else if current_type.is_some() {
            if !line.starts_with("**FROM:**") && !line.starts_with("**SUBJECT:**") {
                current_text.push_str(line);
                current_text.push('\n');
            }
        }
    }

    if let Some(msg_type) = current_type {
        if current_after_cutoff && !current_text.trim().is_empty() {
            let msg = if msg_type == "received" {
                Message::user(current_text.trim().to_string())
            } else {
                Message::assistant(current_text.trim().to_string())
            };
            messages.push(msg);
        }
    }

    messages
}

/// Extract timestamp from a chat history metadata comment line.
fn extract_timestamp(line: &str) -> Option<chrono::DateTime<chrono::Utc>> {
    let inner = line.strip_prefix("<!-- ")?.strip_suffix(" -->")?;
    let ts_str = inner.split('|').next()?.trim();
    chrono::DateTime::parse_from_rfc3339(ts_str)
        .map(|dt| dt.with_timezone(&chrono::Utc))
        .ok()
}
