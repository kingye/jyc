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
    pub created_at: String,
    pub total_input_tokens: u64,
    pub total_output_tokens: u64,
    /// Message dir of the oldest entry to include in context.
    /// If None, includes all available history.
    pub context_start_marker: Option<String>,
}

/// Load prior conversation context from chat_history files.
///
/// Reads the latest chat history entries and converts them into
/// Message objects for the LLM's conversation context.
pub async fn load_context(thread_path: &Path) -> Vec<Message> {
    // Check if session was reset (no session file = fresh start)
    let session_path = thread_path.join(".jyc").join("agent-session.json");
    let session_state = load_session_state(&session_path).await;

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

        let entries = parse_chat_entries(&content, session_state.context_start_marker.as_deref());

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
pub async fn update_tokens(thread_path: &Path, input_tokens: u64, output_tokens: u64) {
    let session_path = thread_path.join(".jyc").join("agent-session.json");
    let mut state = load_session_state(&session_path).await;

    state.total_input_tokens += input_tokens;
    state.total_output_tokens += output_tokens;

    if state.created_at.is_empty() {
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
/// <!-- timestamp | type:received | sender:... -->
/// **FROM:** ...
/// **SUBJECT:** ...
///
/// message content...
///
/// ---
/// ```
fn parse_chat_entries(content: &str, context_start_marker: Option<&str>) -> Vec<Message> {
    let mut messages = Vec::new();
    let mut current_type: Option<&str> = None;
    let mut current_text = String::new();
    let mut past_marker = context_start_marker.is_none(); // If no marker, include all

    for line in content.lines() {
        // Check for metadata comment
        if line.starts_with("<!-- ") && line.ends_with(" -->") {
            // If we have accumulated text from a previous entry, save it
            if let Some(msg_type) = current_type {
                if past_marker && !current_text.trim().is_empty() {
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

            // Check for context_start_marker in the metadata line
            if let Some(marker) = context_start_marker {
                if line.contains(marker) {
                    past_marker = true;
                }
            }
        } else if line == "---" {
            // Entry separator — flush current entry
            if let Some(msg_type) = current_type {
                if past_marker && !current_text.trim().is_empty() {
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
        if past_marker && !current_text.trim().is_empty() {
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
