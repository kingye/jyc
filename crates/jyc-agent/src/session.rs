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

const CONVERSATION_FILE: &str = "agent-conversation.json";
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

/// Save the full conversation history to disk.
///
/// Called after each agent_loop::run() completes. Stores the complete
/// message history including tool calls and results.
pub async fn save_conversation(thread_path: &Path, history: &[Message]) {
    let jyc_dir = thread_path.join(".jyc");
    tokio::fs::create_dir_all(&jyc_dir).await.ok();
    let path = jyc_dir.join(CONVERSATION_FILE);

    match serde_json::to_string(history) {
        Ok(json) => {
            if let Err(e) = tokio::fs::write(&path, json).await {
                tracing::warn!(error = %e, "Failed to save conversation log");
            }
        }
        Err(e) => {
            tracing::warn!(error = %e, "Failed to serialize conversation");
        }
    }
}

/// Load prior conversation context.
///
/// Priority:
/// 1. If `.jyc/agent-conversation.json` exists → load it (full fidelity with tool calls)
/// 2. Fall back to parsing `chat_history_*.md` (text-only, for upgraded threads)
/// 3. If no session file exists (fresh or after reset) → return empty
pub async fn load_context(thread_path: &Path) -> Vec<Message> {
    let jyc_dir = thread_path.join(".jyc");
    let session_path = jyc_dir.join(SESSION_FILE);
    let conversation_path = jyc_dir.join(CONVERSATION_FILE);

    // No session file = fresh start (or after full reset). No prior context.
    if !session_path.exists() {
        return Vec::new();
    }

    // Try loading full conversation log first (includes tool calls)
    if conversation_path.exists() {
        if let Ok(content) = tokio::fs::read_to_string(&conversation_path).await {
            if let Ok(messages) = serde_json::from_str::<Vec<Message>>(&content) {
                if !messages.is_empty() {
                    tracing::debug!(
                        context_messages = messages.len(),
                        "Loaded conversation from agent-conversation.json"
                    );
                    return messages;
                }
            }
        }
    }

    // Fallback: parse chat_history_*.md (text-only, no tool calls)
    let session_state = load_session_state(&session_path).await;
    let cutoff = chrono::DateTime::parse_from_rfc3339(&session_state.created_at)
        .map(|dt| dt.with_timezone(&chrono::Utc))
        .ok();

    load_from_chat_history(thread_path, cutoff.as_ref()).await
}

// ─── Token Tracking ──────────────────────────────────────────────────

/// Update token tracking in the session state.
/// Creates the session file if it doesn't exist.
/// Auto-resets (with summary) if total tokens exceed max_input_tokens.
pub async fn update_tokens(thread_path: &Path, input_tokens: u64, output_tokens: u64, context_window: Option<u64>) {
    let session_path = thread_path.join(".jyc").join(SESSION_FILE);
    let mut state = load_session_state(&session_path).await;

    state.total_input_tokens += input_tokens;
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

        // Summarize the conversation (keep last few turns)
        summarize_conversation(thread_path).await;

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
/// - Summarizes `agent-conversation.json` (keeps last few user+reply pairs, removes tool calls)
pub async fn reset_session(thread_path: &Path) {
    let jyc_dir = thread_path.join(".jyc");

    // Summarize conversation before deleting session
    summarize_conversation(thread_path).await;

    // Delete session state (triggers fresh start on next invocation)
    let session_path = jyc_dir.join(SESSION_FILE);
    tokio::fs::remove_file(&session_path).await.ok();

    tracing::info!("Agent session reset (conversation summarized)");
}

/// Summarize the conversation log: keep only the last N user+assistant text pairs.
///
/// Removes all tool_use and tool_result blocks. Keeps only user text messages
/// and assistant text responses as a compact summary of recent interactions.
async fn summarize_conversation(thread_path: &Path) {
    let conversation_path = thread_path.join(".jyc").join(CONVERSATION_FILE);

    if !conversation_path.exists() {
        return;
    }

    let content = match tokio::fs::read_to_string(&conversation_path).await {
        Ok(c) => c,
        Err(_) => return,
    };

    let messages: Vec<Message> = match serde_json::from_str(&content) {
        Ok(m) => m,
        Err(_) => return,
    };

    // Extract user+assistant text pairs (skip tool_use and tool_result)
    let mut pairs: Vec<(Message, Message)> = Vec::new();
    let mut last_user: Option<Message> = None;

    for msg in &messages {
        match msg.role {
            Role::User => {
                // Only keep user messages that have text (not tool results)
                let has_text = msg.content.iter().any(|b| matches!(b, ContentBlock::Text { .. }));
                if has_text {
                    last_user = Some(Message {
                        role: Role::User,
                        content: msg.content.iter()
                            .filter(|b| matches!(b, ContentBlock::Text { .. }))
                            .cloned()
                            .collect(),
                    });
                }
            }
            Role::Assistant => {
                // Only keep assistant text (not tool_use blocks)
                let text = msg.text();
                if !text.is_empty() {
                    if let Some(user_msg) = last_user.take() {
                        pairs.push((user_msg, Message::assistant(text)));
                    }
                }
            }
            Role::Tool => {
                // Skip tool results entirely
            }
        }
    }

    // Keep only the last N pairs
    let summary: Vec<Message> = pairs
        .into_iter()
        .rev()
        .take(SUMMARY_KEEP_PAIRS)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .flat_map(|(user, assistant)| vec![user, assistant])
        .collect();

    tracing::debug!(
        original_messages = messages.len(),
        summary_messages = summary.len(),
        "Conversation summarized"
    );

    // Write summary back
    if summary.is_empty() {
        tokio::fs::remove_file(&conversation_path).await.ok();
    } else {
        match serde_json::to_string(&summary) {
            Ok(json) => { tokio::fs::write(&conversation_path, json).await.ok(); }
            Err(_) => { tokio::fs::remove_file(&conversation_path).await.ok(); }
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
