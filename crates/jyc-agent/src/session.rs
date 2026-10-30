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
use jyc_types::ContextRegionInfo;
use jyc_types::channel::ContextStrategyConfig;
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
    /// How the most recent completed LLM call's wire messages split into
    /// context regions. Empty when no call has completed, and on session
    /// files written before the field existed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub context_composition: Vec<ContextRegionInfo>,
    /// Journal byte offset where the sliding window's oldest retained turn
    /// begins (`agent-context.jsonl`). The window reads forward from here,
    /// so between slides the wire prefix is stable across turns — what the
    /// provider's prompt cache matches on.
    ///
    /// `None` = no anchor yet (fresh topic, upgraded session file, or a
    /// reset): the next load cold-starts from the journal's head and
    /// persists an anchor. `serde(default)` so session files written
    /// before the field existed deserialize as `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_offset: Option<u64>,
    /// Accumulated cost of this session, in the currency configured for
    /// the model(s) used. **One of this struct's accumulating fields** —
    /// with `total_call_duration_ms`, `call_count` and `turn_cost`, which a
    /// call increments as it happens — whereas every other field is
    /// assigned from the caller's running total (see `persist_tokens`).
    ///
    /// Scoped to the session: zeroed on reset along with the token
    /// counters, so it answers "what has this session cost so far". The
    /// durable per-day ledger lives in `.jyc/bill-YYYY-MM-DD.jsonl`
    /// (see `jyc_core::billing_log_store`), which no reset touches.
    #[serde(default)]
    pub session_cost: f64,
    /// Accumulated cost of the LLM calls in the latest turn — the
    /// turn-scoped twin of `session_cost`, and never larger than it. Each
    /// call's own cost is added via `+=` as the call completes, so a turn
    /// that runs many iterations climbs while it is still running rather
    /// than only at the end. Zeroed by [`begin_turn_counters`] at every
    /// turn start. `serde(default)` so session files written before the
    /// field existed deserialize as `0`.
    #[serde(default)]
    pub turn_cost: f64,
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
/// `Full` mode reads the whole journal. `sliding_window` reads an
/// **anchored window**: the journal forward from the anchor stored in the
/// session state ([`SessionState::context_offset`]), so the window only
/// ever grows between slides — the loader never trims its head. The wire
/// payload that follows from this is append-only: the text-only history
/// grows by append and only the current turn is re-sent (see
/// `build_send_context_cached`). The window slides (anchor moves forward, whole turns dropped) when it holds
/// more than [`ContextStrategyConfig::max_window`] turns, or when it has
/// reached that bound and its estimated weight exceeds
/// `max_history_tokens`; the slide returns it to `window` turns.
///
/// A missing or stale anchor cold-starts: the journal is read from its
/// head once and the anchor is dropped (a window that covers the whole
/// journal needs none); the first slide persists a new one.
///
/// If no session file exists (fresh or after reset), returns empty.
pub async fn load_context(
    topic_name: &str,
    topic_path: &Path,
    strategy: &ContextStrategyConfig,
) -> (Vec<Message>, Vec<serde_json::Value>) {
    let state_dir = resolve_state_dir(topic_name, topic_path);
    let session_path = state_dir.join(SESSION_FILE);
    let journal_path = state_dir.join(AGENT_CONTEXT_FILE);

    // No session file = fresh start. No prior context.
    if !session_path.exists() {
        tracing::warn!(
            session_path = %session_path.display(),
            journal_path = %journal_path.display(),
            "load_context: no session file, returning empty context"
        );
        return (Vec::new(), Vec::new());
    }

    if !journal_path.exists() {
        tracing::warn!(
            journal_path = %journal_path.display(),
            "load_context: session file exists but no context file"
        );
        return (Vec::new(), Vec::new());
    }

    let reader = ContextReader::new(topic_name, &state_dir);
    match strategy.mode {
        jyc_types::channel::ContextStrategy::Full => {
            let slice = reader.read_prior(PriorDepth::All).await;
            finish_load(&reader, &state_dir, slice.messages, slice.reached_head).await
        }
        jyc_types::channel::ContextStrategy::SlidingWindow => {
            load_context_windowed(&reader, &state_dir, topic_name, strategy).await
        }
    }
}

/// `load_context`'s anchored sliding-window arm: read the journal forward
/// from the persisted anchor, and slide the anchor when the window has
/// outgrown its bound.
///
/// The read is bounded by how far the window has grown, which
/// `max_window` (and the optional `max_history_tokens`) keeps small — the
/// only unbounded read is the cold start, once per anchor loss.
async fn load_context_windowed(
    reader: &ContextReader,
    state_dir: &Path,
    topic_name: &str,
    strategy: &ContextStrategyConfig,
) -> (Vec<Message>, Vec<serde_json::Value>) {
    let journal_len = tokio::fs::metadata(state_dir.join(AGENT_CONTEXT_FILE))
        .await
        .map(|m| m.len())
        .unwrap_or(0);

    let stored = read_context_offset(state_dir).await;

    let mut slice = match stored {
        Some(offset) if offset > 0 && offset < journal_len => {
            let slice = reader.read_from(offset).await;
            // A stale anchor — the journal was replaced or truncated under
            // it — parses garbage. Only a slice that opens on a turn
            // boundary (what the writer guarantees) is trusted; anything
            // else falls back to a cold start and drops the anchor, so the
            // next turn does not re-read the whole journal and warn again.
            if slice.messages.first().is_some_and(opens_turn) {
                slice
            } else {
                tracing::warn!(
                    offset,
                    "sliding-window anchor is not a turn boundary; cold-starting"
                );
                clear_context_offset(topic_name, state_dir).await;
                reader.read_from(0).await
            }
        }
        Some(offset) => {
            // Past the journal's end (or 0, which the writer never
            // persists): the anchor names nothing. A cold start needs no
            // anchor at all — the window covers the whole journal — and a
            // new one is written when it next slides.
            tracing::warn!(
                offset,
                journal_len,
                "sliding-window anchor is outside the journal; cold-starting"
            );
            clear_context_offset(topic_name, state_dir).await;
            reader.read_from(0).await
        }
        None => reader.read_from(0).await,
    };

    let turns = count_turn_openers(&slice.messages);
    let max_window = strategy.max_window();
    let over_budget = strategy
        .max_history_tokens
        .filter(|&budget| budget > 0)
        .is_some_and(|budget| {
            slice
                .messages
                .iter()
                .map(crate::agent_loop::context::est_message_tokens)
                .sum::<usize>()
                > budget
        });
    // The budget is a second trigger for the *same* slide, not a per-turn
    // trim: it may only fire at the top of the extension cycle, where the
    // turn bound is about to fire anyway. A window whose `window` turns
    // alone exceed the budget can never get under it, and chasing it every
    // turn would move the anchor on every turn — re-encoding the whole
    // payload, the failure this design exists to avoid.
    let budget_slide = over_budget && turns >= max_window;

    let mut reached_head = slice.reached_head;
    if (turns > max_window || budget_slide)
        && let Some((index, offset)) =
            window_start(&slice.messages, &slice.offsets, strategy.window)
    {
        tracing::info!(
            turns,
            max_window,
            over_budget,
            dropped_turns = count_turn_openers(&slice.messages[..index]),
            new_offset = offset,
            "sliding window: anchored tail outgrew its bound, sliding back to `window` turns"
        );
        write_context_offset(topic_name, state_dir, offset).await;
        slice.messages.drain(..index);
        slice.offsets.drain(..index);
        reached_head = false;
    }

    finish_load(reader, state_dir, slice.messages, reached_head).await
}

/// Shared tail of both arms: validate the assembled prior and build the
/// internal view. The journal is only deleted when the slice we validated
/// covers all of it (`reached_head`) and holds no assistant message at
/// all — that is corruption (e.g. a summarization produced user-only
/// context); a window that happens to hold no assistant message says
/// nothing about the turns behind it.
async fn finish_load(
    reader: &ContextReader,
    state_dir: &Path,
    raw: Vec<serde_json::Value>,
    reached_head: bool,
) -> (Vec<Message>, Vec<serde_json::Value>) {
    if raw.is_empty() {
        return (Vec::new(), raw);
    }
    let has_assistant = raw
        .iter()
        .any(|m| m.get("role").and_then(|r| r.as_str()) == Some("assistant"));
    if !has_assistant && reached_head {
        tracing::error!(
            context_messages = raw.len(),
            roles = ?raw.iter()
                .map(|m| m.get("role").and_then(|r| r.as_str()).unwrap_or("?"))
                .collect::<Vec<_>>(),
            "Context file has no assistant messages after filtering, deleting. \
             This indicates context corruption (e.g. summarization produced \
             user-only context)."
        );
        jyc_core::session_state::remove_raw_context_files(state_dir).await;
        reader.invalidate();
        return (Vec::new(), Vec::new());
    }
    tracing::debug!(
        context_messages = raw.len(),
        reached_head,
        "Loaded raw context from agent-context.jsonl"
    );
    let internal = raw_context_to_messages(&raw);
    (internal, raw)
}

/// The persisted sliding-window anchor: the journal byte offset where the
/// window's oldest retained turn begins. `None` when the session file
/// predates the field or has no anchor yet — the loader then cold-starts.
async fn read_context_offset(state_dir: &Path) -> Option<u64> {
    load_session_state(&state_dir.join(SESSION_FILE))
        .await
        .context_offset
}

/// Persist the sliding-window anchor. Best-effort, like the counters: a
/// missing anchor costs one cold-start read, not a lost turn.
async fn write_context_offset(topic_name: &str, state_dir: &Path, offset: u64) {
    let path = state_dir.join(SESSION_FILE);
    let mut state = load_session_state(&path).await;
    if state.context_offset == Some(offset) {
        return;
    }
    state.context_offset = Some(offset);
    save_session_state(topic_name, &path, &state).await;
}

/// Drop the persisted sliding-window anchor. Used when the stored offset
/// cannot be trusted (the journal was replaced, truncated or shortened
/// under it): leaving it in place would make every later load repeat the
/// cold start and its warning.
async fn clear_context_offset(topic_name: &str, state_dir: &Path) {
    let path = state_dir.join(SESSION_FILE);
    let mut state = load_session_state(&path).await;
    if state.context_offset.take().is_none() {
        return;
    }
    save_session_state(topic_name, &path, &state).await;
}

/// Where the retained window begins after a slide: the index (into
/// `messages`) and the journal byte offset of the turn that keeps exactly
/// `keep` turns — the start of the `keep`-th turn counted from the end.
///
/// Whole turns are the unit because a partial cut would leave a tool
/// result whose `tool_use` is gone, which providers reject. `None` when
/// the slice already holds `keep` turns or fewer, or when it carries no
/// per-message offsets to anchor with.
pub(crate) fn window_start(
    messages: &[serde_json::Value],
    offsets: &[u64],
    keep: usize,
) -> Option<(usize, u64)> {
    let keep = keep.max(1);
    let openers: Vec<usize> = messages
        .iter()
        .enumerate()
        .filter(|(_, msg)| opens_turn(msg))
        .map(|(index, _)| index)
        .collect();
    let first = openers.len().checked_sub(keep)?;
    if first == 0 {
        return None;
    }
    let index = openers[first];
    let offset = *offsets.get(index)?;
    Some((index, offset))
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
/// the output, reasoning and cost figures — and nothing else. The session-wide counters
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
        && state.turn_cost == 0.0
    {
        return;
    }
    state.turn_call_count = 0;
    state.turn_total_call_duration_ms = 0;
    state.turn_cache_hit_tokens = 0;
    state.turn_input_tokens = 0;
    state.turn_output_tokens = 0;
    state.turn_reasoning_tokens = 0;
    state.turn_cost = 0.0;
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
/// **added** to `session_cost` and `turn_cost` (unlike every other field,
/// which is assigned). Pass `0.0` when the model has no configured
/// pricing.
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
    persist_tokens_and_composition(
        topic_name,
        topic_path,
        context_input_tokens,
        context_output_tokens,
        total_input_tokens,
        total_output_tokens,
        total_cache_hit_tokens,
        total_cache_creation_tokens,
        total_reasoning_tokens,
        context_window,
        call_cost,
        call_ms,
        call_cache_hit_tokens,
        call_input_tokens,
        call_output_tokens,
        call_reasoning_tokens,
        None,
    )
    .await;
}

/// [`persist_tokens`] plus the composition of the call's wire payload, in
/// one load and one save.
///
/// The agent loop has both figures immediately after a call returns;
/// writing them separately rewrote `agent-session.json` twice back to
/// back. The composition is still only written when a call completes —
/// a failed call never overwrites the last successful snapshot.
///
/// The arguments after `topic_path` carry the same meaning as in
/// [`persist_tokens`]; `composition` is the snapshot the info pane and
/// `/info` show.
#[allow(clippy::too_many_arguments)]
pub async fn persist_tokens_with_composition(
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
    composition: &[ContextRegionInfo],
) {
    persist_tokens_and_composition(
        topic_name,
        topic_path,
        context_input_tokens,
        context_output_tokens,
        total_input_tokens,
        total_output_tokens,
        total_cache_hit_tokens,
        total_cache_creation_tokens,
        total_reasoning_tokens,
        context_window,
        call_cost,
        call_ms,
        call_cache_hit_tokens,
        call_input_tokens,
        call_output_tokens,
        call_reasoning_tokens,
        Some(composition),
    )
    .await;
}

#[allow(clippy::too_many_arguments)]
async fn persist_tokens_and_composition(
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
    composition: Option<&[ContextRegionInfo]>,
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
    // total rather than replacing it, and to the current turn's share.
    state.session_cost += call_cost;
    state.turn_cost += call_cost;
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

    // A `None` write (the post-loop save) leaves the last completed
    // call's snapshot in place: it loads the state above, so the field
    // survives without the caller having to re-send it.
    if let Some(composition) = composition {
        state.context_composition = composition.to_vec();
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
/// displayed total is truthful — in `turn_cost` as well, because such a
/// call happens inside a turn and is that turn's spend.
///
/// No-op when `cost` is zero (no pricing configured, or the call failed).
pub async fn add_session_cost(topic_name: &str, topic_path: &Path, cost: f64) {
    if cost <= 0.0 {
        return;
    }
    let session_path = resolve_state_dir(topic_name, topic_path).join(SESSION_FILE);
    let mut state = load_session_state(&session_path).await;
    state.session_cost += cost;
    state.turn_cost += cost;
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
/// share ONE parsing implementation (previously two copies parsed the
/// OpenAI/Anthropic wire formats independently and had already drifted
/// apart). Each pair renders as `USER: ...` / `ASSISTANT: ...`; the turn's
/// tool calls are appended on their own line.
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
        if let Some(calls) = render_turn_tool_calls(&pair) {
            out.push('\n');
            out.push_str(&calls);
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

/// The assistant's visible text for the text-only transcript view.
///
/// Raw OpenAI-compatible assistant messages may carry reasoning twice:
/// inline as one leading reasoning block and as `reasoning_content`. The
/// transcript is semantic history, not provider replay input, so it keeps
/// only the response text and removes that duplicate.
///
/// The inline block is removed **only when it repeats `reasoning_content`
/// exactly**. Without that field, a leading block-shaped string is treated
/// as ordinary visible text: after a provider conversion the separate field
/// can be gone, and guessing would silently delete a legitimate reply (and
/// with it the whole turn). A reasoning marker anywhere after the leading
/// block is likewise left untouched.
pub(crate) fn extract_assistant_transcript_text(msg: &serde_json::Value) -> String {
    let text = extract_message_text(msg);
    let Some(reasoning) = msg.get("reasoning_content").and_then(|v| v.as_str()) else {
        return text;
    };
    strip_duplicated_inline_reasoning(&text, reasoning)
}

/// Drop one leading inline block that duplicates `reasoning`, plus the
/// single blank-line separator the raw builder writes after it. Returns
/// `text` unchanged unless every piece matches.
fn strip_duplicated_inline_reasoning(text: &str, reasoning: &str) -> String {
    if reasoning.is_empty() {
        return text.to_string();
    }
    let open = concat!("<", "think>");
    let close = concat!("</", "think>");
    let Some(rest) = text.strip_prefix(open) else {
        return text.to_string();
    };
    let Some(rest) = rest.strip_prefix(reasoning) else {
        return text.to_string();
    };
    let Some(visible) = rest.strip_prefix(close) else {
        return text.to_string();
    };
    visible.strip_prefix("\n\n").unwrap_or(visible).to_string()
}

/// Max length of a single rendered tool-call argument value kept in a
/// turn's rendered calls; longer values are truncated with an explicit
/// `… [truncated N bytes]` marker.
/// ponytail: fixed cap; raise if real arguments routinely exceed it and
/// the tail matters.
const TOOL_ARG_MAX: usize = 200;

/// Max tool calls rendered for one turn in the transcript view
/// ([`crate::tools::builtin::context_browse`]). A long turn can issue
/// dozens of calls; the rest are counted in [`TurnPair::omitted_tool_calls`]
/// and render as an explicit `…(K more calls)` marker, so one page of the
/// transcript cannot balloon into megabytes.
const TOOL_CALLS_PER_TURN_MAX: usize = 20;

/// Leading text of the history-note message that builds of the past wrote
/// into `raw_context`.
///
/// The note generator is gone (it produced a format the model mimicked),
/// but the messages it produced are *input data* that may still sit in a
/// journal: `summarize_context_heuristic` asked for notes on every pair and
/// wrote the result back with `replace_raw_context` until 2026-10-20, and
/// `/reset` after a failed summary ran exactly that path. So
/// [`opens_turn`] still refuses to treat one as a turn opener.
const HISTORY_NOTE_PREFIX: &str = "[History note]";

/// Collect bare tool calls from a raw wire-format assistant message as
/// `(name, args)` pairs.
///
/// Handles both OpenAI (`tool_calls: [{id, function: {name, arguments}}]`,
/// arguments a JSON string) and Anthropic (`content: [{type: "tool_use",
/// id, name, input}]`) shapes. `args` is the parsed JSON arguments (the
/// raw string kept as a JSON string when it does not parse, e.g. broken or
/// empty OpenAI `arguments`). The wire `id` is not kept: nothing links a
/// call to its result any more (results do not reach the history).
fn collect_tool_calls(msg: &serde_json::Value) -> Vec<(String, serde_json::Value)> {
    let mut calls = Vec::new();

    // OpenAI-compat: `tool_calls` array with string `function.arguments`.
    if let Some(tool_calls) = msg.get("tool_calls").and_then(|t| t.as_array()) {
        for tc in tool_calls {
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
            calls.push((name, args));
        }
    }

    // Anthropic: `content` array with `tool_use` blocks.
    if let Some(blocks) = msg.get("content").and_then(|c| c.as_array()) {
        for block in blocks {
            if block.get("type").and_then(|t| t.as_str()) == Some("tool_use") {
                let name = block
                    .get("name")
                    .and_then(|n| n.as_str())
                    .unwrap_or("?")
                    .to_string();
                let args = block
                    .get("input")
                    .cloned()
                    .unwrap_or(serde_json::Value::Null);
                calls.push((name, args));
            }
        }
    }

    calls
}

/// Render one tool call as `name(k=v, …)`, keeping **all** parameters and
/// truncating only a single argument value that exceeds
/// `TOOL_ARG_MAX`. (`jyc_reply_message` is filtered out at the
/// caller before reaching here.)
fn render_tool_call(name: &str, args: &serde_json::Value) -> String {
    if let Some(map) = args.as_object() {
        // Empty maps join to `name()` naturally; no empty-args special case.
        let parts = map
            .iter()
            .map(|(k, v)| format!("{k}={}", truncate_json_value(v, TOOL_ARG_MAX)))
            .collect::<Vec<_>>();
        format!("{name}({})", parts.join(", "))
    } else {
        format!("{name}({})", truncate_json_value(args, TOOL_ARG_MAX))
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

/// The smallest cap for which [`truncate_text_keeping_edges`] really keeps
/// both ends. The definition lives in `jyc_types` because the `/context`
/// command enforces the same floor; this alias keeps the call sites in this
/// crate reading naturally.
#[cfg(test)]
pub(crate) use jyc_types::channel::MIN_EDGE_KEEPING_CAP;

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
/// small to hold the marker plus both ends — see
/// [`jyc_types::channel::MIN_EDGE_KEEPING_CAP`] for the smallest cap that
/// really keeps both.
pub(crate) fn truncate_text_keeping_edges(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    // Reserve room for the marker so the result really fits in `max`.
    // The reserve lives in `jyc_types` beside the floor that is derived
    // from it, so the two halves of this arithmetic cannot drift apart.
    let budget = max.saturating_sub(jyc_types::channel::MARKER_RESERVE);
    if budget == 0 {
        return truncate_text(s, max);
    }
    let head_end = s.floor_char_boundary(budget * 2 / 3);
    let tail_start = s.ceil_char_boundary(s.len() - (budget - head_end));
    // `tail_start >= s.len()` means the rounding ate the whole tail (the
    // last character is multi-byte and the budget only bought one byte).
    // Falling back beats emitting a marker that claims a kept end it did
    // not keep.
    if head_end == 0 || head_end >= tail_start || tail_start >= s.len() {
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

/// Wire-only marker inserted at the start of the history block.
/// It is **not** persisted to `raw_context`, so `extract_pairs` must never
/// recognize it as a real user message.
pub(crate) const HISTORY_MARKER_TEXT: &str =
    "[History messages] earlier conversation follows — text-only, tool calls/results omitted.";

/// One user→assistant turn extracted from raw context.
///
/// The assistant entry holds ONLY the assistant's own text; the turn's
/// tool calls are kept separately in [`Self::tool_calls`] — never merged
/// into that text, so the transcript view cannot teach the model to write
/// its tool use as content.
pub(crate) struct TurnPair {
    /// The turn-opening user message (original wire format).
    pub user: serde_json::Value,
    /// Pure assistant text (multi-step turns merged); `None` for
    /// text-less (tool-call-only) turns.
    pub assistant: Option<serde_json::Value>,
    /// The turn's tool calls, one `name(args)` string per call in
    /// chronological order, capped at [`TOOL_CALLS_PER_TURN_MAX`]. Empty
    /// when the turn made none (or only the legacy `jyc_reply_message`
    /// calls). Never holds a marker: the overflow count lives in
    /// [`Self::omitted_tool_calls`] and renderers add it back, so every
    /// entry here really is a call.
    pub tool_calls: Vec<String>,
    /// Calls beyond [`TOOL_CALLS_PER_TURN_MAX`], absent from
    /// [`Self::tool_calls`]. Renderers append `…(N more calls)`.
    pub omitted_tool_calls: usize,
}

/// The turn's rendered tool-call list for the transcript view: calls joined
/// with `, ` plus the overflow marker when [`flush_turn`] dropped any.
/// `None` when the turn made no calls.
#[cfg(test)]
pub(crate) fn render_turn_tool_calls(pair: &TurnPair) -> Option<String> {
    render_turn_tool_calls_bounded(pair, usize::MAX)
}

/// [`render_turn_tool_calls`] with a byte budget.
///
/// When the full list does not fit, whole calls are dropped from the end —
/// never cut mid-call, which would leave half an argument looking like a
/// real one — and the overflow marker absorbs the count, so "how many are
/// hidden" survives any bound that can hold the marker itself (only a
/// `limit` smaller than the marker's own length cuts into it). That is the
/// opposite of trimming the finished string: a blind byte cut at the end
/// removes exactly the marker and keeps the unreadable fragment.
pub(crate) fn render_turn_tool_calls_bounded(pair: &TurnPair, limit: usize) -> Option<String> {
    if pair.tool_calls.is_empty() {
        return None;
    }
    let marker = |dropped: usize| -> String {
        match dropped {
            0 => String::new(),
            1 => " …(1 more call)".to_string(),
            n => format!(" …({n} more calls)"),
        }
    };
    let total = pair.tool_calls.len();
    let mut keep = total;
    loop {
        let body = pair.tool_calls[..keep].join(", ");
        let suffix = marker(pair.omitted_tool_calls + (total - keep));
        if keep == 0 {
            // Not even one call fits. Return the bare marker — trimmed of
            // its leading space, which only separates it from a non-empty
            // list and would otherwise render as an empty `TOOLS:` value.
            // Hard-cut at a char boundary *without* a truncation marker:
            // `truncate_text` appends one of its own, which would push the
            // result back over the caller's bound.
            let marker_only = suffix.trim_start();
            let end = limit.min(marker_only.len());
            return Some(marker_only[..marker_only.floor_char_boundary(end)].to_string());
        }
        if body.len() + suffix.len() <= limit {
            return Some(format!("{body}{suffix}"));
        }
        keep -= 1;
    }
}

/// Flush the accumulated turn into `pairs`: merge every assistant message
/// of the turn (tool-call steps included) into a single assistant entry
/// holding ONLY the steps' text, and collect the turn's tool calls into
/// [`TurnPair::tool_calls`]. Always clears the turn state.
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
) {
    let user_msg = cur_user.take();
    let assistants = std::mem::take(cur_assistants);
    if assistants.is_empty() {
        return;
    }
    let Some(user_msg) = user_msg else { return };

    let mut texts: Vec<String> = Vec::with_capacity(assistants.len());
    let mut tool_calls: Vec<String> = Vec::new();
    let mut has_payload = false;
    for msg in &assistants {
        // One extraction and one call parse per message: the keep-rule and
        // the transcript view both need them. Keep only role + content for
        // the text; the calls are rendered separately so a tool-call-only
        // step (empty text) still shows up as a call instead of being
        // dropped.
        let text = extract_assistant_transcript_text(msg);
        let calls = collect_tool_calls(msg);
        has_payload |= keeps_turn_alive(&text, !calls.is_empty());
        for (name, args) in &calls {
            // Legacy `jyc_reply_message` calls (from transcripts persisted
            // before replies became plain final text) are not tool use:
            // their `message` is the text the user already saw.
            if name != "jyc_reply_message" {
                tool_calls.push(render_tool_call(name, args));
            }
        }
        if !text.is_empty() {
            texts.push(text);
        }
    }
    let omitted_tool_calls = tool_calls.len().saturating_sub(TOOL_CALLS_PER_TURN_MAX);
    tool_calls.truncate(TOOL_CALLS_PER_TURN_MAX);
    // Drop only when the assistant truly had nothing — no text AND no tool
    // calls (`keeps_turn_alive`, the rule the tail scan shares). The call
    // list can be empty while the turn still counts: a step whose every
    // call was a filtered legacy `jyc_reply_message` renders no call, yet
    // it was a tool step and the trigger user message still matters.
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
    pairs.push(TurnPair {
        user: user_msg,
        assistant,
        tool_calls,
        omitted_tool_calls,
    });
}

/// Whether a message opens a turn: it is a user message with non-empty
/// text that is not a legacy `[History note]` line.
///
/// The one definition shared by pair extraction, the tail scan and the
/// reader's slice start — a turn boundary that means different things in
/// different places loses or duplicates turns.
///
/// The prefix test is an *input-data* guard, not the note machinery: a
/// journal written by a compressing `/reset` before 2026-10-20 can hold a
/// persisted note (see [`HISTORY_NOTE_PREFIX`]). Letting one open a turn
/// would replay machine text the user never wrote back into the history.
pub(crate) fn opens_turn(msg: &serde_json::Value) -> bool {
    if msg.get("role").and_then(|r| r.as_str()) != Some("user") {
        return false;
    }
    let text = extract_message_text(msg);
    !text.is_empty() && !text.starts_with(HISTORY_NOTE_PREFIX)
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
    let text = extract_assistant_transcript_text(msg);
    keeps_turn_alive(&text, false) || !collect_tool_calls(msg).is_empty()
}

/// Extract **all** user+assistant turn pairs from raw context.
///
/// Pairing unit is the **turn**, not the single message: a turn opens at
/// a text-bearing user message and closes at the next one (or end of
/// context). Every assistant message in between — intermediate tool-call
/// steps and the final reply alike — is merged into one assistant entry
/// (see [`flush_turn`]), so the transcript never loses the turn's
/// conclusion. Legacy `jyc_reply_message` calls (pre-final-text-reply
/// transcripts) still count as the step they were, but render no tool call.
///
/// Assistant messages are cleaned to only role + content (strip
/// reasoning_content, tool_calls); the calls themselves are rendered into
/// [`TurnPair::tool_calls`], never into the assistant's own text. Returns
/// pairs in oldest→newest order.
///
/// A trailing turn whose user message has no assistant reply yet is
/// dropped — a completed turn only (`extract_user_assistant_pairs` has a
/// fallback for the no-pairs case).
///
/// Shared by the sliding-window payload and the `context_browse` built-in
/// tool.
pub(crate) fn extract_pairs(raw_context: &[serde_json::Value]) -> Vec<TurnPair> {
    let mut pairs: Vec<TurnPair> = Vec::new();
    let mut cur_user: Option<serde_json::Value> = None;
    let mut cur_assistants: Vec<serde_json::Value> = Vec::new();

    for msg in raw_context {
        let role = msg.get("role").and_then(|r| r.as_str()).unwrap_or("");
        match role {
            "user" => {
                // A text-bearing user message opens a new turn; user-role
                // messages with no extractable text are Anthropic
                // tool_result wrappers, which stay in the current turn.
                if opens_turn(msg) {
                    flush_turn(&mut pairs, &mut cur_user, &mut cur_assistants);
                    cur_user = Some(msg.clone());
                }
            }
            "assistant" => cur_assistants.push(msg.clone()),
            _ => {}
        }
    }
    flush_turn(&mut pairs, &mut cur_user, &mut cur_assistants);

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
/// [`extract_pairs`] and [`turn_openers_start`] use. A turn opener
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
/// Flattens to user → assistant per pair: the wire history is text-only, so
/// the turn's tool calls and the tool results they produced are not part of
/// the result (the journal keeps them; `context_browse` still reads them).
///
/// Shared by the sliding-window wire payload and any other caller that
/// needs the same text-only view.
pub(crate) fn extract_user_assistant_pairs(
    raw_context: &[serde_json::Value],
    keep_pairs: usize,
) -> Vec<serde_json::Value> {
    // Start the extraction where the kept turns begin: `extract_pairs`
    // deep-clones every message it visits, and on a long history the
    // discarded prefix is the bulk of that work.
    let tail_start = pairs_tail_start(raw_context, keep_pairs).0;
    let pairs: Vec<_> = extract_pairs(&raw_context[tail_start..])
        .into_iter()
        .rev()
        .take(keep_pairs)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    let summary: Vec<serde_json::Value> = pairs
        .into_iter()
        .flat_map(|pair| {
            let mut v = vec![pair.user];
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
        // of the current turn.)
        if let Some(last_user) = raw_context.iter().rev().find(|m| opens_turn(m)) {
            vec![last_user.clone()]
        } else {
            Vec::new()
        }
    } else {
        summary
    }
}

/// Index in `raw_context` where the last `turns` turn openers begin (0 when
/// `raw_context` holds fewer openers than `turns`, and `raw_context.len()`
/// when `turns` is 0).
///
/// The reader uses it to start its slice at a turn boundary: an opener
/// without a reply still costs a turn of depth. Turn boundaries use the
/// same semantics as [`extract_pairs`] — a turn opens at a text-bearing
/// user message.
pub(crate) fn turn_openers_start(raw_context: &[serde_json::Value], turns: usize) -> usize {
    if turns == 0 {
        return raw_context.len();
    }
    let mut seen = 0usize;
    for (idx, msg) in raw_context.iter().enumerate().rev() {
        if opens_turn(msg) {
            seen += 1;
            if seen == turns {
                return idx;
            }
        }
    }
    0
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
        // 128 leaves no budget after the marker reserve and 129 leaves no
        // head, so both degrade — that is why the command's floor is 130.
        for max in [1usize, 8, 40, 120, 128, 129] {
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
        // 130 is the smallest cap that really keeps both ends: the whole
        // 2-byte content budget goes to a 1-byte head and a 1-byte tail.
        let out = truncate_text_keeping_edges(&s, 130);
        assert!(out.starts_with('z'), "130 must keep the head: {out}");
        assert!(out.ends_with('L'), "130 must keep the tail: {out}");
        assert!(
            out.contains("bytes dropped from the middle"),
            "130 must still fit the marker: {out}"
        );
        assert!(out.len() <= 130, "130 must bound the result: {}", out.len());
    }

    /// A turn with more calls than [`TOOL_CALLS_PER_TURN_MAX`] keeps the
    /// first N verbatim and reports the rest as a count — the count bound on
    /// the transcript view (the byte bound lives in
    /// `render_turn_tool_calls_bounded`).
    #[test]
    fn caps_rendered_tool_calls_per_turn() {
        let calls: Vec<serde_json::Value> = (0..25)
            .map(|i| {
                json!({
                    "id": format!("t{i}"),
                    "function": {"name": "bash", "arguments": format!(r#"{{"command":"c{i}"}}"#)}
                })
            })
            .collect();
        let ctx = vec![
            json!({"role": "user", "content": "u1"}),
            json!({"role": "assistant", "content": "running", "tool_calls": calls}),
        ];
        let pairs = super::extract_pairs(&ctx);
        assert_eq!(pairs.len(), 1);
        assert_eq!(pairs[0].tool_calls.len(), super::TOOL_CALLS_PER_TURN_MAX);
        assert_eq!(pairs[0].omitted_tool_calls, 5);
        // The marker is NOT a call: every entry is a real `name(args)`.
        assert!(
            pairs[0]
                .tool_calls
                .iter()
                .all(|c| c.starts_with("bash(") && !c.contains("more"))
        );
        let rendered = super::render_turn_tool_calls(&pairs[0]).unwrap();
        assert!(
            rendered.ends_with("…(5 more calls)"),
            "rendered: {rendered}"
        );
        assert!(rendered.starts_with(r#"bash(command="c0")"#));
    }

    /// One dropped call reads "call", not "calls".
    #[test]
    fn singular_overflow_marker() {
        let calls: Vec<serde_json::Value> = (0..21)
            .map(
                |i| json!({"id": format!("t{i}"), "function": {"name": "bash", "arguments": "{}"}}),
            )
            .collect();
        let ctx = vec![
            json!({"role": "user", "content": "u1"}),
            json!({"role": "assistant", "content": "running", "tool_calls": calls}),
        ];
        let pairs = super::extract_pairs(&ctx);
        assert_eq!(pairs[0].omitted_tool_calls, 1);
        let rendered = super::render_turn_tool_calls(&pairs[0]).unwrap();
        assert!(rendered.ends_with("…(1 more call)"), "rendered: {rendered}");
    }

    /// The byte budget on the rendered call list drops **whole calls** and
    /// keeps the overflow marker last.
    ///
    /// A blind `&s[..limit]` cut does the opposite: it removes the marker
    /// (so "how many are hidden" is lost) and leaves a half-rendered
    /// argument that reads like a real one.
    #[test]
    fn bounded_rendering_drops_whole_calls_and_keeps_the_marker() {
        let calls: Vec<serde_json::Value> = (0..20)
            .map(|i| {
                json!({
                    "id": format!("t{i}"),
                    "function": {"name": "bash", "arguments": format!("{{\"command\":\"c{i}\"}}")}
                })
            })
            .collect();
        let ctx = vec![
            json!({"role": "user", "content": "u1"}),
            json!({"role": "assistant", "content": "running", "tool_calls": calls}),
        ];
        let pairs = super::extract_pairs(&ctx);
        let full = super::render_turn_tool_calls(&pairs[0]).unwrap();
        assert!(full.len() > 200, "fixture must overflow: {}", full.len());

        let bounded = super::render_turn_tool_calls_bounded(&pairs[0], 200).unwrap();
        assert!(bounded.len() <= 200, "budget exceeded: {}", bounded.len());
        assert!(
            bounded.ends_with("more calls)"),
            "the overflow marker must survive the bound: {bounded}"
        );
        // No half a call: every kept entry ends at a `)` boundary, and the
        // count in the marker plus the kept calls accounts for all 20.
        let body = bounded.split(" …(").next().unwrap();
        let kept = body.split(", ").count();
        let hidden: usize = bounded
            .rsplit("…(")
            .next()
            .unwrap()
            .split_whitespace()
            .next()
            .unwrap()
            .parse()
            .unwrap();
        assert_eq!(
            kept + hidden,
            20,
            "kept {kept} + hidden {hidden}: {bounded}"
        );
        assert!(
            body.split(", ")
                .all(|c| c.ends_with(')') && c.starts_with("bash(")),
            "a kept call must be whole: {body}"
        );
    }

    /// When not even one call plus the marker fits the budget, the marker
    /// alone is returned — hard-cut, with no second truncation marker
    /// pushing it back over the bound and no leading space left where the
    /// (empty) body would have been.
    #[test]
    fn bounded_rendering_degrades_to_the_marker_alone() {
        let long = "x".repeat(300);
        let ctx = vec![
            json!({"role": "user", "content": "u1"}),
            json!({"role": "assistant", "content": "running", "tool_calls": [
                {"id": "t0", "function": {
                    "name": "bash",
                    "arguments": format!("{{\"command\":\"{long}\"}}"),
                }}
            ]}),
        ];
        let pairs = super::extract_pairs(&ctx);
        let full = super::render_turn_tool_calls(&pairs[0]).unwrap();
        assert!(full.len() > 200, "fixture must overflow: {}", full.len());

        let bounded = super::render_turn_tool_calls_bounded(&pairs[0], 200).unwrap();
        assert_eq!(bounded, "…(1 more call)", "marker alone, no empty body");
        // A budget smaller than the marker itself still holds.
        let tight = super::render_turn_tool_calls_bounded(&pairs[0], 8).unwrap();
        assert!(tight.len() <= 8, "budget exceeded: {tight}");
    }

    /// The 130-byte floor keeps both ends for ASCII-bounded content only:
    /// when the last character is multi-byte the one-byte tail budget rounds
    /// past it, and the trim falls back instead of emitting a marker that
    /// claims a tail it did not keep.
    #[test]
    fn edge_keeping_falls_back_when_the_tail_is_multibyte() {
        let s = format!("{}{}", "z".repeat(400), "尾");
        let out = truncate_text_keeping_edges(&s, super::MIN_EDGE_KEEPING_CAP);
        assert_eq!(
            out,
            truncate_text(&s, super::MIN_EDGE_KEEPING_CAP),
            "a one-byte tail budget cannot hold 尾"
        );
        assert!(!out.contains("dropped from the middle"));
    }

    /// A `[History note]` message persisted by a build that still wrote
    /// notes must not open a turn — it is machine text the user never wrote,
    /// and replaying it teaches the model to imitate it.
    ///
    /// Such a message reaches the journal through
    /// `summarize_context_heuristic`, which called
    /// `extract_user_assistant_pairs(.., None)` (notes on every pair) and
    /// wrote the result back with `replace_raw_context` — so a `/reset`
    /// under heuristic compression before 2026-10-20 can leave one behind.
    #[test]
    fn legacy_history_note_does_not_open_a_turn() {
        let note = json!({
            "role": "user",
            "content": "[History note] assistant tool calls: bash(command=\"ls\") → ok",
        });
        assert!(!super::opens_turn(&note));

        // A note ahead of a complete turn: the turn pairs as usual, and the
        // note neither becomes a pair of its own nor rides along as one.
        let ctx = vec![
            note.clone(),
            json!({"role": "user", "content": "u1"}),
            json!({"role": "assistant", "content": "a1"}),
        ];
        let pairs = super::extract_pairs(&ctx);
        assert_eq!(pairs.len(), 1);
        assert_eq!(pairs[0].user["content"], "u1");
        assert_eq!(pairs[0].assistant.as_ref().unwrap()["content"], "a1");
        assert_eq!(super::count_turn_openers(&ctx), 1);

        // A note between two turns: it must not split the second turn off
        // from its own user message.
        let ctx = vec![
            json!({"role": "user", "content": "u1"}),
            json!({"role": "assistant", "content": "a1"}),
            note,
            json!({"role": "user", "content": "u2"}),
            json!({"role": "assistant", "content": "a2"}),
        ];
        let pairs = super::extract_pairs(&ctx);
        assert_eq!(pairs.len(), 2);
        assert_eq!(pairs[1].user["content"], "u2");
        assert_eq!(pairs[1].assistant.as_ref().unwrap()["content"], "a2");
        assert_eq!(super::count_turn_openers(&ctx), 2);
    }

    /// Anthropic-shaped context (array content): assistant text pairs
    /// correctly; tool_result user-role wrappers are skipped; the whole
    /// turn's assistant steps merge into one pure-text entry, and the tool
    /// call renders on the pair's own call list — never inside the
    /// assistant's text.
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
        let pairs = super::extract_pairs(&ctx);
        assert_eq!(pairs.len(), 1);
        assert_eq!(pairs[0].user, ctx[0]);
        assert_eq!(pairs[0].assistant.as_ref().unwrap()["content"], "a1\ndone");
        assert_eq!(pairs[0].tool_calls, vec![r#"bash(command="ls")"#]);
        // The wire view is text-only: no call, no result.
        assert_eq!(
            super::extract_user_assistant_pairs(&ctx, 10),
            vec![
                ctx[0].clone(),
                json!({"role": "assistant", "content": "a1\ndone"})
            ]
        );
    }

    /// OpenAI-shaped context: tool calls render with all parameters,
    /// multiple calls comma-separated, on the pair's call list — NOT in the
    /// assistant text (the model must never see the annotation in its own
    /// voice).
    #[test]
    fn extract_pairs_annotates_tool_call_args() {
        let ctx = vec![
            json!({"role": "user", "content": "u1"}),
            json!({"role": "assistant", "content": "running", "tool_calls": [
                {"id": "1", "type": "function", "function": {"name": "bash", "arguments": r#"{"command": "ls -la", "timeout": 30}"#}},
                {"id": "2", "type": "function", "function": {"name": "read", "arguments": r#"{"path": "a.txt"}"#}},
            ]}),
        ];
        let pairs = super::extract_pairs(&ctx);
        assert_eq!(
            pairs[0].tool_calls,
            vec![
                r#"bash(command="ls -la", timeout=30)"#,
                r#"read(path="a.txt")"#
            ]
        );
        assert_eq!(pairs[0].assistant.as_ref().unwrap()["content"], "running");
        assert_eq!(
            super::extract_user_assistant_pairs(&ctx, 10),
            vec![
                json!({"role": "user", "content": "u1"}),
                json!({"role": "assistant", "content": "running"}),
            ]
        );
    }

    /// Tool-call-only assistant turns (no text) are kept as a turn — the
    /// pair survives with no assistant entry — and emit no empty assistant
    /// message.
    #[test]
    fn extract_pairs_keeps_tool_call_only_assistant() {
        let ctx = vec![
            json!({"role": "user", "content": "u1"}),
            json!({"role": "assistant", "content": "", "tool_calls": [
                {"id": "1", "type": "function", "function": {"name": "bash", "arguments": r#"{"command": "ls -la"}"#}}
            ]}),
        ];
        let pairs = super::extract_pairs(&ctx);
        assert_eq!(pairs.len(), 1);
        assert!(pairs[0].assistant.is_none());
        assert_eq!(pairs[0].tool_calls, vec![r#"bash(command="ls -la")"#]);
        assert_eq!(
            super::extract_user_assistant_pairs(&ctx, 10),
            vec![json!({"role": "user", "content": "u1"})]
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
        let pairs = super::extract_pairs(&ctx);
        let call = &pairs[0].tool_calls[0];
        assert!(call.starts_with("bash(command="), "no call: {call:?}");
        assert!(call.contains('…'), "long arg not truncated: {call:?}");
        // 200-char cap + ellipsis + `command=` prefix + quotes.
        assert!(call.len() < 300, "call too big: {call:?}");
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
        assert!(pairs[0].tool_calls.is_empty());
        assert_eq!(pairs[1].user["content"], "u2");
        assert_eq!(pairs[1].assistant.as_ref().unwrap()["content"], "a2");
        assert_eq!(pairs[1].tool_calls, vec!["bash()"]);
        assert_eq!(pairs[2].user["content"], "u3");
        assert_eq!(pairs[2].assistant.as_ref().unwrap()["content"], "a3");
        // u4 must NOT be paired.
        assert!(pairs.iter().all(|p| p.user["content"] != "u4"));
    }

    /// The transcript keeps the visible reply, not the reasoning copy that
    /// raw OpenAI-compatible messages carry inline on top of
    /// `reasoning_content`.
    #[test]
    fn extract_pairs_strips_inline_reasoning_from_assistant_text() {
        let reasoning = "secret chain of thought";
        let content = format!(
            "{}{reasoning}{}\n\nvisible answer",
            concat!("<", "think>"),
            concat!("</", "think>")
        );
        let ctx = vec![
            json!({"role": "user", "content": "u1"}),
            json!({
                "role": "assistant",
                "content": content,
                "reasoning_content": reasoning,
            }),
            json!({"role": "assistant", "content": "second visible step"}),
        ];
        let pairs = super::extract_pairs(&ctx);
        assert_eq!(pairs.len(), 1);
        let text = pairs[0].assistant.as_ref().unwrap()["content"]
            .as_str()
            .unwrap();
        assert_eq!(text, "visible answer\nsecond visible step");
        assert!(!text.contains(reasoning), "reasoning leaked: {text:?}");
    }

    /// Only one leading block is stripped, and only when it duplicates the
    /// separate reasoning field; a literal marker later in the reply stays.
    #[test]
    fn extract_pairs_strips_only_one_leading_reasoning_block() {
        let reasoning = "hidden";
        let ctx = vec![
            json!({"role": "user", "content": "u1"}),
            json!({
                "role": "assistant",
                "content": format!(
                    "{}{reasoning}{}\n\nvisible {}{}",
                    concat!("<", "think>"),
                    concat!("</", "think>"),
                    concat!("<", "think>"),
                    concat!("</", "think>")
                ),
                "reasoning_content": reasoning,
            }),
        ];
        let pairs = super::extract_pairs(&ctx);
        let text = pairs[0].assistant.as_ref().unwrap()["content"]
            .as_str()
            .unwrap();
        assert_eq!(
            text,
            format!(
                "visible {}{}",
                concat!("<", "think>"),
                concat!("</", "think>")
            )
        );
    }

    /// A leading block-shaped string without `reasoning_content` is visible
    /// text, not hidden reasoning: converted histories may have lost the
    /// separate field, and deleting the reply would drop the whole turn.
    #[test]
    fn extract_pairs_keeps_leading_literal_block_without_reasoning_field() {
        let content = format!(
            "{}literal{}",
            concat!("<", "think>"),
            concat!("</", "think>")
        );
        let ctx = vec![
            json!({"role": "user", "content": "u1"}),
            json!({"role": "assistant", "content": content}),
        ];
        let pairs = super::extract_pairs(&ctx);
        assert_eq!(
            pairs[0].assistant.as_ref().unwrap()["content"],
            content,
            "a block without reasoning_content must survive"
        );
        assert_eq!(super::pairs_tail_start(&ctx, 1), (0, 1));
    }

    /// A leading inline block that does not repeat `reasoning_content` is
    /// also visible text; only the exact duplicate is removed.
    #[test]
    fn extract_pairs_keeps_mismatched_leading_block() {
        let content = format!(
            "{}visible{}",
            concat!("<", "think>"),
            concat!("</", "think>")
        );
        let ctx = vec![
            json!({"role": "user", "content": "u1"}),
            json!({
                "role": "assistant",
                "content": content,
                "reasoning_content": "some other reasoning",
            }),
        ];
        let pairs = super::extract_pairs(&ctx);
        assert_eq!(
            pairs[0].assistant.as_ref().unwrap()["content"],
            content,
            "mismatched field must not cause deletion"
        );
    }

    /// Reasoning-only and tool-call-only assistants apply the same keep rule
    /// in pair extraction and the tail scan, so a trimmed turn boundary
    /// cannot differ between the two readers.
    #[test]
    fn reasoning_only_assistant_does_not_keep_turn_alive() {
        let reasoning_only = json!({
            "role": "assistant",
            "content": format!("{}hidden{}", concat!("<", "think>"), concat!("</", "think>")),
            "reasoning_content": "hidden",
        });
        let tool_only = json!({"role": "assistant", "content": "", "tool_calls": [
            {"id": "1", "type": "function", "function": {"name": "bash", "arguments": "{}"}}
        ]});
        let ctx = vec![
            json!({"role": "user", "content": "u1"}),
            reasoning_only,
            tool_only,
        ];
        let pairs = super::extract_pairs(&ctx);
        assert_eq!(pairs.len(), 1, "the tool step must keep the turn");
        assert!(
            pairs[0].assistant.is_none(),
            "reasoning is not visible text"
        );
        assert_eq!(
            super::pairs_tail_start(&ctx, 1),
            (0, 1),
            "the tail scan must agree with pair extraction"
        );
    }

    /// A multi-step tool turn (user → assistant(call) → tool → assistant(call)
    /// → tool → assistant(final reply)) merges into ONE pair: every step's
    /// text is kept in the assistant entry, ending with the turn's
    /// conclusion, and both steps' tool calls land in the pair's call list.
    /// Before
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
        assert_eq!(pairs[0].tool_calls, vec!["bash()", "read()"]);
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
        assert!(pairs[0].tool_calls.is_empty());
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
            super::extract_user_assistant_pairs(&ctx, 1),
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
        // The same extraction the sliding window performs, as the oracle.
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
            // wrapper.
            vec![
                json!({"role": "user", "content": "u1"}),
                json!({"role": "assistant", "content": "a1"}),
                json!({"role": "user", "content": "u2"}),
                json!({"role": "user", "content": "u2b"}),
                json!({"role": "assistant", "content": [{"type": "text", "text": "a2"}]}),
                json!({"role": "user", "content": [{"type": "tool_result", "tool_use_id": "t1", "content": "r1"}]}),
                json!({"role": "assistant", "content": [{"type": "tool_use", "id": "t2", "name": "bash", "input": {}}]}),
                json!({"role": "user", "content": "u2c"}),
                json!({"role": "user", "content": "u3"}),
                json!({"role": "assistant", "content": "a3"}),
            ],
        ];

        for ctx in &shapes {
            for keep in 1..=3 {
                assert_eq!(
                    super::extract_user_assistant_pairs(ctx, keep),
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
        let pairs = super::extract_user_assistant_pairs(&ctx, 1);
        assert_eq!(pairs.len(), 2);
        assert_eq!(pairs[0]["content"], "u2");
        assert_eq!(pairs[1]["content"], "a2");
    }

    /// No complete pairs: the fallback keeps the LAST text-bearing user
    /// message (nearest to an unfinished turn), not the first — and skips
    /// Anthropic tool_result wrappers, which carry no real user text.
    #[test]
    fn fallback_keeps_last_text_user() {
        let ctx = vec![
            json!({"role": "user", "content": "ancient"}),
            json!({"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "t1", "content": "ok"}
            ]}),
            json!({"role": "user", "content": "latest"}),
        ];
        let pairs = super::extract_user_assistant_pairs(&ctx, 10);
        assert_eq!(pairs, vec![json!({"role": "user", "content": "latest"})]);
    }

    /// Legacy `jyc_reply_message` calls are filtered out of the rendered
    /// calls: a turn that called only the reply tool renders none of them
    /// (the message the user saw is already in the assistant's own text).
    #[test]
    fn extract_pairs_no_call_when_only_reply_was_called() {
        let ctx = vec![
            json!({"role": "user", "content": "u1"}),
            json!({"role": "assistant", "content": "", "tool_calls": [
                {"id": "1", "type": "function", "function": {"name": "jyc_reply_message",
                    "arguments": r#"{"message": "hi"}"#}}
            ]}),
        ];
        let pairs = super::extract_pairs(&ctx);
        assert!(
            pairs[0].tool_calls.is_empty(),
            "reply-only turn must render no tool call: {:?}",
            pairs[0].tool_calls
        );
    }

    /// A mixed turn (real work + a legacy reply call) keeps the real work
    /// and drops the reply call — the reply's message is already in the
    /// assistant text, so the rendered calls must not duplicate it.
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
        assert_eq!(pairs[0].tool_calls, vec![r#"bash(command="ls")"#]);
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
            extend_rate: 0.5,
            turn_tool_result_cap: None,
            max_history_tokens: None,
            ..ContextStrategyConfig::default()
        };
        let wire = vec![serde_json::json!({"role":"user","content":"hi"})];
        let regions = vec![2u8];

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
            extend_rate: 0.5,
            turn_tool_result_cap: None,
            max_history_tokens: None,
            ..ContextStrategyConfig::default()
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

#[cfg(test)]
mod windowed_load_tests {
    use super::*;
    use jyc_types::channel::{ContextStrategy, ContextStrategyConfig};
    use jyc_types::state_dir;

    // A sliding-window strategy whose window is `window` turns and whose
    // bound is `max_window = window + round(extend_rate x window)`.
    fn sliding(window: usize, extend_rate: f64) -> ContextStrategyConfig {
        ContextStrategyConfig {
            mode: ContextStrategy::SlidingWindow,
            window,
            extend_rate,
            ..Default::default()
        }
    }

    fn turns_journal(from: usize, to: usize) -> String {
        let mut out = String::new();
        for i in from..=to {
            out.push_str(&format!(
                "{}\n",
                serde_json::json!({"role": "user", "content": format!("u{i}")})
            ));
            out.push_str(&format!(
                "{}\n",
                serde_json::json!({"role": "assistant", "content": format!("a{i}")})
            ));
        }
        out
    }

    // Session file + journal in a temp state dir, registered so
    // `resolve_state_dir` finds them.
    async fn setup(tmp: &tempfile::TempDir, topic: &str, journal: &str) -> std::path::PathBuf {
        let state_dir = tmp.path().join(".jyc");
        tokio::fs::create_dir_all(&state_dir).await.unwrap();
        state_dir::register(topic, &state_dir);
        tokio::fs::write(
            state_dir.join(SESSION_FILE),
            r#"{"created_at":"2026-01-01T00:00:00Z"}"#,
        )
        .await
        .unwrap();
        tokio::fs::write(state_dir.join(AGENT_CONTEXT_FILE), journal)
            .await
            .unwrap();
        state_dir
    }

    async fn append_turn(state_dir: &std::path::Path, i: usize) {
        let body = turns_journal(i, i);
        let mut f = tokio::fs::OpenOptions::new()
            .append(true)
            .open(state_dir.join(AGENT_CONTEXT_FILE))
            .await
            .unwrap();
        use tokio::io::AsyncWriteExt;
        f.write_all(body.as_bytes()).await.unwrap();
    }

    // The persisted anchor, if any.
    async fn stored_offset(state_dir: &std::path::Path) -> Option<u64> {
        read_context_offset(state_dir).await
    }

    async fn offset_is_line_start(state_dir: &std::path::Path, offset: u64) -> bool {
        let journal = tokio::fs::read(state_dir.join(AGENT_CONTEXT_FILE))
            .await
            .unwrap();
        journal.get(offset as usize).is_some_and(|&b| b == b'{')
    }

    #[tokio::test]
    async fn cold_start_slides_down_to_window_turns() {
        // 10 turns in the journal, window 4 -> the window keeps the last 4.
        let tmp = tempfile::tempdir().unwrap();
        let topic = "windowed-cold-start-slides";
        let state_dir = setup(&tmp, topic, &turns_journal(1, 10)).await;

        let (_, raw) = load_context(topic, tmp.path(), &sliding(4, 0.5)).await;

        assert_eq!(raw.len(), 8, "4 turns x (user, assistant)");
        assert_eq!(raw[0]["content"], "u7");
        assert_eq!(raw[7]["content"], "a10");
        let offset = stored_offset(&state_dir).await.expect("anchor persisted");
        assert!(
            offset_is_line_start(&state_dir, offset).await,
            "anchor must sit on a line start"
        );
    }

    #[tokio::test]
    async fn reload_appends_with_a_frozen_prefix() {
        // The #995 property: between slides everything before the newest
        // turn comes back byte-identical -- a pure append.
        let tmp = tempfile::tempdir().unwrap();
        let topic = "windowed-reload-frozen-prefix";
        let state_dir = setup(&tmp, topic, &turns_journal(1, 6)).await;

        let (_, raw) = load_context(topic, tmp.path(), &sliding(10, 0.5)).await;
        let offset_before = stored_offset(&state_dir).await;

        append_turn(&state_dir, 7).await;
        let (_, raw2) = load_context(topic, tmp.path(), &sliding(10, 0.5)).await;

        assert_eq!(stored_offset(&state_dir).await, offset_before, "no slide");
        assert_eq!(raw2.len(), raw.len() + 2, "one turn appended");
        assert_eq!(
            raw2[..raw.len()],
            raw[..],
            "the whole prior is a stable prefix of the next turn's prior"
        );
        assert_eq!(raw2[raw.len()]["content"], "u7");
        assert_eq!(raw2[raw.len() + 1]["content"], "a7");
    }

    #[tokio::test]
    async fn window_grows_to_max_window_then_slides_back_to_window() {
        // window 4, extend_rate 0.5 -> max_window 6: the window takes 5 and
        // 6 turns without sliding, and the 7th slides it back to 4.
        let tmp = tempfile::tempdir().unwrap();
        let topic = "windowed-grow-then-slide";
        let state_dir = setup(&tmp, topic, &turns_journal(1, 6)).await;

        let (_, six) = load_context(topic, tmp.path(), &sliding(4, 0.5)).await;
        assert_eq!(six.len(), 12, "6 turns = max_window, no slide yet");
        assert_eq!(six[0]["content"], "u1");
        assert!(
            stored_offset(&state_dir).await.is_none(),
            "a window that still covers the journal head needs no anchor"
        );

        append_turn(&state_dir, 7).await;
        let (_, seven) = load_context(topic, tmp.path(), &sliding(4, 0.5)).await;
        assert_eq!(seven.len(), 8, "7 > max_window -> slide back to 4 turns");
        assert_eq!(seven[0]["content"], "u4");
        assert_eq!(seven[7]["content"], "a7");
        let anchor = stored_offset(&state_dir).await.expect("the slide anchors");
        assert!(
            offset_is_line_start(&state_dir, anchor).await,
            "the anchor sits on a turn boundary"
        );

        // And it grows again from there.
        append_turn(&state_dir, 8).await;
        let (_, eight) = load_context(topic, tmp.path(), &sliding(4, 0.5)).await;
        assert_eq!(eight.len(), 10, "the window grows by append again");
        assert_eq!(eight[0]["content"], "u4");
        assert_eq!(eight[9]["content"], "a8");
    }

    #[tokio::test]
    async fn extend_rate_zero_is_a_hard_window() {
        // max_window == window: every turn past the window slides.
        let tmp = tempfile::tempdir().unwrap();
        let topic = "windowed-hard-window";
        let state_dir = setup(&tmp, topic, &turns_journal(1, 4)).await;

        let (_, four) = load_context(topic, tmp.path(), &sliding(4, 0.0)).await;
        assert_eq!(four.len(), 8);

        append_turn(&state_dir, 5).await;
        let (_, five) = load_context(topic, tmp.path(), &sliding(4, 0.0)).await;
        assert_eq!(five.len(), 8, "5 > 4 -> slide to 4");
        assert_eq!(five[0]["content"], "u2");
        assert_eq!(five[7]["content"], "a5");
    }

    #[tokio::test]
    async fn max_history_tokens_slides_at_the_top_of_the_cycle() {
        // 6 turns reach `max_window` (4 + round(0.5 × 4)), so the turn
        // trigger is still quiet (it needs *more* than max_window) and the
        // budget is what fires.
        let tmp = tempfile::tempdir().unwrap();
        let topic = "windowed-budget-slide";
        let state_dir = setup(&tmp, topic, &turns_journal(1, 6)).await;

        let mut strategy = sliding(4, 0.5);
        strategy.max_history_tokens = Some(1); // tiny: always over
        let (_, raw) = load_context(topic, tmp.path(), &strategy).await;

        assert_eq!(raw.len(), 8, "budget trip slides to `window` turns");
        assert_eq!(raw[0]["content"], "u3", "the last 4 of 6 turns");
        assert_eq!(raw[7]["content"], "a6");
        let offset = stored_offset(&state_dir).await.expect("anchor moved");
        assert!(
            offset_is_line_start(&state_dir, offset).await,
            "the anchor sits on a turn boundary"
        );
    }

    #[tokio::test]
    async fn max_history_tokens_does_not_slide_below_max_window() {
        // The budget is a second trigger for the same slide, not a
        // per-turn trim: a window below `max_window` stays put even when it
        // is over budget, because sliding it every turn would re-encode
        // the payload every turn.
        let tmp = tempfile::tempdir().unwrap();
        let topic = "windowed-budget-no-slide";
        let state_dir = setup(&tmp, topic, &turns_journal(1, 5)).await;

        let mut strategy = sliding(4, 0.5); // max_window = 6
        strategy.max_history_tokens = Some(1);
        let (_, raw) = load_context(topic, tmp.path(), &strategy).await;

        assert_eq!(raw.len(), 10, "5 turns < max_window: nothing dropped");
        assert_eq!(raw[0]["content"], "u1");
        assert!(
            stored_offset(&state_dir).await.is_none(),
            "no slide, so no anchor"
        );
    }

    #[tokio::test]
    async fn stale_anchor_restarts_cold() {
        // An anchor pointing into a line's middle (journal replaced or
        // truncated under it) must not be trusted.
        let tmp = tempfile::tempdir().unwrap();
        let topic = "windowed-stale-anchor";
        let state_dir = setup(&tmp, topic, &turns_journal(1, 6)).await;

        let mut state = load_session_state(&state_dir.join(SESSION_FILE)).await;
        state.context_offset = Some(3); // middle of the first line
        save_session_state(topic, &state_dir.join(SESSION_FILE), &state).await;

        let (_, raw) = load_context(topic, tmp.path(), &sliding(4, 0.5)).await;
        assert_eq!(raw.len(), 12, "cold start keeps all 6 turns");
        assert_eq!(raw[0]["content"], "u1");
        assert!(
            stored_offset(&state_dir).await.is_none(),
            "the bad anchor is dropped, not left to repeat the cold start"
        );
    }

    #[tokio::test]
    async fn anchor_past_the_journal_end_restarts_cold() {
        let tmp = tempfile::tempdir().unwrap();
        let topic = "windowed-anchor-past-end";
        let state_dir = setup(&tmp, topic, &turns_journal(1, 2)).await;

        let mut state = load_session_state(&state_dir.join(SESSION_FILE)).await;
        state.context_offset = Some(9_999_999);
        save_session_state(topic, &state_dir.join(SESSION_FILE), &state).await;

        let (_, raw) = load_context(topic, tmp.path(), &sliding(4, 0.5)).await;
        assert_eq!(raw.len(), 4, "the journal still loads from its head");
        assert!(
            stored_offset(&state_dir).await.is_none(),
            "an anchor past the end is dropped too"
        );
    }

    #[tokio::test]
    async fn an_anchored_window_without_an_assistant_message_keeps_the_journal() {
        // The corruption guard deletes the journal only when a
        // *head-covering* slice holds no assistant message. A slice that
        // starts at an anchor is not entitled to that verdict: a turn
        // persisted user-first and interrupted before its reply leaves a
        // legitimately assistant-free tail.
        let tmp = tempfile::tempdir().unwrap();
        let topic = "windowed-user-only-tail";
        let mut journal = turns_journal(1, 6);
        let anchor = journal.len() as u64;
        journal.push_str(&format!(
            "{}\n",
            serde_json::json!({"role": "user", "content": "u7"})
        ));
        let state_dir = setup(&tmp, topic, &journal).await;

        let mut state = load_session_state(&state_dir.join(SESSION_FILE)).await;
        state.context_offset = Some(anchor);
        save_session_state(topic, &state_dir.join(SESSION_FILE), &state).await;

        let (_, raw) = load_context(topic, tmp.path(), &sliding(4, 0.5)).await;

        assert_eq!(raw.len(), 1, "the window holds the unanswered turn alone");
        assert!(
            state_dir.join(AGENT_CONTEXT_FILE).exists(),
            "an anchored window must not delete the journal"
        );
        assert_eq!(
            stored_offset(&state_dir).await,
            Some(anchor),
            "no slide, so the anchor stays put"
        );
    }

    #[test]
    fn window_start_keeps_whole_turns_from_the_end() {
        let journal: Vec<serde_json::Value> = (1..=6)
            .flat_map(|i| {
                [
                    serde_json::json!({"role": "user", "content": format!("u{i}")}),
                    serde_json::json!({"role": "assistant", "content": format!("a{i}")}),
                    serde_json::json!({"role": "user", "content": [{"type": "tool_result", "tool_use_id": "t", "content": "r"}]}),
                ]
            })
            .collect();
        let offsets: Vec<u64> = (0..journal.len() as u64).map(|i| i * 100).collect();

        // Keep the last 2 turns: they open at message index 12 and 15.
        assert_eq!(window_start(&journal, &offsets, 2), Some((12, 1200)));
        // Keeping all 6 is a no-op.
        assert_eq!(window_start(&journal, &offsets, 6), None);
        assert_eq!(window_start(&journal, &offsets, 99), None);
        // `keep == 0` is clamped to one turn, never to "drop everything".
        assert_eq!(window_start(&journal, &offsets, 0), Some((15, 1500)));
    }

    #[test]
    fn window_start_needs_offsets_to_persist() {
        let journal = vec![
            serde_json::json!({"role": "user", "content": "u1"}),
            serde_json::json!({"role": "user", "content": "u2"}),
        ];
        // No offsets (a backward `read_prior` slice): dropping is refused
        // rather than resuming from a guessed anchor.
        assert_eq!(window_start(&journal, &[], 1), None);
    }
}
