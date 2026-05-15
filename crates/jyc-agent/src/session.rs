//! Session management for the in-process agent.
//!
//! Tracks conversation context and token usage per thread.
//! Reads chat_history_*.md to build prior conversation context for the LLM.

use serde::{Deserialize, Serialize};
use std::path::Path;
use tracing;

use crate::types::Message;

/// Maximum number of prior messages to include as context.
const MAX_CONTEXT_MESSAGES: usize = 20;

/// Maximum total character length of prior context (rough token proxy: ~4 chars/token).
const MAX_CONTEXT_CHARS: usize = 100_000;

/// Session state persisted to `.jyc/agent-session.json`.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SessionState {
    /// When this session was created (ISO 8601). Only messages after this
    /// timestamp are included in the conversation context.
    pub created_at: String,
    pub total_input_tokens: u64,
    pub total_output_tokens: u64,
    /// Max tokens (context window) for the model.
    #[serde(default)]
    pub max_input_tokens: u64,
}

/// Load prior conversation context from chat_history files.
///
/// Reads the latest chat history entries and converts them into
/// Message objects for the LLM's conversation context.
///
/// Only includes entries created AFTER the session's `created_at` timestamp.
/// If no agent-session.json exists (fresh thread or after reset),
/// returns empty history — the agent starts with no prior context.
pub async fn load_context(thread_path: &Path) -> Vec<Message> {
    let session_path = thread_path.join(".jyc").join("agent-session.json");

    // No session file = fresh start (or after reset). No prior context.
    if !session_path.exists() {
        return Vec::new();
    }

    let session_state = load_session_state(&session_path).await;

    // Parse session created_at as cutoff timestamp
    let cutoff = chrono::DateTime::parse_from_rfc3339(&session_state.created_at)
        .map(|dt| dt.with_timezone(&chrono::Utc))
        .ok();

    // Find chat history files
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

    // Read and parse entries from most recent files
    let mut messages: Vec<Message> = Vec::new();
    let mut total_chars: usize = 0;

    // Read files in reverse (newest first) to respect the character limit
    for file in history_files.iter().rev() {
        let content = match std::fs::read_to_string(file) {
            Ok(c) => c,
            Err(_) => continue,
        };

        let entries = parse_chat_entries(&content, cutoff.as_ref());

        for entry in entries.into_iter().rev() {
            if messages.len() >= MAX_CONTEXT_MESSAGES {
                break;
            }
            let entry_len = entry.text().len();
            if total_chars + entry_len > MAX_CONTEXT_CHARS {
                break;
            }
            total_chars += entry_len;
            messages.push(entry);
        }

        if messages.len() >= MAX_CONTEXT_MESSAGES {
            break;
        }
    }

    // Reverse to chronological order
    messages.reverse();

    if !messages.is_empty() {
        tracing::debug!(
            context_messages = messages.len(),
            context_chars = total_chars,
            "Loaded conversation context from chat history"
        );
    }

    messages
}

/// Update token tracking in the session state.
/// Creates the session file if it doesn't exist (sets created_at for context cutoff).
/// Auto-resets the session if total tokens exceed max_input_tokens (context overflow).
pub async fn update_tokens(thread_path: &Path, input_tokens: u64, output_tokens: u64, context_window: Option<u64>) {
    let session_path = thread_path.join(".jyc").join("agent-session.json");
    let mut state = load_session_state(&session_path).await;

    state.total_input_tokens += input_tokens;
    state.total_output_tokens += output_tokens;

    if let Some(cw) = context_window {
        // Use 95% of context window as max input tokens (reserve 5% for output)
        state.max_input_tokens = (cw as f64 * 0.95) as u64;
    }

    // Set created_at on first creation — this becomes the cutoff for context loading
    if state.created_at.is_empty() {
        state.created_at = chrono::Utc::now().to_rfc3339();
    }

    // Auto-reset if tokens exceed max context window
    if state.max_input_tokens > 0 && state.total_input_tokens >= state.max_input_tokens {
        tracing::info!(
            total_input_tokens = state.total_input_tokens,
            max_input_tokens = state.max_input_tokens,
            "Session exceeded max input tokens, auto-resetting"
        );
        // Reset: clear tokens and set new created_at (excludes old history from context)
        state.total_input_tokens = 0;
        state.total_output_tokens = 0;
        state.created_at = chrono::Utc::now().to_rfc3339();
    }

    save_session_state(&session_path, &state).await;
}

/// Reset the session (clears context marker, resets tokens).
/// Called when user triggers a session reset (e.g., from dashboard).
pub async fn reset_session(thread_path: &Path) {
    let session_path = thread_path.join(".jyc").join("agent-session.json");
    tokio::fs::remove_file(&session_path).await.ok();
    tracing::info!("Agent session reset");
}

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

/// Parse chat history markdown entries into Messages.
///
/// Chat history format:
/// ```
/// <!-- 2026-05-15T03:15:29+00:00 | type:received | sender:... -->
/// **FROM:** ...
/// **SUBJECT:** ...
///
/// message content...
///
/// ---
/// ```
///
/// Only includes entries with timestamps >= cutoff (if provided).
fn parse_chat_entries(content: &str, cutoff: Option<&chrono::DateTime<chrono::Utc>>) -> Vec<Message> {
    let mut messages = Vec::new();
    let mut current_type: Option<&str> = None;
    let mut current_text = String::new();
    let mut current_after_cutoff = cutoff.is_none(); // If no cutoff, include all

    for line in content.lines() {
        // Check for metadata comment
        if line.starts_with("<!-- ") && line.ends_with(" -->") {
            // If we have accumulated text from a previous entry, save it
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

            // Parse metadata
            current_type = if line.contains("type:received") {
                Some("received")
            } else if line.contains("type:reply") {
                Some("reply")
            } else {
                None
            };
            current_text.clear();

            // Check timestamp against cutoff
            if let Some(cutoff_ts) = cutoff {
                current_after_cutoff = extract_timestamp(line)
                    .map(|ts| ts >= *cutoff_ts)
                    .unwrap_or(false);
            }
        } else if line == "---" {
            // Entry separator — flush current entry
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
            // Skip metadata lines (FROM, SUBJECT, etc.)
            if !line.starts_with("**FROM:**") && !line.starts_with("**SUBJECT:**") {
                current_text.push_str(line);
                current_text.push('\n');
            }
        }
    }

    // Flush last entry if any
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
/// Format: `<!-- 2026-05-15T03:15:29+00:00 | type:received | ... -->`
fn extract_timestamp(line: &str) -> Option<chrono::DateTime<chrono::Utc>> {
    let inner = line.strip_prefix("<!-- ")?.strip_suffix(" -->")?;
    let ts_str = inner.split('|').next()?.trim();
    chrono::DateTime::parse_from_rfc3339(ts_str)
        .map(|dt| dt.with_timezone(&chrono::Utc))
        .ok()
}
