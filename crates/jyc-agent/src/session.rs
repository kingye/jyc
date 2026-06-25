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
    if context_path.exists()
        && let Ok(content) = tokio::fs::read_to_string(&context_path).await
        && let Ok(raw_context) = serde_json::from_str::<Vec<serde_json::Value>>(&content)
    {
        // Filter out invalid assistant messages (no content, no tool_calls)
        let raw_context = crate::provider::filter_valid_messages(&raw_context);

        if !raw_context.is_empty() {
            // Validate: must contain at least one assistant message
            let has_assistant = raw_context
                .iter()
                .any(|m| m.get("role").and_then(|r| r.as_str()) == Some("assistant"));
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

    // Fallback: no raw context available, start fresh
    (Vec::new(), Vec::new())
}

/// Convert raw provider JSON context to internal Messages (best-effort).
/// Used for internal logic only (reply detection, etc.).
fn raw_context_to_messages(raw: &[serde_json::Value]) -> Vec<Message> {
    raw.iter()
        .filter_map(|m| {
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
                    Some(Message::tool_result(
                        tool_call_id.to_string(),
                        content.to_string(),
                        false,
                    ))
                }
                _ => None,
            }
        })
        .collect()
}

// ─── Token Tracking ──────────────────────────────────────────────────

/// Update token tracking in the session state.
/// Creates the session file if it doesn't exist.
/// Auto-resets (with LLM-generated summary) if total tokens exceed max_input_tokens.
///
/// `input_tokens` is the tokens reported by the last API call — this already
/// includes all prior context, so we store it directly (not accumulated).
///
/// `summary_provider` is the provider used to generate the LLM summary when
/// the auto-reset threshold is crossed. Callers should pass the small model's
/// provider when configured (`[agent].small_model`), otherwise the main
/// provider — falling back is the caller's responsibility.
pub async fn update_tokens(
    thread_path: &Path,
    input_tokens: u64,
    output_tokens: u64,
    context_window: Option<u64>,
    summary_provider: &dyn crate::provider::Provider,
) {
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
            summary_provider = %summary_provider.name(),
            summary_model = %summary_provider.model(),
            "Session exceeded max input tokens, auto-resetting with summary"
        );

        // Summarize the context using the (small) summary provider.
        summarize_context(thread_path, summary_provider).await;

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
///
/// Uses the heuristic compaction (no LLM call) because the inspect server's
/// `reset_session` handler doesn't have a provider context to pass through.
/// The auto-reset path in `update_tokens` uses the LLM-based summarizer when
/// a provider is configured.
pub async fn reset_session(thread_path: &Path) {
    let jyc_dir = thread_path.join(".jyc");

    // Summarize context before deleting session (heuristic; no provider).
    summarize_context_heuristic(thread_path).await;

    // Delete session state (triggers fresh start on next invocation)
    let session_path = jyc_dir.join(SESSION_FILE);
    tokio::fs::remove_file(&session_path).await.ok();

    tracing::info!("Agent session reset (context summarized)");
}

/// Summarize the raw context using an LLM call, then replace
/// `agent-context.json` with a compact `[task_anchor, summary_user_message]`
/// pair so the next message starts from a small, valid context.
///
/// On any failure (no context file, JSON parse error, LLM call error, empty
/// reply) this falls back to `summarize_context_heuristic` which keeps the
/// last few user+assistant text pairs without touching the LLM.
///
/// `provider` should be the small/fast model when configured
/// (`[agent].small_model`); the caller is responsible for passing the right
/// provider.
async fn summarize_context(thread_path: &Path, provider: &dyn crate::provider::Provider) {
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

    if raw_context.is_empty() {
        return;
    }

    // Find the original task anchor — the first user message — so we can
    // preserve it in the compacted output. Without it the model would lose
    // the task description on the next message.
    let first_user = raw_context
        .iter()
        .find(|m| m.get("role").and_then(|r| r.as_str()) == Some("user"))
        .cloned();

    // Render the entire context to plain text and ask the LLM to summarize.
    // Mirrors the cycle-boundary helper in `agent_loop`.
    let joined = render_raw_context_as_text(&raw_context);
    let summary_text = match generate_context_summary(provider, &joined).await {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(
                error = %e,
                "LLM context summary failed, falling back to heuristic compaction"
            );
            summarize_context_heuristic(thread_path).await;
            return;
        }
    };

    // Build the compacted context: [task_anchor, synthetic_user_with_summary].
    // The synthetic user message uses a tagged delimiter so the model can
    // recognize it as machine-generated context.
    let summary_user = serde_json::json!({
        "role": "user",
        "content": format!(
            "<jyc-context-summary>\nPrior conversation summary (auto-generated when token budget was exceeded):\n\n{}\n</jyc-context-summary>",
            summary_text
        ),
    });

    let mut compacted: Vec<serde_json::Value> = Vec::with_capacity(2);
    if let Some(fu) = first_user {
        compacted.push(fu);
    }
    compacted.push(summary_user);

    tracing::info!(
        original_messages = raw_context.len(),
        summary_messages = compacted.len(),
        provider = %provider.name(),
        model = %provider.model(),
        "Context summarized via LLM"
    );

    match serde_json::to_string(&compacted) {
        Ok(json) => {
            tokio::fs::write(&context_path, json).await.ok();
        }
        Err(_) => {
            tokio::fs::remove_file(&context_path).await.ok();
        }
    }
}

/// Issue an isolated LLM call to produce a context summary.
///
/// The conversation transcript is sent as a single user message — no tools,
/// no prior assistant turns, no `reasoning_content` round-trip. This decouples
/// the summary call from the main conversation's contract (e.g., DeepSeek's
/// thinking mode requirements).
async fn generate_context_summary(
    provider: &dyn crate::provider::Provider,
    joined_history: &str,
) -> anyhow::Result<String> {
    let system_prompt = "You are summarizing a conversation between a user and an AI agent. \
        Based on the transcript below, produce a faithful, concise summary in the language used \
        in the transcript. Cover:\n\
        - The original task / user goal\n\
        - Key decisions made and why\n\
        - What was implemented (files changed, commands run, tools used)\n\
        - Outstanding work and next steps\n\n\
        Reply with ONLY the summary text. No preamble, no markdown headers, no tool calls.";

    let user_msg = provider.format_user_message(&[ContentBlock::Text {
        text: joined_history.to_string(),
    }]);
    let stream = provider
        .complete_raw(&[user_msg], &[], system_prompt)
        .await?;

    let mut text = String::new();
    use futures::StreamExt;
    let mut stream = std::pin::pin!(stream);
    while let Some(event) = stream.next().await {
        match event {
            Ok(crate::types::StreamEvent::TextDelta(t)) => text.push_str(&t),
            Ok(crate::types::StreamEvent::Done) => break,
            Ok(crate::types::StreamEvent::Error(msg)) => {
                anyhow::bail!("LLM error during summary: {msg}");
            }
            Ok(_) => {}
            Err(e) => return Err(e),
        }
    }

    if text.is_empty() {
        anyhow::bail!("LLM returned empty context summary");
    }
    Ok(text)
}

/// Render `raw_context` as a single plain-text transcript suitable for
/// one-shot summarization. Lossy by design — used only by the summary call,
/// never replayed to the main loop.
fn render_raw_context_as_text(raw_context: &[serde_json::Value]) -> String {
    let mut out = String::with_capacity(raw_context.len() * 256);
    out.push_str("=== Conversation transcript ===\n\n");
    for msg in raw_context {
        let role = msg
            .get("role")
            .and_then(|r| r.as_str())
            .unwrap_or("unknown");
        match role {
            "user" => {
                let text = msg.get("content").and_then(|c| c.as_str()).unwrap_or("");
                if !text.is_empty() {
                    out.push_str("USER: ");
                    out.push_str(text);
                    out.push_str("\n\n");
                }
            }
            "assistant" => {
                out.push_str("ASSISTANT");
                if let Some(text) = msg.get("content").and_then(|c| c.as_str())
                    && !text.is_empty()
                {
                    out.push_str(": ");
                    out.push_str(text);
                }
                if let Some(blocks) = msg.get("content").and_then(|c| c.as_array()) {
                    for block in blocks {
                        let t = block.get("type").and_then(|t| t.as_str()).unwrap_or("");
                        match t {
                            "text" => {
                                if let Some(s) = block.get("text").and_then(|x| x.as_str()) {
                                    out.push_str(": ");
                                    out.push_str(s);
                                }
                            }
                            "tool_use" => {
                                let name =
                                    block.get("name").and_then(|n| n.as_str()).unwrap_or("?");
                                out.push_str(&format!("\n  [tool_use: {}]", name));
                            }
                            _ => {}
                        }
                    }
                }
                if let Some(tcs) = msg.get("tool_calls").and_then(|t| t.as_array()) {
                    for tc in tcs {
                        let name = tc
                            .get("function")
                            .and_then(|f| f.get("name"))
                            .and_then(|n| n.as_str())
                            .unwrap_or("?");
                        out.push_str(&format!("\n  [tool_call: {}]", name));
                    }
                }
                out.push_str("\n\n");
            }
            "tool" => {
                let text = msg.get("content").and_then(|c| c.as_str()).unwrap_or("");
                let truncated = if text.len() > 500 {
                    let cut = text.floor_char_boundary(500);
                    format!("{}…", &text[..cut])
                } else {
                    text.to_string()
                };
                out.push_str("TOOL_RESULT: ");
                out.push_str(&truncated);
                out.push_str("\n\n");
            }
            _ => {}
        }
    }
    out
}

/// Heuristic context compaction: keep only the last N user+assistant text
/// pairs.
///
/// Removes tool calls, tool results, and reasoning_content. Used as a
/// fallback when the LLM-based summarizer is unavailable or fails, and as
/// the primary path for user-triggered `reset_session` (which has no
/// provider context).
async fn summarize_context_heuristic(thread_path: &Path) {
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
                if !content.is_empty()
                    && let Some(user_msg) = last_user.take()
                {
                    // Keep only role + content (strip reasoning_content, tool_calls)
                    let clean_assistant = serde_json::json!({
                        "role": "assistant",
                        "content": content,
                    });
                    pairs.push((user_msg, clean_assistant));
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
        "Context summarized (heuristic)"
    );

    // Write summary back
    if summary.is_empty() {
        tokio::fs::remove_file(&context_path).await.ok();
    } else {
        match serde_json::to_string(&summary) {
            Ok(json) => {
                tokio::fs::write(&context_path, json).await.ok();
            }
            Err(_) => {
                tokio::fs::remove_file(&context_path).await.ok();
            }
        }
    }
}

// ─── Internal helpers ────────────────────────────────────────────────

/// Load session state from disk.
async fn load_session_state(path: &Path) -> SessionState {
    if path.exists()
        && let Ok(content) = tokio::fs::read_to_string(path).await
        && let Ok(state) = serde_json::from_str(&content)
    {
        return state;
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

/// Fallback: Load context from chat_history_*.jsonl files (text-only).
#[allow(dead_code)]
async fn load_from_chat_history(
    thread_path: &Path,
    cutoff: Option<&chrono::DateTime<chrono::Utc>>,
) -> Vec<Message> {
    let mut history_files: Vec<_> = match std::fs::read_dir(thread_path) {
        Ok(entries) => entries
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.file_name()
                    .to_str()
                    .is_some_and(|n| n.starts_with("chat_history_") && n.ends_with(".jsonl"))
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

        let entries = parse_jsonl_entries(&content, cutoff);
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

/// Parse JSONL chat history entries into Messages.
#[allow(dead_code)]
fn parse_jsonl_entries(
    content: &str,
    cutoff: Option<&chrono::DateTime<chrono::Utc>>,
) -> Vec<Message> {
    let mut messages = Vec::new();

    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        let record: serde_json::Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue,
        };

        // Apply cutoff filter
        if let Some(cutoff_ts) = cutoff
            && let Some(ts_str) = record.get("ts").and_then(|v| v.as_str())
            && let Ok(ts) = chrono::DateTime::parse_from_rfc3339(ts_str)
            && ts.with_timezone(&chrono::Utc) < *cutoff_ts
        {
            continue;
        }

        let msg_type = record.get("type").and_then(|v| v.as_str()).unwrap_or("");
        let content = record
            .get("content")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        let msg = match msg_type {
            "received" => Message::user(content),
            "reply" => Message::assistant(content),
            _ => continue,
        };

        messages.push(msg);
    }

    messages
}
