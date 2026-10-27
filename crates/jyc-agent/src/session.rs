//! Session management for the in-process agent.
//!
//! Manages:
//! - Full conversation log (`.jyc/agent-conversation.json`) — complete LLM history
//!   including tool calls and results for multi-turn context
//! - Session state (`.jyc/agent-session.json`) — token and cost tracking
//!
//! On reset: session state and the raw context are cleared — the next turn
//! starts from an empty context.

use jyc_core::session_state::AGENT_CONTEXT_FILE;
use jyc_types::state_dir::resolve_state_dir;
use serde::{Deserialize, Serialize};
use std::path::Path;
use tracing;

use crate::context_reader::ContextReader;
use crate::types::{Message, PriorDepth, Role};

const SESSION_FILE: &str = "agent-session.json";

// ─── Raw context journal ─────────────────────────────────────────────

/// Append the messages to the journal, truncating a torn tail (a crash
/// mid-append) first. `offset` is the end of the journal's last complete
/// line — the reader owns that lookup, so the two never disagree about
/// where the journal ends.
async fn append_journal(
    journal_path: &Path,
    point: &crate::context_reader::AppendPoint,
    new: &[serde_json::Value],
) -> std::io::Result<()> {
    let mut buf = String::new();
    for v in new {
        if let Ok(line) = serde_json::to_string(v) {
            buf.push_str(&line);
            buf.push('\n');
        }
    }
    use tokio::io::{AsyncSeekExt, AsyncWriteExt};
    let mut f = tokio::fs::OpenOptions::new()
        .write(true)
        .create(true)
        // No truncate: the cut point is applied explicitly via set_len
        // below (a bare truncate would drop the whole prefix).
        .truncate(false)
        .open(journal_path)
        .await?;
    f.set_len(point.offset).await?;
    f.seek(std::io::SeekFrom::Start(point.offset)).await?;
    if !point.prefix.is_empty() {
        f.write_all(point.prefix).await?;
    }
    f.write_all(buf.as_bytes()).await?;
    f.sync_data().await?;
    Ok(())
}

/// Session state persisted to `.jyc/agent-session.json`.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SessionState {
    /// When this session was created (ISO 8601).
    pub created_at: String,
    /// The input the most recent LLM call was sent, in tokens. Since each
    /// call sends the full conversation context, this is the context size
    /// as the model saw it. NOT a sum across calls — for accumulated input
    /// tokens see `total_input_tokens`, for accumulated output tokens see
    /// `total_output_tokens`.
    pub context_input_tokens: u64,
    /// The output tokens of the most recent LLM call — the reply that gets
    /// appended to the context. Together with `context_input_tokens` this
    /// is the size of the context the *next* call will carry, which is
    /// what the context-window figures are computed from.
    /// `serde(default)` so session files written before the field existed
    /// deserialize as `0`.
    #[serde(default)]
    pub context_output_tokens: u64,
    /// Accumulated input tokens across all LLM calls in this session.
    /// Each call's `input_tokens` (= full context size) is added via `+=`
    /// from the agent loop, so this represents the cumulative tokens sent
    /// to the API over the session's lifetime. Reset to 0 on session reset.
    #[serde(default)]
    pub total_input_tokens: u64,
    pub total_output_tokens: u64,
    /// Accumulated prompt-cache-hit tokens across all LLM calls in this
    /// session. Each call's `cache_hit_tokens` is added via `+=` from the
    /// agent loop. `0` when the provider didn't surface cache hits for
    /// any call in this session. Reset to 0 on session reset, mirroring
    /// `total_input_tokens`.
    ///
    /// **For Anthropic**, `cache_hit_tokens` carries the cache-**read**
    /// bucket only; cache writes accumulate in
    /// [`total_cache_creation_tokens`](#field.total_cache_creation_tokens).
    /// For every other provider (OpenAI / DeepSeek / Kimi / 火山引擎 /
    /// MiniMax) this is the single reported cache bucket. See
    /// `provider::usage` for the per-vendor field mapping.
    #[serde(default)]
    pub total_cache_hit_tokens: u64,
    /// Accumulated prompt-cache-**creation** (write) tokens across
    /// all LLM calls in this session. Anthropic is the only provider
    /// that reports writes separately from reads; for every other
    /// vendor this is `0`. Reset to `0` on session reset, mirroring
    /// `total_cache_hit_tokens`. `serde(default)` so session files
    /// written before the field existed deserialize as `0`.
    #[serde(default)]
    pub total_cache_creation_tokens: u64,
    /// Accumulated reasoning (thinking) tokens across all LLM calls in
    /// this session — the hidden chain-of-thought share of the output,
    /// already included in `total_output_tokens`. Informational only.
    /// `serde(default)` so session files written before the field
    /// existed deserialize as `0`.
    #[serde(default)]
    pub total_reasoning_tokens: u64,
    /// Accumulated wall-clock time of every measured LLM call in this
    /// session, in milliseconds. Accumulated like `session_cost`: each
    /// call adds its own duration as it completes, so the file keeps the
    /// sum across turns. Covers the main-loop calls only — ancillary
    /// calls (compression, cycle summaries) are billed but not timed.
    /// Reset to 0 with the token counters. `serde(default)` so session
    /// files written before the field existed deserialize as `0`.
    #[serde(default)]
    pub total_call_duration_ms: u64,
    /// Number of LLM calls folded into `total_call_duration_ms`. The
    /// per-call average the dashboard shows is the total divided by this
    /// count; `0` means no call has been measured yet.
    #[serde(default)]
    pub call_count: u64,
    /// Accumulated wall-clock time of the LLM calls in the latest turn,
    /// in milliseconds — the turn-scoped twin of
    /// `total_call_duration_ms`. Zeroed by [`begin_turn_counters`] at every
    /// turn start, so it never carries data across turns.
    /// `serde(default)` so session files written before the field existed
    /// deserialize as `0`.
    #[serde(default)]
    pub turn_total_call_duration_ms: u64,
    /// Number of LLM calls folded into `turn_total_call_duration_ms`.
    /// `0` while the current turn has not completed a measured call yet.
    #[serde(default)]
    pub turn_call_count: u64,
    /// The prompt-cache-hit tokens of the LLM calls in the latest turn —
    /// the turn-scoped twin of `total_cache_hit_tokens`, and the numerator
    /// of the turn's cache rate. Each call's `cache_hit_tokens` is added
    /// via `+=` as the call completes. Zeroed by
    /// [`begin_turn_counters`] at every turn start, so it never carries
    /// data across turns. `serde(default)` so session files written
    /// before the field existed deserialize as `0`.
    #[serde(default)]
    pub turn_cache_hit_tokens: u64,
    /// The input tokens of the LLM calls in the latest turn — the
    /// turn-scoped twin of `total_input_tokens`, and the denominator of
    /// the turn's cache rate. Zeroed by [`begin_turn_counters`] at every
    /// turn start. `serde(default)` so session files written before the
    /// field existed deserialize as `0`.
    #[serde(default)]
    pub turn_input_tokens: u64,
    /// The output tokens of the LLM calls in the latest turn — the
    /// turn-scoped twin of `total_output_tokens`. Each call's own
    /// `output_tokens` is added via `+=` as the call completes, so the
    /// info pane can show the turn's output beside the session total.
    /// Zeroed by [`begin_turn_counters`] at every turn start.
    /// `serde(default)` so session files written before the field existed
    /// deserialize as `0`.
    #[serde(default)]
    pub turn_output_tokens: u64,
    /// The reasoning (thinking) tokens of the LLM calls in the latest
    /// turn — the turn-scoped twin of `total_reasoning_tokens`. Each
    /// call's own `reasoning_tokens` is added via `+=` as the call
    /// completes, so the info pane can show the turn's hidden
    /// chain-of-thought beside the session total, as it already does for
    /// output. Zeroed by [`begin_turn_counters`] at every turn start.
    /// `serde(default)` so session files written before the field existed
    /// deserialize as `0`.
    #[serde(default)]
    pub turn_reasoning_tokens: u64,
    /// The model's context window, in tokens — the whole budget the next
    /// call has to fit into.
    ///
    /// Renamed from `max_input_tokens`, which held the *auto-reset cap*
    /// (the window scaled by a threshold). The alias keeps session files
    /// written before the auto-reset removal readable; the next LLM call
    /// rewrites the field with the raw window.
    #[serde(default, alias = "max_input_tokens")]
    pub context_window: u64,
    /// Accumulated cost of this session, in the currency configured for
    /// the model(s) used. **One of this struct's three accumulating
    /// fields** — with `total_call_duration_ms` and `call_count`, which a
    /// call increments as it happens — whereas every other field is
    /// assigned from the caller's running total (see `persist_tokens`).
    ///
    /// Scoped to the session: zeroed on reset along with the token
    /// counters, so it answers "what has this session cost so far". The
    /// durable per-day ledger lives in `.jyc/bill-YYYY-MM-DD.jsonl`
    /// (see `jyc_core::billing_log_store`), which no reset touches.
    #[serde(default)]
    pub session_cost: f64,
}

// ─── Conversation Persistence ────────────────────────────────────────

/// Save the raw provider-formatted context to disk.
///
/// Called after each agent_loop::run() completes. Stores the raw API messages
/// exactly as they were sent/received (preserves provider-specific fields like
/// DeepSeek's reasoning_content).
///
/// `current` is this turn's messages — the ones the journal does not hold
/// yet. The journal is append-only: they are appended at the end of the
/// file and nothing before them is touched, so the write costs what the
/// turn added, not what the topic accumulated.
///
/// The reader's pair count is extended with the same messages, so the next
/// turn does not count the journal again. A failed write drops that count:
/// the next load recounts the journal instead of serving a total that never
/// reached disk.
///
/// A **closed** topic writes nothing: a turn that runs `/close` on itself
/// finishes after the close has already deleted its state dir, and recreating
/// it here would leave a parentless, config-less topic behind (the dir's
/// existence is what `list_topic_names` counts as an open topic).
pub async fn save_raw_context(
    topic_name: &str,
    topic_path: &Path,
    current: Vec<serde_json::Value>,
) {
    if jyc_types::state_dir::is_closed(topic_name) {
        tracing::debug!("save_raw_context: topic closed, skipping");
        return;
    }
    let state_dir = resolve_state_dir(topic_name, topic_path);
    tokio::fs::create_dir_all(&state_dir).await.ok();
    let journal_path = state_dir.join(AGENT_CONTEXT_FILE);
    let reader = ContextReader::new(topic_name, &state_dir);

    tracing::info!(
        message_count = current.len(),
        file = %journal_path.display(),
        "save_raw_context: saving context"
    );

    let before = reader.stat().await;
    // The append point: after the journal's last complete line, or after a
    // torn tail that is a complete message missing only its newline. When
    // it cannot be determined the append is skipped — truncating a journal
    // that is merely unreadable would destroy its history.
    let Some(point) = reader.clean_end().await else {
        tracing::warn!(
            file = %journal_path.display(),
            "save_raw_context: journal unreadable, append skipped"
        );
        reader.invalidate();
        return;
    };
    match append_journal(&journal_path, &point, &current).await {
        Ok(()) => {
            if let (Some(before), Some(after)) = (before, reader.stat().await) {
                reader.note_append(&current, before, after);
            }
        }
        Err(e) => {
            tracing::warn!(
                error = %e,
                "save_raw_context: journal write failed, reader cache dropped"
            );
            reader.invalidate();
        }
    }
}

/// Wire-payload debug dump — toggled by the user via `/context dump on|off`.
/// When enabled, every wire payload sent to the LLM is appended as one
/// JSON line to `<topic>/.jyc/wire-payload.jsonl`. The file is capped at
/// [`jyc_core::session_state::WIRE_PAYLOAD_DUMP_MAX_LINES`] lines (oldest
/// dropped) to keep a single topic's debug footprint bounded.
/// Read the on/off flag for wire-payload dumping. Returns false when the
/// flag file is absent or malformed — the absence of the file is the
/// default "off" state.
pub async fn read_wire_payload_dump_enabled(topic_name: &str, topic_path: &Path) -> bool {
    let path = resolve_state_dir(topic_name, topic_path)
        .join(jyc_core::session_state::WIRE_PAYLOAD_DUMP_FLAG_FILE);
    let Ok(bytes) = tokio::fs::read(&path).await else {
        return false;
    };
    serde_json::from_slice::<serde_json::Value>(&bytes)
        .ok()
        .and_then(|v| v.get("enabled").and_then(|e| e.as_bool()))
        .unwrap_or(false)
}

/// Append one JSON line to the wire-payload dump. Best-effort — file IO
/// errors are logged at `warn` level and never propagated, since dumping
/// is a debug-only side effect.
///
/// **Single-writer assumption**: the read-modify-write on
/// `wire-payload.jsonl` is safe because the agent loop is serial per
/// topic — one iteration runs to completion before the next starts, so
/// two appenders never race. If that ever changes, this needs a lock.
pub async fn append_wire_payload_dump(
    topic_name: &str,
    topic_path: &Path,
    iter: usize,
    strategy: &jyc_types::channel::ContextStrategyConfig,
    regions: &[u8],
    wire: &[serde_json::Value],
) {
    if jyc_types::state_dir::is_closed(topic_name) {
        return;
    }
    let state_dir = resolve_state_dir(topic_name, topic_path);
    if tokio::fs::create_dir_all(&state_dir).await.is_err() {
        return;
    }
    let path = state_dir.join(jyc_core::session_state::WIRE_PAYLOAD_DUMP_FILE);

    let entry = serde_json::json!({
        "ts": chrono::Utc::now().to_rfc3339(),
        "iter": iter,
        "strategy": strategy,
        "regions": regions,
        "wire_payload": wire,
    });
    let Ok(new_line) = serde_json::to_string(&entry) else {
        tracing::warn!("wire-payload dump: serialize failed");
        return;
    };

    let existing = tokio::fs::read_to_string(&path).await.unwrap_or_default();
    let mut lines: Vec<String> = existing.lines().map(str::to_owned).collect();
    if lines.len() >= jyc_core::session_state::WIRE_PAYLOAD_DUMP_MAX_LINES {
        let drop_n = lines.len() + 1 - jyc_core::session_state::WIRE_PAYLOAD_DUMP_MAX_LINES;
        lines.drain(..drop_n);
    }
    lines.push(new_line);
    let body = lines.join("\n") + "\n";

    if let Err(e) = tokio::fs::write(&path, body).await {
        tracing::warn!(
            error = %e,
            file = %path.display(),
            "wire-payload dump: write failed"
        );
    }
}

/// Load prior raw context from the `agent-context.jsonl` journal.
///
/// Returns (internal_messages, raw_context):
/// - internal_messages: for logic (reply detection, text extraction)
/// - raw_context: for sending to the API (preserves provider-specific fields).
///
/// `depth` is how much of the journal the caller needs: the strategies
/// read a window, `full` mode reads everything. The reader parses only
/// that much — a journal that grew to hundreds of megabytes costs the
/// same per turn as a small one.
///
/// If no session file exists (fresh or after reset), returns empty.
pub async fn load_context(
    topic_name: &str,
    topic_path: &Path,
    depth: PriorDepth,
) -> (Vec<Message>, Vec<serde_json::Value>) {
    let state_dir = resolve_state_dir(topic_name, topic_path);
    let session_path = state_dir.join(SESSION_FILE);
    let journal_path = state_dir.join(AGENT_CONTEXT_FILE);

    let empty = (Vec::new(), Vec::new());

    // No session file = fresh start. No prior context.
    if !session_path.exists() {
        tracing::warn!(
            session_path = %session_path.display(),
            journal_path = %journal_path.display(),
            "load_context: no session file, returning empty context"
        );
        return empty;
    }

    if !journal_path.exists() {
        tracing::warn!(
            journal_path = %journal_path.display(),
            "load_context: session file exists but no context file"
        );
        return empty;
    }

    let reader = ContextReader::new(topic_name, &state_dir);
    let slice = reader.read_prior(depth).await;
    let raw_context = slice.messages;

    if !raw_context.is_empty() {
        // Validate: must contain at least one assistant message
        let has_assistant = raw_context
            .iter()
            .any(|m| m.get("role").and_then(|r| r.as_str()) == Some("assistant"));
        if has_assistant {
            tracing::debug!(
                context_messages = raw_context.len(),
                reached_head = slice.reached_head,
                "Loaded raw context from agent-context.jsonl"
            );
            // Build internal messages from raw context (for reply detection logic)
            let internal = raw_context_to_messages(&raw_context);
            return (internal, raw_context);
        } else if slice.reached_head {
            // The whole journal holds no assistant message — the context
            // is corrupt (e.g. a summarization produced user-only
            // context). Only then is it safe to drop it: a window that
            // happens to hold no assistant message says nothing about
            // the turns behind it.
            tracing::error!(
                context_messages = raw_context.len(),
                roles = ?raw_context.iter()
                    .map(|m| m.get("role").and_then(|r| r.as_str()).unwrap_or("?"))
                    .collect::<Vec<_>>(),
                "Context file has no assistant messages after filtering, deleting. \
                 This indicates context corruption (e.g. summarization produced \
                 user-only context)."
            );
            jyc_core::session_state::remove_raw_context_files(&state_dir).await;
            reader.invalidate();
        }
    }

    // Fallback: no raw context available, start fresh
    empty
}

/// Convert raw provider JSON context to internal Messages (best-effort).
/// Used for internal logic only (reply detection, etc.).
fn raw_context_to_messages(raw: &[serde_json::Value]) -> Vec<Message> {
    raw.iter()
        .filter_map(|m| {
            let role = m.get("role")?.as_str()?;
            match role {
                "user" => {
                    let text = extract_text_content(m.get("content")?)?;
                    Some(Message::user(text))
                }
                "assistant" => {
                    let text = extract_text_content(m.get("content")?).unwrap_or_default();
                    if text.is_empty() {
                        // Check for tool_calls (OpenAI: separate field, Anthropic: content array)
                        let has_tool_calls = m
                            .get("tool_calls")
                            .and_then(|t| t.as_array())
                            .is_some_and(|a| !a.is_empty())
                            || m.get("content")
                                .and_then(|c| c.as_array())
                                .is_some_and(|blocks| {
                                    blocks.iter().any(|b| {
                                        b.get("type").and_then(|t| t.as_str()) == Some("tool_use")
                                    })
                                });
                        if has_tool_calls {
                            Some(Message {
                                role: Role::Assistant,
                                content: vec![], // Will be populated if needed
                            })
                        } else {
                            None
                        }
                    } else {
                        Some(Message::assistant(text))
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

/// Extract text from a provider message's `content` field.
/// Supports both OpenAI format (direct string) and Anthropic format
/// (array of typed blocks like `[{"type": "text", "text": "..."}]`).
fn extract_text_content(content: &serde_json::Value) -> Option<String> {
    // OpenAI format: "content": "text string"
    if let Some(text) = content.as_str() {
        let s = text.to_string();
        if !s.is_empty() {
            return Some(s);
        }
    }
    // Anthropic format: "content": [{"type": "text", "text": "..."}, ...]
    if let Some(blocks) = content.as_array() {
        for block in blocks {
            if block.get("type").and_then(|t| t.as_str()) == Some("text")
                && let Some(text) = block.get("text").and_then(|t| t.as_str())
                && !text.is_empty()
            {
                return Some(text.to_string());
            }
        }
    }
    None
}

// ─── Token Tracking ──────────────────────────────────────────────────

/// Ensure `.jyc/agent-session.json` exists at the start of agent
/// processing. Idempotent: if the file is already present (e.g., from a
/// prior turn), this is a no-op so existing token counts and `created_at`
/// are preserved.
///
/// Without this, a brand-new topic — or one whose session was just
/// deleted by `/reset` or `/new` — has no `agent-session.json` on disk
/// until the first mid-loop `persist_tokens` call after the first LLM
/// response. During that window, the dashboard and outbound-channel
/// probes (`read_token_state` / `read_input_tokens`) all see nothing.
/// Pre-creating the file here makes the topic visible immediately with the
/// right `context_window` and zeroed counters.
///
/// When `context_window` is `None`, the field is left at 0 (the agent
/// loop fills it in on the next turn).
pub async fn ensure_session_file(topic_name: &str, topic_path: &Path, context_window: Option<u64>) {
    let session_path = resolve_state_dir(topic_name, topic_path).join(SESSION_FILE);
    if session_path.exists() {
        // Already there — never overwrite existing token data.
        return;
    }

    let mut state = SessionState::default();
    if let Some(cw) = context_window {
        state.context_window = cw;
    }
    state.created_at = chrono::Utc::now().to_rfc3339();

    save_session_state(topic_name, &session_path, &state).await;
    tracing::info!(
        file = %session_path.display(),
        context_window = state.context_window,
        "ensure_session_file: created session file at start of agent processing"
    );
}

/// Clear the previous turn's counters so the "latest turn" figures
/// describe the turn that is starting.
///
/// Called once per turn, right after [`ensure_session_file`]. Skips the
/// write when every turn counter is already zero (a brand-new session),
/// so a turn pays one small file write only when there is something to
/// clear.
///
/// Zeroes the turn-scoped counters — the latency pair, the cache pair,
/// the output and reasoning figures — and nothing else. The session-wide counters
/// (`total_call_duration_ms` / `call_count`, the session's token totals)
/// answer a different question and outlive the turn. Session reset does
/// clear them as a side effect of rebuilding the file — a reset session
/// is a fresh slate, and the "latest turn" rows reappear once the next
/// turn makes its first call.
pub async fn begin_turn_counters(topic_name: &str, topic_path: &Path) {
    let session_path = resolve_state_dir(topic_name, topic_path).join(SESSION_FILE);
    let mut state = load_session_state(&session_path).await;
    if state.turn_call_count == 0
        && state.turn_total_call_duration_ms == 0
        && state.turn_cache_hit_tokens == 0
        && state.turn_input_tokens == 0
        && state.turn_output_tokens == 0
        && state.turn_reasoning_tokens == 0
    {
        return;
    }
    state.turn_call_count = 0;
    state.turn_total_call_duration_ms = 0;
    state.turn_cache_hit_tokens = 0;
    state.turn_input_tokens = 0;
    state.turn_output_tokens = 0;
    state.turn_reasoning_tokens = 0;
    save_session_state(topic_name, &session_path, &state).await;
}

/// The session's token totals so far — the base the agent loop's running
/// totals start from.
///
/// A turn continues the session's counts rather than restarting them: the
/// totals in `agent-session.json` are the session's, and the loop adds
/// this turn's calls on top of them.
#[derive(Debug, Clone, Copy, Default)]
pub struct TokenTotals {
    /// Input tokens of every call in the session so far.
    pub input: u64,
    /// Output tokens of every call in the session so far.
    pub output: u64,
    /// Prompt-cache-hit tokens of every call in the session so far.
    pub cache_hit: u64,
    /// Prompt-cache-creation tokens of every call in the session so far.
    pub cache_creation: u64,
    /// Reasoning tokens of every call in the session so far.
    pub reasoning: u64,
}

/// Read the session's token totals — the base the agent loop's running
/// totals start from.
///
/// Reads the same file the loop writes, so the totals it returns are the
/// ones the session has banked so far.
pub async fn session_token_totals(topic_name: &str, topic_path: &Path) -> TokenTotals {
    let session_path = resolve_state_dir(topic_name, topic_path).join(SESSION_FILE);
    let state = load_session_state(&session_path).await;
    TokenTotals {
        input: state.total_input_tokens,
        output: state.total_output_tokens,
        cache_hit: state.total_cache_hit_tokens,
        cache_creation: state.total_cache_creation_tokens,
        reasoning: state.total_reasoning_tokens,
    }
}

/// Persist the latest input/output token counts to disk. Called from the
/// agent loop after every LLM response so the dashboard polls see fresh data
/// mid-round.
///
/// `context_input_tokens` and `context_output_tokens` are the last API
/// call's own figures — stored directly (not accumulated) since each call
/// already includes the full context. `total_input_tokens`,
/// `total_output_tokens`, `total_cache_hit_tokens`, and
/// `total_cache_creation_tokens` are running totals accumulated by the
/// caller (`agent_loop`), passed in as the current sum.
///
/// `call_cost` is the cost of the single call that just completed and is
/// **added** to `session_cost` (unlike every other field, which is
/// assigned). Pass `0.0` when the model has no configured pricing.
///
/// `call_ms` is that same call's wall-clock time in milliseconds and is
/// **added** to `total_call_duration_ms` and `turn_total_call_duration_ms`
/// (each incrementing its count with it). Pass `0` when nothing was
/// measured (the post-loop call, whose duration was already banked per
/// call).
///
/// `call_cache_hit_tokens` and `call_input_tokens` are that same call's
/// own cache-hit and input tokens, **added** to the turn-scoped cache
/// pair — the numerator and the denominator of the turn's cache rate.
/// Pass `0` for both when the call is not part of the main loop (the
/// post-loop write, whose calls were banked per call).
///
/// `call_output_tokens` is that same call's own output tokens, **added**
/// to `turn_output_tokens` — the turn's share of `total_output_tokens`.
/// Pass `0` for it on the same terms as the cache pair: a call that is
/// not part of the main loop has already been banked.
///
/// `call_reasoning_tokens` is that same call's own reasoning tokens,
/// **added** to `turn_reasoning_tokens` — the turn's share of
/// `total_reasoning_tokens`, which already contains it. `0` on the same
/// terms again.
#[allow(clippy::too_many_arguments)]
pub async fn persist_tokens(
    topic_name: &str,
    topic_path: &Path,
    context_input_tokens: u64,
    context_output_tokens: u64,
    total_input_tokens: u64,
    total_output_tokens: u64,
    total_cache_hit_tokens: u64,
    total_cache_creation_tokens: u64,
    total_reasoning_tokens: u64,
    context_window: Option<u64>,
    call_cost: f64,
    call_ms: u64,
    call_cache_hit_tokens: u64,
    call_input_tokens: u64,
    call_output_tokens: u64,
    call_reasoning_tokens: u64,
) {
    let session_path = resolve_state_dir(topic_name, topic_path).join(SESSION_FILE);
    let mut state = load_session_state(&session_path).await;

    state.context_input_tokens = context_input_tokens;
    state.context_output_tokens = context_output_tokens;
    state.total_input_tokens = total_input_tokens;
    state.total_output_tokens = total_output_tokens;
    state.total_cache_hit_tokens = total_cache_hit_tokens;
    state.total_cache_creation_tokens = total_cache_creation_tokens;
    state.total_reasoning_tokens = total_reasoning_tokens;
    // The one accumulating field: each call's cost adds to the session
    // total rather than replacing it.
    state.session_cost += call_cost;
    // Accumulated the same way, and for the same reason: the caller passes
    // one call's duration and the file keeps the running sum, so the
    // mid-loop persist and the post-loop write (which passes 0) both leave
    // the average intact. The turn-scoped pair (reset by
    // `begin_turn_counters` at each turn start) covers exactly this turn's
    // measured calls.
    if call_ms > 0 {
        state.total_call_duration_ms += call_ms;
        state.call_count += 1;
        state.turn_total_call_duration_ms += call_ms;
        state.turn_call_count += 1;
    }
    // The turn-scoped cache pair: the caller passes one call's own
    // cache-hit and input tokens, and the file keeps this turn's running
    // sums. Zeroed by `begin_turn_counters` at every turn start, so
    // "latest turn" always covers exactly this turn's calls. Unlike the
    // latency pair above it counts every call, measured or not — a call
    // that completed spent tokens even when no duration was recorded.
    state.turn_cache_hit_tokens += call_cache_hit_tokens;
    state.turn_input_tokens += call_input_tokens;
    // The turn's own output and reasoning, accumulated the same way — the
    // figures the info pane pairs with the session's `total_output_tokens`
    // and `total_reasoning_tokens`.
    state.turn_output_tokens += call_output_tokens;
    state.turn_reasoning_tokens += call_reasoning_tokens;

    if let Some(cw) = context_window {
        state.context_window = cw;
    }

    if state.created_at.is_empty() {
        state.created_at = chrono::Utc::now().to_rfc3339();
    }

    save_session_state(topic_name, &session_path, &state).await;
}

/// Add to `session_cost` without touching the token counters.
///
/// Used for ancillary LLM calls (cycle-boundary progress summaries) whose
/// tokens are real spend but are not part of the main loop's context
/// accounting — folding them into `total_input_tokens` would inflate the
/// context usage shown to the user. The cost is still recorded so the
/// displayed total is truthful.
///
/// No-op when `cost` is zero (no pricing configured, or the call failed).
pub async fn add_session_cost(topic_name: &str, topic_path: &Path, cost: f64) {
    if cost <= 0.0 {
        return;
    }
    let session_path = resolve_state_dir(topic_name, topic_path).join(SESSION_FILE);
    let mut state = load_session_state(&session_path).await;
    state.session_cost += cost;
    if state.created_at.is_empty() {
        state.created_at = chrono::Utc::now().to_rfc3339();
    }
    save_session_state(topic_name, &session_path, &state).await;
}

// ─── Reset ───────────────────────────────────────────────────────────

/// Clear the session: drop the session file and the raw context.
///
/// Called when the user triggers a session reset (`/reset`, dashboard).
///
/// A reset is a clean slate — the next turn starts from an empty context.
/// There is no compression any more: the sliding-window context strategy
/// keeps the context inside the window, so there is nothing to summarize
/// before starting over.
pub async fn reset_session(topic_name: &str, topic_path: &Path) {
    let state_dir = resolve_state_dir(topic_name, topic_path);

    // Session file and the context journal go; a pre-journal
    // `agent-context.json` array is never read, so it is left alone —
    // nothing replaced it, and it is the user's data.
    jyc_core::session_state::remove_raw_context_files(&state_dir).await;
    // The reader's parsed tail describes a journal that is gone.
    ContextReader::new(topic_name, &state_dir).invalidate();
    let session_path = state_dir.join(SESSION_FILE);
    tokio::fs::remove_file(&session_path).await.ok();
    tracing::info!("Agent session reset");
}

/// Render `raw_context` as a single plain-text transcript.
/// Lossy by design — a test-only view of the raw context, never replayed to
/// the main loop.
///
/// Built on [`extract_pairs`] so this view and the sliding-window view
/// share ONE parsing/annotation implementation (previously two copies
/// parsed the OpenAI/Anthropic wire formats independently and had already
/// drifted apart). Each pair renders as `USER: ...` / `ASSISTANT: ...`; the
/// pair's tool-call history note (with truncated results) is appended on
/// its own line.
#[cfg(test)]
pub(crate) fn render_raw_context_as_text(raw_context: &[serde_json::Value]) -> String {
    let mut out = String::with_capacity(raw_context.len() * 256);
    out.push_str("=== Conversation transcript ===\n\n");
    for pair in extract_pairs(raw_context) {
        out.push_str("USER: ");
        out.push_str(&extract_message_text(&pair.user));
        out.push_str("\n\nASSISTANT: ");
        if let Some(assistant) = &pair.assistant {
            out.push_str(&extract_message_text(assistant));
        }
        if let Some(note) = &pair.note {
            out.push('\n');
            out.push_str(&extract_message_text(note));
        }
        out.push_str("\n\n");
    }
    out
}

/// Concatenate text content from a message in provider wire format. Handles
/// both OpenAI (`content: "string"`) and Anthropic (`content: [{type:"text",...}]`)
/// shapes. Tool_use / tool_result / tool_calls are ignored.
pub(crate) fn extract_message_text(msg: &serde_json::Value) -> String {
    if let Some(s) = msg.get("content").and_then(|c| c.as_str()) {
        return s.to_string();
    }
    let mut text = String::new();
    if let Some(blocks) = msg.get("content").and_then(|c| c.as_array()) {
        for b in blocks {
            if b.get("type").and_then(|t| t.as_str()) == Some("text")
                && let Some(s) = b.get("text").and_then(|x| x.as_str())
            {
                text.push_str(s);
            }
        }
    }
    text
}

/// Max length of a single rendered tool-call argument value kept in the
/// windowed annotation; longer values are truncated with an explicit
/// `… [truncated N bytes]` marker.
/// ponytail: fixed cap; raise if real arguments routinely exceed it and
/// the tail matters.
const WINDOWED_TOOL_ARG_MAX: usize = 200;

/// Max length of a tool result kept in the windowed annotation. Larger
/// than the arg cap because the result carries the grounding the agent
/// needs to pick its next step; still bounded to avoid blowing up the
/// context on a multi-call turn. The full result is one `context_browse`
/// call away.
/// ponytail: fixed cap; raise if real results routinely need more.
const WINDOWED_TOOL_RESULT_MAX: usize = 500;

/// A tool result collected for the windowed annotation: truncated text
/// plus the error flag, so failures render as `→ [error] …` and the agent
/// can tell a failed call apart from a successful one.
type WindowedResult = (String, bool);

/// Collect bare tool calls from a raw wire-format assistant message as
/// `(id, name, args)` triples.
///
/// Handles both OpenAI (`tool_calls: [{id, function: {name, arguments}}]`,
/// arguments a JSON string) and Anthropic (`content: [{type: "tool_use",
/// id, name, input}]`) shapes. `args` is the parsed JSON arguments (the
/// raw string kept as a JSON string when it does not parse, e.g. broken or
/// empty OpenAI `arguments`). The `id` links a call to its result.
fn collect_tool_calls(msg: &serde_json::Value) -> Vec<(String, String, serde_json::Value)> {
    let mut calls = Vec::new();

    // OpenAI-compat: `tool_calls` array with string `function.arguments`.
    if let Some(tool_calls) = msg.get("tool_calls").and_then(|t| t.as_array()) {
        for tc in tool_calls {
            let id = tc
                .get("id")
                .and_then(|i| i.as_str())
                .unwrap_or("")
                .to_string();
            let name = tc
                .pointer("/function/name")
                .and_then(|n| n.as_str())
                .unwrap_or("?")
                .to_string();
            let raw = tc
                .pointer("/function/arguments")
                .and_then(|a| a.as_str())
                .unwrap_or("");
            let args = serde_json::from_str(raw)
                .unwrap_or_else(|_| serde_json::Value::String(raw.to_string()));
            calls.push((id, name, args));
        }
    }

    // Anthropic: `content` array with `tool_use` blocks.
    if let Some(blocks) = msg.get("content").and_then(|c| c.as_array()) {
        for block in blocks {
            if block.get("type").and_then(|t| t.as_str()) == Some("tool_use") {
                let id = block
                    .get("id")
                    .and_then(|i| i.as_str())
                    .unwrap_or("")
                    .to_string();
                let name = block
                    .get("name")
                    .and_then(|n| n.as_str())
                    .unwrap_or("?")
                    .to_string();
                let args = block
                    .get("input")
                    .cloned()
                    .unwrap_or(serde_json::Value::Null);
                calls.push((id, name, args));
            }
        }
    }

    calls
}

/// Render one tool call as `name(k=v, …)`, keeping **all** parameters and
/// truncating only a single argument value that exceeds
/// `WINDOWED_TOOL_ARG_MAX`. (`jyc_reply_message` is filtered out at the
/// caller before reaching here.)
fn render_tool_call(name: &str, args: &serde_json::Value) -> String {
    if let Some(map) = args.as_object() {
        // Empty maps join to `name()` naturally; no empty-args special case.
        let parts = map
            .iter()
            .map(|(k, v)| format!("{k}={}", truncate_json_value(v, WINDOWED_TOOL_ARG_MAX)))
            .collect::<Vec<_>>();
        format!("{name}({})", parts.join(", "))
    } else {
        format!(
            "{name}({})",
            truncate_json_value(args, WINDOWED_TOOL_ARG_MAX)
        )
    }
}

/// Truncate `s` at `max` bytes (on a char boundary) with an explicit
/// `… [truncated N bytes]` marker when longer. The marker distinguishes a
/// real cut from content that genuinely ends in `…` and tells the model how
/// much was dropped.
pub(crate) fn truncate_text(s: &str, max: usize) -> String {
    if s.len() > max {
        let cut = s.floor_char_boundary(max);
        let dropped = s.len() - cut;
        format!("{}… [truncated {dropped} bytes]", &s[..cut])
    } else {
        s.to_string()
    }
}

/// Truncate `s` to at most `max` bytes keeping both ends: the head gets
/// two thirds of the content budget, the tail one third, and the dropped
/// middle is replaced by a marker saying how much is missing and how to
/// get it. Unlike [`truncate_text`] — where `max` is the content budget
/// and the marker sits on top of it — the marker is inside `max` here,
/// so the caller gets the byte bound it asked for.
///
/// Tail-keeping is the point: a head-only cut hides what a tool result is
/// usually read for (the error at the end of a command's output, the
/// totals of a grep). Falls back to [`truncate_text`] when `max` is too
/// small to hold the marker plus both ends.
pub(crate) fn truncate_text_keeping_edges(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    // Reserve room for the marker so the result really fits in `max`.
    // The marker is 107 bytes with a 20-digit count (a 10 EiB result is
    // not a real case, but the reserve has to cover the format either
    // way), so the head+tail split below always leaves room for it.
    const MARKER_RESERVE: usize = 128;
    let budget = max.saturating_sub(MARKER_RESERVE);
    if budget == 0 {
        return truncate_text(s, max);
    }
    let head_end = s.floor_char_boundary(budget * 2 / 3);
    let tail_start = s.ceil_char_boundary(s.len() - (budget - head_end));
    if head_end == 0 || head_end >= tail_start {
        return truncate_text(s, max);
    }
    let dropped = tail_start - head_end;
    let marker = format!(
        "\n… [{dropped} bytes dropped from the middle — re-read with \
         offset/limit to see the gap] …\n"
    );
    format!("{}{marker}{}", &s[..head_end], &s[tail_start..])
}

/// Render a JSON value as text, truncating at `max` bytes (on a char
/// boundary) with `…` when it is longer.
fn truncate_json_value(value: &serde_json::Value, max: usize) -> String {
    truncate_text(&value.to_string(), max)
}

/// Extract text from a tool result `content` value, which may be a string
/// (OpenAI / simple Anthropic) or an array of blocks (Anthropic with text
/// and image blocks). Non-text blocks are skipped.
fn extract_result_text(content: &serde_json::Value) -> String {
    if let Some(s) = content.as_str() {
        return s.to_string();
    }
    if let Some(blocks) = content.as_array() {
        let mut text = String::new();
        for b in blocks {
            if b.get("type").and_then(|t| t.as_str()) == Some("text")
                && let Some(s) = b.get("text").and_then(|x| x.as_str())
            {
                if !text.is_empty() {
                    text.push('\n');
                }
                text.push_str(s);
            }
        }
        return text;
    }
    String::new()
}

/// Strip the `[SUCCESS] `/`[ERROR] ` prefix that `format_tool_result`
/// (openai_compat) bakes into OpenAI tool-result content for the main
/// loop. The windowed annotation renders its own `[error] ` marker, so the
/// provider plumbing is redundant noise here — and `[ERROR] ` is the only
/// error signal an OpenAI `role: "tool"` message carries (no `is_error`
/// field). Returns the cleaned text and whether it was an error.
fn strip_tool_result_prefix(text: &str) -> (&str, bool) {
    if let Some(rest) = text.strip_prefix("[ERROR] ") {
        (rest, true)
    } else if let Some(rest) = text.strip_prefix("[SUCCESS] ") {
        (rest, false)
    } else {
        (text, false)
    }
}

/// Collect tool results from one raw wire-format message into `results`
/// (`tool_call_id → (truncated result text, is_error)`). Handles OpenAI
/// (`role: "tool"` with `tool_call_id`) and Anthropic (`role: "user"`
/// with `tool_result` blocks carrying `tool_use_id`) shapes. Lets the
/// windowed annotation show *what each tool returned* — and whether it
/// failed — not just that it was called.
fn collect_results_from(
    msg: &serde_json::Value,
    results: &mut std::collections::HashMap<String, WindowedResult>,
) {
    let is_error_of =
        |v: &serde_json::Value| v.get("is_error").and_then(|e| e.as_bool()).unwrap_or(false);
    match msg.get("role").and_then(|r| r.as_str()).unwrap_or("") {
        // OpenAI: {"role": "tool", "tool_call_id": "...", "content": "..."}
        "tool" => {
            if let Some(id) = msg.get("tool_call_id").and_then(|i| i.as_str()) {
                let raw = msg
                    .get("content")
                    .map(extract_result_text)
                    .unwrap_or_default();
                let (text, error_from_prefix) = strip_tool_result_prefix(&raw);
                results.insert(
                    id.to_string(),
                    (
                        truncate_text(text, WINDOWED_TOOL_RESULT_MAX),
                        is_error_of(msg) || error_from_prefix,
                    ),
                );
            }
        }
        // Anthropic: {"role": "user", "content": [{type: "tool_result", tool_use_id, content}]}
        "user" => {
            if let Some(blocks) = msg.get("content").and_then(|c| c.as_array()) {
                for block in blocks {
                    if block.get("type").and_then(|t| t.as_str()) == Some("tool_result")
                        && let Some(id) = block.get("tool_use_id").and_then(|i| i.as_str())
                    {
                        let text = block
                            .get("content")
                            .map(extract_result_text)
                            .unwrap_or_default();
                        results.insert(
                            id.to_string(),
                            (
                                truncate_text(&text, WINDOWED_TOOL_RESULT_MAX),
                                is_error_of(block),
                            ),
                        );
                    }
                }
            }
        }
        _ => {}
    }
}

/// Max total length of one message's tool-call annotation. Individual
/// args/results are already capped, but the NUMBER of parallel calls is
/// not — a 10-call turn would otherwise produce a ~7KB annotation. Calls
/// past the budget are summarized as `…(N more calls)`.
/// ponytail: fixed cap; raise if real turns routinely need more.
const WINDOWED_ANNOTATION_MAX: usize = 2000;

/// Prefix marking a user-role **history note** — the windowed view's
/// tool-call summary, emitted as its own message BEFORE the assistant text
/// so the model never sees the annotation in its own voice (models were
/// observed mimicking the previous in-text `(incl. followed tool calls:
/// …)` format and emitting fake tool-call text as their reply). The note
/// precedes the assistant text (matching the chronological order: tool
/// calls happen before the final reply) and so an `assistant → user(note)`
/// adjacency never sits right before the next real user message.
/// `extract_pairs` skips these notes so a round-tripped note does not
/// open a turn.
pub(crate) const HISTORY_NOTE_PREFIX: &str = "[History note] assistant tool calls:";

/// Wire-only marker inserted at the start of the compacted history region.
/// It is **not** persisted to `raw_context`, so `extract_pairs` must never
/// recognize it as a real user message.
pub(crate) const HISTORY_MARKER_TEXT: &str =
    "[History messages] earlier conversation follows — text-only, tool calls/results omitted.";

/// One user→assistant turn extracted from raw context for the windowed
/// view. The assistant entry holds ONLY the assistant's own text; the
/// turn's tool calls are summarized in `note` — a separate user-role
/// history note (see [`HISTORY_NOTE_PREFIX`]).
pub(crate) struct TurnPair {
    /// The turn-opening user message (original wire format).
    pub user: serde_json::Value,
    /// Pure assistant text (multi-step turns merged); `None` for
    /// text-less (tool-call-only) turns.
    pub assistant: Option<serde_json::Value>,
    /// Tool-call summary as a user-role history note; `None` when the
    /// turn made no tool calls.
    pub note: Option<serde_json::Value>,
}

/// Build a compact summary of one assistant message's bare tool calls — as
/// [`collect_tool_calls`] returns them, so the caller's parse is reused —
/// e.g. `bash(command="ls -la") → BRANCH main…`.
/// When a tool result is known for a call's `id` (looked up in `results`),
/// it is appended as `→ <truncated result>` — prefixed with `[error] `
/// when the call failed — so the agent can see *what the tool returned*,
/// not just that it was called. Returns `None` when there are no calls.
fn tool_call_summary(
    calls: &[(String, String, serde_json::Value)],
    results: &std::collections::HashMap<String, WindowedResult>,
) -> Option<String> {
    if calls.is_empty() {
        return None;
    }
    let mut parts: Vec<String> = Vec::with_capacity(calls.len());
    let mut used = 0usize;
    let mut omitted = 0usize;
    for (id, name, args) in calls {
        // Legacy `jyc_reply_message` calls (from transcripts persisted
        // before replies became plain final text) are excluded from the
        // annotation: their `message` is the text the user already saw.
        if name == "jyc_reply_message" {
            continue;
        }
        let mut s = render_tool_call(name, args);
        if let Some((result, is_error)) = results.get(id)
            && !result.is_empty()
        {
            s.push_str(" → ");
            if *is_error {
                s.push_str("[error] ");
            }
            s.push_str(result);
        }
        // +2 for the ", " separator. Always keep the first call so the
        // annotation is never empty; cap the rest.
        if !parts.is_empty() && used + s.len() + 2 > WINDOWED_ANNOTATION_MAX {
            omitted += 1;
            continue;
        }
        used += s.len() + 2;
        parts.push(s);
    }
    if omitted > 0 {
        parts.push(format!("…({omitted} more calls)"));
    }
    // All calls filtered → empty annotation. Skip the note rather than
    // emit `[History note] assistant tool calls: ` with no payload.
    (!parts.is_empty()).then(|| parts.join(", "))
}

/// Flush the accumulated turn into `pairs`: merge every assistant message
/// of the turn (tool-call steps included) into a single assistant entry
/// holding ONLY the steps' text, and collect each step's tool-call summary
/// into the pair's separate history note. Always clears the turn state.
///
/// A turn with no assistant reply yet (interrupted between user message
/// and first response) produces no pair; the fallback in
/// `extract_user_assistant_pairs` keeps the last user message for that
/// case. Assistant messages seen before any user message (no pairing
/// anchor) are dropped.
fn flush_turn(
    pairs: &mut Vec<TurnPair>,
    cur_user: &mut Option<serde_json::Value>,
    cur_assistants: &mut Vec<serde_json::Value>,
    results: &std::collections::HashMap<String, WindowedResult>,
) {
    let user_msg = cur_user.take();
    let assistants = std::mem::take(cur_assistants);
    if assistants.is_empty() {
        return;
    }
    let Some(user_msg) = user_msg else { return };

    let mut texts: Vec<String> = Vec::with_capacity(assistants.len());
    let mut summaries: Vec<String> = Vec::new();
    let mut has_payload = false;
    for msg in &assistants {
        // One extraction and one call parse per message: both the keep-rule
        // and the note need them. Keep only role + content for the text;
        // bare tool calls go into the history note so the windowed view
        // shows which tools ran and what they returned. A tool-call-only
        // step (empty text) contributes to the note instead of being
        // dropped.
        let text = extract_message_text(msg);
        let calls = collect_tool_calls(msg);
        has_payload |= keeps_turn_alive(&text, !calls.is_empty());
        if let Some(summary) = tool_call_summary(&calls, results) {
            summaries.push(summary);
        }
        if !text.is_empty() {
            texts.push(text);
        }
    }
    // Drop only when the assistant truly had nothing — no text AND no tool
    // calls (`keeps_turn_alive`, the rule the tail scan shares). `summaries`
    // can be empty while the turn still counts: a step whose every call was
    // a filtered legacy `jyc_reply_message` has no summary, yet it was a
    // tool step and the trigger user message still matters.
    if !has_payload {
        return;
    }

    let assistant = if texts.is_empty() {
        None
    } else {
        Some(serde_json::json!({
            "role": "assistant",
            "content": texts.join("\n"),
        }))
    };
    let note = if summaries.is_empty() {
        None
    } else {
        Some(serde_json::json!({
            "role": "user",
            "content": format!("{HISTORY_NOTE_PREFIX} {}", summaries.join("; ")),
        }))
    };
    pairs.push(TurnPair {
        user: user_msg,
        assistant,
        note,
    });
}

/// Whether a message's text opens a turn: non-empty, and not a round-tripped
/// history note.
///
/// The one definition shared by pair extraction, the tail scan, the verbatim
/// window and the wire-payload region split — a turn boundary that means
/// different things in different places loses or duplicates turns. It takes
/// the text so callers that already extracted it do not extract it twice.
fn opens_turn_text(text: &str) -> bool {
    !text.is_empty() && !text.starts_with(HISTORY_NOTE_PREFIX)
}

/// [`opens_turn_text`] for a whole message.
pub(crate) fn opens_turn(msg: &serde_json::Value) -> bool {
    msg.get("role").and_then(|r| r.as_str()) == Some("user")
        && opens_turn_text(&extract_message_text(msg))
}

/// Whether one assistant message keeps its turn alive: it carries text or a
/// tool call.
///
/// A turn whose assistants all fail this is dropped by [`flush_turn`] and
/// produces no pair, so [`pairs_tail_start`] must apply the same test — the
/// signature that decides a pair exists belongs next to the code that builds
/// it, not re-derived from roles.
fn keeps_turn_alive(text: &str, has_calls: bool) -> bool {
    !text.is_empty() || has_calls
}

/// [`keeps_turn_alive`] for a whole message: the tool calls are parsed only
/// when the text is empty (a text-bearing reply needs no second look).
fn message_keeps_turn_alive(msg: &serde_json::Value) -> bool {
    let text = extract_message_text(msg);
    keeps_turn_alive(&text, false) || !collect_tool_calls(msg).is_empty()
}

/// Extract **all** user+assistant turn pairs from raw context.
///
/// Pairing unit is the **turn**, not the single message: a turn opens at
/// a text-bearing user message and closes at the next one (or end of
/// context). Every assistant message in between — intermediate tool-call
/// steps and the final reply alike — is merged into one assistant entry
/// (see [`flush_turn`]), so the windowed view never loses the turn's
/// conclusion. Legacy `jyc_reply_message` calls (pre-final-text-reply
/// transcripts) are merged into the assistant text but intentionally
/// excluded from the history-note annotation (see [`tool_call_summary`]).
///
/// Assistant messages are cleaned to only role + content (strip
/// reasoning_content, tool_calls); bare tool calls are summarized into the
/// pair's separate user-role **history note** (parameters kept, over-long
/// values truncated, tool results appended as `→ <result>`), never into
/// the assistant's own text. Returns pairs in oldest→newest order.
///
/// History notes already present in the input (round-tripped from an
/// older context) are skipped — they are metadata, not turn boundaries.
///
/// A trailing turn whose user message has no assistant reply yet is
/// dropped, mirroring the windowed-view semantics — a completed turn only
/// (`extract_user_assistant_pairs` has a fallback for the no-pairs case).
///
/// Single pass: tool results are collected into a **turn-scoped** map as
/// they stream by, so a tool_call_id reused in a later turn can never
/// misattach to an earlier turn's call.
///
/// Shared by the sliding-window payload and the `context_browse` built-in
/// tool.
pub(crate) fn extract_pairs(raw_context: &[serde_json::Value]) -> Vec<TurnPair> {
    let mut pairs: Vec<TurnPair> = Vec::new();
    let mut cur_user: Option<serde_json::Value> = None;
    let mut cur_assistants: Vec<serde_json::Value> = Vec::new();
    let mut results: std::collections::HashMap<String, WindowedResult> =
        std::collections::HashMap::new();

    for msg in raw_context {
        let role = msg.get("role").and_then(|r| r.as_str()).unwrap_or("");
        match role {
            "user" => {
                let text = extract_message_text(msg);
                if text.starts_with(HISTORY_NOTE_PREFIX) {
                    // Round-tripped history note: metadata, not a turn
                    // boundary.
                    continue;
                }
                // Text-bearing user message opens a new turn; user-role
                // messages with no extractable text are Anthropic
                // tool_result wrappers, which stay in the current turn and
                // only feed the results map.
                if opens_turn_text(&text) {
                    flush_turn(&mut pairs, &mut cur_user, &mut cur_assistants, &results);
                    results.clear();
                    cur_user = Some(msg.clone());
                } else {
                    collect_results_from(msg, &mut results);
                }
            }
            "assistant" => cur_assistants.push(msg.clone()),
            // OpenAI tool results.
            "tool" => collect_results_from(msg, &mut results),
            _ => {}
        }
    }
    flush_turn(&mut pairs, &mut cur_user, &mut cur_assistants, &results);

    pairs
}

/// Index where the last `keep_pairs` pair-producing turns begin, and how
/// many pair-producing turns the slice holds.
///
/// A turn counts only when [`flush_turn`] would keep it — an assistant with
/// text or a tool call. Counting openers alone starts too late for an
/// unanswered user message; counting the presence of an assistant message
/// alone starts too late for one that carried neither (it leaves no pair
/// behind, so `take(keep_pairs)` cannot recover it). Scanning backward costs
/// one pass over the roles and the text of user messages, instead of the
/// deep clone [`extract_pairs`] spends on everything before the tail.
pub(crate) fn pairs_tail_start(
    raw_context: &[serde_json::Value],
    keep_pairs: usize,
) -> (usize, usize) {
    if keep_pairs == 0 {
        return (raw_context.len(), 0);
    }
    let mut turns = 0usize;
    let mut seen_payload = false;
    let mut start = 0usize;
    for (idx, msg) in raw_context.iter().enumerate().rev() {
        if msg.get("role").and_then(|r| r.as_str()) == Some("assistant") {
            // Same keep-rule as `flush_turn`: an assistant with neither text
            // nor tool calls leaves no pair behind.
            seen_payload |= message_keeps_turn_alive(msg);
        } else if opens_turn(msg) && seen_payload {
            turns += 1;
            if turns == keep_pairs && start == 0 {
                start = idx;
            }
            seen_payload = false;
        }
    }
    (start, turns)
}

/// How many turn openers the slice holds.
///
/// An opener is a text-bearing user message — the same boundary
/// [`extract_pairs`] and [`verbatim_start_index`] use. A turn opener
/// without a reply (an unanswered user message) is an opener without a
/// pair, so the two counts differ.
pub(crate) fn count_turn_openers(raw_context: &[serde_json::Value]) -> usize {
    raw_context.iter().filter(|m| opens_turn(m)).count()
}

/// Extract user+assistant text pairs from raw context and keep only the
/// last `keep_pairs` of them, flattened into a single Vec. Pair extraction
/// semantics are defined in [`extract_pairs`].
///
/// Only the kept tail is extracted (see [`pairs_tail_start`]): the result is
/// the same as extracting the whole history and keeping the last
/// `keep_pairs`, at a fraction of the cloning.
///
/// `note_window` limits tool-call history notes to the most recent M kept
/// pairs (`None` = notes on every kept pair); older pairs are text-only.
/// Values above `keep_pairs` clamp to `keep_pairs`.
///
/// Test convenience wrapper around [`extract_user_assistant_pairs_range`]
/// with no skipped prefix. The production sliding-window builder uses the
/// range form directly.
#[cfg(test)]
pub(crate) fn extract_user_assistant_pairs(
    raw_context: &[serde_json::Value],
    keep_pairs: usize,
    note_window: Option<usize>,
) -> Vec<serde_json::Value> {
    extract_user_assistant_pairs_impl(raw_context, keep_pairs, note_window)
}

/// Like [`extract_user_assistant_pairs`], but skips the oldest `skip_pairs`
/// pair-producing turns first. Used by the stable-prefix sliding window to
/// advance the compacted-history start without rebuilding the kept tail.
pub(crate) fn extract_user_assistant_pairs_range(
    raw_context: &[serde_json::Value],
    skip_pairs: usize,
    keep_pairs: usize,
    note_window: Option<usize>,
) -> Vec<serde_json::Value> {
    if keep_pairs == 0 {
        return Vec::new();
    }
    let (_, total_pairs) = pairs_tail_start(raw_context, usize::MAX);
    let tail_pairs = total_pairs.saturating_sub(skip_pairs);
    let tail_start = pairs_tail_start(raw_context, tail_pairs).0;
    extract_user_assistant_pairs_impl(
        &raw_context[tail_start..],
        keep_pairs.min(tail_pairs),
        note_window,
    )
}

fn extract_user_assistant_pairs_impl(
    raw_context: &[serde_json::Value],
    keep_pairs: usize,
    note_window: Option<usize>,
) -> Vec<serde_json::Value> {
    // Keep only the last N pairs, flattened as user → note → assistant
    // (the note summarizes the tool calls, which happened before the
    // assistant's final text). Start the extraction where the kept turns
    // begin: `extract_pairs` deep-clones every message it visits, and on a
    // long history the discarded prefix is the bulk of that work.
    let tail_start = pairs_tail_start(raw_context, keep_pairs).0;
    let pairs: Vec<_> = extract_pairs(&raw_context[tail_start..])
        .into_iter()
        .rev()
        .take(keep_pairs)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    // Only the last `notes_kept` pairs carry their history note.
    let notes_kept = note_window.unwrap_or(usize::MAX).min(pairs.len());
    let notes_skipped = pairs.len() - notes_kept;
    let summary: Vec<serde_json::Value> = pairs
        .into_iter()
        .enumerate()
        .flat_map(|(i, pair)| {
            let mut v = vec![pair.user];
            if i >= notes_skipped
                && let Some(note) = pair.note
            {
                v.push(note);
            }
            if let Some(assistant) = pair.assistant {
                v.push(assistant);
            }
            v
        })
        .collect();

    if summary.is_empty() {
        // Fallback: keep the LAST text-bearing user message — the closest
        // thing to an unfinished turn. (The first user message is usually
        // ancient, unrelated history and would sit misleadingly in front
        // of the current turn.) History notes are metadata, never the
        // anchor.
        if let Some(last_user) = raw_context.iter().rev().find(|m| {
            if m.get("role").and_then(|r| r.as_str()) != Some("user") {
                return false;
            }
            let text = extract_message_text(m);
            !text.is_empty() && !text.starts_with(HISTORY_NOTE_PREFIX)
        }) {
            vec![last_user.clone()]
        } else {
            Vec::new()
        }
    } else {
        summary
    }
}

/// Index in `raw_context` where the last `note_window` completed turns
/// begin (0 when `raw_context` holds fewer turns than `note_window`, and
/// `raw_context.len()` when `note_window` is 0).
///
/// The sliding-window wire payload sends those turns VERBATIM — structured
/// `tool_calls` and tool results intact — rather than folded into user-role
/// text notes, so the model keeps seeing tool calls in the wire format it
/// is supposed to emit (text notes taught thinking models to write their
/// process as content instead). Turn boundaries use the same semantics as
/// [`extract_pairs`]: a turn opens at a text-bearing user message, and
/// round-tripped history notes are metadata, never openers.
pub(crate) fn verbatim_start_index(raw_context: &[serde_json::Value], note_window: usize) -> usize {
    if note_window == 0 {
        return raw_context.len();
    }
    let mut turns = 0usize;
    for (idx, msg) in raw_context.iter().enumerate().rev() {
        if opens_turn(msg) {
            turns += 1;
            if turns == note_window {
                return idx;
            }
        }
    }
    0
}

/// Whether a raw wire-format message is a round-tripped history note
/// (user-role metadata summarizing assistant tool calls). Such messages are
/// skipped when re-sending the verbatim region — they are not conversation,
/// and re-injecting them as user text is the exact pattern that triggered
/// the thinking-content leak.
pub(crate) fn is_history_note(msg: &serde_json::Value) -> bool {
    msg.get("role").and_then(|r| r.as_str()) == Some("user")
        && extract_message_text(msg).starts_with(HISTORY_NOTE_PREFIX)
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
async fn save_session_state(topic_name: &str, path: &Path, state: &SessionState) {
    // As in `save_raw_context`: a turn that closes its own topic finishes
    // after the close deleted the state dir, and this write must not bring
    // it back.
    if jyc_types::state_dir::is_closed(topic_name) {
        return;
    }
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await.ok();
    }
    if let Ok(json) = serde_json::to_string_pretty(state) {
        tokio::fs::write(path, json).await.ok();
    }
}

#[cfg(test)]
mod tests {
    use super::{truncate_text, truncate_text_keeping_edges};
    use serde_json::json;

    /// Head+tail truncation keeps both ends: the head carries the start of
    /// the content, the tail the end (where errors and totals live), and the
    /// gap is announced with a paging hint. The result fits in `max` —
    /// the marker is inside the budget, unlike [`truncate_text`].
    #[test]
    fn truncate_text_keeping_edges_keeps_both_ends() {
        let s = "H".repeat(2000) + "TAIL-MARKER";
        let out = truncate_text_keeping_edges(&s, 600);

        // Cut the result open on the marker: the head is whatever precedes
        // it, the tail whatever follows. Asserting the conservation
        // invariant rather than hardcoded sizes keeps this pinned to the
        // arithmetic without restating it — a split that dropped or
        // duplicated bytes fails here whatever it does to the numbers.
        let (head, rest) = out.split_once("\n… [").expect("marker opener");
        let (count, rest) = rest
            .split_once(" bytes dropped from the middle")
            .expect("marker body");
        let dropped: usize = count.parse().expect("marker must state a byte count");
        let tail = rest
            .strip_prefix(" — re-read with offset/limit to see the gap] …\n")
            .expect("marker tail");

        assert!(
            head.chars().all(|c| c == 'H') && !head.is_empty(),
            "head must be the content's start: {head}"
        );
        // The tail is the last slice of the content, so it carries the end
        // plus whatever preceded it inside the budget — what matters is
        // that the end survived at all.
        assert!(
            tail.ends_with("TAIL-MARKER"),
            "tail must carry the content's end, got: {}",
            &tail[tail.len().saturating_sub(20)..]
        );
        assert!(
            tail.chars().all(|c| c == 'H' || "TAIL-MARKER".contains(c)),
            "tail must be the content's end plus its run-up: {tail}"
        );
        assert!(
            head.len() > tail.len(),
            "head takes two thirds of the budget, tail one: {} vs {}",
            head.len(),
            tail.len()
        );
        assert_eq!(
            dropped + head.len() + tail.len(),
            s.len(),
            "every byte is either kept or counted"
        );
        assert!(out.len() <= 600, "cap must hold, got {}", out.len());
    }

    /// Short input is returned untouched — the cap never pads.
    #[test]
    fn truncate_text_keeping_edges_passthrough_when_short() {
        assert_eq!(truncate_text_keeping_edges("short", 100), "short");
        // Exactly at the cap is not "longer than".
        let exact = "x".repeat(100);
        assert_eq!(truncate_text_keeping_edges(&exact, 100), exact);
    }

    /// Multi-byte characters must never be split: the cut points land on
    /// char boundaries, so indexing the result cannot panic and the tail is
    /// intact.
    #[test]
    fn truncate_text_keeping_edges_respects_char_boundaries() {
        let s = "中".repeat(500) + "END";
        let out = truncate_text_keeping_edges(&s, 400);
        assert!(out.starts_with('中'), "head must survive");
        assert!(
            out.ends_with("END"),
            "tail must survive, got: {}",
            &out[out.len() - 20..]
        );
        assert!(out.len() <= 400, "cap must hold, got {}", out.len());
    }

    /// A cap too small to hold the marker plus both ends degrades to the
    /// plain head-only cut rather than returning something over the cap.
    #[test]
    fn truncate_text_keeping_edges_falls_back_when_cap_tiny() {
        let s = "z".repeat(500) + "TAIL";
        for max in [1usize, 8, 40, 120] {
            let out = truncate_text_keeping_edges(&s, max);
            assert_eq!(
                out,
                truncate_text(&s, max),
                "max={max} must fall back to the plain cut"
            );
            assert!(
                !out.contains("TAIL"),
                "a head-only cut cannot have kept the tail"
            );
        }
    }

    /// Anthropic-shaped context (array content): assistant text pairs
    /// correctly; tool_result user-role wrappers are skipped; the whole
    /// turn's assistant steps merge into one pure-text entry, and the tool
    /// call (with its truncated result, `→ ok`) goes into a separate
    /// user-role history note preceding the assistant message.
    #[test]
    fn extract_pairs_anthropic_shape() {
        let ctx = vec![
            json!({"role": "user", "content": [{"type": "text", "text": "u1"}]}),
            json!({"role": "assistant", "content": [
                {"type": "text", "text": "a1"},
                {"type": "tool_use", "id": "t1", "name": "bash", "input": {"command": "ls"}}
            ]}),
            json!({"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "t1", "content": "ok"}
            ]}),
            json!({"role": "assistant", "content": [{"type": "text", "text": "done"}]}),
        ];
        let pairs = super::extract_user_assistant_pairs(&ctx, 10, None);
        assert_eq!(pairs.len(), 3);
        assert_eq!(pairs[0], ctx[0]);
        assert_eq!(
            pairs[1],
            json!({"role": "user", "content":
                "[History note] assistant tool calls: bash(command=\"ls\") → ok"})
        );
        assert_eq!(
            pairs[2],
            json!({"role": "assistant", "content": "a1\ndone"})
        );
    }

    /// OpenAI-shaped context: tool calls render with all parameters,
    /// multiple calls comma-separated, in the note — NOT in the assistant
    /// text (the model must never see the annotation in its own voice).
    #[test]
    fn extract_pairs_annotates_tool_call_args() {
        let ctx = vec![
            json!({"role": "user", "content": "u1"}),
            json!({"role": "assistant", "content": "running", "tool_calls": [
                {"id": "1", "type": "function", "function": {"name": "bash", "arguments": r#"{"command": "ls -la", "timeout": 30}"#}},
                {"id": "2", "type": "function", "function": {"name": "read", "arguments": r#"{"path": "a.txt"}"#}},
            ]}),
        ];
        let pairs = super::extract_user_assistant_pairs(&ctx, 10, None);
        assert_eq!(pairs.len(), 3);
        assert_eq!(
            pairs[1],
            json!({"role": "user", "content":
                "[History note] assistant tool calls: bash(command=\"ls -la\", timeout=30), read(path=\"a.txt\")"})
        );
        assert_eq!(pairs[2], json!({"role": "assistant", "content": "running"}));
    }

    /// Tool-call-only assistant turns (no text) are kept via the note —
    /// no empty assistant message is emitted.
    #[test]
    fn extract_pairs_keeps_tool_call_only_assistant() {
        let ctx = vec![
            json!({"role": "user", "content": "u1"}),
            json!({"role": "assistant", "content": "", "tool_calls": [
                {"id": "1", "type": "function", "function": {"name": "bash", "arguments": r#"{"command": "ls -la"}"#}}
            ]}),
        ];
        let pairs = super::extract_user_assistant_pairs(&ctx, 10, None);
        assert_eq!(pairs.len(), 2);
        assert_eq!(pairs[0], json!({"role": "user", "content": "u1"}));
        assert_eq!(
            pairs[1],
            json!({"role": "user", "content":
                "[History note] assistant tool calls: bash(command=\"ls -la\")"})
        );
    }

    /// `note_window = Some(1)`: only the most recent kept pair carries its
    /// history note; older pairs in the window are text-only.
    #[test]
    fn extract_pairs_note_window_keeps_recent_notes_only() {
        let ctx = vec![
            json!({"role": "user", "content": "u1"}),
            json!({"role": "assistant", "content": "a1", "tool_calls": [
                {"id": "1", "type": "function", "function": {"name": "bash", "arguments": r#"{"command": "ls"}"#}}
            ]}),
            json!({"role": "user", "content": "u2"}),
            json!({"role": "assistant", "content": "a2", "tool_calls": [
                {"id": "2", "type": "function", "function": {"name": "read", "arguments": r#"{"path": "f"}"#}}
            ]}),
        ];
        let pairs = super::extract_user_assistant_pairs(&ctx, 10, Some(1));
        assert_eq!(
            pairs,
            vec![
                json!({"role": "user", "content": "u1"}),
                json!({"role": "assistant", "content": "a1"}),
                json!({"role": "user", "content": "u2"}),
                json!({"role": "user", "content":
                    "[History note] assistant tool calls: read(path=\"f\")"}),
                json!({"role": "assistant", "content": "a2"}),
            ]
        );
    }

    #[test]
    fn extract_pairs_range_skips_oldest_pairs() {
        let ctx = vec![
            json!({"role": "user", "content": "u1"}),
            json!({"role": "assistant", "content": "a1"}),
            json!({"role": "user", "content": "u2"}),
            json!({"role": "assistant", "content": "a2"}),
            json!({"role": "user", "content": "u3"}),
            json!({"role": "assistant", "content": "a3"}),
        ];
        assert_eq!(
            super::extract_user_assistant_pairs_range(&ctx, 1, 1, Some(0)),
            vec![
                json!({"role": "user", "content": "u2"}),
                json!({"role": "assistant", "content": "a2"}),
            ]
        );
    }

    /// `note_window = Some(0)`: text-only window, no notes at all.
    #[test]
    fn extract_pairs_note_window_zero_drops_all_notes() {
        let ctx = vec![
            json!({"role": "user", "content": "u1"}),
            json!({"role": "assistant", "content": "a1", "tool_calls": [
                {"id": "1", "type": "function", "function": {"name": "bash", "arguments": r#"{"command": "ls"}"#}}
            ]}),
        ];
        let pairs = super::extract_user_assistant_pairs(&ctx, 10, Some(0));
        assert_eq!(
            pairs,
            vec![
                json!({"role": "user", "content": "u1"}),
                json!({"role": "assistant", "content": "a1"}),
            ]
        );
    }

    /// `note_window` above the number of kept pairs clamps: every pair
    /// keeps its note (same as `None`).
    #[test]
    fn extract_pairs_note_window_clamps_to_kept_pairs() {
        let ctx = vec![
            json!({"role": "user", "content": "u1"}),
            json!({"role": "assistant", "content": "a1", "tool_calls": [
                {"id": "1", "type": "function", "function": {"name": "bash", "arguments": r#"{"command": "ls"}"#}}
            ]}),
        ];
        let pairs = super::extract_user_assistant_pairs(&ctx, 10, Some(99));
        assert_eq!(pairs.len(), 3);
        assert_eq!(
            pairs[1],
            json!({"role": "user", "content":
                "[History note] assistant tool calls: bash(command=\"ls\")"})
        );
    }

    /// Over-long single argument values are truncated with `…`; the tool
    /// call list itself is never capped.
    #[test]
    fn extract_pairs_truncates_long_tool_args() {
        let long = "x".repeat(500);
        let ctx = vec![
            json!({"role": "user", "content": "u1"}),
            json!({"role": "assistant", "content": "running", "tool_calls": [
                {"id": "1", "type": "function", "function": {"name": "bash", "arguments": json!({"command": long}).to_string()}}
            ]}),
        ];
        let pairs = super::extract_user_assistant_pairs(&ctx, 10, None);
        let content = pairs[1]["content"].as_str().unwrap();
        assert!(
            content.contains("[History note] assistant tool calls: bash(command="),
            "no note"
        );
        assert!(content.contains('…'), "long arg not truncated");
        // 200-char cap + ellipsis + `command=` prefix + quotes + note prefix.
        assert!(content.len() < 400, "note too big: {content:?}");
    }

    /// Tool results are appended to the note as `→ <truncated text>`,
    /// matched by the call's id; calls without a known result get no arrow.
    #[test]
    fn extract_pairs_includes_tool_results() {
        let ctx = vec![
            json!({"role": "user", "content": "u1"}),
            json!({"role": "assistant", "content": "running", "tool_calls": [
                {"id": "1", "type": "function", "function": {"name": "bash", "arguments": r#"{"command": "ls"}"#}},
                {"id": "2", "type": "function", "function": {"name": "read", "arguments": r#"{"path": "a.txt"}"#}}
            ]}),
            // Result for call 1 only; call 2 has no result.
            json!({"role": "tool", "tool_call_id": "1", "content": "BRANCH main"}),
        ];
        let pairs = super::extract_user_assistant_pairs(&ctx, 10, None);
        let content = pairs[1]["content"].as_str().unwrap();
        // Call 1 gets its result appended after the arrow.
        assert!(
            content.contains(r#"bash(command="ls") → BRANCH main"#),
            "result not appended: {content:?}"
        );
        // Call 2 has no result: no arrow, just the rendered call.
        assert!(
            content.contains(r#"read(path="a.txt")"#),
            "call missing from note: {content:?}"
        );
        assert!(
            !content.contains(r#"read(path="a.txt") →"#),
            "call without result should have no arrow: {content:?}"
        );
    }

    /// Over-long tool results are truncated with `… [truncated N bytes]`
    /// (500-char cap).
    #[test]
    fn extract_pairs_truncates_long_tool_results() {
        let long = "y".repeat(2000);
        let ctx = vec![
            json!({"role": "user", "content": "u1"}),
            json!({"role": "assistant", "content": "", "tool_calls": [
                {"id": "1", "type": "function", "function": {"name": "bash", "arguments": "{}"}}
            ]}),
            json!({"role": "tool", "tool_call_id": "1", "content": long}),
        ];
        let pairs = super::extract_user_assistant_pairs(&ctx, 10, None);
        let content = pairs[1]["content"].as_str().unwrap();
        assert!(content.contains('→'), "no result arrow: {content:?}");
        assert!(
            content.contains('…'),
            "long result not truncated: {content:?}"
        );
        assert!(
            content.contains("truncated"),
            "truncation must be explicit, not a bare ellipsis: {content:?}"
        );
        // Result cap 500 + marker + arrow + note prefix stay well under 700.
        assert!(content.len() < 700, "note too big: {content:?}");
    }

    /// `extract_pairs` returns ALL completed pairs in oldest→newest order
    /// (no keep-pairs truncation), skipping tool/result messages and
    /// dropping an unpaired trailing user turn.
    #[test]
    fn extract_pairs_returns_all_pairs_in_order() {
        let ctx = vec![
            json!({"role": "user", "content": "u1"}),
            json!({"role": "assistant", "content": "a1"}),
            json!({"role": "user", "content": "u2"}),
            json!({"role": "assistant", "content": "a2", "tool_calls": [
                {"id": "1", "type": "function", "function": {"name": "bash", "arguments": "{}"}}
            ]}),
            json!({"role": "tool", "tool_call_id": "1", "content": "out"}),
            json!({"role": "user", "content": "u3"}),
            json!({"role": "assistant", "content": "a3"}),
            // Unpaired trailing user: no assistant reply follows.
            json!({"role": "user", "content": "u4"}),
        ];
        let pairs = super::extract_pairs(&ctx);
        assert_eq!(pairs.len(), 3);
        assert_eq!(pairs[0].user["content"], "u1");
        assert_eq!(pairs[0].assistant.as_ref().unwrap()["content"], "a1");
        assert!(pairs[0].note.is_none());
        assert_eq!(pairs[1].user["content"], "u2");
        assert_eq!(pairs[1].assistant.as_ref().unwrap()["content"], "a2");
        assert_eq!(
            pairs[1].note.as_ref().unwrap()["content"],
            "[History note] assistant tool calls: bash() → out"
        );
        assert_eq!(pairs[2].user["content"], "u3");
        assert_eq!(pairs[2].assistant.as_ref().unwrap()["content"], "a3");
        // u4 must NOT be paired.
        assert!(pairs.iter().all(|p| p.user["content"] != "u4"));
    }

    /// A multi-step tool turn (user → assistant(call) → tool → assistant(call)
    /// → tool → assistant(final reply)) merges into ONE pair: every step's
    /// text is kept in the assistant entry, ending with the turn's
    /// conclusion, and both steps' tool calls land in the note. Before
    /// turn-based pairing, only the FIRST assistant message was paired and
    /// the final reply was silently dropped.
    #[test]
    fn extract_pairs_merges_multi_step_tool_turn() {
        let ctx = vec![
            json!({"role": "user", "content": "u1"}),
            json!({"role": "assistant", "content": "step1", "tool_calls": [
                {"id": "1", "type": "function", "function": {"name": "bash", "arguments": "{}"}}
            ]}),
            json!({"role": "tool", "tool_call_id": "1", "content": "out1"}),
            json!({"role": "assistant", "content": "step2", "tool_calls": [
                {"id": "2", "type": "function", "function": {"name": "read", "arguments": "{}"}}
            ]}),
            json!({"role": "tool", "tool_call_id": "2", "content": "out2"}),
            json!({"role": "assistant", "content": "final answer"}),
        ];
        let pairs = super::extract_pairs(&ctx);
        assert_eq!(pairs.len(), 1);
        let text = pairs[0].assistant.as_ref().unwrap()["content"]
            .as_str()
            .unwrap();
        assert!(text.contains("step1"), "step1 lost: {text:?}");
        assert!(text.contains("step2"), "step2 lost: {text:?}");
        assert!(
            text.ends_with("final answer"),
            "turn conclusion must be kept: {text:?}"
        );
        let note = pairs[0].note.as_ref().unwrap()["content"].as_str().unwrap();
        assert!(note.contains("bash() → out1"), "call1 lost: {note:?}");
        assert!(note.contains("read() → out2"), "call2 lost: {note:?}");
    }

    /// Tool results are matched per turn: a tool_call_id reused in a later
    /// turn must not misattach its result to the earlier turn's call.
    #[test]
    fn extract_pairs_scopes_results_per_turn() {
        let ctx = vec![
            json!({"role": "user", "content": "u1"}),
            json!({"role": "assistant", "content": "a1", "tool_calls": [
                {"id": "1", "type": "function", "function": {"name": "bash", "arguments": "{}"}}
            ]}),
            json!({"role": "tool", "tool_call_id": "1", "content": "first"}),
            json!({"role": "user", "content": "u2"}),
            json!({"role": "assistant", "content": "a2", "tool_calls": [
                {"id": "1", "type": "function", "function": {"name": "bash", "arguments": "{}"}}
            ]}),
            json!({"role": "tool", "tool_call_id": "1", "content": "second"}),
        ];
        let pairs = super::extract_pairs(&ctx);
        assert_eq!(pairs.len(), 2);
        assert!(
            pairs[0].note.as_ref().unwrap()["content"]
                .as_str()
                .unwrap()
                .contains("→ first"),
            "turn 1 got wrong result: {:?}",
            pairs[0].note
        );
        assert!(
            pairs[1].note.as_ref().unwrap()["content"]
                .as_str()
                .unwrap()
                .contains("→ second"),
            "turn 2 got wrong result: {:?}",
            pairs[1].note
        );
    }

    /// Consecutive text-bearing user messages (user sent again before any
    /// reply): the unanswered message is dropped, the latest one pairs.
    #[test]
    fn extract_pairs_consecutive_users_keeps_latest() {
        let ctx = vec![
            json!({"role": "user", "content": "u1"}),
            json!({"role": "user", "content": "u2"}),
            json!({"role": "assistant", "content": "a2"}),
        ];
        let pairs = super::extract_pairs(&ctx);
        assert_eq!(pairs.len(), 1);
        assert_eq!(pairs[0].user["content"], "u2");
        assert_eq!(pairs[0].assistant.as_ref().unwrap()["content"], "a2");
        assert!(pairs[0].note.is_none());
    }

    /// The literal rule the tail scan shares with `flush_turn`: an assistant
    /// with neither text nor tool calls leaves no pair, so the turn before it
    /// is the one kept. This is the exact shape that diverged in review — the
    /// differential test below cannot catch a drift of the rule itself, since
    /// both sides of it would drift together.
    #[test]
    fn extract_user_assistant_pairs_drops_assistant_without_text_or_calls() {
        let ctx = vec![
            json!({"role": "user", "content": "u1"}),
            json!({"role": "assistant", "content": "a1"}),
            json!({"role": "user", "content": "u2"}),
            json!({"role": "assistant", "content": ""}),
        ];
        assert_eq!(
            super::extract_user_assistant_pairs(&ctx, 1, Some(0)),
            vec![
                json!({"role": "user", "content": "u1"}),
                json!({"role": "assistant", "content": "a1"}),
            ]
        );
    }

    /// The tail scan must return exactly what the old full extraction plus a
    /// `take` returned, across the shapes that stress turn boundaries and the
    /// keep counts the sliding window uses.
    #[test]
    fn extract_user_assistant_pairs_matches_full_extraction() {
        // The old behaviour (`note_window = Some(0)`), kept as the oracle.
        fn full_tail(ctx: &[serde_json::Value], keep: usize) -> Vec<serde_json::Value> {
            let pairs = super::extract_pairs(ctx);
            let skip = pairs.len().saturating_sub(keep);
            pairs[skip..]
                .iter()
                .flat_map(|p| {
                    let mut v = vec![p.user.clone()];
                    if let Some(assistant) = &p.assistant {
                        v.push(assistant.clone());
                    }
                    v
                })
                .collect()
        }

        let shapes: Vec<Vec<serde_json::Value>> = vec![
            // Plain turns.
            vec![
                json!({"role": "user", "content": "u1"}),
                json!({"role": "assistant", "content": "a1"}),
                json!({"role": "user", "content": "u2"}),
                json!({"role": "assistant", "content": "a2"}),
                json!({"role": "user", "content": "u3"}),
                json!({"role": "assistant", "content": "a3"}),
            ],
            // Trailing unanswered turn.
            vec![
                json!({"role": "user", "content": "u1"}),
                json!({"role": "assistant", "content": "a1"}),
                json!({"role": "user", "content": "u2"}),
                json!({"role": "assistant", "content": "a2"}),
                json!({"role": "user", "content": "never answered"}),
            ],
            // Trailing assistant with neither text nor tool calls: no pair.
            vec![
                json!({"role": "user", "content": "u1"}),
                json!({"role": "assistant", "content": "a1"}),
                json!({"role": "user", "content": "u2"}),
                json!({"role": "assistant", "content": ""}),
            ],
            // Consecutive users, tool-only assistant, Anthropic tool_result
            // wrapper, round-tripped note.
            vec![
                json!({"role": "user", "content": "u1"}),
                json!({"role": "assistant", "content": "a1"}),
                json!({"role": "user", "content": "u2"}),
                json!({"role": "user", "content": "u2b"}),
                json!({"role": "assistant", "content": [{"type": "text", "text": "a2"}]}),
                json!({"role": "user", "content": [{"type": "tool_result", "tool_use_id": "t1", "content": "r1"}]}),
                json!({"role": "assistant", "content": [{"type": "tool_use", "id": "t2", "name": "bash", "input": {}}]}),
                json!({"role": "user", "content": "[History note] assistant tool calls: bash"}),
                json!({"role": "user", "content": "u3"}),
                json!({"role": "assistant", "content": "a3"}),
            ],
        ];

        for ctx in &shapes {
            for keep in 1..=3 {
                assert_eq!(
                    super::extract_user_assistant_pairs(ctx, keep, Some(0)),
                    full_tail(ctx, keep),
                    "keep={keep}, ctx={ctx:?}"
                );
            }
        }
    }

    /// The tail scan must not be fooled by an unanswered user message at the
    /// end: it produces no pair, so the kept tail starts at the last turn
    /// that has an assistant reply.
    #[test]
    fn extract_user_assistant_pairs_skips_trailing_orphan_turn() {
        let ctx = vec![
            json!({"role": "user", "content": "u1"}),
            json!({"role": "assistant", "content": "a1"}),
            json!({"role": "user", "content": "u2"}),
            json!({"role": "assistant", "content": "a2"}),
            json!({"role": "user", "content": "never answered"}),
        ];
        let pairs = super::extract_user_assistant_pairs(&ctx, 1, Some(0));
        assert_eq!(pairs.len(), 2);
        assert_eq!(pairs[0]["content"], "u2");
        assert_eq!(pairs[1]["content"], "a2");
    }

    /// A history note round-tripped through compaction is metadata, not a
    /// turn boundary: it must not open a new turn nor pair with the next
    /// assistant message.
    #[test]
    fn extract_pairs_skips_round_tripped_notes() {
        let ctx = vec![
            json!({"role": "user", "content": "u1"}),
            json!({"role": "assistant", "content": "a1"}),
            json!({"role": "user", "content":
                "[History note] assistant tool calls: bash(command=\"ls\") → ok"}),
            json!({"role": "user", "content": "u2"}),
            json!({"role": "assistant", "content": "a2"}),
        ];
        let pairs = super::extract_pairs(&ctx);
        assert_eq!(pairs.len(), 2);
        assert_eq!(pairs[0].user["content"], "u1");
        assert_eq!(pairs[1].user["content"], "u2");
        assert_eq!(pairs[1].assistant.as_ref().unwrap()["content"], "a2");
    }

    /// No complete pairs: the fallback keeps the LAST text-bearing user
    /// message (nearest to an unfinished turn), not the first — and skips
    /// Anthropic tool_result wrappers and history notes, which carry no
    /// real user text.
    #[test]
    fn fallback_keeps_last_text_user() {
        let ctx = vec![
            json!({"role": "user", "content": "ancient"}),
            json!({"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "t1", "content": "ok"}
            ]}),
            json!({"role": "user", "content": "latest"}),
            json!({"role": "user", "content":
                "[History note] assistant tool calls: bash() → ok"}),
        ];
        let pairs = super::extract_user_assistant_pairs(&ctx, 10, None);
        assert_eq!(pairs, vec![json!({"role": "user", "content": "latest"})]);
    }

    /// Failed tool results render as `→ [error] …` so the agent can tell a
    /// failed call apart from a successful one. Covers both the OpenAI
    /// (`is_error` on the tool message) and Anthropic (`is_error` on the
    /// tool_result block) shapes.
    #[test]
    fn extract_pairs_marks_error_results() {
        let ctx = vec![
            json!({"role": "user", "content": [{"type": "text", "text": "u1"}]}),
            json!({"role": "assistant", "content": "", "tool_calls": [
                {"id": "1", "type": "function", "function": {"name": "bash", "arguments": "{}"}}
            ]}),
            json!({"role": "tool", "tool_call_id": "1", "content": "boom", "is_error": true}),
            json!({"role": "user", "content": [{"type": "text", "text": "u2"}]}),
            json!({"role": "assistant", "content": [
                {"type": "tool_use", "id": "t2", "name": "read", "input": {"path": "x"}}
            ]}),
            json!({"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "t2", "content": "nope", "is_error": true}
            ]}),
        ];
        let pairs = super::extract_pairs(&ctx);
        assert_eq!(pairs.len(), 2);
        assert!(
            pairs[0].note.as_ref().unwrap()["content"]
                .as_str()
                .unwrap()
                .contains("bash() → [error] boom"),
            "openai error not marked: {:?}",
            pairs[0].note
        );
        assert!(
            pairs[1].note.as_ref().unwrap()["content"]
                .as_str()
                .unwrap()
                .contains(r#"read(path="x") → [error] nope"#),
            "anthropic error not marked: {:?}",
            pairs[1].note
        );
    }

    /// The `[SUCCESS] `/`[ERROR] ` prefix that `format_tool_result`
    /// bakes into OpenAI tool-result content is stripped from the note
    /// (provider plumbing, not user-facing) — and `[ERROR] ` is honored as
    /// the error signal, so both providers render failures as `→ [error] …`.
    #[test]
    fn extract_pairs_strips_provider_result_prefix() {
        let ctx = vec![
            json!({"role": "user", "content": "u1"}),
            json!({"role": "assistant", "content": "", "tool_calls": [
                {"id": "1", "type": "function", "function": {"name": "bash", "arguments": "{}"}},
                {"id": "2", "type": "function", "function": {"name": "read", "arguments": r#"{"path": "x"}"#}}
            ]}),
            json!({"role": "tool", "tool_call_id": "1", "content": "[SUCCESS] ok"}),
            json!({"role": "tool", "tool_call_id": "2", "content": "[ERROR] boom"}),
        ];
        let pairs = super::extract_pairs(&ctx);
        let note = pairs[0].note.as_ref().unwrap()["content"].as_str().unwrap();
        assert!(
            note.contains(r#"bash() → ok"#),
            "success prefix not stripped: {note:?}"
        );
        assert!(
            note.contains(r#"read(path="x") → [error] boom"#),
            "error not detected from [ERROR] prefix: {note:?}"
        );
        assert!(
            !note.contains("[SUCCESS]") && !note.contains("[ERROR]"),
            "provider plumbing leaked into the note: {note:?}"
        );
    }

    /// Legacy `jyc_reply_message` calls are filtered out of the annotation:
    /// a turn that called only the reply tool produces no note at all (the
    /// message the user saw is already in the assistant's own text).
    #[test]
    fn extract_pairs_no_note_when_only_reply_was_called() {
        let ctx = vec![
            json!({"role": "user", "content": "u1"}),
            json!({"role": "assistant", "content": "", "tool_calls": [
                {"id": "1", "type": "function", "function": {"name": "jyc_reply_message",
                    "arguments": r#"{"message": "hi"}"#}}
            ]}),
        ];
        let pairs = super::extract_pairs(&ctx);
        assert!(
            pairs[0].note.is_none(),
            "reply-only turn must not emit a history note: {:?}",
            pairs[0].note
        );
    }

    /// A mixed turn (real work + a legacy reply call) keeps the real work
    /// in the note and drops the reply call — the reply's message is
    /// already in the assistant text, so the annotation must not duplicate
    /// it.
    #[test]
    fn extract_pairs_drops_reply_call_but_keeps_real_work() {
        let ctx = vec![
            json!({"role": "user", "content": "u1"}),
            json!({"role": "assistant", "content": "looking", "tool_calls": [
                {"id": "1", "type": "function", "function": {"name": "bash",
                    "arguments": r#"{"command": "ls"}"#}},
                {"id": "2", "type": "function", "function": {"name": "jyc_reply_message",
                    "arguments": r#"{"message": "done"}"#}}
            ]}),
        ];
        let pairs = super::extract_pairs(&ctx);
        let content = pairs[0].note.as_ref().unwrap()["content"].as_str().unwrap();
        assert!(
            content.contains(r#"bash(command="ls")"#),
            "real call missing from note: {content:?}"
        );
        assert!(
            !content.contains("jyc_reply_message"),
            "reply call must not appear in note: {content:?}"
        );
    }

    /// The note as a whole is capped: calls past the budget are
    /// dropped and summarized as `…(N more calls)`; the first call is
    /// always kept.
    #[test]
    fn extract_pairs_caps_total_annotation_length() {
        let arg = "x".repeat(super::WINDOWED_TOOL_ARG_MAX);
        let calls: Vec<serde_json::Value> = (0..10)
            .map(|i| {
                json!({"id": i.to_string(), "type": "function",
                    "function": {"name": "bash",
                        "arguments": json!({"command": arg}).to_string()}})
            })
            .collect();
        let ctx = vec![
            json!({"role": "user", "content": "u1"}),
            json!({"role": "assistant", "content": "", "tool_calls": calls}),
        ];
        let pairs = super::extract_pairs(&ctx);
        let content = pairs[0].note.as_ref().unwrap()["content"].as_str().unwrap();
        assert!(
            content.len() < super::WINDOWED_ANNOTATION_MAX + 100,
            "note not capped: {} bytes",
            content.len()
        );
        assert!(
            content.contains("more calls"),
            "omitted calls not summarized: {content:?}"
        );
        assert!(
            content.contains("bash("),
            "first call must be kept: {content:?}"
        );
    }

    #[tokio::test]
    async fn wire_payload_dump_flag_off_by_default() {
        let tmp = tempfile::TempDir::new().unwrap();
        jyc_types::state_dir::register("wire-flag-default", &tmp.path().join(".jyc"));
        assert!(
            !super::read_wire_payload_dump_enabled("wire-flag-default", tmp.path()).await,
            "no flag file → enabled=false"
        );
    }

    #[tokio::test]
    async fn wire_payload_dump_flag_roundtrip() {
        let tmp = tempfile::TempDir::new().unwrap();
        jyc_types::state_dir::register("wire-flag-roundtrip", &tmp.path().join(".jyc"));
        let state_dir = tmp.path().join(".jyc");
        tokio::fs::create_dir_all(&state_dir).await.unwrap();
        tokio::fs::write(
            state_dir.join(jyc_core::session_state::WIRE_PAYLOAD_DUMP_FLAG_FILE),
            r#"{"enabled":true}"#,
        )
        .await
        .unwrap();
        assert!(super::read_wire_payload_dump_enabled("wire-flag-roundtrip", tmp.path()).await);

        tokio::fs::write(
            state_dir.join(jyc_core::session_state::WIRE_PAYLOAD_DUMP_FLAG_FILE),
            r#"{"enabled":false}"#,
        )
        .await
        .unwrap();
        assert!(!super::read_wire_payload_dump_enabled("wire-flag-roundtrip", tmp.path()).await);

        // Malformed JSON must be tolerated as "off", not panic.
        tokio::fs::write(
            state_dir.join(jyc_core::session_state::WIRE_PAYLOAD_DUMP_FLAG_FILE),
            "not json",
        )
        .await
        .unwrap();
        assert!(!super::read_wire_payload_dump_enabled("wire-flag-roundtrip", tmp.path()).await);
    }

    #[tokio::test]
    async fn wire_payload_dump_appends_and_caps_at_max_lines() {
        use jyc_types::channel::{ContextStrategy, ContextStrategyConfig};

        let tmp = tempfile::TempDir::new().unwrap();
        jyc_types::state_dir::register("wire-dump-caps", &tmp.path().join(".jyc"));
        let cfg = ContextStrategyConfig {
            mode: ContextStrategy::SlidingWindow,
            window: 5,
            note_window: Some(2),
            tool_result_cap: None,
            turn_tool_result_cap: None,
            max_history_tokens: None,
        };
        let wire = vec![serde_json::json!({"role":"user","content":"hi"})];
        let regions = vec![3u8];

        // Write WIRE_PAYLOAD_DUMP_MAX_LINES + 5 entries.
        let total = jyc_core::session_state::WIRE_PAYLOAD_DUMP_MAX_LINES + 5;
        for i in 0..total {
            super::append_wire_payload_dump("wire-dump-caps", tmp.path(), i, &cfg, &regions, &wire)
                .await;
        }

        let body = tokio::fs::read_to_string(
            tmp.path()
                .join(".jyc")
                .join(jyc_core::session_state::WIRE_PAYLOAD_DUMP_FILE),
        )
        .await
        .unwrap();
        let lines: Vec<&str> = body.lines().collect();
        assert_eq!(
            lines.len(),
            jyc_core::session_state::WIRE_PAYLOAD_DUMP_MAX_LINES,
            "should be capped at MAX_LINES"
        );
        // The oldest entries are dropped; the newest `iter` is preserved.
        let last: serde_json::Value = serde_json::from_str(lines.last().unwrap()).unwrap();
        assert_eq!(last["iter"], total - 1, "last entry must be the newest");
        let first: serde_json::Value = serde_json::from_str(lines.first().unwrap()).unwrap();
        assert_eq!(
            first["iter"],
            total - jyc_core::session_state::WIRE_PAYLOAD_DUMP_MAX_LINES,
            "first must be the (total - MAX) entry"
        );
        // Round-trip the regions array.
        assert_eq!(
            last["regions"],
            serde_json::json!(regions),
            "regions preserved"
        );
    }

    /// Empty `wire_payload` / `regions` should serialize as `[]` and be
    /// readable back; nothing in the dump pipeline assumes non-empty
    /// input.
    #[tokio::test]
    async fn wire_payload_dump_handles_empty_payload() {
        use jyc_types::channel::{ContextStrategy, ContextStrategyConfig};

        let tmp = tempfile::TempDir::new().unwrap();
        jyc_types::state_dir::register("wire-dump-empty", &tmp.path().join(".jyc"));
        let cfg = ContextStrategyConfig {
            mode: ContextStrategy::Full,
            window: 10,
            note_window: None,
            tool_result_cap: None,
            turn_tool_result_cap: None,
            max_history_tokens: None,
        };
        let empty_wire: Vec<serde_json::Value> = vec![];
        let empty_regions: Vec<u8> = vec![];
        super::append_wire_payload_dump(
            "wire-dump-empty",
            tmp.path(),
            0,
            &cfg,
            &empty_regions,
            &empty_wire,
        )
        .await;

        let body = tokio::fs::read_to_string(
            tmp.path()
                .join(".jyc")
                .join(jyc_core::session_state::WIRE_PAYLOAD_DUMP_FILE),
        )
        .await
        .unwrap();
        let lines: Vec<&str> = body.lines().collect();
        assert_eq!(lines.len(), 1);
        let entry: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(entry["wire_payload"], serde_json::json!([]));
        assert_eq!(entry["regions"], serde_json::json!([]));
        assert_eq!(entry["iter"], 0);
    }
}
