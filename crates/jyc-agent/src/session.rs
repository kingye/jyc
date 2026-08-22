//! Session management for the in-process agent.
//!
//! Manages:
//! - Full conversation log (`.jyc/agent-conversation.json`) — complete LLM history
//!   including tool calls and results for multi-turn context
//! - Session state (`.jyc/agent-session.json`) — token tracking, auto-reset
//!
//! On reset: session state is cleared, conversation is summarized (last few turns kept).

use jyc_types::channel::{CompressionMode, ResetCompressionConfig};
use serde::{Deserialize, Serialize};
use std::path::Path;
use tracing;

use crate::types::{ContentBlock, Message, Role};

const CONTEXT_FILE: &str = "agent-context.json";
const SESSION_FILE: &str = "agent-session.json";

/// Session state persisted to `.jyc/agent-session.json`.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SessionState {
    /// When this session was created (ISO 8601).
    pub created_at: String,
    /// Current context size in tokens. Equal to the input tokens reported by
    /// the most recent LLM call, since each call sends the full conversation
    /// context. NOT a sum across calls — for accumulated input tokens see
    /// `total_input_tokens`, for accumulated output tokens see
    /// `total_output_tokens`.
    pub context_input_tokens: u64,
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
    /// Max tokens (context window) for the model.
    #[serde(default)]
    pub max_input_tokens: u64,
    /// Accumulated cost of this session, in the currency configured for
    /// the model(s) used. **The only accumulating field on this struct** —
    /// every other field is assigned from the caller's running total,
    /// whereas this one is incremented by each call's cost as it happens
    /// (see `persist_tokens`).
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
pub async fn save_raw_context(topic_path: &Path, raw_context: &[serde_json::Value]) {
    let jyc_dir = topic_path.join(".jyc");
    tokio::fs::create_dir_all(&jyc_dir).await.ok();
    let path = jyc_dir.join(CONTEXT_FILE);

    tracing::info!(
        message_count = raw_context.len(),
        file = %path.display(),
        "save_raw_context: saving context"
    );

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
pub async fn load_context(topic_path: &Path) -> (Vec<Message>, Vec<serde_json::Value>) {
    let jyc_dir = topic_path.join(".jyc");
    let session_path = jyc_dir.join(SESSION_FILE);
    let context_path = jyc_dir.join(CONTEXT_FILE);

    // No session file = fresh start. No prior context.
    if !session_path.exists() {
        tracing::warn!(
            session_path = %session_path.display(),
            context_path = %context_path.display(),
            "load_context: no session file, returning empty context"
        );
        return (Vec::new(), Vec::new());
    }

    // Load raw context (provider-formatted JSON)
    if !context_path.exists() {
        tracing::warn!(
            context_path = %context_path.display(),
            "load_context: session file exists but no context file"
        );
        return (Vec::new(), Vec::new());
    }

    let content = match tokio::fs::read_to_string(&context_path).await {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(
                error = %e,
                path = %context_path.display(),
                "load_context: failed to read context file"
            );
            return (Vec::new(), Vec::new());
        }
    };

    let raw_context: Vec<serde_json::Value> = match serde_json::from_str(&content) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(
                error = %e,
                file_len = content.len(),
                "load_context: failed to parse context file"
            );
            return (Vec::new(), Vec::new());
        }
    };
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
            tracing::error!(
                context_messages = raw_context.len(),
                roles = ?raw_context.iter()
                    .map(|m| m.get("role").and_then(|r| r.as_str()).unwrap_or("?"))
                    .collect::<Vec<_>>(),
                "Context file has no assistant messages after filtering, deleting. \
                 This indicates context corruption (e.g. summarization produced \
                 user-only context)."
            );
            tokio::fs::remove_file(&context_path).await.ok();
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
/// probes (`read_token_state` / `read_input_tokens`) all see `(None,
/// None, None)`. Pre-creating the file here makes the topic visible
/// immediately with the correct `max_input_tokens` and zeroed counters.
///
/// `context_window` is used to seed `max_input_tokens = context_window *
/// auto_reset_threshold` (matching the convention used by
/// `persist_tokens_returning_state`). When `context_window` is `None`,
/// `max_input_tokens` is left at 0 (the post-loop `update_tokens` will
/// fill it in on the next turn).
pub async fn ensure_session_file(
    topic_path: &Path,
    context_window: Option<u64>,
    auto_reset_threshold: f64,
) {
    let session_path = topic_path.join(".jyc").join(SESSION_FILE);
    if session_path.exists() {
        // Already there — never overwrite existing token data.
        return;
    }

    let mut state = SessionState::default();
    if let Some(cw) = context_window {
        state.max_input_tokens = (cw as f64 * auto_reset_threshold) as u64;
    }
    state.created_at = chrono::Utc::now().to_rfc3339();

    save_session_state(&session_path, &state).await;
    tracing::info!(
        file = %session_path.display(),
        max_input_tokens = state.max_input_tokens,
        "ensure_session_file: created session file at start of agent processing"
    );
}

/// Persist the latest input/output token counts to disk without triggering
/// auto-reset. Called from the agent loop after every LLM response so the
/// dashboard polls see fresh data mid-round. The post-loop `update_tokens`
/// still owns the actual reset decision.
///
/// `input_tokens` is the tokens reported by the last API call — stored
/// directly (not accumulated) since each call already includes the full
/// context. `total_input_tokens`, `output_tokens`,
/// `total_cache_hit_tokens`, and `total_cache_creation_tokens` are
/// running totals accumulated by the caller (`agent_loop`), passed in
/// as the current sum.
///
/// `call_cost` is the cost of the single call that just completed and is
/// **added** to `session_cost` (unlike every other field, which is
/// assigned). Pass `0.0` when the model has no configured pricing.
#[allow(clippy::too_many_arguments)]
pub async fn persist_tokens(
    topic_path: &Path,
    input_tokens: u64,
    total_input_tokens: u64,
    output_tokens: u64,
    total_cache_hit_tokens: u64,
    total_cache_creation_tokens: u64,
    context_window: Option<u64>,
    auto_reset_threshold: f64,
    call_cost: f64,
) {
    let _ = persist_tokens_returning_state(
        topic_path,
        input_tokens,
        total_input_tokens,
        output_tokens,
        total_cache_hit_tokens,
        total_cache_creation_tokens,
        context_window,
        auto_reset_threshold,
        call_cost,
    )
    .await;
}

/// Like `persist_tokens` but returns the final state and path so callers
/// (currently only `update_tokens` for the post-loop auto-reset decision)
/// can inspect threshold-crossing without a second disk read.
#[allow(clippy::too_many_arguments)]
async fn persist_tokens_returning_state(
    topic_path: &Path,
    input_tokens: u64,
    total_input_tokens: u64,
    output_tokens: u64,
    total_cache_hit_tokens: u64,
    total_cache_creation_tokens: u64,
    context_window: Option<u64>,
    auto_reset_threshold: f64,
    call_cost: f64,
) -> (std::path::PathBuf, SessionState) {
    let session_path = topic_path.join(".jyc").join(SESSION_FILE);
    let mut state = load_session_state(&session_path).await;

    state.context_input_tokens = input_tokens;
    state.total_input_tokens = total_input_tokens;
    state.total_output_tokens = output_tokens;
    state.total_cache_hit_tokens = total_cache_hit_tokens;
    state.total_cache_creation_tokens = total_cache_creation_tokens;
    // The one accumulating field: each call's cost adds to the session
    // total rather than replacing it.
    state.session_cost += call_cost;

    if let Some(cw) = context_window {
        state.max_input_tokens = (cw as f64 * auto_reset_threshold) as u64;
    }

    if state.created_at.is_empty() {
        state.created_at = chrono::Utc::now().to_rfc3339();
    }

    save_session_state(&session_path, &state).await;
    (session_path, state)
}

/// Add to `session_cost` without touching the token counters.
///
/// Used for ancillary LLM calls (cycle-boundary progress summaries,
/// context compression on reset) whose tokens are real spend but are not
/// part of the main loop's context accounting -- folding them into
/// `total_input_tokens` would corrupt the context-window math that drives
/// auto-reset. The cost is still recorded so the displayed total is
/// truthful.
///
/// No-op when `cost` is zero (no pricing configured, or the call failed).
pub async fn add_session_cost(topic_path: &Path, cost: f64) {
    if cost <= 0.0 {
        return;
    }
    let session_path = topic_path.join(".jyc").join(SESSION_FILE);
    let mut state = load_session_state(&session_path).await;
    state.session_cost += cost;
    if state.created_at.is_empty() {
        state.created_at = chrono::Utc::now().to_rfc3339();
    }
    save_session_state(&session_path, &state).await;
}

/// Update token tracking in the session state.
/// Creates the session file if it doesn't exist.
/// Auto-resets when `context_input_tokens` crosses `max_input_tokens`, using
/// the configured `reset_compression` strategy (same as manual `/reset`).
///
/// `input_tokens` is the tokens reported by the last API call — this already
/// includes all prior context, so we store it directly (not accumulated).
///
/// `summary_provider` is the provider used to generate the LLM summary when
/// the auto-reset threshold is crossed AND `compression_config.mode` is `Llm`.
/// Callers should pass the small model's provider when configured
/// (`[agent].small_model`), otherwise the main provider — falling back is
/// the caller's responsibility.
///
/// `auto_reset_threshold` is the fraction of context window at which to trigger
/// auto-reset (0.0~1.0, default 0.95).
///
/// `compression_config` controls how the session is compacted on auto-reset.
/// All three paths — manual `/reset`, pre-loop pre-check, and this post-loop
/// auto-reset — go through `reset_session()` with this config so user
/// preferences (`mode`, `keep_pairs`) are honored consistently.
///
/// Does **not** add to `session_cost`: cost is banked per-call inside the
/// agent loop via `persist_tokens`, so adding here would double-count the
/// round.
#[allow(clippy::too_many_arguments)]
pub async fn update_tokens(
    topic_path: &Path,
    input_tokens: u64,
    total_input_tokens: u64,
    output_tokens: u64,
    total_cache_hit_tokens: u64,
    total_cache_creation_tokens: u64,
    context_window: Option<u64>,
    summary_provider: &dyn crate::provider::Provider,
    auto_reset_threshold: f64,
    compression_config: &ResetCompressionConfig,
    billing: Option<&BillingContext>,
) {
    // Persist the latest token counts. The returned state carries the
    // post-mutation values so the auto-reset check below doesn't need a
    // second disk read.
    let (_, state) = persist_tokens_returning_state(
        topic_path,
        input_tokens,
        total_input_tokens,
        output_tokens,
        total_cache_hit_tokens,
        total_cache_creation_tokens,
        context_window,
        auto_reset_threshold,
        // Cost already banked per-call by the agent loop.
        0.0,
    )
    .await;

    // Auto-reset if tokens exceed max context window. Delegates to
    // `reset_session` so the user's `reset_compression` config is honored
    // (previously this inlined `summarize_context` which always used LLM,
    // ignoring the configured mode).
    if state.max_input_tokens > 0 && state.context_input_tokens >= state.max_input_tokens {
        tracing::info!(
            context_input_tokens = state.context_input_tokens,
            max_input_tokens = state.max_input_tokens,
            mode = ?compression_config.mode,
            "Session exceeded max input tokens, auto-resetting with configured compression",
        );

        reset_session(
            topic_path,
            compression_config,
            Some(summary_provider),
            billing,
        )
        .await;

        // reset_session deletes the session file; rebuild it with the
        // current max_input_tokens and zero counters so the next turn
        // starts clean. session_cost zeroes with them — it is scoped to
        // the session, and the durable ledger is bill-YYYY-MM-DD.jsonl.
        persist_tokens(
            topic_path,
            0,
            0,
            0,
            0,
            0,
            context_window,
            auto_reset_threshold,
            0.0,
        )
        .await;
    }
}

/// If the loaded session's tokens exceed the new context window, reset the
/// session using the configured compression strategy. Must be called BEFORE
/// the agent loop when the active model changes to a smaller window —
/// otherwise the first LLM call rejects the oversized context and the
/// post-loop auto-reset never fires.
///
/// Returns `true` if the session was reset.
pub async fn maybe_reset_for_new_context(
    topic_path: &Path,
    new_max_input_tokens: u64,
    compression_config: &ResetCompressionConfig,
    provider: Option<&dyn crate::provider::Provider>,
    billing: Option<&BillingContext>,
) -> bool {
    if new_max_input_tokens == 0 {
        return false;
    }
    let session_path = topic_path.join(".jyc").join(SESSION_FILE);
    let state = load_session_state(&session_path).await;
    if state.context_input_tokens < new_max_input_tokens {
        return false;
    }
    tracing::info!(
        old_tokens = state.context_input_tokens,
        new_max = new_max_input_tokens,
        mode = ?compression_config.mode,
        "Loaded session exceeds new context window; resetting before agent loop",
    );
    reset_session(topic_path, compression_config, provider, billing).await;
    true
}

/// What the ledger needs in order to bill an ancillary LLM call.
///
/// Passed as `Option` through the reset path because most callers (e.g.
/// the `/reset` command) have no pricing in scope; `None` simply means
/// the call is not billed, matching the unpriced-model behaviour.
#[derive(Debug, Clone)]
pub struct BillingContext {
    /// Rates for the active model.
    pub pricing: jyc_types::ModelPricing,
    /// `"provider/model"` label recorded on the ledger entry.
    pub model_label: String,
}

// ─── Reset ───────────────────────────────────────────────────────────

/// Reset the session with configurable compression.
///
/// Called when user triggers a session reset (e.g., from dashboard or /reset command).
///
/// Compression behavior depends on `config.mode`:
/// - `None` → delete both `agent-session.json` and `agent-context.json`
/// - `Heuristic` → `summarize_context_heuristic()` then delete `agent-session.json`
/// - `Llm` → if `provider` is available, `summarize_context()` (LLM) then delete
///   `agent-session.json`; fallback to heuristic if no provider
///
/// `config.keep_pairs` controls how many user+assistant pairs to retain in heuristic mode.
pub async fn reset_session(
    topic_path: &Path,
    config: &ResetCompressionConfig,
    provider: Option<&dyn crate::provider::Provider>,
    billing: Option<&BillingContext>,
) {
    let jyc_dir = topic_path.join(".jyc");

    match config.mode {
        CompressionMode::None => {
            // Delete everything — no summary
            let context_path = jyc_dir.join(CONTEXT_FILE);
            tokio::fs::remove_file(&context_path).await.ok();
            let session_path = jyc_dir.join(SESSION_FILE);
            tokio::fs::remove_file(&session_path).await.ok();
            tracing::info!("Agent session reset (no compression)");
        }
        CompressionMode::Heuristic => {
            // Heuristic compaction: keep last N pairs, then delete session
            summarize_context_heuristic(topic_path, config.keep_pairs).await;
            let session_path = jyc_dir.join(SESSION_FILE);
            tokio::fs::remove_file(&session_path).await.ok();
            tracing::info!("Agent session reset (heuristic compression)");
        }
        CompressionMode::Llm => {
            if let Some(p) = provider {
                // LLM summary, then delete session
                summarize_context(topic_path, p, billing).await;
            } else {
                // No provider available — fallback to heuristic
                tracing::warn!(
                    "LLM compression mode selected but no provider available, \
                     falling back to heuristic"
                );
                summarize_context_heuristic(topic_path, config.keep_pairs).await;
            }
            let session_path = jyc_dir.join(SESSION_FILE);
            tokio::fs::remove_file(&session_path).await.ok();
            tracing::info!("Agent session reset (LLM compression)");
        }
    }
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
async fn summarize_context(
    topic_path: &Path,
    provider: &dyn crate::provider::Provider,
    billing: Option<&BillingContext>,
) {
    let context_path = topic_path.join(".jyc").join(CONTEXT_FILE);

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
    let (summary_text, usage) = match generate_context_summary(provider, &joined).await {
        Ok(pair) => pair,
        Err(e) => {
            tracing::warn!(
                error = %e,
                "LLM context summary failed, falling back to heuristic compaction"
            );
            summarize_context_heuristic(topic_path, 3).await;
            return;
        }
    };

    // Build the compacted context: [task_anchor, synthetic_assistant_with_summary].
    // The synthetic assistant message uses a tagged delimiter so the model can
    // recognize it as machine-generated context. Using "assistant" role is
    // critical: `load_context` rejects context files with no assistant messages
    // (treating them as corrupted and deleting them).
    let summary_assistant = serde_json::json!({
        "role": "assistant",
        "content": format!(
            "<jyc-context-summary>\nPrior conversation summary (auto-generated when token budget was exceeded):\n\n{}\n</jyc-context-summary>",
            summary_text
        ),
    });

    // Bill the compression call. `session_cost` is deliberately NOT updated:
    // `reset_session` deletes the session file immediately after this returns,
    // so the session total resets to zero by design. The ledger entry is what
    // must survive -- this was real spend, and the durable per-day total has
    // to include it.
    if let Some(b) = billing {
        let (input_tokens, output_tokens, cache_hit_tokens, cache_creation_tokens) = usage;
        if input_tokens > 0
            || output_tokens > 0
            || cache_hit_tokens > 0
            || cache_creation_tokens > 0
        {
            let (cost, rates) = jyc_types::pricing::compute_cost_split_with_rates(
                &b.pricing,
                input_tokens,
                output_tokens,
                cache_hit_tokens,
                cache_creation_tokens,
            );
            let (time_window, utc_offset) = rates.source.billing_fields();
            let entry = jyc_core::billing_log_store::BillingEntry {
                ts: chrono::Utc::now().to_rfc3339(),
                model: b.model_label.clone(),
                input_tokens,
                output_tokens,
                cache_hit_tokens,
                cache_creation_tokens,
                cost,
                currency: b.pricing.currency_label().to_string(),
                kind: jyc_core::billing_log_store::KIND_SUMMARY.to_string(),
                input_rate_per_million: rates.input_per_million,
                output_rate_per_million: rates.output_per_million,
                cache_hit_rate_per_million: rates.cache_hit_per_million,
                time_window,
                utc_offset,
            };
            if let Err(e) = jyc_core::billing_log_store::BillingLogStore::append(topic_path, &entry)
            {
                tracing::warn!(error = %e, "Failed to append context-compression billing entry");
            }
        }
    }

    let mut compacted: Vec<serde_json::Value> = Vec::with_capacity(2);
    if let Some(fu) = first_user {
        compacted.push(fu);
    }
    compacted.push(summary_assistant);

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
) -> anyhow::Result<(String, (u64, u64, u64, u64))> {
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
    // Capture usage so the caller can bill this call. Summarizing the whole
    // transcript is expensive, and dropping the `Usage` event here is what
    // previously made compression spend invisible in the ledger.
    let mut usage = (0u64, 0u64, 0u64, 0u64);
    use futures::StreamExt;
    let mut stream = std::pin::pin!(stream);
    while let Some(event) = stream.next().await {
        match event {
            Ok(crate::types::StreamEvent::TextDelta(t)) => text.push_str(&t),
            Ok(crate::types::StreamEvent::Usage {
                input_tokens,
                output_tokens,
                cache_hit_tokens,
                cache_creation_tokens,
            }) => {
                usage = (
                    input_tokens,
                    output_tokens,
                    cache_hit_tokens,
                    cache_creation_tokens,
                )
            }
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
    Ok((text, usage))
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
/// windowed annotation; longer values are truncated with `…`.
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
/// `WINDOWED_TOOL_ARG_MAX`.
fn render_tool_call(name: &str, args: &serde_json::Value) -> String {
    if let Some(map) = args.as_object() {
        // Empty maps join to `name()` naturally; no empty-args special case.
        let parts = map
            .iter()
            .map(|(k, v)| format!("{k}={}", truncate_json_value(v)))
            .collect::<Vec<_>>();
        format!("{name}({})", parts.join(", "))
    } else {
        format!("{name}({})", truncate_json_value(args))
    }
}

/// Truncate `s` at `max` bytes (on a char boundary) with `…` when longer.
fn truncate_text(s: &str, max: usize) -> String {
    if s.len() > max {
        let cut = s.floor_char_boundary(max);
        format!("{}…", &s[..cut])
    } else {
        s.to_string()
    }
}

/// Render a JSON value as text, truncating at `WINDOWED_TOOL_ARG_MAX`
/// bytes (on a char boundary) with `…` when it is longer.
fn truncate_json_value(value: &serde_json::Value) -> String {
    truncate_text(&value.to_string(), WINDOWED_TOOL_ARG_MAX)
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

/// Collect tool results from one raw wire-format message into `results`
/// (`tool_call_id → truncated result text`). Handles OpenAI (`role: "tool"`
/// with `tool_call_id`) and Anthropic (`role: "user"` with `tool_result`
/// blocks carrying `tool_use_id`) shapes. Lets the windowed annotation
/// show *what each tool returned*, not just that it was called.
fn collect_results_from(
    msg: &serde_json::Value,
    results: &mut std::collections::HashMap<String, String>,
) {
    match msg.get("role").and_then(|r| r.as_str()).unwrap_or("") {
        // OpenAI: {"role": "tool", "tool_call_id": "...", "content": "..."}
        "tool" => {
            if let Some(id) = msg.get("tool_call_id").and_then(|i| i.as_str()) {
                let text = msg
                    .get("content")
                    .map(extract_result_text)
                    .unwrap_or_default();
                results.insert(
                    id.to_string(),
                    truncate_text(&text, WINDOWED_TOOL_RESULT_MAX),
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
                            truncate_text(&text, WINDOWED_TOOL_RESULT_MAX),
                        );
                    }
                }
            }
        }
        _ => {}
    }
}

/// Build the bare tool-call annotation for an assistant message, e.g.
/// `(incl. followed tool calls: bash(command="ls -la") → BRANCH main…)`.
/// When a tool result is known for a call's `id` (looked up in `results`),
/// it is appended as `→ <truncated result>` so the agent can see *what the
/// tool returned*, not just that it was called. Returns `None` when the
/// message carries no tool calls.
fn tool_call_annotation(
    msg: &serde_json::Value,
    results: &std::collections::HashMap<String, String>,
) -> Option<String> {
    let calls = collect_tool_calls(msg);
    if calls.is_empty() {
        return None;
    }
    let rendered = calls
        .iter()
        .map(|(id, name, args)| {
            let mut s = render_tool_call(name, args);
            if let Some(result) = results.get(id)
                && !result.is_empty()
            {
                s.push_str(" → ");
                s.push_str(result);
            }
            s
        })
        .collect::<Vec<_>>();
    Some(format!(
        "(incl. followed tool calls: {})",
        rendered.join(", ")
    ))
}

/// Flush the accumulated turn into `pairs`: merge every assistant message
/// of the turn (tool-call steps included) into a single assistant entry
/// whose text carries each step's text plus its folded tool-call
/// annotation, joined by newlines. Always clears the turn state.
///
/// A turn with no assistant reply yet (interrupted between user message
/// and first response) produces no pair; the fallback in
/// `extract_user_assistant_pairs` keeps the last user message for that
/// case. Assistant messages seen before any user message (no pairing
/// anchor) are dropped.
fn flush_turn(
    pairs: &mut Vec<(serde_json::Value, serde_json::Value)>,
    cur_user: &mut Option<serde_json::Value>,
    cur_assistants: &mut Vec<serde_json::Value>,
    results: &std::collections::HashMap<String, String>,
) {
    let user_msg = cur_user.take();
    let assistants = std::mem::take(cur_assistants);
    if assistants.is_empty() {
        return;
    }
    let Some(user_msg) = user_msg else { return };

    let mut segments: Vec<String> = Vec::with_capacity(assistants.len());
    for msg in &assistants {
        let text = extract_message_text(msg);
        // Keep only role + content, folding bare tool calls into the text
        // so the windowed part shows which tools ran and what they
        // returned. A tool-call-only step (empty text) is kept too,
        // annotated alone, instead of being dropped.
        let content = match tool_call_annotation(msg, results) {
            Some(annotation) if text.is_empty() => annotation,
            Some(annotation) => format!("{text}\n{annotation}"),
            None => text,
        };
        if !content.is_empty() {
            segments.push(content);
        }
    }
    if segments.is_empty() {
        return;
    }

    let clean_assistant = serde_json::json!({
        "role": "assistant",
        "content": segments.join("\n"),
    });
    pairs.push((user_msg, clean_assistant));
}

/// Extract **all** user+assistant turn pairs from raw context.
///
/// Pairing unit is the **turn**, not the single message: a turn opens at
/// a text-bearing user message and closes at the next one (or end of
/// context). Every assistant message in between — intermediate tool-call
/// steps and the final reply alike — is merged into one assistant entry
/// (see [`flush_turn`]), so the windowed view never loses the turn's
/// conclusion (including `jyc_reply_message` calls, which live in the
/// turn's later assistant messages).
///
/// Assistant messages are cleaned to only role + content (strip
/// reasoning_content, tool_calls); bare tool calls are folded into the
/// text as a `(incl. followed tool calls: …)` annotation (parameters kept,
/// over-long values truncated, tool results appended as `→ <result>`).
/// Returns pairs in oldest→newest order.
///
/// A trailing turn whose user message has no assistant reply yet is
/// dropped, mirroring the windowed-view semantics — a completed turn only
/// (`extract_user_assistant_pairs` has a fallback for the no-pairs case).
///
/// Single pass: tool results are collected into a **turn-scoped** map as
/// they stream by, so a tool_call_id reused in a later turn can never
/// misattach to an earlier turn's call.
///
/// Shared by session heuristic compaction, mid-loop compression, and the
/// `context_browse` built-in tool.
pub(crate) fn extract_pairs(
    raw_context: &[serde_json::Value],
) -> Vec<(serde_json::Value, serde_json::Value)> {
    let mut pairs: Vec<(serde_json::Value, serde_json::Value)> = Vec::new();
    let mut cur_user: Option<serde_json::Value> = None;
    let mut cur_assistants: Vec<serde_json::Value> = Vec::new();
    let mut results: std::collections::HashMap<String, String> = std::collections::HashMap::new();

    for msg in raw_context {
        let role = msg.get("role").and_then(|r| r.as_str()).unwrap_or("");
        match role {
            "user" => {
                // Text-bearing user message opens a new turn; user-role
                // messages with no extractable text are Anthropic
                // tool_result wrappers, which stay in the current turn and
                // only feed the results map.
                if extract_message_text(msg).is_empty() {
                    collect_results_from(msg, &mut results);
                } else {
                    flush_turn(&mut pairs, &mut cur_user, &mut cur_assistants, &results);
                    results.clear();
                    cur_user = Some(msg.clone());
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

/// Extract user+assistant text pairs from raw context and keep only the
/// last `keep_pairs` of them, flattened into a single Vec. Pair extraction
/// semantics are defined in [`extract_pairs`].
///
/// Shared between session heuristic compaction and mid-loop compression.
pub(crate) fn extract_user_assistant_pairs(
    raw_context: &[serde_json::Value],
    keep_pairs: usize,
) -> Vec<serde_json::Value> {
    // Keep only the last N pairs
    let summary: Vec<serde_json::Value> = extract_pairs(raw_context)
        .into_iter()
        .rev()
        .take(keep_pairs)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .flat_map(|(user, assistant)| vec![user, assistant])
        .collect();

    if summary.is_empty() {
        // Fallback: keep the LAST text-bearing user message — the closest
        // thing to an unfinished turn. (The first user message is usually
        // ancient, unrelated history and would sit misleadingly in front
        // of the current turn.)
        if let Some(last_user) = raw_context.iter().rev().find(|m| {
            m.get("role").and_then(|r| r.as_str()) == Some("user")
                && !extract_message_text(m).is_empty()
        }) {
            vec![last_user.clone()]
        } else {
            Vec::new()
        }
    } else {
        summary
    }
}

/// Heuristic context compaction: keep only the last N user+assistant text
/// pairs.
///
/// Removes tool calls, tool results, and reasoning_content. Used as a
/// fallback when the LLM-based summarizer is unavailable or fails, and as
/// the primary path for user-triggered `reset_session` (which has no
/// provider context).
///
/// `keep_pairs` controls how many user+assistant pairs to retain.
async fn summarize_context_heuristic(topic_path: &Path, keep_pairs: usize) {
    let context_path = topic_path.join(".jyc").join(CONTEXT_FILE);

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

    // Extract user+assistant text pairs using shared logic
    let summary = extract_user_assistant_pairs(&raw_context, keep_pairs);

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
/// Reads from `.jyc/` first (new location), falls back to topic root (legacy).
#[allow(dead_code)]
async fn load_from_chat_history(
    topic_path: &Path,
    cutoff: Option<&chrono::DateTime<chrono::Utc>>,
) -> Vec<Message> {
    let (history_files, _dir) = jyc_core::chat_log_store::list_chat_history_files(topic_path);

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

#[cfg(test)]
mod tests {
    use serde_json::json;

    /// Anthropic-shaped context (array content): assistant text pairs
    /// correctly; tool_result user-role wrappers are skipped; the
    /// assistant's bare tool use is folded into a text annotation, now
    /// including the truncated result (`→ ok`).
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
        let pairs = super::extract_user_assistant_pairs(&ctx, 10);
        assert_eq!(pairs.len(), 2);
        assert_eq!(pairs[0], ctx[0]);
        assert_eq!(
            pairs[1],
            json!({"role": "assistant",
                "content": "a1\n(incl. followed tool calls: bash(command=\"ls\") → ok)"})
        );
    }

    /// OpenAI-shaped context: tool calls render with all parameters,
    /// multiple calls comma-separated.
    #[test]
    fn extract_pairs_annotates_tool_call_args() {
        let ctx = vec![
            json!({"role": "user", "content": "u1"}),
            json!({"role": "assistant", "content": "running", "tool_calls": [
                {"id": "1", "type": "function", "function": {"name": "bash", "arguments": r#"{"command": "ls -la", "timeout": 30}"#}},
                {"id": "2", "type": "function", "function": {"name": "read", "arguments": r#"{"path": "a.txt"}"#}},
            ]}),
        ];
        let pairs = super::extract_user_assistant_pairs(&ctx, 10);
        assert_eq!(pairs.len(), 2);
        assert_eq!(
            pairs[1],
            json!({"role": "assistant",
                "content": "running\n(incl. followed tool calls: bash(command=\"ls -la\", timeout=30), read(path=\"a.txt\"))"})
        );
    }

    /// Tool-call-only assistant turns (no text) are kept, annotated alone,
    /// rather than dropped.
    #[test]
    fn extract_pairs_keeps_tool_call_only_assistant() {
        let ctx = vec![
            json!({"role": "user", "content": "u1"}),
            json!({"role": "assistant", "content": "", "tool_calls": [
                {"id": "1", "type": "function", "function": {"name": "bash", "arguments": r#"{"command": "ls -la"}"#}}
            ]}),
        ];
        let pairs = super::extract_user_assistant_pairs(&ctx, 10);
        assert_eq!(pairs.len(), 2);
        assert_eq!(
            pairs[1],
            json!({"role": "assistant",
                "content": "(incl. followed tool calls: bash(command=\"ls -la\"))"})
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
        let pairs = super::extract_user_assistant_pairs(&ctx, 10);
        let content = pairs[1]["content"].as_str().unwrap();
        assert!(
            content.contains("(incl. followed tool calls: bash(command="),
            "no annotation"
        );
        assert!(content.contains('…'), "long arg not truncated");
        // 200-char cap + ellipsis + `command=` prefix + quotes + annotation wrapper.
        assert!(content.len() < 400, "annotation too big: {content:?}");
    }

    /// Tool results are appended to the annotation as `→ <truncated text>`,
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
        let pairs = super::extract_user_assistant_pairs(&ctx, 10);
        let content = pairs[1]["content"].as_str().unwrap();
        // Call 1 gets its result appended after the arrow.
        assert!(
            content.contains(r#"bash(command="ls") → BRANCH main"#),
            "result not appended: {content:?}"
        );
        // Call 2 has no result: no arrow, just the rendered call then the
        // annotation's closing paren.
        assert!(
            content.contains(r#"read(path="a.txt"))"#),
            "call without result should have no arrow: {content:?}"
        );
    }

    /// Over-long tool results are truncated with `…` (500-char cap).
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
        let pairs = super::extract_user_assistant_pairs(&ctx, 10);
        let content = pairs[1]["content"].as_str().unwrap();
        assert!(content.contains('→'), "no result arrow: {content:?}");
        assert!(
            content.contains('…'),
            "long result not truncated: {content:?}"
        );
        // Result cap 500 + arrow + annotation wrapper stay well under 700.
        assert!(content.len() < 700, "annotation too big: {content:?}");
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
        assert_eq!(pairs[0].0["content"], "u1");
        assert_eq!(pairs[0].1["content"], "a1");
        assert_eq!(pairs[1].0["content"], "u2");
        assert_eq!(
            pairs[1].1["content"],
            "a2\n(incl. followed tool calls: bash() → out)"
        );
        assert_eq!(pairs[2].0["content"], "u3");
        assert_eq!(pairs[2].1["content"], "a3");
        // u4 must NOT be paired.
        assert!(pairs.iter().all(|(u, _)| u["content"] != "u4"));
    }

    /// A multi-step tool turn (user → assistant(call) → tool → assistant(call)
    /// → tool → assistant(final reply)) merges into ONE pair: every step's
    /// text + annotation is kept, ending with the turn's conclusion. Before
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
        let content = pairs[0].1["content"].as_str().unwrap();
        assert!(content.contains("step1"), "step1 lost: {content:?}");
        assert!(content.contains("bash() → out1"), "call1 lost: {content:?}");
        assert!(content.contains("step2"), "step2 lost: {content:?}");
        assert!(content.contains("read() → out2"), "call2 lost: {content:?}");
        assert!(
            content.ends_with("final answer"),
            "turn conclusion must be kept: {content:?}"
        );
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
            pairs[0].1["content"].as_str().unwrap().contains("→ first"),
            "turn 1 got wrong result: {:?}",
            pairs[0].1["content"]
        );
        assert!(
            pairs[1].1["content"].as_str().unwrap().contains("→ second"),
            "turn 2 got wrong result: {:?}",
            pairs[1].1["content"]
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
        assert_eq!(pairs[0].0["content"], "u2");
        assert_eq!(pairs[0].1["content"], "a2");
    }

    /// No complete pairs: the fallback keeps the LAST text-bearing user
    /// message (nearest to an unfinished turn), not the first — and skips
    /// Anthropic tool_result wrappers, which carry no user text.
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
}
