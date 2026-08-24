//! Raw-context rendering and history compaction helpers.
//!
//! Extracted from the monolithic `agent_loop.rs`.

use std::borrow::Cow;

use jyc_types::channel::{ContextStrategy, ContextStrategyConfig};

use crate::provider::Provider;
use crate::types::{ContentBlock, Message, Role};

/// Render the raw context as a plain-text transcript for one-shot
/// summarization (cycle-boundary progress updates). Thin wrapper over
/// `session::render_raw_context_as_text` — single implementation shared
/// with the sliding-window view.
pub(crate) fn render_raw_context_as_text(raw_context: &[serde_json::Value]) -> String {
    crate::session::render_raw_context_as_text(raw_context)
}

pub(crate) fn compact_raw_context_heuristic(
    raw_context: &[serde_json::Value],
    keep_pairs: usize,
) -> Vec<serde_json::Value> {
    // Compaction keeps notes on every pair (`None`); `note_window` only
    // shapes the sliding-window wire payload.
    crate::session::extract_user_assistant_pairs(raw_context, keep_pairs, None)
}

/// Heuristic compaction of internal history: keep only the last N user+assistant
/// text pairs. Synced with `compact_raw_context_heuristic`.
///
/// INTENTIONALLY divergent from the raw-context path: this view keeps plain
/// text only (no folded tool-call annotations) and drops tool-call-only
/// turns entirely, because internal history is used for reply detection and
/// text extraction — not for model grounding. Do not "fix" the difference
/// by porting annotations here; the raw path (`extract_pairs`) owns the
/// rich view.
pub(crate) fn compact_history_heuristic(history: &[Message], keep_pairs: usize) -> Vec<Message> {
    let mut pairs: Vec<(Message, Message)> = Vec::new();
    let mut last_user: Option<Message> = None;

    for msg in history {
        match msg.role {
            Role::User => {
                last_user = Some(msg.clone());
            }
            Role::Assistant => {
                let text = msg.text();
                if !text.is_empty()
                    && let Some(user_msg) = last_user.take()
                {
                    pairs.push((user_msg, Message::assistant(text)));
                }
            }
            _ => {}
        }
    }

    // Keep only the last N pairs
    pairs
        .into_iter()
        .rev()
        .take(keep_pairs)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .flat_map(|(user, assistant)| vec![user, assistant])
        .collect()
}

/// Reformat a cleaned message (string content from `extract_user_assistant_pairs`)
/// into the active provider's wire format via the Provider trait. This is
/// what makes sliding-window output valid for Anthropic, which requires
/// user/assistant `content` to be an array of blocks. Returns `None` when
/// the message has no extractable text, so callers must never emit empty
/// text blocks (Anthropic rejects them).
fn format_cleaned_message(
    provider: &dyn Provider,
    msg: &serde_json::Value,
) -> Option<serde_json::Value> {
    let text = crate::session::extract_message_text(msg);
    if text.is_empty() {
        return None;
    }
    Some(
        if msg.get("role").and_then(|r| r.as_str()) == Some("assistant") {
            provider.build_raw_assistant_message(&text, "", &[])
        } else {
            provider.format_user_message(&[ContentBlock::Text { text }])
        },
    )
}

/// Build the context to send to the LLM for the next request.
///
/// The full `raw_context` is always persisted to `.jyc/agent-context.json`
/// untouched; this function only shapes what is sent to the LLM.
///
/// * `Full` — borrow the full context (no copy).
/// * `SlidingWindow` — three parts, in order:
///   1. The last `window - recent` user+assistant text pairs from the
///      older prior, reformatted in provider wire format (string content →
///      provider-correct content shape).
///   2. The most recent `note_window` prior turns **verbatim** (structured
///      `tool_calls` + tool results intact, like the current turn), so the
///      model keeps seeing tool calls in the wire format it is supposed to
///      emit instead of user-role text notes.
///   3. The full current turn (`raw_context[prior_len..]`) verbatim, so
///      tool calls / results stay coherent mid-loop.
///
/// Also returns a per-message region label (parallel to the wire payload)
/// so callers that need to distinguish regions — the wire-payload debug
/// dump and tests — can do so without re-running the strategy logic.
///
/// Region labels:
/// - `1` = first region — compacted history (`SlidingWindow`) or the
///   whole context (`Full` mode, no region split).
/// - `2` = second region — verbatim prior turns (`SlidingWindow` only).
/// - `3` = third region — current turn verbatim (`SlidingWindow` only).
///
/// When `strategy.tool_result_cap` is `Some(cap > 0)`, every tool result
/// in regions ② and ③ is truncated to at most `cap` bytes with the
/// standard `… [truncated N bytes]` marker. `Some(0)` or `None` is a
/// pass-through.
pub(crate) fn build_send_context_with_regions<'a>(
    provider: &dyn Provider,
    raw_context: &'a [serde_json::Value],
    prior_len: usize,
    strategy: &ContextStrategyConfig,
) -> (Cow<'a, [serde_json::Value]>, Vec<u8>) {
    match strategy.mode {
        ContextStrategy::Full => (Cow::Borrowed(raw_context), vec![1; raw_context.len()]),
        ContextStrategy::SlidingWindow => {
            // Mid-loop compression can shorten `raw_context` below `prior_len`;
            // clamp so the slice math never underflows.
            let boundary = prior_len.min(raw_context.len());
            let prior = &raw_context[..boundary];
            let current = &raw_context[boundary..];
            // `tool_result_cap`: Some(0) and None both mean "off" — let the
            // helper short-circuit on cap == 0.
            let cap = strategy.tool_result_cap.unwrap_or(0);

            let mut out = Vec::new();
            let mut regions = Vec::new();

            // The most recent `note_window` prior turns are sent VERBATIM
            // (structured tool_calls + results) instead of being folded into
            // user-role text notes. Text notes taught thinking models
            // (MiniMax, DeepSeek) to write their process — tool calls and
            // action announcements — as content instead of using the
            // structured channel, which leaked "thinking" into delivered
            // replies. Keeping the structured wire format in the recent
            // window restores the format the model should mimic.
            let note_window = strategy.note_window.unwrap_or(usize::MAX);
            let verbatim_start = crate::session::verbatim_start_index(prior, note_window);
            let (compact_prior, verbatim_prior) = prior.split_at(verbatim_start);

            // Older pairs compact to text-only user/assistant. No history
            // notes here — the recent pairs are already verbatim, so notes
            // would be redundant (and are the leak trigger).
            let verbatim_pairs = crate::session::extract_pairs(verbatim_prior).len();
            let compact_keep = strategy.window.saturating_sub(verbatim_pairs);
            let compacted =
                crate::session::extract_user_assistant_pairs(compact_prior, compact_keep, Some(0));
            for msg in &compacted {
                // Defensive: skip messages with no extractable text so the
                // wire payload never contains empty text blocks (which
                // Anthropic rejects, especially under cache_control).
                if let Some(formatted) = format_cleaned_message(provider, msg) {
                    out.push(formatted);
                    regions.push(1);
                }
            }

            // Recent turns verbatim — skip round-tripped history notes
            // (metadata, not conversation; re-sending them as user text is
            // the pattern that triggered the leak). Apply the
            // tool-result byte cap to each message (non-tool messages are
            // pass-through, so the cap is cheap).
            for msg in verbatim_prior {
                if crate::session::is_history_note(msg) {
                    continue;
                }
                out.push(cap_tool_result_content(msg, cap));
                regions.push(2);
            }

            // Current turn verbatim — same cap applies so a 1000-line
            // `read` result mid-loop doesn't blow the budget either.
            for msg in current {
                out.push(cap_tool_result_content(msg, cap));
                regions.push(3);
            }
            (Cow::Owned(out), regions)
        }
    }
}

/// Truncate oversized tool-result `content` to `cap` bytes. Returns the
/// input unchanged for non-tool messages and for `cap == 0` (the
/// explicit-off sentinel). Handles both wire formats:
///
/// - OpenAI / simple: `{"role":"tool","content":"<string>"}` — the string
///   is truncated in place.
/// - Anthropic: `{"role":"user","content":[{"type":"tool_result",
///   "content":"<string>"}, …]}` — each `tool_result` block's string
///   `content` is truncated. Non-`tool_result` blocks (text/image) and
///   block-level array `content` are left untouched (rare for tool
///   results, and the truncate_text marker is string-specific).
fn cap_tool_result_content(msg: &serde_json::Value, cap: usize) -> serde_json::Value {
    if cap == 0 {
        return msg.clone();
    }
    let mut new_msg = msg.clone();
    let role = new_msg.get("role").and_then(|r| r.as_str()).unwrap_or("");

    match role {
        "tool" => {
            let Some(content) = new_msg.get("content").and_then(|c| c.as_str()) else {
                return new_msg;
            };
            let capped = crate::session::truncate_text(content, cap);
            if capped == content {
                return new_msg;
            }
            if let Some(obj) = new_msg.as_object_mut()
                && let Some(v) = obj.get_mut("content")
            {
                *v = serde_json::Value::String(capped);
            }
        }
        "user" => {
            let Some(blocks) = new_msg.get_mut("content").and_then(|c| c.as_array_mut()) else {
                return new_msg;
            };
            for block in blocks {
                if block.get("type").and_then(|t| t.as_str()) != Some("tool_result") {
                    continue;
                }
                let Some(content) = block.get("content").and_then(|c| c.as_str()) else {
                    continue;
                };
                let capped = crate::session::truncate_text(content, cap);
                if capped == content {
                    continue;
                }
                if let Some(obj) = block.as_object_mut()
                    && let Some(v) = obj.get_mut("content")
                {
                    *v = serde_json::Value::String(capped);
                }
            }
        }
        _ => {}
    }
    new_msg
}

/// Test-only debug dump — human-readable, region-labeled view of the
/// wire payload. Used by the test suite to verify the region partition
/// (`with_regions_labels_*` tests). Production debug dumps go through
/// `session::append_wire_payload_dump` instead, which writes
/// structured JSONL — see `docs/context-management.md` / Debug dump.
#[cfg(test)]
fn dump_send_context(
    provider: &dyn Provider,
    raw_context: &[serde_json::Value],
    prior_len: usize,
    strategy: &ContextStrategyConfig,
) -> String {
    let (sent, regions) =
        build_send_context_with_regions(provider, raw_context, prior_len, strategy);
    let mut s = format!(
        "=== context sent to LLM (strategy={:?} window={} note_window={:?}) ===\n\
         {} msgs, ~{} bytes\n\n",
        strategy.mode,
        strategy.window,
        strategy.note_window,
        sent.len(),
        serde_json::to_string(sent.as_ref())
            .map(|s| s.len())
            .unwrap_or(0),
    );
    append_region_summary(
        &mut s,
        sent.as_ref(),
        &regions,
        matches!(strategy.mode, ContextStrategy::Full),
    );
    s.push_str("\n--- wire payload ---\n");
    match serde_json::to_string_pretty(sent.as_ref()) {
        Ok(p) => {
            s.push_str(&p);
            s.push('\n');
        }
        Err(e) => {
            s.push_str(&format!("<serialize failed: {e}>"));
        }
    }
    s
}

#[cfg(test)]
fn append_region_summary(
    s: &mut String,
    sent: &[serde_json::Value],
    regions: &[u8],
    is_full: bool,
) {
    if is_full {
        s.push_str("(Full mode — single region, no windowing)\n");
        for (i, msg) in sent.iter().enumerate() {
            s.push_str(&format!("  [{i}] {}\n", msg_one_line(msg)));
        }
        return;
    }

    // SlidingWindow: walk through msgs, group by region, emit ASCII box per region.
    let region_headers = [
        (1u8, "① Compacted history (text-only)"),
        (2u8, "② Verbatim (structured tool calls kept)"),
        (3u8, "③ Current turn (verbatim)"),
    ];

    let mut first = true;
    for (region, header) in region_headers {
        let msgs_in_region: Vec<(usize, &serde_json::Value)> = sent
            .iter()
            .zip(regions.iter())
            .enumerate()
            .filter(|(_, (_, r))| **r == region)
            .map(|(i, (m, _))| (i, m))
            .collect();
        if msgs_in_region.is_empty() {
            continue;
        }
        if !first {
            s.push_str("├─\n");
        }
        s.push_str(&format!("┌─ {}\n", header));
        for (i, m) in &msgs_in_region {
            s.push_str(&format!("│  [{i}] {}\n", msg_one_line(m)));
        }
        first = false;
    }
    if sent.is_empty() {
        s.push_str("(empty wire payload)\n");
    } else {
        s.push_str("└─\n");
    }
}

#[cfg(test)]
fn msg_one_line(msg: &serde_json::Value) -> String {
    let role = msg.get("role").and_then(|r| r.as_str()).unwrap_or("?");
    let mut out = role.to_string();
    let text = crate::session::extract_message_text(msg);
    if !text.is_empty() {
        let trimmed = text.replace('\n', " ");
        let preview: String = trimmed.chars().take(60).collect();
        out.push_str(&format!(" \"{preview}\""));
    }
    if let Some(calls) = msg.get("tool_calls").and_then(|v| v.as_array())
        && !calls.is_empty()
    {
        out.push_str(&format!(" ({} tool_calls)", calls.len()));
    }
    out
}

#[cfg(test)]
mod render_raw_context_tests {
    use super::*;
    use jyc_types::channel::{ContextStrategy, ContextStrategyConfig};
    use serde_json::json;

    /// Minimal provider that mimics the OpenAI-compatible wire format:
    /// string `content` for both user and assistant. Mirrors the actual
    /// `OpenAICompatProvider` output closely enough for these helpers.
    struct OpenAiCompatProvider;
    #[async_trait::async_trait]
    impl Provider for OpenAiCompatProvider {
        fn name(&self) -> &str {
            "openai-compat"
        }
        fn model(&self) -> &str {
            "test"
        }
        async fn complete(
            &self,
            _messages: &[Message],
            _tools: &[crate::types::ToolDefinition],
            _system: &str,
        ) -> anyhow::Result<crate::provider::EventStream> {
            unimplemented!()
        }
        async fn complete_raw(
            &self,
            _raw_messages: &[serde_json::Value],
            _tools: &[crate::types::ToolDefinition],
            _system: &str,
        ) -> anyhow::Result<crate::provider::EventStream> {
            unimplemented!()
        }
        fn format_user_message(&self, blocks: &[ContentBlock]) -> serde_json::Value {
            let text: String = blocks
                .iter()
                .filter_map(|b| match b {
                    ContentBlock::Text { text } => Some(text.as_str()),
                    _ => None,
                })
                .collect();
            json!({"role": "user", "content": text})
        }
        fn format_tool_result(
            &self,
            tool_call_id: &str,
            content: &str,
            _is_error: bool,
        ) -> serde_json::Value {
            json!({"role": "tool", "tool_call_id": tool_call_id, "content": content})
        }
        fn build_raw_assistant_message(
            &self,
            text: &str,
            _reasoning: &str,
            _tool_calls: &[(String, String, String)],
        ) -> serde_json::Value {
            json!({"role": "assistant", "content": text})
        }
    }

    /// Mirrors `AnthropicProvider` wire format: `content` is an array of
    /// blocks for both user and assistant. Used to assert that
    /// `build_send_context_with_regions` never emits empty text blocks — which Anthropic
    /// rejects with 400 `cache_control cannot be set for empty text blocks`
    /// when `apply_cache_breakpoints` lands on a `n-3`/`n-2` message.
    struct AnthropicProvider;
    #[async_trait::async_trait]
    impl Provider for AnthropicProvider {
        fn name(&self) -> &str {
            "anthropic"
        }
        fn model(&self) -> &str {
            "test"
        }
        async fn complete(
            &self,
            _messages: &[Message],
            _tools: &[crate::types::ToolDefinition],
            _system: &str,
        ) -> anyhow::Result<crate::provider::EventStream> {
            unimplemented!()
        }
        async fn complete_raw(
            &self,
            _raw_messages: &[serde_json::Value],
            _tools: &[crate::types::ToolDefinition],
            _system: &str,
        ) -> anyhow::Result<crate::provider::EventStream> {
            unimplemented!()
        }
        fn format_user_message(&self, blocks: &[ContentBlock]) -> serde_json::Value {
            let content: Vec<serde_json::Value> = blocks
                .iter()
                .filter_map(|b| match b {
                    ContentBlock::Text { text } => Some(json!({"type": "text", "text": text})),
                    _ => None,
                })
                .collect();
            json!({"role": "user", "content": content})
        }
        fn format_tool_result(
            &self,
            _tool_use_id: &str,
            _content: &str,
            _is_error: bool,
        ) -> serde_json::Value {
            unimplemented!()
        }
        fn build_raw_assistant_message(
            &self,
            text: &str,
            _reasoning: &str,
            _tool_calls: &[(String, String, String)],
        ) -> serde_json::Value {
            let mut content = Vec::new();
            if !text.is_empty() {
                content.push(json!({"type": "text", "text": text}));
            }
            json!({"role": "assistant", "content": content})
        }
    }

    fn prov() -> OpenAiCompatProvider {
        OpenAiCompatProvider
    }

    fn anthropic() -> AnthropicProvider {
        AnthropicProvider
    }

    #[test]
    fn renders_user_assistant_tool_sequence() {
        let ctx = vec![
            json!({"role": "user", "content": "fix bug"}),
            json!({
                "role": "assistant",
                "content": "I'll start",
                "reasoning_content": "thinking...",
                "tool_calls": [{
                    "id": "1",
                    "type": "function",
                    "function": {"name": "bash", "arguments": "{}"}
                }]
            }),
            json!({"role": "tool", "tool_call_id": "1", "content": "output"}),
            json!({"role": "assistant", "content": "Done."}),
        ];
        let rendered = render_raw_context_as_text(&ctx);
        assert!(rendered.contains("USER: fix bug"));
        // One turn → one merged ASSISTANT block: first step's text, the
        // folded tool-call annotation with its result, then the reply.
        assert!(rendered.contains("ASSISTANT: I'll start"));
        assert!(rendered.contains("bash() → output"));
        assert!(rendered.contains("Done."));
    }

    #[test]
    fn truncates_long_tool_results() {
        let long = "x".repeat(2000);
        let ctx = vec![
            json!({"role": "user", "content": "u1"}),
            json!({"role": "assistant", "content": "", "tool_calls": [{
                "id": "1", "type": "function",
                "function": {"name": "bash", "arguments": "{}"}
            }]}),
            json!({"role": "tool", "tool_call_id": "1", "content": long}),
        ];
        let rendered = render_raw_context_as_text(&ctx);
        // Result cap is 500 + "…", inside the annotation, plus the
        // header and USER/ASSISTANT framing.
        assert!(rendered.len() < 700);
        assert!(rendered.contains("…"));
    }

    #[test]
    fn skips_unknown_roles_and_empty_content() {
        let ctx = vec![
            json!({"role": "system", "content": "ignored"}),
            json!({"role": "user", "content": ""}),
            json!({"role": "user", "content": "real"}),
            json!({"role": "assistant", "content": "reply"}),
        ];
        let rendered = render_raw_context_as_text(&ctx);
        assert!(!rendered.contains("ignored"));
        assert!(rendered.contains("USER: real"));
        assert!(rendered.contains("ASSISTANT: reply"));
    }

    #[test]
    fn full_strategy_borrows_unchanged() {
        let ctx = vec![
            json!({"role": "user", "content": "u1"}),
            json!({"role": "assistant", "content": "a1"}),
            json!({"role": "user", "content": "u2"}),
        ];
        let prior_len = 2;
        let cfg = ContextStrategyConfig {
            mode: ContextStrategy::Full,
            window: 10,
            note_window: None,
            tool_result_cap: None,
        };
        let sent = build_send_context_with_regions(&prov(), &ctx, prior_len, &cfg).0;
        assert!(matches!(sent, Cow::Borrowed(_)));
        assert_eq!(sent.len(), ctx.len());
    }

    #[test]
    fn sliding_window_emits_recent_verbatim_and_older_compacted() {
        // Prior: 3 user+assistant turns, plus a tool turn in the middle.
        let prior = vec![
            json!({"role": "user", "content": "u1"}),
            json!({"role": "assistant", "content": "a1"}),
            json!({"role": "user", "content": "u2"}),
            json!({"role": "assistant", "content": "a2", "tool_calls": [{"id":"1","type":"function","function":{"name":"bash","arguments":"{}"}}]}),
            json!({"role": "tool", "tool_call_id": "1", "content": "out"}),
            json!({"role": "user", "content": "u3"}),
            json!({"role": "assistant", "content": "a3"}),
        ];
        let current = vec![
            json!({"role": "user", "content": "u4"}),
            json!({"role": "assistant", "content": "a4", "tool_calls": [{"id":"2","type":"function","function":{"name":"bash","arguments":"{}"}}]}),
            json!({"role": "tool", "tool_call_id": "2", "content": "out2"}),
        ];
        let prior_len = prior.len();
        let mut ctx = prior.clone();
        ctx.extend(current.clone());

        // window=2, note_window=1: the last prior turn (u3, a3) is verbatim,
        // the older turn (u2 with its tool call) compacts to text-only. No
        // history note anywhere — notes taught the model to write tool calls
        // as text (the thinking-leak trigger).
        let cfg = ContextStrategyConfig {
            mode: ContextStrategy::SlidingWindow,
            window: 2,
            note_window: Some(1),
            tool_result_cap: None,
        };
        let sent = build_send_context_with_regions(&prov(), &ctx, prior_len, &cfg).0;
        let sent = sent.into_owned();

        // 1. Older pair compacted to pure text (no note, tool_calls dropped).
        assert_eq!(sent[0], json!({"role": "user", "content": "u2"}));
        assert_eq!(sent[1], json!({"role": "assistant", "content": "a2"}));

        // 2. Recent pair verbatim.
        assert_eq!(sent[2], json!({"role": "user", "content": "u3"}));
        assert_eq!(sent[3], json!({"role": "assistant", "content": "a3"}));

        // 3. Current turn verbatim (tool_calls preserved).
        assert_eq!(sent[4], current[0]);
        assert_eq!(sent[5], current[1]);
        assert_eq!(sent[6], current[2]);
    }

    #[test]
    fn sliding_window_prior_shorter_than_raw_context() {
        // After mid-loop compression, prior_len could exceed raw_context.len().
        let ctx = vec![
            json!({"role": "user", "content": "u1"}),
            json!({"role": "assistant", "content": "a1"}),
        ];
        let cfg = ContextStrategyConfig {
            mode: ContextStrategy::SlidingWindow,
            window: 5,
            note_window: None,
            tool_result_cap: None,
        };
        let sent = build_send_context_with_regions(&prov(), &ctx, 99, &cfg)
            .0
            .into_owned();
        // boundary clamps to raw_context.len(), so the whole ctx is
        // treated as prior. Expected: 1 complete pair (u1, a1) = 2 messages.
        assert_eq!(sent.len(), 2);
        assert_eq!(sent[0], json!({"role": "user", "content": "u1"}));
        assert_eq!(sent[1], json!({"role": "assistant", "content": "a1"}));
    }

    #[test]
    fn sliding_window_empty_prior_returns_only_current() {
        let ctx = vec![
            json!({"role": "user", "content": "current"}),
            json!({"role": "assistant", "content": "reply"}),
        ];
        let cfg = ContextStrategyConfig {
            mode: ContextStrategy::SlidingWindow,
            window: 10,
            note_window: None,
            tool_result_cap: None,
        };
        let sent = build_send_context_with_regions(&prov(), &ctx, 0, &cfg)
            .0
            .into_owned();
        // No prior → no windowed. Just current turn verbatim.
        assert_eq!(sent, ctx);
    }

    /// Regression: with Anthropic-shaped prior context, sliding-window must
    /// not produce empty text blocks (Anthropic rejects them, especially
    /// under cache_control on `messages[n-3]`/`[n-2]`). The compacted part
    /// goes through provider reformatting; the recent verbatim part is
    /// passed through untouched (with tool_use/tool_result kept paired).
    #[test]
    fn sliding_window_anthropic_shaped_prior_emits_no_empty_blocks() {
        let prior = vec![
            json!({"role": "user", "content": [{"type": "text", "text": "u1"}]}),
            json!({"role": "assistant", "content": [{"type": "text", "text": "a1"}]}),
            json!({"role": "assistant", "content": [
                {"type": "text", "text": "calling bash"},
                {"type": "tool_use", "id": "t1", "name": "bash", "input": {}},
            ]}),
            // Anthropic tool result: role "user" with tool_result block, no text.
            json!({"role": "user", "content": [{"type": "tool_result", "tool_use_id": "t1", "content": "ok"}]}),
            json!({"role": "user", "content": [{"type": "text", "text": "u2"}]}),
            json!({"role": "assistant", "content": [{"type": "text", "text": "a2"}]}),
        ];
        let current = vec![
            json!({"role": "user", "content": [{"type": "text", "text": "u3"}]}),
            json!({"role": "assistant", "content": [{"type": "text", "text": "a3"}]}),
        ];
        let prior_len = prior.len();
        let mut ctx = prior.clone();
        ctx.extend(current.clone());

        // note_window=1: only the last prior turn (u2, a2) is verbatim; the
        // older u1 turn (with its tool_use + tool_result) compacts to text.
        let cfg = ContextStrategyConfig {
            mode: ContextStrategy::SlidingWindow,
            window: 4,
            note_window: Some(1),
            tool_result_cap: None,
        };
        let sent = build_send_context_with_regions(&anthropic(), &ctx, prior_len, &cfg)
            .0
            .into_owned();

        // Walk every emitted message: assert no text block has empty text.
        for (i, msg) in sent.iter().enumerate() {
            let role = msg["role"].as_str().unwrap_or("");
            if let Some(blocks) = msg["content"].as_array() {
                for b in blocks {
                    if b["type"].as_str() == Some("text") {
                        let text = b["text"].as_str().unwrap_or("");
                        assert!(
                            !text.is_empty(),
                            "empty text block at messages.{i} (role={role})",
                        );
                    }
                }
            }
        }

        // Older u1 turn compacted: text merged ("a1\ncalling bash"), no
        // history note (notes are the thinking-leak trigger), no tool blocks.
        assert!(
            sent.iter()
                .any(|m| m["role"] == "user" && m["content"][0]["text"].as_str() == Some("u1"))
        );
        assert!(sent.iter().any(|m| m["role"] == "assistant"
            && m["content"][0]["text"].as_str() == Some("a1\ncalling bash")));
        assert!(
            !sent.iter().any(|m| m["content"]
                .as_str()
                .is_some_and(|t| t.starts_with("[History note]"))),
            "no history note may reach the wire"
        );

        // Recent u2 turn verbatim (passed through untouched).
        assert!(
            sent.iter()
                .any(|m| m["role"] == "user" && m["content"][0]["text"].as_str() == Some("u2"))
        );
        assert!(sent.iter().any(|m| m["role"] == "assistant"
            && m["content"][0]["text"].as_str() == Some("a2")));

        // Current turn preserved verbatim at the tail.
        let tail_start = sent.len() - current.len();
        assert_eq!(&sent[tail_start..], current.as_slice());
    }

    /// The most recent `note_window` prior turns are sent VERBATIM: their
    /// structured tool_calls and tool results are preserved (not stripped
    /// into text notes), so the model keeps seeing the wire format it is
    /// supposed to emit.
    #[test]
    fn sliding_window_recent_turn_keeps_structured_tool_calls() {
        let prior = vec![
            json!({"role": "user", "content": "u1"}),
            json!({"role": "assistant", "content": "a1"}),
            json!({"role": "user", "content": "u2"}),
            json!({"role": "assistant", "content": "calling bash", "tool_calls": [{"id":"1","type":"function","function":{"name":"bash","arguments":"{\"command\":\"ls\"}"}}]}),
            json!({"role": "tool", "tool_call_id": "1", "content": "file.txt"}),
            json!({"role": "assistant", "content": "done"}),
        ];
        let current = vec![json!({"role": "user", "content": "u3"})];
        let prior_len = prior.len();
        let mut ctx = prior.clone();
        ctx.extend(current.clone());

        // note_window=1: the last prior turn (u2 → bash → done) is verbatim.
        let cfg = ContextStrategyConfig {
            mode: ContextStrategy::SlidingWindow,
            window: 5,
            note_window: Some(1),
            tool_result_cap: None,
        };
        let sent = build_send_context_with_regions(&prov(), &ctx, prior_len, &cfg)
            .0
            .into_owned();

        // Older turn compacted text-only.
        assert_eq!(sent[0], json!({"role": "user", "content": "u1"}));
        assert_eq!(sent[1], json!({"role": "assistant", "content": "a1"}));
        // Recent turn VERBATIM: user, structured tool-call assistant, result.
        assert_eq!(sent[2], json!({"role": "user", "content": "u2"}));
        assert_eq!(sent[3], prior[3]);
        assert_eq!(sent[4], prior[4]);
        assert_eq!(sent[5], json!({"role": "assistant", "content": "done"}));
        // Current turn verbatim.
        assert_eq!(sent[6], current[0]);
    }

    /// Round-tripped history notes in the prior (from heuristic compaction)
    /// must NOT be re-sent in the verbatim region — they are metadata, and
    /// re-injecting them as user text is the pattern that triggered the
    /// thinking-content leak.
    #[test]
    fn sliding_window_filters_round_tripped_notes_from_verbatim_region() {
        let prior = vec![
            json!({"role": "user", "content": "u1"}),
            json!({"role": "user", "content": "[History note] assistant tool calls: bash(command=\"ls\") → ok"}),
            json!({"role": "assistant", "content": "a1"}),
            json!({"role": "user", "content": "u2"}),
            json!({"role": "assistant", "content": "a2"}),
        ];
        let current = vec![json!({"role": "user", "content": "u3"})];
        let prior_len = prior.len();
        let mut ctx = prior.clone();
        ctx.extend(current.clone());

        // note_window=5: every prior turn is in the verbatim region.
        let cfg = ContextStrategyConfig {
            mode: ContextStrategy::SlidingWindow,
            window: 10,
            note_window: Some(5),
            tool_result_cap: None,
        };
        let sent = build_send_context_with_regions(&prov(), &ctx, prior_len, &cfg)
            .0
            .into_owned();

        // The round-tripped note is filtered out of the verbatim pass-through.
        let texts: Vec<&str> = sent.iter().filter_map(|m| m["content"].as_str()).collect();
        assert!(
            !texts.iter().any(|t| t.starts_with("[History note]")),
            "history note must not reach the wire, got: {texts:?}"
        );
        assert_eq!(
            sent,
            vec![
                json!({"role": "user", "content": "u1"}),
                json!({"role": "assistant", "content": "a1"}),
                json!({"role": "user", "content": "u2"}),
                json!({"role": "assistant", "content": "a2"}),
                json!({"role": "user", "content": "u3"}),
            ]
        );
    }

    /// `with_regions` and `dump_send_context` — verify region partition is
    /// stable across `Full` and `SlidingWindow` strategies.
    #[test]
    fn with_regions_labels_full_mode_as_single_region() {
        let ctx = vec![
            json!({"role": "user", "content": "u1"}),
            json!({"role": "assistant", "content": "a1"}),
            json!({"role": "user", "content": "u2"}),
        ];
        let cfg = ContextStrategyConfig {
            mode: ContextStrategy::Full,
            window: 10,
            note_window: None,
            tool_result_cap: None,
        };
        let (sent, regions) = build_send_context_with_regions(&prov(), &ctx, 0, &cfg);
        assert_eq!(sent.len(), 3);
        assert_eq!(regions, vec![1, 1, 1]);
        let dump = dump_send_context(&prov(), &ctx, 0, &cfg);
        assert!(
            dump.contains("strategy=Full"),
            "missing strategy summary: {dump}"
        );
        assert!(
            dump.contains("single region"),
            "Full dump should say single region: {dump}"
        );
        assert!(dump.contains("\"u1\""), "missing msg preview: {dump}");
    }

    #[test]
    fn with_regions_labels_sliding_window_three_regions() {
        // ① compacted = (u1, a1), ② verbatim = (u2, a2), ③ current = (u3).
        let prior = vec![
            json!({"role": "user", "content": "u1"}),
            json!({"role": "assistant", "content": "a1"}),
            json!({"role": "user", "content": "u2"}),
            json!({"role": "assistant", "content": "a2"}),
        ];
        let current = vec![json!({"role": "user", "content": "u3"})];
        let mut ctx = prior.clone();
        ctx.extend(current.clone());
        let cfg = ContextStrategyConfig {
            mode: ContextStrategy::SlidingWindow,
            window: 2,
            note_window: Some(1),
            tool_result_cap: None,
        };
        let (sent, regions) = build_send_context_with_regions(&prov(), &ctx, prior.len(), &cfg);
        assert_eq!(sent.len(), 5);
        assert_eq!(regions, vec![1, 1, 2, 2, 3]);

        let dump = dump_send_context(&prov(), &ctx, prior.len(), &cfg);
        assert!(
            dump.contains("① Compacted history"),
            "missing region ①: {dump}"
        );
        assert!(dump.contains("② Verbatim"), "missing region ②: {dump}");
        assert!(dump.contains("③ Current turn"), "missing region ③: {dump}");
        assert!(
            dump.contains("--- wire payload ---"),
            "missing payload section: {dump}"
        );
    }

    /// `cap_tool_result_content` — unit tests for the byte cap helper.
    /// Integration via `build_send_context_with_regions` is covered by
    /// `build_send_context_applies_cap_to_verbatim_region` below.
    #[test]
    fn cap_tool_result_truncates_openai_shape() {
        let big = "x".repeat(2000);
        let msg = json!({"role": "tool", "tool_call_id": "1", "content": big.clone()});
        let capped = cap_tool_result_content(&msg, 100);
        let content = capped["content"].as_str().unwrap();
        assert!(content.len() < 200, "should be much smaller than 2000");
        assert!(content.starts_with('x'), "preserves leading content");
        assert!(content.contains("[truncated"), "appends truncation marker");
        assert!(content.contains("bytes]"), "marker mentions bytes");
        // The marker must report the exact number of dropped bytes —
        // 1900 for a 100-byte cap on a 2000-byte input.
        assert!(
            content.contains("[truncated 1900 bytes]"),
            "marker should report exact dropped bytes, got: {content}"
        );
    }

    #[test]
    fn cap_tool_result_passthrough_when_under_cap() {
        let msg = json!({"role": "tool", "tool_call_id": "1", "content": "short"});
        let capped = cap_tool_result_content(&msg, 100);
        // Identity check (no clone-modify churn) — should be the same content.
        assert_eq!(capped["content"].as_str().unwrap(), "short");
        assert!(!capped["content"].as_str().unwrap().contains("truncated"));
    }

    #[test]
    fn cap_tool_result_passthrough_for_user_or_assistant() {
        // Only `role=tool` (OpenAI) and `role=user` with tool_result
        // blocks (Anthropic) get capped — every other role is a no-op.
        let msg = json!({"role": "assistant", "content": "x".repeat(2000)});
        let capped = cap_tool_result_content(&msg, 100);
        assert_eq!(capped, msg, "assistant should pass through unchanged");

        let msg = json!({"role": "user", "content": "x".repeat(2000)});
        let capped = cap_tool_result_content(&msg, 100);
        assert_eq!(capped, msg, "user with string content (no tool_result blocks) should pass through");
    }

    #[test]
    fn cap_tool_result_zero_is_passthrough() {
        // Some(0) is the explicit-off sentinel.
        let msg = json!({"role": "tool", "tool_call_id": "1", "content": "x".repeat(2000)});
        let capped = cap_tool_result_content(&msg, 0);
        assert_eq!(capped, msg);
    }

    #[test]
    fn cap_tool_result_truncates_anthropic_shape() {
        let big = "x".repeat(2000);
        let msg = json!({
            "role": "user",
            "content": [
                {"type": "text", "text": "framing text"},
                {"type": "tool_result", "tool_use_id": "t1", "content": big},
                // A second tool_result in the same user message — both
                // should be capped.
                {"type": "tool_result", "tool_use_id": "t2", "content": "short"},
                // A non-tool_result block — should pass through.
                {"type": "image", "source": {"type": "base64", "data": "..."}},
            ]
        });
        let capped = cap_tool_result_content(&msg, 100);
        let blocks = capped["content"].as_array().unwrap();
        assert_eq!(blocks.len(), 4, "block count preserved");
        assert_eq!(blocks[0]["text"], "framing text");
        assert!(blocks[1]["content"].as_str().unwrap().contains("[truncated"));
        assert_eq!(blocks[2]["content"].as_str().unwrap(), "short");
        assert_eq!(blocks[3], json!({"type": "image", "source": {"type": "base64", "data": "..."}}));
    }

    /// Integration: a real-shaped sliding window with a 50-byte cap must
    /// reduce the serialized payload size vs. uncapped, and every capped
    /// tool result must carry the standard marker.
    #[test]
    fn build_send_context_applies_cap_to_verbatim_region() {
        let big = "y".repeat(2000);
        let prior = vec![
            json!({"role": "user", "content": "u1"}),
            json!({"role": "assistant", "content": "a1"}),
            json!({"role": "user", "content": "u2"}),
            json!({"role": "assistant", "content": "calling bash", "tool_calls": [{"id":"1","type":"function","function":{"name":"bash","arguments":"{}"}}]}),
            json!({"role": "tool", "tool_call_id": "1", "content": big.clone()}),
            json!({"role": "assistant", "content": "done"}),
        ];
        let current = vec![json!({"role": "tool", "tool_call_id": "2", "content": big.clone()})];
        let mut ctx = prior.clone();
        ctx.extend(current.clone());
        let prior_len = prior.len();

        let uncapped_cfg = ContextStrategyConfig {
            mode: ContextStrategy::SlidingWindow,
            window: 5,
            note_window: Some(5),
            tool_result_cap: None,
        };
        let (uncapped, _) =
            build_send_context_with_regions(&prov(), &ctx, prior_len, &uncapped_cfg);
        let uncapped_bytes = serde_json::to_string(uncapped.as_ref()).unwrap().len();

        let capped_cfg = ContextStrategyConfig {
            tool_result_cap: Some(50),
            ..uncapped_cfg.clone()
        };
        let (capped, _) =
            build_send_context_with_regions(&prov(), &ctx, prior_len, &capped_cfg);
        let capped_bytes = serde_json::to_string(capped.as_ref()).unwrap().len();

        assert!(
            capped_bytes < uncapped_bytes,
            "capping should reduce payload size: uncapped={uncapped_bytes} capped={capped_bytes}"
        );
        // The cap should remove most of the 2000-byte bodies.
        assert!(
            uncapped_bytes - capped_bytes > 3000,
            "cap should drop at least 3KB across two tool results: uncapped={uncapped_bytes} capped={capped_bytes}"
        );

        // The standard truncation marker must appear in the capped payload.
        let capped_str = serde_json::to_string(capped.as_ref()).unwrap();
        assert!(
            capped_str.contains("[truncated ") && capped_str.contains("bytes]"),
            "missing truncation marker: {capped_str}"
        );
    }

    #[test]
    fn build_send_context_cap_zero_is_passthrough() {
        // Some(0) is the explicit-off sentinel — must match None behavior.
        let big = "y".repeat(2000);
        let prior = vec![
            json!({"role": "user", "content": "u1"}),
            json!({"role": "assistant", "content": "a1"}),
            json!({"role": "user", "content": "u2"}),
            json!({"role": "assistant", "content": "calling", "tool_calls": [{"id":"1","type":"function","function":{"name":"bash","arguments":"{}"}}]}),
            json!({"role": "tool", "tool_call_id": "1", "content": big}),
        ];
        let prior_len = prior.len();
        let cfg_none = ContextStrategyConfig {
            mode: ContextStrategy::SlidingWindow,
            window: 5,
            note_window: Some(5),
            tool_result_cap: None,
        };
        let cfg_zero = ContextStrategyConfig {
            tool_result_cap: Some(0),
            ..cfg_none.clone()
        };
        let (a, _) = build_send_context_with_regions(&prov(), &prior, prior_len, &cfg_none);
        let (b, _) = build_send_context_with_regions(&prov(), &prior, prior_len, &cfg_zero);
        assert_eq!(
            serde_json::to_string(a.as_ref()).unwrap(),
            serde_json::to_string(b.as_ref()).unwrap(),
            "Some(0) must behave identically to None"
        );
    }
}
